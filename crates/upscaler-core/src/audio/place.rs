//! 5.1 placement from AI-separated stems (speech, music, effects).
//! Channel order: FL FR FC LFE SL SR.

use super::dsp::{band, db_to_gain, decorrelate, delay, lr4_highpass, zero_phase};
use super::io::{Pcm, RATE};
use super::lfe::{make_lfe, LfeSource};
use crate::settings::LfeLevel;

const CENTRE_HIGHPASS_HZ: f32 = 70.0;
/// FC is written +3 dB (x sqrt 2) over the centre share taken from the fronts,
/// the ITU convention: a stereo phantom centre plays from two speakers, so one
/// real centre speaker needs +3 dB to sound as loud. The standard downmix
/// `L = FL + 0.7071 * FC` then rebuilds the source exactly. Shared with the
/// classic path.
pub(crate) const CENTRE_GAIN: f32 = std::f32::consts::SQRT_2;
/// The ITU downmix coefficient for FC and the surrounds (-3 dB), `1 / CENTRE_GAIN`.
pub(crate) const DOWNMIX_FC: f32 = std::f32::consts::FRAC_1_SQRT_2;
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
/// Speech goes to FC (high-passed at 70 Hz, written at +3 dB, see
/// [`CENTRE_GAIN`]); the sub-70 Hz part of the speech mono and the side part
/// of the speech stay in FL/FR, so the ITU downmix FL + 0.7071 FC
/// reconstructs the source's left channel exactly (before the surrounds),
/// including anything the separation left in the residual.
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
    let centre = zero_phase(&mono_speech, |s| lr4_highpass(s, CENTRE_HIGHPASS_HZ, rate));
    let low: Vec<f32> = zip2(&mono_speech, &centre, |m, c| m - c);
    let fc: Vec<f32> = centre.iter().map(|c| c * CENTRE_GAIN).collect();

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
            let down: Vec<f32> = zip2(&out.channel(front), &out.channel(2), |a, b| {
                a + DOWNMIX_FC * b
            });
            let diff = zip2(&down, &src.channel(other), |a, b| a - b);
            let res = rms_db(&diff);
            assert!(res <= -60.0, "channel {front} residual {res} dBFS");
        }
    }

    #[test]
    fn effects_bass_reaches_lfe() {
        let n = 96_000;
        // A 1 s bass hit between silences (n/4..3n/4, the measured middle):
        // the LFE ignores a steady background, so a constant tone would not
        // reach it.
        let mut fx = vec![0.0f32; n];
        fx[n / 4..n * 3 / 4].copy_from_slice(&sine(50.0, 0.3, n / 2));
        let effects = stereo(fx.clone(), fx);
        let silent = stereo(vec![0.0; n], vec![0.0; n]);
        let out = place(&effects, &silent, &silent, &effects, LfeLevel::Normal);
        let lfe = rms_db(mid(&out.channel(3)));
        assert!((-30.0..=-18.0).contains(&lfe), "LFE at {lfe}");
        let expected = make_lfe(&effects.channel(0), LfeSource::Effects, LfeLevel::Normal, R);
        assert_eq!(out.channel(3), expected);
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
        for c in 0..2 {
            let worst = zip2(&out.channel(c), &src.channel(c), |a, b| (a - b).abs())
                .into_iter()
                .fold(0.0f32, f32::max);
            assert!(worst <= 1e-6, "channel {c} differs by {worst}");
        }
        assert!(rms_db(&out.channel(2)) <= -119.0);
        assert!(rms_db(&out.channel(4)) <= -100.0);
        assert!(rms_db(&out.channel(5)) <= -100.0);
    }

    /// Music with R = -L: the mid is zero and the side signal is exactly `x`.
    fn side_only(x: &[f32]) -> Pcm {
        stereo(x.to_vec(), x.iter().map(|v| -v).collect())
    }

    fn silent(n: usize) -> Pcm {
        stereo(vec![0.0; n], vec![0.0; n])
    }

    fn correlation(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let ea: f32 = a.iter().map(|x| x * x).sum();
        let eb: f32 = b.iter().map(|x| x * x).sum();
        dot / (ea * eb).sqrt()
    }

    #[test]
    fn surround_level_is_minus_3_db_of_banded_side() {
        let n = 96_000;
        let x = noise(n, 21, 0.3);
        let music = side_only(&x);
        let out = place(&music, &silent(n), &music, &silent(n), LfeLevel::Normal);
        let reference = rms_db(mid(&band(&x, 100.0, 7000.0, R)));
        for c in [4, 5] {
            let diff = rms_db(mid(&out.channel(c))) - reference;
            assert!((diff + 3.0).abs() <= 0.5, "channel {c}: {diff} dB, want -3");
        }
    }

    #[test]
    fn surrounds_are_delayed_15_ms() {
        let n = 96_000;
        let delay_samples = (0.015 * R).round() as usize;
        assert_eq!(delay_samples, 720);
        // Noise burst in the middle of silence.
        let mut x = vec![0.0f32; n];
        let burst = noise(4_000, 41, 0.4);
        x[30_000..34_000].copy_from_slice(&burst);
        let music = side_only(&x);
        let out = place(&music, &silent(n), &music, &silent(n), LfeLevel::Normal);
        let banded = band(&x, 100.0, 7000.0, R);
        for (c, seed) in [(4, 1u32), (5, 2)] {
            // The all-pass decorrelator is linear and time-invariant, so the
            // expected output is the same filter applied to the undelayed band.
            let reference = decorrelate(&banded, seed);
            let ch = out.channel(c);
            assert!(rms_db(&ch[..delay_samples]) <= -100.0, "channel {c} early");
            let window = 30_000..34_000;
            let peak = (0..2_000usize)
                .map(|lag| {
                    let shifted = &ch[window.start + lag..window.end + lag];
                    (lag, correlation(&reference[window.clone()], shifted).abs())
                })
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .unwrap()
                .0;
            assert!(
                (peak as i64 - delay_samples as i64).abs() <= 2,
                "channel {c} peak at lag {peak}"
            );
        }
    }

    #[test]
    fn surrounds_are_decorrelated_from_each_other() {
        let n = 96_000;
        let x = noise(n, 51, 0.3);
        let music = side_only(&x);
        let out = place(&music, &silent(n), &music, &silent(n), LfeLevel::Normal);
        let rho = correlation(mid(&out.channel(4)), mid(&out.channel(5)));
        assert!(rho.abs() < 0.3, "SL/SR correlation {rho}");
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
        assert!(out.samples.iter().all(|v| v.is_finite()));
        // Past the end of the stems they are silence: FL + 0.7071 FC is the source again.
        let (fl, fc) = (out.channel(0), out.channel(2));
        for i in n - 5..n {
            let d = (fl[i] + DOWNMIX_FC * fc[i] - v[i]).abs();
            assert!(d <= 1e-6, "sample {i} differs by {d}");
        }
    }
}
