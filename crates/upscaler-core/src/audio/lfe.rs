//! LFE (subwoofer) channel generation. The LFE is built from the bass of the
//! mix, gated so that dialogue and ordinary music never open it, filtered
//! zero-phase to 20..90 Hz and written 10..16 dB below the heard level.

use super::dsp::{band, db_to_gain, envelope_db, lr4_highpass, lr4_lowpass, zero_phase};
use crate::settings::LfeLevel;

/// Where the input of `make_lfe` comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfeSource {
    /// Separated effects stem: no voice guard needed.
    Effects,
    /// Mono sum of the original stereo (classic path): voice guard active.
    MonoSum,
}

const GATE_ATTACK_MS: f32 = 5.0;
const GATE_RELEASE_MS: f32 = 250.0;
const GATE_CLOSED_DB: f32 = -20.0;
/// Width of the soft knee below the threshold, in dB; keeps the gain smooth.
const GATE_KNEE_DB: f32 = 6.0;
const THRESHOLD_EFFECTS_DB: f32 = -45.0;
const THRESHOLD_MONOSUM_DB: f32 = -40.0;
/// Voice guard: band 30..60 must be within 6 dB of band 100..300.
const GUARD_MARGIN_DB: f32 = 6.0;
const GUARD_KNEE_DB: f32 = 3.0;
const GAIN_SMOOTH_MS: f32 = 10.0;
const CEILING_DB: f32 = -1.0;
const SYNTH_MIX_DB: f32 = -9.0;
/// Empirical: makes the filtered square's fundamental match the detection
/// band's sine amplitude before the mix gain is applied.
const SYNTH_NORM: f32 = 2.07;

/// Smooth per-sample gate gain in dB (0 = open, `GATE_CLOSED_DB` = closed).
fn open_amount(value_db: f32, closed_at: f32, knee: f32) -> f32 {
    ((value_db - closed_at) / knee).clamp(0.0, 1.0)
}

fn gate_gain_db(input: &[f32], source: LfeSource, rate: f32) -> Vec<f32> {
    let detect = band(input, 30.0, 100.0, rate);
    let env = envelope_db(&detect, rate, GATE_ATTACK_MS, GATE_RELEASE_MS);
    let threshold = match source {
        LfeSource::Effects => THRESHOLD_EFFECTS_DB,
        LfeSource::MonoSum => THRESHOLD_MONOSUM_DB,
    };
    let mut gain: Vec<f32> = env
        .iter()
        .map(|&e| GATE_CLOSED_DB * (1.0 - open_amount(e, threshold - GATE_KNEE_DB, GATE_KNEE_DB)))
        .collect();
    if source == LfeSource::MonoSum {
        let lo = envelope_db(
            &band(input, 30.0, 60.0, rate),
            rate,
            GATE_ATTACK_MS,
            GATE_RELEASE_MS,
        );
        let hi = envelope_db(
            &band(input, 100.0, 300.0, rate),
            rate,
            GATE_ATTACK_MS,
            GATE_RELEASE_MS,
        );
        for ((g, &l), &h) in gain.iter_mut().zip(&lo).zip(&hi) {
            // margin > 0 means the guard condition (lo >= hi - 6 dB) holds.
            let margin = l - h + GUARD_MARGIN_DB;
            let guard = GATE_CLOSED_DB * (1.0 - open_amount(margin, -GUARD_KNEE_DB, GUARD_KNEE_DB));
            *g = g.min(guard);
        }
    }
    // Extra one-pole smoothing so the gain cannot ripple at audible rates and
    // spread sidebands above the LFE band.
    let coef = (-1.0 / (rate * GAIN_SMOOTH_MS / 1000.0)).exp();
    let mut state = gain.first().copied().unwrap_or(0.0);
    for g in gain.iter_mut() {
        state = coef * state + (1.0 - coef) * *g;
        *g = state;
    }
    gain
}

/// Octave-down synthesis from the 50..90 Hz band of the (already gated) input.
fn subharmonic(gated: &[f32], rate: f32) -> Vec<f32> {
    let det = band(gated, 50.0, 90.0, rate);
    let env = envelope_db(&det, rate, GATE_ATTACK_MS, GATE_RELEASE_MS);
    let mut sign = 1.0f32;
    let mut prev = 0.0f32;
    let square: Vec<f32> = det
        .iter()
        .zip(&env)
        .map(|(&v, &e)| {
            if prev < 0.0 && v >= 0.0 {
                sign = -sign;
            }
            prev = v;
            sign * db_to_gain(e)
        })
        .collect();
    let lp = zero_phase(&square, |s| lr4_lowpass(s, 55.0, rate));
    let hp = zero_phase(&lp, |s| lr4_highpass(s, 20.0, rate));
    let gain = SYNTH_NORM * db_to_gain(SYNTH_MIX_DB);
    hp.into_iter().map(|v| v * gain).collect()
}

/// Smooth limiter: transparent below the knee, never above the ceiling.
fn soft_limit(x: f32) -> f32 {
    let ceiling = db_to_gain(CEILING_DB);
    let knee = ceiling * 0.8;
    let a = x.abs();
    if a <= knee {
        x
    } else {
        let room = ceiling - knee;
        x.signum() * (knee + room * ((a - knee) / room).tanh())
    }
}

/// Builds the LFE channel from `input` (same length as the input).
pub fn make_lfe(input: &[f32], source: LfeSource, level: LfeLevel, rate: f32) -> Vec<f32> {
    let gate = gate_gain_db(input, source, rate);
    let gated: Vec<f32> = input
        .iter()
        .zip(&gate)
        .map(|(&x, &g)| x * db_to_gain(g))
        .collect();
    let hp = zero_phase(&gated, |s| lr4_highpass(s, 20.0, rate));
    let mut lfe = zero_phase(&hp, |s| lr4_lowpass(s, 90.0, rate));
    if level.synth() && source == LfeSource::Effects {
        for (o, s) in lfe.iter_mut().zip(subharmonic(&gated, rate)) {
            *o += s;
        }
    }
    let gain = db_to_gain(level.file_gain_db());
    lfe.into_iter().map(|v| soft_limit(v * gain)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::rms_db;

    const RATE: f32 = 48_000.0;

    fn sine(freq: f32, rms_dbfs: f32, n: usize) -> Vec<f32> {
        let amp = db_to_gain(rms_dbfs) * std::f32::consts::SQRT_2;
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE).sin())
            .collect()
    }

    /// Silence, then `len` samples of `burst`, then silence.
    fn padded(burst: Vec<f32>) -> Vec<f32> {
        let mut x = vec![0.0; 48_000];
        x.extend(burst);
        x.extend(vec![0.0; 48_000]);
        x
    }

    fn noise(n: usize, mut s: u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn silent_in_dialogue() {
        let n = 4 * 48_000;
        let a = sine(120.0, -23.0, n);
        let b = sine(220.0, -23.0, n);
        let voice: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();
        let lfe = make_lfe(&voice, LfeSource::MonoSum, LfeLevel::Normal, RATE);
        let mid = &lfe[n / 4..n * 3 / 4];
        let r = rms_db(mid);
        println!("dialogue LFE rms {r}");
        assert!(r < -60.0, "LFE rms {r}");
    }

    #[test]
    fn explosion_reaches_lfe() {
        let burst = padded(sine(45.0, -18.0, 19_200));
        let lfe = make_lfe(&burst, LfeSource::Effects, LfeLevel::Normal, RATE);
        let seg = |x: &[f32]| rms_db(&x[48_000 + 4_800..48_000 + 14_400]);
        let delta = seg(&lfe) - seg(&burst);
        println!("explosion delta {delta}");
        assert!((delta - -13.0).abs() <= 2.0, "delta {delta}");
    }

    #[test]
    fn levels_step_by_3db() {
        let x = padded(sine(60.0, -18.0, 24_000));
        let r = |l| {
            let y = make_lfe(&x, LfeSource::Effects, l, RATE);
            rms_db(&y[48_000 + 6_000..48_000 + 18_000])
        };
        let (s, n, st) = (
            r(LfeLevel::Subtle),
            r(LfeLevel::Normal),
            r(LfeLevel::Strong),
        );
        println!("subtle {s} normal {n} strong {st}");
        assert!((n - s - 3.0).abs() <= 0.3, "{}", n - s);
        assert!((st - n - 3.0).abs() <= 0.3, "{}", st - n);
    }

    #[test]
    fn cinema_adds_subharmonic() {
        let x = padded(sine(70.0, -18.0, 48_000));
        // Hann-windowed single-bin DFT at 35 Hz: a band-pass would measure the
        // 70 Hz fundamental leaking through its skirts instead.
        let at35 = |l| {
            let y = make_lfe(&x, LfeSource::Effects, l, RATE);
            let w = &y[48_000 + 6_000..48_000 + 42_000];
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (i, &v) in w.iter().enumerate() {
                let hann =
                    0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / w.len() as f64).cos();
                let ph = 2.0 * std::f64::consts::PI * 35.0 * i as f64 / RATE as f64;
                re += v as f64 * hann * ph.cos();
                im += v as f64 * hann * ph.sin();
            }
            20.0 * ((re * re + im * im).sqrt() / w.len() as f64)
                .max(1e-12)
                .log10() as f32
        };
        let (c, s) = (at35(LfeLevel::Cinema), at35(LfeLevel::Strong));
        println!("35 Hz cinema {c} strong {s}");
        assert!(c >= s + 20.0, "cinema {c} strong {s}");
    }

    #[test]
    fn monosum_cinema_behaves_as_strong() {
        let x = padded(sine(70.0, -18.0, 48_000));
        let a = make_lfe(&x, LfeSource::MonoSum, LfeLevel::Cinema, RATE);
        let b = make_lfe(&x, LfeSource::MonoSum, LfeLevel::Strong, RATE);
        assert_eq!(a, b);
    }

    #[test]
    fn nothing_above_120hz() {
        let x = noise(4 * 48_000, 5);
        let lfe = make_lfe(&x, LfeSource::Effects, LfeLevel::Strong, RATE);
        let n = lfe.len();
        let mid = &lfe[n / 4..n * 3 / 4];
        // The analysis band is applied twice: a single LR4 band-pass at 150 Hz
        // only rejects 60..90 Hz by ~38 dB, which would measure the LFE's own
        // passband leaking through the analyser rather than content > 150 Hz.
        let high = band(&band(&lfe, 150.0, 1000.0, RATE), 150.0, 1000.0, RATE);
        let rel = rms_db(&high[n / 4..n * 3 / 4]) - rms_db(mid);
        println!("150-1000 relative {rel}");
        assert!(rel < -50.0, "relative {rel}");
    }

    #[test]
    fn peaks_capped() {
        for level in [LfeLevel::Strong, LfeLevel::Cinema] {
            let x = padded(sine(60.0, 0.0, 48_000).iter().map(|v| v * 4.0).collect());
            let lfe = make_lfe(&x, LfeSource::Effects, level, RATE);
            let peak = lfe.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            println!("peak {peak}");
            assert!(peak <= db_to_gain(-1.0) + 1e-6, "peak {peak}");
        }
    }
}
