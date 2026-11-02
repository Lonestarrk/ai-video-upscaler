//! SMPTE ST 2084 (PQ) transfer functions and light-level measurement of PQ-coded 16-bit
//! frames: per-channel maxima, frame average of maxRGB, percentiles, MaxCLL and MaxFALL.

use std::sync::OnceLock;

use image::{ImageBuffer, Rgb};

const M1: f64 = 2610.0 / 16384.0;
const M2: f64 = 2523.0 / 4096.0 * 128.0;
const C1: f64 = 3424.0 / 4096.0;
const C2: f64 = 2413.0 / 4096.0 * 32.0;
const C3: f64 = 2392.0 / 4096.0 * 32.0;

const PERCENTILES: [f64; 9] = [0.01, 0.05, 0.10, 0.25, 0.50, 0.75, 0.90, 0.95, 0.99];

/// PQ code value (0..1) to absolute luminance in nits (0..10000).
pub fn pq_to_nits(v: f32) -> f32 {
    let p = (v.clamp(0.0, 1.0) as f64).powf(1.0 / M2);
    let num = (p - C1).max(0.0);
    let den = C2 - C3 * p;
    (10000.0 * (num / den).powf(1.0 / M1)) as f32
}

/// Absolute luminance in nits (0..10000) to PQ code value (0..1).
pub fn nits_to_pq(n: f32) -> f32 {
    let y = (n.clamp(0.0, 10000.0) as f64 / 10000.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2) as f32
}

/// Light levels of one frame, in nits.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameLight {
    /// Brightest value per colour channel (MaxSCL).
    pub max_scl: [f32; 3],
    /// Mean over all pixels of maxRGB.
    pub avg_max_rgb: f32,
    /// Percentiles of maxRGB at 1, 5, 10, 25, 50, 75, 90, 95 and 99 %.
    pub percentiles: [f32; 9],
}

/// Light levels of a whole clip.
#[derive(Clone, Debug)]
pub struct Measurement {
    pub max_cll: u32,
    pub max_fall: u32,
    pub frames: Vec<FrameLight>,
}

/// Nits of every 16-bit PQ code, computed once.
fn nits_table() -> &'static [f32] {
    static TABLE: OnceLock<Vec<f32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        (0..=65535u32)
            .map(|c| pq_to_nits(c as f32 / 65535.0))
            .collect()
    })
}

pub fn measure_frame(img: &ImageBuffer<Rgb<u16>, Vec<u16>>) -> FrameLight {
    let table = nits_table();
    let to_nits = |c: usize| table[c];
    // Codes are 16-bit, so a histogram of maxRGB codes gives exact percentiles cheaply.
    let mut hist = vec![0u32; 65536];
    let mut ch_max = [0u16; 3];
    for px in img.pixels() {
        let [r, g, b] = px.0;
        ch_max[0] = ch_max[0].max(r);
        ch_max[1] = ch_max[1].max(g);
        ch_max[2] = ch_max[2].max(b);
        hist[r.max(g).max(b) as usize] += 1;
    }
    let n = (img.width() as u64 * img.height() as u64).max(1);
    let mut sum = 0.0f64;
    let mut percentiles = [0.0f32; 9];
    let (mut seen, mut next) = (0u64, 0usize);
    for (code, &count) in hist.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let nits = to_nits(code);
        sum += nits as f64 * count as f64;
        seen += count as u64;
        while next < PERCENTILES.len() && seen as f64 >= (PERCENTILES[next] * n as f64).ceil() {
            percentiles[next] = nits;
            next += 1;
        }
    }
    FrameLight {
        max_scl: [
            to_nits(ch_max[0] as usize),
            to_nits(ch_max[1] as usize),
            to_nits(ch_max[2] as usize),
        ],
        avg_max_rgb: (sum / n as f64) as f32,
        percentiles,
    }
}

impl Measurement {
    /// MaxCLL and MaxFALL of a clip from its frames' light levels (CTA-861.3: MaxCLL is the
    /// brightest maxRGB of any pixel, MaxFALL the highest frame average of maxRGB).
    pub fn from_frames(frames: Vec<FrameLight>) -> Measurement {
        let max_cll = frames.iter().flat_map(|f| f.max_scl).fold(0.0f32, f32::max);
        let max_fall = frames.iter().map(|f| f.avg_max_rgb).fold(0.0f32, f32::max);
        Measurement {
            max_cll: max_cll.round() as u32,
            max_fall: max_fall.round() as u32,
            frames,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn uniform(code: u16, w: u32, h: u32) -> ImageBuffer<Rgb<u16>, Vec<u16>> {
        ImageBuffer::from_pixel(w, h, Rgb([code, code, code]))
    }

    fn code(nits: f32) -> u16 {
        (nits_to_pq(nits) * 65535.0).round() as u16
    }

    #[test]
    fn pq_round_trip() {
        for n in [0.1f32, 100.0, 203.0, 1000.0, 10000.0] {
            let back = pq_to_nits(nits_to_pq(n));
            assert!((back - n).abs() / n < 0.005, "{n} -> {back}");
        }
    }

    #[test]
    fn pq_reference_points() {
        assert!((nits_to_pq(100.0) - 0.5081).abs() < 0.001);
        assert!((nits_to_pq(1000.0) - 0.7518).abs() < 0.001);
    }

    #[test]
    fn measures_uniform_frames() {
        let f = measure_frame(&uniform(code(203.0), 8, 8));
        for c in f.max_scl {
            assert!((c - 203.0).abs() / 203.0 < 0.01, "{c}");
        }
        assert!((f.avg_max_rgb - 203.0).abs() / 203.0 < 0.01);

        let m = Measurement::from_frames(
            [203.0f32, 1000.0]
                .iter()
                .map(|n| measure_frame(&uniform(code(*n), 8, 8)))
                .collect(),
        );
        assert_eq!(m.frames.len(), 2);
        assert!(
            (m.max_cll as f32 - 1000.0).abs() / 1000.0 < 0.01,
            "{}",
            m.max_cll
        );
        assert!(
            (m.max_fall as f32 - 1000.0).abs() / 1000.0 < 0.01,
            "{}",
            m.max_fall
        );
    }

    #[test]
    fn percentiles_of_a_mixed_frame() {
        // 10 x 10 pixels: 50 at 10 nits, 40 at 100 nits, 10 at 1000 nits.
        let img = ImageBuffer::from_fn(10, 10, |x, y| {
            let n = match y * 10 + x {
                0..50 => 10.0,
                50..90 => 100.0,
                _ => 1000.0,
            };
            Rgb([code(n); 3])
        });
        let f = measure_frame(&img);
        let near = |a: f32, b: f32| (a - b).abs() / b < 0.01;
        let want = [10.0, 10.0, 10.0, 10.0, 10.0, 100.0, 100.0, 1000.0, 1000.0];
        for (got, want) in f.percentiles.iter().zip(want) {
            assert!(near(*got, want), "{:?}", f.percentiles);
        }
        assert!(near(
            f.avg_max_rgb,
            (50.0 * 10.0 + 40.0 * 100.0 + 10.0 * 1000.0) / 100.0
        ));
    }

    #[test]
    fn dark_frame_is_sane() {
        let f = measure_frame(&uniform(code(0.5), 8, 8));
        assert!(f.avg_max_rgb.is_finite() && f.avg_max_rgb >= 0.0);
        assert!(f.max_scl.iter().all(|v| v.is_finite()));
        assert!(f.percentiles.iter().all(|v| v.is_finite()));
        assert!(Measurement::from_frames(vec![f]).max_fall <= 1);
    }
}
