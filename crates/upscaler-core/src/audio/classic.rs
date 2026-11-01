//! Classic 5.1 from stereo without AI: a frequency-domain centre extraction in Rust puts the
//! correlated, equally panned part (dialogue) into FC; the surrounds come from the side
//! signal, delayed and decorrelated; the LFE comes from the mono sum.
//!
//! # Centre extraction
//!
//! STFT with a 4096-point periodic Hann window, 75 % overlap (hop 1024), the same window for
//! analysis and synthesis, overlap-add normalised by the constant sum of the squared windows
//! (1.5). The first frame starts 3072 samples before the signal, so every sample is covered by
//! four frames and the synthesis is an exact identity when the mask is 1: the processed path
//! has zero latency by construction and needs no compensation.
//!
//! Per frame and bin the auto- and cross-spectra `|L|²`, `|R|²` and `Re(L·R*)` are smoothed
//! over frequency (a box of ±max(1, k/24) bins, at most ±32: about ±12 Hz in the bass, ±200 Hz
//! at 5 kHz) and over time (a symmetric cos² window over ±4 frames, about ±85 ms; offline, so
//! it needs no look-ahead latency). From the smoothed spectra:
//!
//! - coherence `γ = Re(Φ_LR) / sqrt(Φ_LL·Φ_RR)`: 1 for one source in phase in both channels,
//!   about 0 for decorrelated content, negative for anti-phase content;
//! - level similarity `λ = sqrt(min(Φ_LL, Φ_RR) / max(Φ_LL, Φ_RR))`, the inverse panning index;
//! - centre power `Pc = max(Re(Φ_LR), 0)` and uncorrelated power per channel
//!   `Pamb = max((Φ_LL + Φ_RR) / 2 − Pc, 0)`.
//!
//! The centre mask is `m = smoothstep((γ − 0.5) / 0.4) · 2λ / (1 + λ) · Pc / (Pc + 5·Pamb)`.
//!
//! - The first factor is the Avendano/Jot similarity idea with the phase kept (anti-phase
//!   content never goes to the centre) and a soft knee so decorrelated ambience (with the
//!   estimator's bias of about 0.3) stays out.
//! - The second factor is chosen so that a coherent source panned with gains `a ≥ b` gives at
//!   most `C = b·S`, `L' = (a − b)·S`, `R' = 0`: a panned source keeps its side and the weaker
//!   front channel is never driven into anti-phase.
//! - The third is a Wiener-style gain with an over-weighted ambient term (β = 5). In a bin
//!   shared by dialogue and panned, decorrelated music, any mask on `(L + R) / 2` moves half
//!   the music's amplitude per unit of mask into FC, so the mask there trades dialogue left in
//!   the fronts against music pulled into the centre. β = 5 balances the two on the synthetic
//!   test (dialogue 14 dB over its front leakage, music 15.7 dB over its centre leakage) and
//!   still puts the real bridge scene's dialogue about 8 dB above FL/FR; β = 0 gives 25 dB and
//!   13 dB on the test and about 10 dB on the bridge.
//!
//! `C = m · (L + R) / 2`, and the fronts are `FL = L − C`, `FR = R − C` in the time domain, so
//! `FL + FC = L` and `FR + FC = R` hold to float rounding whatever the mask does (the same
//! invariant as [`super::place::place`]): a receiver that folds the centre back loses nothing.
//!
//! The extraction runs over the full band. Dialogue sibilance reaches past 8 kHz and would
//! otherwise stay in the fronts, split from the voice; centred bass in FC is normal in a film
//! mix and is bass-managed by the receiver.
//!
//! # Surrounds
//!
//! The side signal `(L − R) / 2`, which is the same as `(FL − FR) / 2` because the centre
//! cancels, band-passed to 100–7000 Hz, delayed 15 ms, decorrelated (seeds 1 and 2) and at
//! −3 dB: the same derivation the AI path uses. Centred dialogue has no side part, so unlike
//! the FFmpeg `surround` filter used before, no dialogue leaks to the rear.

use std::sync::atomic::AtomicBool;

use anyhow::{bail, Result};
use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;

use super::dsp::{band, db_to_gain, decorrelate, delay};
use super::io::{Pcm, RATE};
use super::lfe::{make_lfe, LfeSource};
use crate::process::Cancelled;
use crate::settings::LfeLevel;

/// Surround channels lag the fronts by 15 ms, as a cinema decoder does.
const SURROUND_DELAY: usize = 720;
const SURROUND_GAIN_DB: f32 = -3.0;
const SURROUND_BAND: (f32, f32) = (100.0, 7000.0);

const WIN: usize = 4096;
const HOP: usize = 1024;
const BINS: usize = WIN / 2 + 1;
/// Frames on each side in the time smoothing of the spectra.
const TIME_RADIUS: usize = 4;
/// Frequency smoothing half-width: `max(1, k / FREQ_DIV)` bins, at most `FREQ_MAX`.
const FREQ_DIV: usize = 24;
const FREQ_MAX: usize = 32;
/// Coherence knee: below `COH_LO` nothing goes to the centre, above `COH_HI` everything.
const COH_LO: f32 = 0.5;
const COH_HI: f32 = 0.9;
/// Weight of the uncorrelated power in the Wiener factor `Pc / (Pc + β·Pamb)`. Higher keeps
/// more music out of the centre in bins shared with dialogue and leaves more dialogue in the
/// fronts; 5 balances the two (see the module docs).
const WIENER_BETA: f32 = 5.0;

/// Classic 5.1 from 48 kHz stereo: 6 channels (FL FR FC LFE SL SR), exactly as many frames
/// as the input.
pub fn upmix_classic(src: &Pcm, level: LfeLevel, cancel: &AtomicBool) -> Result<Pcm> {
    if src.channels != 2 || src.rate != RATE {
        bail!("classic 5.1 needs a 48 kHz stereo file");
    }
    let (l, r) = (src.channel(0), src.channel(1));
    let fc = centre(&l, &r, &[], cancel)?.remove(0);
    let n = l.len();
    let fl: Vec<f32> = (0..n).map(|i| l[i] - fc[i]).collect();
    let fr: Vec<f32> = (0..n).map(|i| r[i] - fc[i]).collect();
    let rate = RATE as f32;
    let mono: Vec<f32> = (0..n).map(|i| 0.5 * (l[i] + r[i])).collect();
    let lfe = make_lfe(&mono, LfeSource::MonoSum, level, rate);
    let side: Vec<f32> = (0..n).map(|i| 0.5 * (l[i] - r[i])).collect();
    let side = delay(
        &band(&side, SURROUND_BAND.0, SURROUND_BAND.1, rate),
        SURROUND_DELAY,
    );
    let g = db_to_gain(SURROUND_GAIN_DB);
    let surround = |seed| -> Vec<f32> {
        decorrelate(&side, seed)
            .into_iter()
            .map(|v| v * g)
            .collect()
    };
    Ok(Pcm::from_channels(
        RATE,
        &[fl, fr, fc, lfe, surround(1), surround(2)],
    ))
}

/// Periodic Hann window.
fn hann() -> Vec<f32> {
    (0..WIN)
        .map(|i| {
            let s = (std::f64::consts::PI * i as f64 / WIN as f64).sin();
            (s * s) as f32
        })
        .collect()
}

fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Centre mask from the smoothed spectra of one bin.
fn mask(ll: f32, rr: f32, lr: f32) -> f32 {
    let p = ll * rr;
    if p <= 1e-24 {
        return 0.0;
    }
    let coh = lr / p.sqrt();
    let lam = (ll.min(rr) / ll.max(rr)).sqrt();
    let pc = lr.max(0.0);
    let pamb = (0.5 * (ll + rr) - pc).max(0.0);
    let wiener = if pc > 0.0 {
        pc / (pc + WIENER_BETA * pamb)
    } else {
        0.0
    };
    (smoothstep((coh - COH_LO) / (COH_HI - COH_LO)) * 2.0 * lam / (1.0 + lam) * wiener)
        .clamp(0.0, 1.0)
}

/// Box smoothing over frequency with a half-width that grows with the bin index.
fn smooth_freq(x: &[f32], out: &mut [f32]) {
    let mut prefix = vec![0.0f64; BINS + 1];
    for k in 0..BINS {
        prefix[k + 1] = prefix[k] + x[k] as f64;
    }
    for (k, o) in out.iter_mut().enumerate() {
        let h = (k / FREQ_DIV).clamp(1, FREQ_MAX);
        let (a, b) = (k.saturating_sub(h), (k + h + 1).min(BINS));
        *o = ((prefix[b] - prefix[a]) / (b - a) as f64) as f32;
    }
}

/// One analysed frame: smoothed-over-frequency spectra of the mix and the mid spectrum of
/// every pair the mask is applied to.
struct Frame {
    ll: Vec<f32>,
    rr: Vec<f32>,
    lr: Vec<f32>,
    mids: Vec<Vec<Complex32>>,
}

/// The centre `m · (L + R) / 2` of `(l, r)`, followed by the centre of every pair in `extra`
/// taken with the masks of `(l, r)`. Given the masks the extraction is linear, so the extra
/// pairs show what happens to each component of a mix (tests only; production passes none).
pub(crate) fn centre(
    l: &[f32],
    r: &[f32],
    extra: &[(&[f32], &[f32])],
    cancel: &AtomicBool,
) -> Result<Vec<Vec<f32>>> {
    let n = l.len();
    let pairs: Vec<(&[f32], &[f32])> = std::iter::once((l, r))
        .chain(extra.iter().copied())
        .collect();
    let mut outs = vec![vec![0.0f32; n]; pairs.len()];
    if n == 0 {
        return Ok(outs);
    }
    let win = hann();
    // Sum of the squared windows at every hop phase: constant (1.5) for periodic Hann at 75 %.
    let norm = {
        let mut s = vec![0.0f32; HOP];
        for (i, w) in win.iter().enumerate() {
            s[i % HOP] += w * w;
        }
        debug_assert!(s.iter().all(|v| (v - s[0]).abs() < 1e-4));
        s[0]
    };
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(WIN);
    let inv = planner.plan_fft_inverse(WIN);
    let lead = WIN - HOP;
    let frames = (n + lead).div_ceil(HOP);
    let start = |j: usize| j as isize * HOP as isize - lead as isize;
    let weights: Vec<f32> = (0..=TIME_RADIUS)
        .map(|d| {
            let c = (std::f32::consts::PI * d as f32 / (2.0 * (TIME_RADIUS + 1) as f32)).cos();
            c * c
        })
        .collect();
    let ring_len = 2 * TIME_RADIUS + 1;
    let mut ring: Vec<Option<Frame>> = (0..ring_len).map(|_| None).collect();
    let mut buf = vec![Complex32::new(0.0, 0.0); WIN];
    let spec = |x: &[f32], s: isize, buf: &mut Vec<Complex32>| -> Vec<Complex32> {
        for (i, b) in buf.iter_mut().enumerate() {
            let p = s + i as isize;
            let v = if p >= 0 && (p as usize) < n {
                x[p as usize] * win[i]
            } else {
                0.0
            };
            *b = Complex32::new(v, 0.0);
        }
        fwd.process(buf);
        buf[..BINS].to_vec()
    };
    let (mut tl, mut tr, mut tlr) = (vec![0.0; BINS], vec![0.0; BINS], vec![0.0; BINS]);
    for j in 0..frames + TIME_RADIUS {
        if j % 256 == 0 && cancel.load(std::sync::atomic::Ordering::Relaxed) {
            bail!(Cancelled);
        }
        if j < frames {
            let s = start(j);
            let mut mids = Vec::with_capacity(pairs.len());
            let (mut ll, mut rr, mut lr) = (vec![0.0; BINS], vec![0.0; BINS], vec![0.0; BINS]);
            for (p, (a, b)) in pairs.iter().enumerate() {
                let sa = spec(a, s, &mut buf);
                let sb = spec(b, s, &mut buf);
                if p == 0 {
                    for k in 0..BINS {
                        tl[k] = sa[k].norm_sqr();
                        tr[k] = sb[k].norm_sqr();
                        tlr[k] = (sa[k] * sb[k].conj()).re;
                    }
                    smooth_freq(&tl, &mut ll);
                    smooth_freq(&tr, &mut rr);
                    smooth_freq(&tlr, &mut lr);
                }
                mids.push(sa.iter().zip(&sb).map(|(x, y)| (x + y) * 0.5).collect());
            }
            ring[j % ring_len] = Some(Frame { ll, rr, lr, mids });
        }
        let Some(t) = j.checked_sub(TIME_RADIUS) else {
            continue;
        };
        if t >= frames {
            continue;
        }
        let (mut sl, mut sr, mut slr) =
            (vec![0.0f32; BINS], vec![0.0f32; BINS], vec![0.0f32; BINS]);
        let lo = t.saturating_sub(TIME_RADIUS);
        let hi = (t + TIME_RADIUS).min(frames - 1);
        for u in lo..=hi {
            let f = ring[u % ring_len].as_ref().expect("frame in ring");
            let w = weights[u.abs_diff(t)];
            for k in 0..BINS {
                sl[k] += w * f.ll[k];
                sr[k] += w * f.rr[k];
                slr[k] += w * f.lr[k];
            }
        }
        let m: Vec<f32> = (0..BINS).map(|k| mask(sl[k], sr[k], slr[k])).collect();
        let cur = ring[t % ring_len].as_ref().expect("frame in ring");
        let s = start(t);
        for (mid, out) in cur.mids.iter().zip(outs.iter_mut()) {
            for k in 0..BINS {
                buf[k] = mid[k] * m[k];
            }
            for k in BINS..WIN {
                buf[k] = buf[WIN - k].conj();
            }
            inv.process(&mut buf);
            let scale = 1.0 / (WIN as f32 * norm);
            for (i, b) in buf.iter().enumerate() {
                let p = s + i as isize;
                if p >= 0 && (p as usize) < n {
                    out[p as usize] += b.re * win[i] * scale;
                }
            }
        }
    }
    Ok(outs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::rms_db;

    /// Film dialogue sits around -27 dBFS RMS (the AC-3 dialnorm default).
    const LEVEL: f32 = 0.5;

    fn noise(n: usize, mut s: u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
            })
            .collect()
    }

    /// Speech-like stereo: a few harmonic tones with syllable-rate modulation, panned
    /// differently, plus uncorrelated noise.
    fn test_stereo(secs: usize) -> Pcm {
        let n = secs * RATE as usize;
        let mut s = 12345u32;
        let mut noise = move || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        let tau = std::f32::consts::TAU;
        let mut l = Vec::with_capacity(n);
        let mut r = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let env = 0.5 + 0.5 * (tau * 3.0 * t).sin();
            let voice = env * ((tau * 220.0 * t).sin() + 0.5 * (tau * 440.0 * t).sin());
            let lead = (tau * 660.0 * t).sin() * (0.5 + 0.5 * (tau * 0.7 * t).sin());
            let amb = (tau * 1500.0 * t).sin() * 0.4;
            l.push(LEVEL * (0.15 * voice + 0.10 * lead + 0.05 * noise()));
            r.push(LEVEL * (0.15 * voice + 0.04 * lead + 0.10 * amb + 0.05 * noise()));
        }
        Pcm::from_channels(RATE, &[l, r])
    }

    fn sub(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| x - y).collect()
    }

    fn speech_band(x: &[f32]) -> Vec<f32> {
        band(x, 200.0, 4000.0, RATE as f32)
    }

    const VOICE_DB: f32 = -24.0;
    const MUSIC_DB: f32 = -30.0;

    fn at_db(x: &[f32], db: f32) -> Vec<f32> {
        let g = db_to_gain(db - rms_db(x));
        x.iter().map(|v| v * g).collect()
    }

    /// Skip the first and last 0.25 s (filter edges).
    fn inner(x: &[f32]) -> &[f32] {
        &x[12_000..x.len() - 12_000]
    }

    #[test]
    fn classic_downmix_rebuilds_source() {
        let src = test_stereo(10);
        let out = upmix_classic(&src, LfeLevel::Normal, &AtomicBool::new(false)).unwrap();
        assert_eq!((out.channels, out.frames()), (6, src.frames()));
        let fc = out.channel(2);
        assert!(rms_db(&fc) > rms_db(&src.channel(0)) - 10.0, "centre empty");
        for c in 0..2 {
            let s = src.channel(c);
            let down: Vec<f32> = out.channel(c).iter().zip(&fc).map(|(a, b)| a + b).collect();
            let res = rms_db(&sub(&down, &s)) - rms_db(&s);
            eprintln!("channel {c}: FL/FR + FC vs source residual {res:.1} dB");
            assert!(res <= -40.0, "channel {c} residual {res} dB");
        }
    }

    /// Identical channels: the mask is 1 everywhere and the centre must be the input itself,
    /// sample-aligned (zero latency, exact overlap-add) up to the ends.
    #[test]
    fn mono_goes_to_centre_without_latency() {
        let n = 5 * RATE as usize + 77;
        let x: Vec<f32> = noise(n, 9).iter().map(|v| v * 0.2).collect();
        let c = centre(&x, &x, &[], &AtomicBool::new(false))
            .unwrap()
            .remove(0);
        let res = rms_db(&sub(&c, &x)) - rms_db(&x);
        let shifted = rms_db(&sub(&c[1..], &x[..n - 1])) - rms_db(&x);
        eprintln!("mono centre residual {res:.1} dB, one sample late {shifted:.1} dB");
        assert!(res <= -60.0, "residual {res} dB");
        assert!(shifted >= -3.0, "control {shifted} dB");
    }

    /// A centred "voice" (noise band-limited to 300-3400 Hz, with syllable gaps) plus
    /// hard-panned, decorrelated broadband music. Masks from the mix, applied to each part.
    #[test]
    fn centred_voice_to_fc_panned_music_stays() {
        let n = 8 * RATE as usize;
        let rate = RATE as f32;
        let tau = std::f32::consts::TAU;
        let voice: Vec<f32> = band(&noise(n, 1), 300.0, 3400.0, rate)
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let t = i as f32 / rate;
                // Syllables at 4 Hz with short gaps, like running speech.
                let env = (tau * 4.0 * t).sin().max(0.0).sqrt();
                v * env
            })
            .collect();
        // Dialogue scene: voice at -24 dBFS, music 6 dB under it in each channel.
        let voice = at_db(&voice, VOICE_DB);
        let music_l = at_db(&band(&noise(n, 2), 40.0, 16000.0, rate), MUSIC_DB);
        let music_r = at_db(&band(&noise(n, 3), 40.0, 16000.0, rate), MUSIC_DB);
        let zero = vec![0.0f32; n];
        let l: Vec<f32> = (0..n).map(|i| voice[i] + music_l[i]).collect();
        let r: Vec<f32> = (0..n).map(|i| voice[i] + music_r[i]).collect();
        eprintln!(
            "voice {:.1} dB, music {:.1} dB per side",
            rms_db(&voice),
            rms_db(&music_l)
        );
        let parts = centre(
            &l,
            &r,
            &[(&voice, &voice), (&music_l, &zero), (&zero, &music_r)],
            &AtomicBool::new(false),
        )
        .unwrap();
        let (vc, mlc, mrc) = (&parts[1], &parts[2], &parts[3]);
        // Voice: FC against what is left of it in FL (= FR).
        let v_fc = rms_db(inner(&speech_band(vc)));
        let v_fl = rms_db(inner(&speech_band(&sub(&voice, vc))));
        eprintln!("voice speech band: FC {v_fc:.1} dB, FL/FR leakage {v_fl:.1} dB");
        assert!(v_fc - v_fl >= 10.0, "voice FC {v_fc} vs FL {v_fl}");
        // Music: what reaches FC against what stays in its own front channel.
        for (name, m, c) in [("L", &music_l, mlc), ("R", &music_r, mrc)] {
            let own = rms_db(inner(&sub(m, c)));
            let leak = rms_db(inner(c));

            eprintln!("music {name}: own front {own:.1} dB, FC leakage {leak:.1} dB");
            assert!(own - leak >= 15.0, "music {name}: own {own} vs FC {leak}");
        }
        // The other front gets the music only as -C: also well down.
        let other = rms_db(inner(mlc));
        assert!(other <= rms_db(inner(&music_l)) - 15.0);
    }

    /// One coherent source panned with gains 1 and 0.5: the centre takes at most the weaker
    /// side's share, the weaker front is never driven into anti-phase, and the stronger
    /// front keeps the source.
    #[test]
    fn panned_source_keeps_its_side() {
        let n = 4 * RATE as usize;
        let s = band(&noise(n, 5), 100.0, 8000.0, RATE as f32);
        let l: Vec<f32> = s.iter().map(|v| 0.3 * v).collect();
        let r: Vec<f32> = s.iter().map(|v| 0.15 * v).collect();
        let c = centre(&l, &r, &[], &AtomicBool::new(false))
            .unwrap()
            .remove(0);
        let (fl, fr) = (sub(&l, &c), sub(&r, &c));
        let dot = |a: &[f32], b: &[f32]| -> f64 {
            a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum()
        };
        let (c_db, fl_db) = (rms_db(inner(&c)), rms_db(inner(&fl)));
        eprintln!(
            "panned: FL {fl_db:.1} dB, FR {:.1} dB, FC {c_db:.1} dB",
            rms_db(inner(&fr))
        );
        assert!(dot(inner(&fr), inner(&s)) >= 0.0, "FR in anti-phase");
        assert!(
            c_db <= rms_db(inner(&r)) + 0.5,
            "centre above the weaker side"
        );
        assert!(fl_db >= c_db, "FL {fl_db} below FC {c_db}");
    }

    #[test]
    fn classic_keeps_the_tail() {
        let n = 3 * RATE as usize;
        let tone: Vec<f32> = (0..n)
            .map(|i| LEVEL * 0.5 * (std::f32::consts::TAU * 300.0 * i as f32 / RATE as f32).sin())
            .collect();
        let src = Pcm::from_channels(RATE, &[tone.clone(), tone]);
        let out = upmix_classic(&src, LfeLevel::Normal, &AtomicBool::new(false)).unwrap();
        let sum: Vec<f32> = (0..n)
            .map(|i| out.samples[6 * i] + out.samples[6 * i + 1] + out.samples[6 * i + 2])
            .collect();
        let tail = rms_db(&sum[n - 4800..]);
        let mid = rms_db(&sum[n / 2 - 2400..n / 2 + 2400]);
        eprintln!("tail {tail:.1} dB, middle {mid:.1} dB");
        assert!((tail - mid).abs() <= 1.0, "tail {tail} vs middle {mid}");
    }

    #[test]
    fn cancel_stops_the_extraction() {
        let x = noise(RATE as usize, 4);
        let err = centre(&x, &x, &[], &AtomicBool::new(true)).unwrap_err();
        assert!(crate::process::is_cancelled(&err));
    }
}
