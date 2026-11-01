//! 5.1 placement from AI-separated stems (speech, music, effects).
//! Channel order: FL FR FC LFE SL SR.

use super::dsp::{band, db_to_gain, decorrelate, delay, lr4_highpass, zero_phase};
use super::io::{Pcm, RATE};
use super::lfe::{make_lfe, LfeSource};
use crate::settings::LfeLevel;

const CENTRE_HIGHPASS_HZ: f32 = 70.0;
const SURROUND_GAIN_DB: f32 = -3.0;
const SURROUND_DELAY_S: f32 = 0.015;
const SURROUND_BAND: (f32, f32) = (100.0, 7000.0);

/// Channel `i` of `p`, zero-padded or trimmed to `len` samples (AI stems can
/// differ from the source by a few samples).
fn fitted(p: &Pcm, i: usize, len: usize) -> Vec<f32> {
    let mut c = p.channel(i);
    c.resize(len, 0.0);
    c
}

fn zip2(a: &[f32], b: &[f32], f: impl Fn(f32, f32) -> f32) -> Vec<f32> {
    a.iter().zip(b).map(|(&x, &y)| f(x, y)).collect()
}

/// Places the stems into 6 channels (FL FR FC LFE SL SR) of the source's length.
///
/// All inputs must be stereo (the audio job only calls this with stereo);
/// this is asserted. Stems shorter than `src` are padded with silence, longer
/// ones are trimmed.
///
/// Speech goes to FC (high-passed at 70 Hz, amplitude 1); the sub-70 Hz part
/// of the speech mono and the side part of the speech stay in FL/FR, so
/// FL + FC reconstructs the source's left channel exactly (before the
/// surrounds), including anything the separation left in the residual.
pub fn place(src: &Pcm, speech: &Pcm, music: &Pcm, effects: &Pcm, level: LfeLevel) -> Pcm {
    assert!(
        [src, speech, music, effects]
            .iter()
            .all(|p| p.channels == 2),
        "place needs stereo inputs"
    );
    let n = src.frames();
    let rate = RATE as f32;
    let ch = |p: &Pcm| (fitted(p, 0, n), fitted(p, 1, n));
    let (sl, sr) = ch(src);
    let (spl, spr) = ch(speech);
    let (ml, mr) = ch(music);
    let (el, er) = ch(effects);

    let res = |s: &[f32], sp: &[f32], m: &[f32], e: &[f32]| -> Vec<f32> {
        (0..n).map(|i| s[i] - (sp[i] + m[i] + e[i])).collect()
    };
    let (rl, rr) = (res(&sl, &spl, &ml, &el), res(&sr, &spr, &mr, &er));

    let mono_speech = zip2(&spl, &spr, |a, b| (a + b) * 0.5);
    let fc = zero_phase(&mono_speech, |s| lr4_highpass(s, CENTRE_HIGHPASS_HZ, rate));
    let low: Vec<f32> = zip2(&mono_speech, &fc, |m, c| m - c);

    let front = |m: &[f32], e: &[f32], r: &[f32], sp: &[f32]| -> Vec<f32> {
        (0..n)
            .map(|i| m[i] + e[i] + r[i] + (sp[i] - mono_speech[i]) + low[i])
            .collect()
    };
    let (fl, fr) = (front(&ml, &el, &rl, &spl), front(&mr, &er, &rr, &spr));

    let side: Vec<f32> = (0..n)
        .map(|i| ((ml[i] + el[i]) - (mr[i] + er[i])) * 0.5)
        .collect();
    let side = band(&side, SURROUND_BAND.0, SURROUND_BAND.1, rate);
    let side = delay(&side, (SURROUND_DELAY_S * rate).round() as usize);
    let g = db_to_gain(SURROUND_GAIN_DB);
    let surround = |seed: u32| -> Vec<f32> {
        decorrelate(&side, seed)
            .into_iter()
            .map(|v| v * g)
            .collect()
    };

    let mono_fx = zip2(&el, &er, |a, b| (a + b) * 0.5);
    let lfe = make_lfe(&mono_fx, LfeSource::Effects, level, rate);

    Pcm::from_channels(RATE, &[fl, fr, fc, lfe, surround(1), surround(2)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::rms_db;

    const R: f32 = RATE as f32;

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / R).sin())
            .collect()
    }

    fn noise(n: usize, mut s: u32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                ((s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    fn stereo(l: Vec<f32>, r: Vec<f32>) -> Pcm {
        Pcm::from_channels(RATE, &[l, r])
    }

    fn mid(x: &[f32]) -> &[f32] {
        &x[x.len() / 4..x.len() * 3 / 4]
    }

    #[test]
    fn speech_goes_to_centre_only() {
        let n = 96_000;
        let v = sine(1000.0, 0.3, n);
        let speech = stereo(v.clone(), v.clone());
        let silent = stereo(vec![0.0; n], vec![0.0; n]);
        let out = place(&speech, &speech, &silent, &silent, LfeLevel::Normal);
        let fc = rms_db(mid(&out.channel(2)));
        for c in [0, 1, 4, 5] {
            let level = rms_db(mid(&out.channel(c)));
            assert!(level < fc - 40.0, "channel {c} at {level}, FC at {fc}");
        }
        assert!(fc > -15.0, "FC at {fc}");
        assert!(rms_db(&out.channel(3)) <= -119.0, "LFE not silent");
    }

    #[test]
    fn nothing_lost() {
        let n = 96_000;
        let (spl, spr) = (noise(n, 1, 0.1), noise(n, 2, 0.1));
        let (ml, mr) = (noise(n, 3, 0.1), noise(n, 4, 0.1));
        let (el, er) = (noise(n, 5, 0.1), noise(n, 6, 0.1));
        let (xl, xr) = (noise(n, 7, 0.01), noise(n, 8, 0.01));
        let sum = |a: &[f32], b: &[f32], c: &[f32], d: &[f32]| -> Vec<f32> {
            (0..n).map(|i| a[i] + b[i] + c[i] + d[i]).collect()
        };
        let src = stereo(sum(&spl, &ml, &el, &xl), sum(&spr, &mr, &er, &xr));
        let out = place(
            &src,
            &stereo(spl, spr),
            &stereo(ml, mr),
            &stereo(el, er),
            LfeLevel::Normal,
        );
        for (front, other) in [(0, 0), (1, 1)] {
            let down: Vec<f32> = zip2(&out.channel(front), &out.channel(2), |a, b| a + b);
            let diff = zip2(&down, &src.channel(other), |a, b| a - b);
            let res = rms_db(&diff);
            assert!(res <= -30.0, "channel {front} residual {res} dBFS");
        }
    }

    #[test]
    fn effects_bass_reaches_lfe() {
        let n = 96_000;
        let fx = sine(50.0, 0.3, n);
        let effects = stereo(fx.clone(), fx);
        let silent = stereo(vec![0.0; n], vec![0.0; n]);
        let out = place(&effects, &silent, &silent, &effects, LfeLevel::Normal);
        let lfe = rms_db(mid(&out.channel(3)));
        assert!(lfe > -40.0, "LFE at {lfe}");
    }

    #[test]
    fn output_length_and_order() {
        let n = 48_000;
        let v = noise(n, 9, 0.2);
        let src = stereo(v.clone(), v);
        let silent = stereo(vec![0.0; n], vec![0.0; n]);
        let out = place(&src, &silent, &src, &silent, LfeLevel::Normal);
        assert_eq!(out.channels, 6);
        assert_eq!(out.rate, RATE);
        assert_eq!(out.frames(), n);
        // Identical L/R music: no side signal, so no surround; FL = FR = src.
        assert_eq!(out.channel(0), src.channel(0));
        assert_eq!(out.channel(1), src.channel(1));
        assert!(rms_db(&out.channel(2)) <= -119.0);
        assert!(rms_db(&out.channel(4)) <= -100.0);
        assert!(rms_db(&out.channel(5)) <= -100.0);
    }

    #[test]
    fn surrounds_get_delayed_side_signal() {
        let n = 96_000;
        let l = noise(n, 21, 0.2);
        let r = noise(n, 22, 0.2);
        let src = stereo(l, r);
        let silent = stereo(vec![0.0; n], vec![0.0; n]);
        let out = place(&src, &silent, &src, &silent, LfeLevel::Normal);
        let sl = out.channel(4);
        let delay_samples = (0.015 * R).round() as usize;
        assert!(rms_db(&sl[..delay_samples / 2]) <= -100.0);
        assert!(rms_db(mid(&sl)) > -40.0);
    }

    #[test]
    fn stems_shorter_than_source_are_padded() {
        let n = 48_000;
        let v = noise(n, 31, 0.2);
        let src = stereo(v.clone(), v.clone());
        let short = |amp: f32| {
            let s = noise(n - 5, 32, amp);
            stereo(s.clone(), s)
        };
        let out = place(
            &src,
            &short(0.1),
            &short(0.05),
            &short(0.02),
            LfeLevel::Normal,
        );
        assert_eq!(out.channels, 6);
        assert_eq!(out.frames(), n);
    }
}
