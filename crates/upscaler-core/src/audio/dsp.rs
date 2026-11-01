//! Small offline DSP primitives on mono `f32` slices. Everything is linear in
//! the length of the input, so whole sections of several minutes are fine.

const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
const FLOOR_DB: f32 = -120.0;

/// Second-order section (RBJ cookbook), Direct Form II transposed. Coefficients
/// and state are f64 so a 20 Hz filter at 48 kHz keeps its precision.
#[derive(Debug, Clone)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn from_coeffs(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn lowpass(f: f32, q: f32, rate: f32) -> Self {
        let w = 2.0 * std::f64::consts::PI * f as f64 / rate as f64;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q as f64);
        Self::from_coeffs(
            (1.0 - c) / 2.0,
            1.0 - c,
            (1.0 - c) / 2.0,
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        )
    }

    pub fn highpass(f: f32, q: f32, rate: f32) -> Self {
        let w = 2.0 * std::f64::consts::PI * f as f64 / rate as f64;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q as f64);
        Self::from_coeffs(
            (1.0 + c) / 2.0,
            -(1.0 + c),
            (1.0 + c) / 2.0,
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        )
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let x = x as f64;
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y as f32
    }

    fn run(mut self, x: &[f32]) -> Vec<f32> {
        x.iter().map(|&v| self.process(v)).collect()
    }
}
/// Linkwitz-Riley 4th order low-pass (two cascaded Butterworth sections).
pub fn lr4_lowpass(x: &[f32], f: f32, rate: f32) -> Vec<f32> {
    let once = Biquad::lowpass(f, BUTTERWORTH_Q, rate).run(x);
    Biquad::lowpass(f, BUTTERWORTH_Q, rate).run(&once)
}

/// Linkwitz-Riley 4th order high-pass.
pub fn lr4_highpass(x: &[f32], f: f32, rate: f32) -> Vec<f32> {
    let once = Biquad::highpass(f, BUTTERWORTH_Q, rate).run(x);
    Biquad::highpass(f, BUTTERWORTH_Q, rate).run(&once)
}

/// Forward-backward filtering: no phase shift, magnitude response squared.
/// No edge padding, so expect a short transient at both ends of the slice.
pub fn zero_phase(x: &[f32], filter: impl Fn(&[f32]) -> Vec<f32>) -> Vec<f32> {
    let mut y = filter(x);
    y.reverse();
    let mut y = filter(&y);
    y.reverse();
    y
}

/// Zero-phase LR4 band-pass between `lo` and `hi` Hz.
pub fn band(x: &[f32], lo: f32, hi: f32, rate: f32) -> Vec<f32> {
    zero_phase(x, |s| lr4_highpass(&lr4_lowpass(s, hi, rate), lo, rate))
}

/// Peak follower in dBFS per sample (floor -120 dB).
pub fn envelope_db(x: &[f32], rate: f32, attack_ms: f32, release_ms: f32) -> Vec<f32> {
    let coef = |ms: f32| (-1.0 / (rate * ms.max(0.001) / 1000.0)).exp();
    let (att, rel) = (coef(attack_ms), coef(release_ms));
    let mut env = 0.0f32;
    x.iter()
        .map(|&v| {
            let a = v.abs();
            let c = if a > env { att } else { rel };
            env = c * env + (1.0 - c) * a;
            gain_to_db(env)
        })
        .collect()
}

/// Same length as the input, start zero-filled, tail cut off.
pub fn delay(x: &[f32], samples: usize) -> Vec<f32> {
    let mut y = vec![0.0; x.len()];
    if samples < x.len() {
        y[samples..].copy_from_slice(&x[..x.len() - samples]);
    }
    y
}

/// Prime delay lengths (about 2-6 ms at 48 kHz) for the Schroeder sections.
const ALLPASS_DELAYS: [usize; 8] = [97, 131, 163, 197, 233, 263, 293, 317];

/// Energy-preserving decorrelation: four first-order all-passes followed by
/// two Schroeder all-passes (`y[n] = -g x[n] + x[n-D] + g y[n-D]`) with
/// seed-dependent coefficients in 0.3..0.7 and seed-dependent prime delays.
/// The long delay lines are what make two different seeds actually
/// uncorrelated at low/mid frequencies; first-order sections alone do not.
pub fn decorrelate(x: &[f32], seed: u32) -> Vec<f32> {
    let mut state = (seed.wrapping_mul(2654435761).wrapping_add(0x9E37_79B9)) | 1;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let coef = |s: u32| 0.3 + 0.4 * (s >> 8) as f64 / (1u32 << 24) as f64;
    let mut y = x.to_vec();
    for _ in 0..4 {
        let a = coef(next());
        let (mut x1, mut y1) = (0.0f64, 0.0f64);
        for v in y.iter_mut() {
            let input = *v as f64;
            let out = -a * input + x1 + a * y1;
            x1 = input;
            y1 = out;
            *v = out as f32;
        }
    }
    for _ in 0..2 {
        let g = coef(next());
        let d = ALLPASS_DELAYS[next() as usize % ALLPASS_DELAYS.len()];
        let input = y.clone();
        for n in 0..y.len() {
            let (xd, yd) = if n >= d {
                (input[n - d] as f64, y[n - d] as f64)
            } else {
                (0.0, 0.0)
            };
            y[n] = (-g * input[n] as f64 + xd + g * yd) as f32;
        }
    }
    y
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn gain_to_db(g: f32) -> f32 {
    if g <= db_to_gain(FLOOR_DB) {
        FLOOR_DB
    } else {
        (20.0 * g.log10()).max(FLOOR_DB)
    }
}

/// RMS level in dBFS; empty or silent input gives -120.
pub fn rms_db(x: &[f32]) -> f32 {
    if x.is_empty() {
        return FLOOR_DB;
    }
    let ms = x.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / x.len() as f64;
    gain_to_db(ms.sqrt() as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE).sin())
            .collect()
    }

    fn noise(n: usize, mut s: u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
            })
            .collect()
    }

    fn mid(x: &[f32]) -> &[f32] {
        &x[x.len() / 4..x.len() * 3 / 4]
    }

    #[test]
    fn lowpass_90_passes_40_blocks_300() {
        let n = 48_000;
        let lo = zero_phase(&sine(40.0, 0.5, n), |s| lr4_lowpass(s, 90.0, RATE));
        let hi = zero_phase(&sine(300.0, 0.5, n), |s| lr4_lowpass(s, 90.0, RATE));
        let loss40 = rms_db(mid(&sine(40.0, 0.5, n))) - rms_db(mid(&lo));
        let loss300 = rms_db(mid(&sine(300.0, 0.5, n))) - rms_db(mid(&hi));
        assert!(loss40 < 1.0, "40 Hz lost {loss40} dB");
        assert!(loss300 > 30.0, "300 Hz lost only {loss300} dB");
    }

    #[test]
    fn zero_phase_keeps_peak_position() {
        let mut x = vec![0.0f32; 48_000];
        x[24_000] = 1.0;
        let y = zero_phase(&x, |s| lr4_lowpass(s, 90.0, RATE));
        let peak = y
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        assert!((peak as i64 - 24_000).abs() <= 1, "peak at {peak}");
    }

    #[test]
    fn band_isolates_range() {
        let n = 96_000;
        let level = |f: f32| {
            let y = band(&sine(f, 0.5, n), 100.0, 7000.0, RATE);
            rms_db(mid(&y))
        };
        let inside = level(1000.0);
        assert!(inside > -9.0 - 1.0, "1 kHz at {inside}");
        assert!(level(30.0) < inside - 30.0);
        assert!(level(15_000.0) < inside - 30.0);
    }

    #[test]
    fn envelope_follows_burst() {
        let mut x = vec![0.0f32; 24_000];
        x.extend(sine(50.0, db_to_gain(-20.0), 24_000));
        x.extend(vec![0.0; 48_000]);
        let slow = envelope_db(&x, RATE, 1.0, 250.0);
        let end = 48_000;
        assert!(slow[end - 100] > -22.0);
        assert!(
            slow[end + 48_000 - 1] < slow[end - 100] - 20.0,
            "release too slow"
        );
        let env = envelope_db(&x, RATE, 1.0, 100.0);
        assert_eq!(env.len(), x.len());
        assert!(env[100] <= -119.0);
        let onset = 24_000;
        for i in onset + 960..onset + 24_000 {
            assert!(
                (env[i] + 20.0).abs() < 2.0,
                "env {} at +{}",
                env[i],
                i - onset
            );
        }
    }

    #[test]
    fn decorrelated_pair_is_different_but_same_energy() {
        let x = noise(96_000, 7);
        let y = decorrelate(&x, 3);
        let dot: f32 = x.iter().zip(&y).map(|(a, b)| a * b).sum();
        let ex: f32 = x.iter().map(|a| a * a).sum();
        let ey: f32 = y.iter().map(|a| a * a).sum();
        let corr = dot / (ex * ey).sqrt();
        assert!(corr.abs() < 0.5, "correlation {corr}");
        assert!((rms_db(&x) - rms_db(&y)).abs() < 0.5);
        assert_eq!(y, decorrelate(&x, 3));
        assert_ne!(y, decorrelate(&x, 4));
    }

    #[test]
    fn seed_pair_is_decorrelated() {
        let s = band(&noise(96_000, 11), 100.0, 7000.0, RATE);
        let a = decorrelate(&s, 1);
        let b = decorrelate(&s, 2);
        let (a, b, m) = (mid(&a), mid(&b), mid(&s));
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let ea: f32 = a.iter().map(|x| x * x).sum();
        let eb: f32 = b.iter().map(|x| x * x).sum();
        let corr = dot / (ea * eb).sqrt();
        assert!(corr.abs() < 0.5, "correlation {corr}");
        assert!((rms_db(a) - rms_db(m)).abs() < 0.5);
        assert!((rms_db(b) - rms_db(m)).abs() < 0.5);
    }

    #[test]
    fn highpass_20_is_accurate() {
        let n = 96_000;
        let through = |f: f32| {
            let x = sine(f, 0.5, n);
            let y = zero_phase(&x, |s| lr4_highpass(s, 20.0, RATE));
            rms_db(mid(&x)) - rms_db(mid(&y))
        };
        // Theory: zero-phase LR4 at 20 Hz is -1.05 dB at 40 Hz, so 1.2 dB is the tight bound.
        assert!(through(40.0) < 1.2, "40 Hz lost {}", through(40.0));
        assert!(through(10.0) > 20.0, "10 Hz lost {}", through(10.0));
    }

    #[test]
    fn delay_and_rms_basics() {
        let d = delay(&[1.0, 2.0, 3.0], 2);
        assert_eq!(d, vec![0.0, 0.0, 1.0]);
        assert_eq!(delay(&[1.0, 2.0], 5), vec![0.0, 0.0]);
        assert_eq!(rms_db(&[]), -120.0);
        assert_eq!(rms_db(&[0.0; 10]), -120.0);
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-3);
    }
}
