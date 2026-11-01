//! Classic 5.1 from stereo: FFmpeg's `surround` filter (per-bin STFT steering) for
//! FL/FR/FC/surrounds, surrounds delayed and decorrelated in Rust, LFE from the mono sum.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{bail, Result};

use super::dsp::{decorrelate, delay};
use super::io::{read_wav, Pcm, RATE};
use super::lfe::{make_lfe, LfeSource};
use crate::ffmpeg::Tools;
use crate::process;
use crate::settings::LfeLevel;
/// Latency of `surround` with `win_size=4096`: half a window.
const LATENCY: usize = 2048;
/// Surround channels lag the fronts by 15 ms, as a cinema decoder does.
const SURROUND_DELAY: usize = 720;
const FILTER: &str = "aformat=sample_fmts=fltp:channel_layouts=stereo,\
    surround=chl_out=5.1:lfe=0:win_size=4096:overlap=0.5:smooth=0.1,apad=pad_len=8192";

/// Classic 5.1 from a stereo 48 kHz WAV: 6 channels (FL FR FC LFE SL SR), exactly as many
/// frames as the input.
pub fn upmix_classic(
    tools: &Tools,
    stereo_wav: &Path,
    level: LfeLevel,
    cancel: &AtomicBool,
) -> Result<Pcm> {
    let raw = upmix_classic_raw(tools, stereo_wav, cancel)?;
    let src = read_wav(stereo_wav)?;
    let mono: Vec<f32> = (0..src.frames())
        .map(|i| 0.5 * (src.samples[2 * i] + src.samples[2 * i + 1]))
        .collect();
    let rate = raw.rate as f32;
    let lfe = make_lfe(&mono, LfeSource::MonoSum, level, rate);
    let sl = decorrelate(&delay(&raw.channel(4), SURROUND_DELAY), 1);
    let sr = decorrelate(&delay(&raw.channel(5), SURROUND_DELAY), 2);
    Ok(Pcm::from_channels(
        raw.rate,
        &[raw.channel(0), raw.channel(1), raw.channel(2), lfe, sl, sr],
    ))
}

/// The latency-compensated `surround` output before the surround delay, decorrelation and
/// LFE: FL FR FC LFE (silent) SL SR (FFmpeg's BL/BR relabelled).
pub(crate) fn upmix_classic_raw(
    tools: &Tools,
    stereo_wav: &Path,
    cancel: &AtomicBool,
) -> Result<Pcm> {
    upmix_raw_impl(tools, stereo_wav, cancel, true)
}

/// `compensate = false` leaves the filter latency in (control for the null test).
pub(crate) fn upmix_raw_impl(
    tools: &Tools,
    stereo_wav: &Path,
    cancel: &AtomicBool,
    compensate: bool,
) -> Result<Pcm> {
    let src = read_wav(stereo_wav)?;
    if src.channels != 2 || src.rate != RATE {
        bail!("classic 5.1 needs a 48 kHz stereo file");
    }
    let n = src.frames();
    let stem = stereo_wav
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = stereo_wav.with_file_name(format!("classic-{stem}.6ch.wav"));
    let mut cmd = tools.ffmpeg_cmd();
    cmd.args(["-y", "-i"])
        .arg(stereo_wav)
        .arg("-af")
        .arg(FILTER)
        .args(["-c:a", "pcm_f32le", "-f", "wav"])
        .arg(&tmp);
    let result = (|| -> Result<Pcm> {
        let fin = process::run(cmd, None, cancel, None)?;
        if !fin.success {
            bail!("ffmpeg could not build the 5.1 mix: {}", fin.tail(6));
        }
        read_wav(&tmp)
    })();
    let _ = std::fs::remove_file(&tmp);
    let out = result?;
    if out.channels != 6 {
        bail!("the 5.1 mix has {} channels, expected 6", out.channels);
    }
    let skip = if compensate { LATENCY } else { 0 };
    let chans: Vec<Vec<f32>> = (0..6)
        .map(|c| {
            let mut ch: Vec<f32> = out.channel(c).into_iter().skip(skip).take(n).collect();
            ch.resize(n, 0.0);
            if c == 3 {
                ch.fill(0.0);
            }
            ch
        })
        .collect();
    Ok(Pcm::from_channels(RATE, &chans))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::{db_to_gain, rms_db};
    use crate::audio::io::write_wav;

    /// Film dialogue sits around -27 dBFS RMS (the AC-3 dialnorm default). The null residual of\n    /// the `surround` filter is proportional to the level (about -13 dB relative), so the\n    /// absolute bound is only meaningful at a realistic level: this signal is -27 dBFS.\n    const LEVEL: f32 = 0.5;\n\n    /// Film dialogue sits around -27 dBFS RMS (the AC-3 dialnorm default). The null residual of
    /// the `surround` filter is proportional to the level (about -13 dB relative), so the
    /// absolute bound is only meaningful at a realistic level: this signal is -27 dBFS.
    const LEVEL: f32 = 0.5;

    /// Speech-like stereo: a few harmonic tones with syllable-rate modulation, panned
    /// differently, plus uncorrelated noise.
    pub(crate) fn test_stereo(secs: usize) -> Pcm {
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

    fn tools() -> Option<Tools> {
        let t = Tools::locate("");
        if t.is_none() {
            eprintln!("ffmpeg not installed, skipping");
        }
        t
    }

    fn null_db(raw: &Pcm, src: &Pcm) -> f32 {
        let (fl, fr, fc) = (raw.channel(0), raw.channel(1), raw.channel(2));
        let (sl, sr) = (raw.channel(4), raw.channel(5));
        let k = db_to_gain(-3.0103);
        let (l, r) = (src.channel(0), src.channel(1));
        let mut diff = Vec::new();
        // Skip the first and last 0.25 s (filter edges).
        let skip = 12_000;
        for i in skip..l.len() - skip {
            diff.push(fl[i] + k * fc[i] + k * sl[i] - l[i]);
            diff.push(fr[i] + k * fc[i] + k * sr[i] - r[i]);
        }
        rms_db(&diff)
    }

    #[test]
    fn classic_downmix_nulls() {
        let Some(tools) = tools() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = test_stereo(10);
        let wav = dir.path().join("in.wav");
        write_wav(&wav, &src).unwrap();
        let cancel = AtomicBool::new(false);
        let good = upmix_classic_raw(&tools, &wav, &cancel).unwrap();
        assert_eq!((good.channels, good.frames()), (6, src.frames()));
        let bad = upmix_raw_impl(&tools, &wav, &cancel, false).unwrap();
        let (g, b) = (null_db(&good, &src), null_db(&bad, &src));
        eprintln!("null: compensated {g:.1} dB, uncompensated {b:.1} dB");
        assert!(g <= -35.0, "compensated null {g}");
        assert!(b >= g + 10.0, "uncompensated {b} vs {g}");
        assert!(!dir.path().join("in.wav.part").exists());
    }
}
