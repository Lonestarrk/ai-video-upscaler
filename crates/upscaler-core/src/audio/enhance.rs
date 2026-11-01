//! Quality enhancement by band-splice: the original below the source's codec cutoff, only
//! the top band from the AI (Apollo) above it, level-controlled per 1/3 octave.
//!
//! The splice is `original - HP(original) + G * HP(model)`: the low part is the exact
//! complement of the zero-phase LR4 high-pass, so with `model == original` and `G = 1` the
//! result is the original bit for bit, and both paths are zero-phase, so they line up.
//! `G` is a static gain per 1/3-octave band above the cutoff (one set per call; the audio
//! job calls this per section of about 300 s). It keeps every band
//! - at or above the original's level (never duller: Apollo's top band on an AC-3 source
//!   measured about 3 dB below the source), and
//! - at or below a cap: a least-squares line through the original's 1/3-octave levels
//!   between 6 kHz and the cutoff (dB against log2 f), taken at the cutoff and extended
//!   from there at -12 dB/octave.
//!
//! Within those bounds each band aims at the model's own level. When the two bounds
//! conflict (the original itself above the cap) the original's level wins; when the model
//! cannot reach the original's level (nearly empty band) the original's band is kept.
//! Below the cutoff, where the model only shows through the high-pass skirt, its gain is
//! level-matched to the original in the band just below the cutoff.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Result};
use rustfft::{num_complex::Complex, FftPlanner};

use super::ai::AudioAi;
use super::dsp::{lr4_highpass, zero_phase};
use super::io::{read_wav, write_wav, Pcm, RATE};
use crate::process::Cancelled;

/// Analysis FFT size (5.9 Hz bins at 48 kHz) and hop of the averaged spectra.
const FFT: usize = 8192;
const HOP: usize = FFT / 2;
/// `detect_cutoff`'s answer when the spectrum does not drop: nothing for the AI to add.
pub const NO_CUTOFF_HZ: f32 = 20_000.0;
const MIN_CUTOFF_HZ: f32 = 10_000.0;
const DROP_DB: f32 = 15.0;
const REFERENCE_BAND: (f32, f32) = (8_000.0, 12_000.0);
/// Half-width of the box smoothing of the averaged power spectrum.
const SMOOTH_HZ: f32 = 150.0;
/// The cap's reference slope is fitted to the original's bands from here to the cutoff.
const SLOPE_FROM_HZ: f32 = 6_000.0;
const CAP_DB_PER_OCTAVE: f32 = -12.0;
/// Most a band of the model is raised to reach the original's level; beyond that the
/// original's own band is kept instead.
const MAX_BOOST_DB: f32 = 12.0;
/// Linear-phase FIR that applies the per-band gains (delay removed, so zero-phase).
const FIR_TAPS: usize = 4097;
const CONV_FFT: usize = 65_536;

fn power_db(p: f64) -> f32 {
    (10.0 * p.max(1e-30).log10()) as f32
}

fn db_power(db: f32) -> f64 {
    10f64.powf(db as f64 / 10.0)
}

fn bin_hz() -> f32 {
    RATE as f32 / FFT as f32
}

/// Welch analysis: Hann-windowed FFT frames (8192, hop 4096) of signals of equal length,
/// the last frame zero-padded. `visit` gets the spectra of all signals for each frame;
/// bins `0..=FFT/2` are the ones that matter.
fn welch(signals: &[&[f32]], mut visit: impl FnMut(&[Vec<Complex<f32>>])) {
    let n = signals.iter().map(|s| s.len()).max().unwrap_or(0);
    if n == 0 {
        return;
    }
    let fft = FftPlanner::<f32>::new().plan_fft_forward(FFT);
    let window: Vec<f32> = (0..FFT)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT as f32).cos())
        .collect();
    let mut bufs = vec![vec![Complex::new(0.0, 0.0); FFT]; signals.len()];
    let mut start = 0;
    loop {
        for (s, buf) in signals.iter().zip(bufs.iter_mut()) {
            for (i, b) in buf.iter_mut().enumerate() {
                let v = s.get(start + i).copied().unwrap_or(0.0);
                *b = Complex::new(v * window[i], 0.0);
            }
            fft.process(buf);
        }
        visit(&bufs);
        if start + FFT >= n {
            break;
        }
        start += HOP;
    }
}

/// Frequency where the smoothed spectrum (FFT 8192, averaged over the whole input and all
/// channels) first falls 15 dB below its 8-12 kHz average, clamped to 10-20 kHz.
/// [`NO_CUTOFF_HZ`] (20 kHz) when it does not drop below 20 kHz, or for silence.
pub fn detect_cutoff(x: &Pcm) -> f32 {
    let chans: Vec<Vec<f32>> = (0..x.channels).map(|c| x.channel(c)).collect();
    let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
    let mut power = vec![0f64; FFT / 2 + 1];
    welch(&refs, |specs| {
        for s in specs {
            for (p, v) in power.iter_mut().zip(s) {
                *p += v.norm_sqr() as f64;
            }
        }
    });
    let hz = bin_hz();
    let bin = |f: f32| (f / hz).round() as usize;
    let mean = |a: usize, b: usize| power[a..=b].iter().sum::<f64>() / (b - a + 1) as f64;
    let reference = mean(bin(REFERENCE_BAND.0), bin(REFERENCE_BAND.1));
    if reference <= 1e-20 {
        return NO_CUTOFF_HZ;
    }
    let threshold = reference * db_power(-DROP_DB);
    let w = bin(SMOOTH_HZ);
    let first = bin(MIN_CUTOFF_HZ);
    let mut k = first;
    while (k as f32) * hz < NO_CUTOFF_HZ {
        if mean(k - w, k + w) < threshold {
            // Already down at the start: the drop is at 10 kHz or lower.
            return if k == first {
                MIN_CUTOFF_HZ
            } else {
                k as f32 * hz
            };
        }
        k += 1;
    }
    NO_CUTOFF_HZ
}

/// 1/3-octave bands anchored at the cutoff, edges `cutoff * 2^(k/3)`: whole bands from
/// 6 kHz up to the cutoff, then bands up to Nyquist (a sliver of less than 1/6 octave at
/// the top is merged into the band below).
fn band_grid(cutoff: f32) -> Vec<(f32, f32)> {
    let nyquist = RATE as f32 / 2.0;
    let edge = |k: i32| cutoff * 2f32.powf(k as f32 / 3.0);
    let mut below = Vec::new();
    let mut k = -1;
    while edge(k) >= SLOPE_FROM_HZ * 0.9999 {
        below.push((edge(k), edge(k + 1)));
        k -= 1;
    }
    below.reverse();
    let mut grid = below;
    let mut k = 0;
    while edge(k) < nyquist {
        let (lo, hi) = (edge(k), edge(k + 1).min(nyquist));
        match grid.last_mut() {
            Some(last) if k > 0 && lo * 2f32.powf(1.0 / 6.0) >= nyquist => last.1 = hi,
            _ => grid.push((lo, hi)),
        }
        k += 1;
    }
    grid
}

/// Which band of `grid` each analysis bin falls in.
fn bins_to_bands(grid: &[(f32, f32)]) -> Vec<Option<usize>> {
    let hz = bin_hz();
    let nyquist = RATE as f32 / 2.0;
    (0..=FFT / 2)
        .map(|k| {
            let f = k as f32 * hz;
            grid.iter()
                .position(|&(lo, hi)| f >= lo && (f < hi || (hi >= nyquist && f <= hi)))
        })
        .collect()
}

/// `(lo, hi, dB)` per band of the grid anchored at `cutoff`: the summed averaged power
/// of the bins in the band (a 1/3-octave level, relative units).
pub fn band_levels_db(x: &[f32], cutoff: f32) -> Vec<(f32, f32, f32)> {
    let grid = band_grid(cutoff);
    let map = bins_to_bands(&grid);
    let mut p = vec![0f64; grid.len()];
    welch(&[x], |s| {
        for (k, b) in map.iter().enumerate() {
            if let Some(b) = b {
                p[*b] += s[0][k].norm_sqr() as f64;
            }
        }
    });
    grid.iter()
        .zip(&p)
        .map(|(&(lo, hi), &p)| (lo, hi, power_db(p)))
        .collect()
}

/// The cap's reference: a least-squares line through the levels of the bands between
/// 6 kHz and the cutoff (dB against log2 of the band centre), evaluated at the cutoff.
/// One band gives its own level; none gives `None` (no cap).
pub fn reference_at_cutoff(levels: &[(f32, f32, f32)], cutoff: f32) -> Option<f32> {
    let pts: Vec<(f64, f64)> = levels
        .iter()
        .filter(|b| b.1 <= cutoff * 1.0001)
        .map(|&(lo, hi, db)| (((lo * hi).sqrt() as f64).log2(), db as f64))
        .collect();
    match pts.len() {
        0 => None,
        1 => Some(pts[0].1 as f32),
        n => {
            let n = n as f64;
            let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
            let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
            let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
            let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
            let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
            Some((my + slope * ((cutoff as f64).log2() - mx)) as f32)
        }
    }
}

/// Real zero-phase FIR (odd length, Hann-windowed) for a gain per analysis bin.
fn fir_from_curve(gain: &[f32]) -> Vec<f32> {
    let mut spec: Vec<Complex<f32>> = (0..FFT)
        .map(|k| Complex::new(gain[k.min(FFT - k)], 0.0))
        .collect();
    FftPlanner::<f32>::new()
        .plan_fft_inverse(FFT)
        .process(&mut spec);
    let half = FIR_TAPS / 2;
    (0..FIR_TAPS)
        .map(|j| {
            let w =
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * j as f32 / (FIR_TAPS - 1) as f32).cos();
            spec[(j + FFT - half) % FFT].re / FFT as f32 * w
        })
        .collect()
}

/// `x` filtered by the symmetric FIR `h` with its delay removed (same length as `x`).
fn convolve_centered(x: &[f32], h: &[f32]) -> Vec<f32> {
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(CONV_FFT);
    let inv = planner.plan_fft_inverse(CONV_FFT);
    let block = CONV_FFT - h.len() + 1;
    let mut hf: Vec<Complex<f32>> = (0..CONV_FFT)
        .map(|i| Complex::new(h.get(i).copied().unwrap_or(0.0), 0.0))
        .collect();
    fwd.process(&mut hf);
    let mut full = vec![0f32; x.len() + h.len() - 1];
    let mut buf = vec![Complex::new(0.0, 0.0); CONV_FFT];
    for start in (0..x.len()).step_by(block) {
        let chunk = &x[start..(start + block).min(x.len())];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = Complex::new(chunk.get(i).copied().unwrap_or(0.0), 0.0);
        }
        fwd.process(&mut buf);
        for (b, g) in buf.iter_mut().zip(&hf) {
            *b *= g;
        }
        inv.process(&mut buf);
        for (i, b) in buf.iter().take(chunk.len() + h.len() - 1).enumerate() {
            full[start + i] += b.re / CONV_FFT as f32;
        }
    }
    let half = h.len() / 2;
    full[half..half + x.len()].to_vec()
}

/// One channel of [`splice`].
fn splice_channel(o: &[f32], m: &[f32], cutoff: f32) -> Vec<f32> {
    let rate = RATE as f32;
    let hp = |s: &[f32]| zero_phase(s, |v| lr4_highpass(v, cutoff, rate));
    let ho = hp(o);
    let hm = hp(m);
    let low: Vec<f32> = o.iter().zip(&ho).map(|(a, b)| a - b).collect();

    let grid = band_grid(cutoff);
    let map = bins_to_bands(&grid);
    let nb = grid.len();
    // Per band: original, low part, model's top, low part x model's top (cross power),
    // and the whole model.
    let mut acc = vec![[0f64; 5]; nb];
    welch(&[o, &low, &hm, m], |s| {
        for (k, b) in map.iter().enumerate() {
            if let Some(b) = *b {
                let a = &mut acc[b];
                a[0] += s[0][k].norm_sqr() as f64;
                a[1] += s[1][k].norm_sqr() as f64;
                a[2] += s[2][k].norm_sqr() as f64;
                a[3] += (s[1][k] * s[2][k].conj()).re as f64;
                a[4] += s[3][k].norm_sqr() as f64;
            }
        }
    });
    let levels: Vec<(f32, f32, f32)> = grid
        .iter()
        .zip(&acc)
        .map(|(&(lo, hi), a)| (lo, hi, power_db(a[0])))
        .collect();
    let reference = reference_at_cutoff(&levels, cutoff);
    let max_gain = db_power(MAX_BOOST_DB).sqrt();

    // Below the cutoff the model only shows through the high-pass skirt: level-match it
    // to the original in the band just below, so a quieter or louder model does not dent
    // or bump the crossover.
    let skirt = grid
        .iter()
        .rposition(|b| b.1 <= cutoff * 1.0001)
        .map(|b| (acc[b][0], acc[b][4]))
        .filter(|&(_, pr)| pr > 1e-20)
        .map_or(1.0, |(po, pr)| (po / pr).sqrt().min(max_gain)) as f32;

    // Gain on the model's top and share of the original's top kept, per band. The target
    // is the model's own band level (so the skirt's loss in the first band is made up),
    // held between the original's level and the cap.
    let mut gain = vec![1f32; nb];
    let mut keep = vec![0f32; nb];
    let first_top = grid.iter().position(|b| b.0 >= cutoff * 0.9999);
    for b in first_top.into_iter().flat_map(|f| f..nb) {
        let [po, py, pm, px, pr] = acc[b];
        let centre = (grid[b].0 * grid[b].1).sqrt();
        let cap = reference
            .map(|r| db_power(r + CAP_DB_PER_OCTAVE * (centre / cutoff).log2()))
            .unwrap_or(f64::INFINITY);
        let target = pr.max(po).min(cap.max(po));
        // Level with gain g: py + 2 g px + g^2 pm = target. The larger root is the one
        // that crosses the target, as py <= po <= target (the low part is the original
        // through 1 - HP).
        let disc = px * px - pm * (py - target);
        let g = if pm > 1e-20 && disc >= 0.0 {
            ((-px + disc.sqrt()) / pm).max(0.0)
        } else {
            f64::INFINITY
        };
        if (g - 1.0).abs() < 1e-3 {
            // Within 0.01 dB: leave the band as it is.
        } else if g <= max_gain {
            gain[b] = g as f32;
        } else {
            // The model has (almost) nothing here: keep the original's band.
            gain[b] = 0.0;
            keep[b] = 1.0;
        }
    }
    let skirt = if (skirt - 1.0).abs() < 1e-3 {
        1.0
    } else {
        skirt
    };
    if skirt == 1.0 && gain.iter().all(|&g| g == 1.0) {
        return low.iter().zip(&hm).map(|(a, b)| a + b).collect();
    }
    // Per analysis bin: the band's value above the cutoff, `below` under it.
    let curve = |v: &[f32], below: f32| -> Vec<f32> {
        map.iter()
            .enumerate()
            .map(|(k, b)| match b {
                Some(b) if k as f32 * bin_hz() >= cutoff * 0.9999 => v[*b],
                _ => below,
            })
            .collect()
    };
    let top = convolve_centered(&hm, &fir_from_curve(&curve(&gain, skirt)));
    let kept = if keep.iter().any(|&k| k > 0.0) {
        convolve_centered(&ho, &fir_from_curve(&curve(&keep, 0.0)))
    } else {
        vec![0.0; o.len()]
    };
    (0..o.len()).map(|i| low[i] + top[i] + kept[i]).collect()
}

/// The original below `cutoff`, the model's top band above it (see the module docs).
/// The model is padded or trimmed to the original's length; its channel count must match
/// (asserted). A cutoff of 20 kHz or more returns the original unchanged. 48 kHz.
pub fn splice(original: &Pcm, model: &Pcm, cutoff: f32) -> Pcm {
    if cutoff >= NO_CUTOFF_HZ || original.frames() == 0 {
        return original.clone();
    }
    assert_eq!(
        original.channels, model.channels,
        "splice needs equal channels"
    );
    let n = original.frames();
    let chans: Vec<Vec<f32>> = (0..original.channels)
        .map(|c| {
            let mut m = model.channel(c);
            m.resize(n, 0.0);
            splice_channel(&original.channel(c), &m, cutoff)
        })
        .collect();
    Pcm::from_channels(original.rate, &chans)
}

/// Stereo L/R to M/S: `M = (L + R) / 2`, `S = (L - R) / 2`.
pub fn to_mid_side(p: &Pcm) -> Pcm {
    assert_eq!(p.channels, 2, "mid/side needs stereo");
    let samples = p
        .samples
        .chunks_exact(2)
        .flat_map(|f| [(f[0] + f[1]) * 0.5, (f[0] - f[1]) * 0.5])
        .collect();
    Pcm {
        rate: p.rate,
        channels: 2,
        samples,
    }
}

/// M/S back to L/R: `L = M + S`, `R = M - S`.
pub fn from_mid_side(p: &Pcm) -> Pcm {
    assert_eq!(p.channels, 2, "mid/side needs stereo");
    let samples = p
        .samples
        .chunks_exact(2)
        .flat_map(|f| [f[0] + f[1], f[0] - f[1]])
        .collect();
    Pcm {
        rate: p.rate,
        channels: 2,
        samples,
    }
}

/// Improves the top band of `pcm` with the AI: writes WAVs to `tmp`, runs
/// [`AudioAi::enhance`], reads the result back and splices it above the cutoff.
///
/// Returns exactly the input's length and channel count. Mono and stereo go to the AI in
/// one call; more channels go one by one as mono (the AI worker takes mono or stereo
/// only), each with its own cutoff. The caller must leave LFE out: it is never processed
/// with enhancement. With `dolby_surround` and two channels the pair is sent as M/S and
/// spliced in M/S, so the surround matrix in S gets its own level control.
///
/// A part whose spectrum does not drop below 20 kHz is returned as it is without an AI
/// call (and nothing is written). Temp files are removed on success and kept on error.
pub fn enhance_pcm(
    ai: &dyn AudioAi,
    pcm: &Pcm,
    dolby_surround: bool,
    tmp: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<Pcm> {
    if pcm.frames() == 0 {
        return Ok(pcm.clone());
    }
    if pcm.rate != RATE {
        bail!("enhancement needs {RATE} Hz audio, got {} Hz", pcm.rate);
    }
    let mid_side = dolby_surround && pcm.channels == 2;
    let work = if mid_side {
        to_mid_side(pcm)
    } else {
        pcm.clone()
    };
    let groups: Vec<Vec<usize>> = if work.channels <= 2 {
        vec![(0..work.channels).collect()]
    } else {
        (0..work.channels).map(|c| vec![c]).collect()
    };
    let mut chans: Vec<Vec<f32>> = (0..work.channels).map(|c| work.channel(c)).collect();
    let mut changed = false;
    let count = groups.len() as f32;
    for (i, group) in groups.iter().enumerate() {
        let part_chans: Vec<Vec<f32>> = group.iter().map(|&c| chans[c].clone()).collect();
        let part = Pcm::from_channels(RATE, &part_chans);
        let cutoff = detect_cutoff(&part);
        if cutoff >= NO_CUTOFF_HZ {
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            bail!(Cancelled);
        }
        let input = tmp.join(format!("enhance-{i}-in.wav"));
        let output = tmp.join(format!("enhance-{i}-out.wav"));
        write_wav(&input, &part)?;
        ai.enhance(&input, &output, cancel, &mut |p| {
            progress((i as f32 + p.clamp(0.0, 1.0)) / count)
        })?;
        let model = read_wav(&output)?;
        if model.channels != part.channels || model.rate != RATE {
            bail!(
                "the AI enhancement returned {} channels at {} Hz, expected {} at {RATE} Hz",
                model.channels,
                model.rate,
                part.channels
            );
        }
        let spliced = splice(&part, &model, cutoff);
        for (j, &c) in group.iter().enumerate() {
            chans[c] = spliced.channel(j);
        }
        std::fs::remove_file(&input)?;
        std::fs::remove_file(&output)?;
        changed = true;
    }
    progress(1.0);
    if !changed {
        return Ok(pcm.clone());
    }
    let out = Pcm::from_channels(RATE, &chans);
    Ok(if mid_side { from_mid_side(&out) } else { out })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::ai::Stems;
    use crate::audio::dsp::{band, rms_db};
    use crate::audio::io::{read_wav, write_wav, RATE};
    use anyhow::bail;
    use rustfft::{num_complex::Complex, FftPlanner};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    const N: usize = 196_608; // 4.1 s, FFT-friendly

    fn noise(n: usize, mut s: u32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                ((s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// `x` with every bin above `f` scaled by `above` (0 = brick wall), like a lossy codec.
    fn shelved(x: &[f32], f: f32, above: f32) -> Vec<f32> {
        let n = x.len();
        let mut planner = FftPlanner::<f32>::new();
        let mut buf: Vec<Complex<f32>> = x.iter().map(|&v| Complex::new(v, 0.0)).collect();
        planner.plan_fft_forward(n).process(&mut buf);
        for (k, b) in buf.iter_mut().enumerate() {
            let freq = k.min(n - k) as f32 * RATE as f32 / n as f32;
            if freq > f {
                *b *= above;
            }
        }
        planner.plan_fft_inverse(n).process(&mut buf);
        buf.iter().map(|c| c.re / n as f32).collect()
    }

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin())
            .collect()
    }

    fn mono(x: Vec<f32>) -> Pcm {
        Pcm::from_channels(RATE, &[x])
    }

    fn mid(x: &[f32]) -> &[f32] {
        &x[x.len() / 8..x.len() * 7 / 8]
    }

    /// The original's level in the bands above `cutoff`, with the output's beside it.
    fn top_bands(orig: &[f32], out: &[f32], cutoff: f32) -> Vec<(f32, f32, f32)> {
        let a = band_levels_db(mid(orig), cutoff);
        let b = band_levels_db(mid(out), cutoff);
        a.iter()
            .zip(&b)
            .filter(|(x, _)| x.0 >= cutoff)
            .map(|(x, y)| (x.0, x.2, y.2))
            .collect()
    }

    /// Fake AI: reads the input, applies `f` to every channel, writes the output.
    struct FakeAi<F: Fn(&[f32]) -> Vec<f32> + Send + Sync> {
        f: F,
        calls: AtomicUsize,
        channels_seen: Mutex<Vec<usize>>,
        inputs: Mutex<Vec<Pcm>>,
        fail: bool,
    }

    impl<F: Fn(&[f32]) -> Vec<f32> + Send + Sync> FakeAi<F> {
        fn new(f: F) -> Self {
            Self {
                f,
                calls: AtomicUsize::new(0),
                channels_seen: Mutex::new(Vec::new()),
                inputs: Mutex::new(Vec::new()),
                fail: false,
            }
        }
    }

    impl<F: Fn(&[f32]) -> Vec<f32> + Send + Sync> AudioAi for FakeAi<F> {
        fn separate(
            &self,
            _wav: &Path,
            _out_dir: &Path,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(f32),
        ) -> Result<Stems> {
            bail!("not used")
        }

        fn enhance(
            &self,
            wav: &Path,
            out: &Path,
            _cancel: &AtomicBool,
            progress: &mut dyn FnMut(f32),
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let p = read_wav(wav)?;
            self.channels_seen.lock().unwrap().push(p.channels);
            self.inputs.lock().unwrap().push(p.clone());
            if self.fail {
                bail!("fake AI failure");
            }
            let chans: Vec<Vec<f32>> = (0..p.channels).map(|c| (self.f)(&p.channel(c))).collect();
            progress(1.0);
            write_wav(out, &Pcm::from_channels(p.rate, &chans))
        }
    }

    fn files_in(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|r| r.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default()
    }

    #[test]
    fn finds_14k_cutoff() {
        let x = shelved(&noise(N, 1, 0.5), 14_000.0, 0.0);
        let c = detect_cutoff(&Pcm::from_channels(RATE, &[x.clone(), x]));
        assert!((13_500.0..=14_500.0).contains(&c), "cutoff {c}");
        // A residue 20 dB down above the cutoff (AC-3 coupling) is still a cutoff.
        let c = detect_cutoff(&mono(shelved(&noise(N, 2, 0.5), 14_000.0, 0.1)));
        assert!((13_500.0..=14_500.0).contains(&c), "shelf cutoff {c}");
    }

    #[test]
    fn full_band_has_no_cutoff() {
        assert_eq!(detect_cutoff(&mono(noise(N, 3, 0.5))), 20_000.0);
        assert_eq!(detect_cutoff(&mono(vec![0.0; N])), 20_000.0);
        // A cutoff below the range is clamped to 10 kHz.
        let low = shelved(&noise(N, 4, 0.5), 9_000.0, 0.0);
        assert_eq!(detect_cutoff(&mono(low)), 10_000.0);
    }

    #[test]
    fn below_cutoff_is_untouched() {
        let orig = shelved(&noise(N, 5, 0.5), 14_000.0, 0.0);
        let model = noise(N, 6, 0.5);
        let out = splice(&mono(orig.clone()), &mono(model), 14_000.0);
        assert_eq!(out.frames(), N);
        let level = |x: &[f32]| rms_db(mid(&band(x, 100.0, 12_000.0, RATE as f32)));
        let d = level(&out.samples) - level(&orig);
        assert!(d.abs() < 0.1, "100 Hz-12 kHz changed by {d} dB");
    }

    #[test]
    fn mid_side_round_trip() {
        let l = noise(4800, 7, 0.5);
        let r = noise(4800, 8, 0.5);
        let p = Pcm::from_channels(RATE, &[l.clone(), r.clone()]);
        let ms = to_mid_side(&p);
        assert!((ms.channel(0)[10] - (l[10] + r[10]) * 0.5).abs() < 1e-6);
        assert!((ms.channel(1)[10] - (l[10] - r[10]) * 0.5).abs() < 1e-6);
        let back = from_mid_side(&ms);
        assert_eq!(back.channels, 2);
        for (a, b) in back.samples.iter().zip(&p.samples) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn never_duller() {
        // Content 20 dB down above 14 kHz; the "model" is the original at -6 dB.
        let orig = shelved(&noise(N, 9, 0.5), 14_000.0, 0.1);
        let cutoff = detect_cutoff(&mono(orig.clone()));
        let model: Vec<f32> = orig.iter().map(|v| v * 0.5).collect();
        let out = splice(&mono(orig.clone()), &mono(model), cutoff);
        let bands = top_bands(&orig, &out.samples, cutoff);
        assert!(!bands.is_empty());
        for (lo, o, y) in bands {
            assert!((y - o).abs() < 0.5, "band from {lo} Hz: {y} dB vs {o} dB");
        }

        // A model whose top is new, uncorrelated content 6 dB below the original's.
        let full = noise(N, 20, 0.5);
        let low = shelved(&full, 14_000.0, 0.0);
        let orig_low = shelved(&orig, 14_000.0, 0.0);
        let model: Vec<f32> = (0..N)
            .map(|i| orig_low[i] + (full[i] - low[i]) * 0.05)
            .collect();
        let out = splice(&mono(orig.clone()), &mono(model), cutoff);
        for (lo, o, y) in top_bands(&orig, &out.samples, cutoff) {
            assert!(
                (y - o).abs() < 0.5,
                "new top, band from {lo} Hz: {y} dB vs {o} dB"
            );
        }
    }

    #[test]
    fn enhance_with_fake_ai() {
        let orig = shelved(&noise(N, 10, 0.5), 14_000.0, 0.0);
        let pcm = Pcm::from_channels(RATE, &[orig.clone(), orig.clone()]);
        let tone = sine(16_000.0, 0.3, N);
        let ai = FakeAi::new(|x: &[f32]| x.iter().zip(&tone).map(|(a, b)| a + b).collect());
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let out = enhance_pcm(&ai, &pcm, false, dir.path(), &cancel, &mut |_| {}).unwrap();
        assert_eq!(ai.calls.load(Ordering::SeqCst), 1);
        assert_eq!((out.channels, out.frames()), (2, N));
        assert!(
            files_in(dir.path()).is_empty(),
            "{:?}",
            files_in(dir.path())
        );

        let cutoff = detect_cutoff(&pcm);
        let left = out.channel(0);
        let tone_band = top_bands(&orig, &left, cutoff)
            .into_iter()
            .find(|b| b.0 <= 16_000.0)
            .unwrap();
        // Present: far above what the original had there.
        assert!(tone_band.2 > tone_band.1 + 20.0, "{tone_band:?}");
        // Capped: the raw tone is about 13 dB louder than the cap allows.
        let raw = band_levels_db(mid(&tone), cutoff)
            .into_iter()
            .find(|b| b.0 == tone_band.0)
            .unwrap()
            .2;
        assert!(tone_band.2 < raw - 6.0, "tone {} vs raw {raw}", tone_band.2);
        let cap = top_band_cap(&orig, cutoff);
        assert!(
            tone_band.2 <= cap + 0.5 && tone_band.2 >= cap - 0.5,
            "tone {} vs cap {cap}",
            tone_band.2
        );
        // Below the cutoff the original is kept.
        let level = |x: &[f32]| rms_db(mid(&band(x, 100.0, 12_000.0, RATE as f32)));
        assert!((level(&left) - level(&orig)).abs() < 0.1);
    }

    /// The cap for the first band above `cutoff`: the fit through the original's levels
    /// between 6 kHz and the cutoff, taken at the cutoff, minus 12 dB/octave.
    fn top_band_cap(orig: &[f32], cutoff: f32) -> f32 {
        let levels = band_levels_db(mid(orig), cutoff);
        let (lo, hi, _) = *levels.iter().find(|b| b.0 >= cutoff).unwrap();
        reference_at_cutoff(&levels, cutoff).unwrap() - 12.0 * ((lo * hi).sqrt() / cutoff).log2()
    }

    #[test]
    fn quiet_top_band_passes_unchanged() {
        // Content the AI adds below the cap and above the original is kept as it is.
        let orig = shelved(&noise(N, 11, 0.5), 14_000.0, 0.0);
        let full = noise(N, 12, 0.5);
        let low = shelved(&full, 14_000.0, 0.0);
        let hiss: Vec<f32> = full
            .iter()
            .zip(&low)
            .map(|(a, b)| (a - b) * 0.1) // the part above 14 kHz, 20 dB down
            .collect();
        let model: Vec<f32> = orig.iter().zip(&hiss).map(|(a, b)| a + b).collect();
        let cutoff = detect_cutoff(&mono(orig.clone()));
        let out = splice(&mono(orig.clone()), &mono(model.clone()), cutoff);
        let want = top_bands(&orig, &model, cutoff);
        let got = top_bands(&orig, &out.samples, cutoff);
        for (w, g) in want.iter().zip(&got) {
            assert!(
                (w.2 - g.2).abs() < 0.5,
                "band from {}: {} vs {}",
                w.0,
                g.2,
                w.2
            );
        }
    }

    #[test]
    fn full_band_source_skips_the_ai() {
        let pcm = mono(noise(N, 14, 0.5));
        let ai = FakeAi::new(|x: &[f32]| x.to_vec());
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let out = enhance_pcm(&ai, &pcm, false, dir.path(), &cancel, &mut |_| {}).unwrap();
        assert_eq!(out, pcm);
        assert_eq!(ai.calls.load(Ordering::SeqCst), 0);
        assert!(files_in(dir.path()).is_empty());
        assert_eq!(splice(&pcm, &mono(noise(N, 15, 0.5)), 20_000.0), pcm);
    }

    #[test]
    fn keeps_length_and_channels() {
        let x = shelved(&noise(N, 16, 0.5), 14_000.0, 0.0);
        // The AI returns 100 samples short; the result is still the input's length.
        let ai = FakeAi::new(|x: &[f32]| x[..x.len() - 100].to_vec());
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        for ch in [1usize, 2, 3] {
            let pcm = Pcm::from_channels(RATE, &vec![x.clone(); ch]);
            let out = enhance_pcm(&ai, &pcm, false, dir.path(), &cancel, &mut |_| {}).unwrap();
            assert_eq!((out.channels, out.frames()), (ch, N), "{ch} channels");
        }
        // Mono and stereo go in one call each; three channels go one by one as mono.
        assert_eq!(*ai.channels_seen.lock().unwrap(), vec![1, 2, 1, 1, 1]);
    }

    #[test]
    fn dolby_surround_sends_mid_side() {
        let l = shelved(&noise(N, 17, 0.5), 14_000.0, 0.0);
        let r = shelved(&noise(N, 18, 0.3), 14_000.0, 0.0);
        let pcm = Pcm::from_channels(RATE, &[l.clone(), r.clone()]);
        let ai = FakeAi::new(|x: &[f32]| x.to_vec());
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let out = enhance_pcm(&ai, &pcm, true, dir.path(), &cancel, &mut |_| {}).unwrap();
        let sent = ai.inputs.lock().unwrap()[0].clone();
        assert!((sent.channel(0)[1000] - (l[1000] + r[1000]) * 0.5).abs() < 1e-6);
        assert!((sent.channel(1)[1000] - (l[1000] - r[1000]) * 0.5).abs() < 1e-6);
        // An identity AI gives back the input.
        for (a, b) in out.samples.iter().zip(&pcm.samples) {
            assert!((a - b).abs() < 1e-4);
        }
    }

    #[test]
    fn temp_files_kept_on_error_and_cancel_stops() {
        let pcm = mono(shelved(&noise(N, 19, 0.5), 14_000.0, 0.0));
        let mut ai = FakeAi::new(|x: &[f32]| x.to_vec());
        ai.fail = true;
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        assert!(enhance_pcm(&ai, &pcm, false, dir.path(), &cancel, &mut |_| {}).is_err());
        assert!(!files_in(dir.path()).is_empty());

        let ai = FakeAi::new(|x: &[f32]| x.to_vec());
        let cancel = AtomicBool::new(true);
        let err = enhance_pcm(&ai, &pcm, false, dir.path(), &cancel, &mut |_| {}).unwrap_err();
        assert!(crate::process::is_cancelled(&err), "{err}");
        assert_eq!(ai.calls.load(Ordering::SeqCst), 0);
    }
}
