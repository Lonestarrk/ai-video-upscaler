//! Inverse tone mapping of SDR frames to HDR, in pure Rust. The curve is the owner-approved
//! "hdr10-v2" look: SDR white sits at `sdr_white` nits (203 by default, BT.2408), everything
//! up to 80 % of SDR white luminance is unchanged, and above that a cubic expands the
//! highlights towards the peak. Colour is kept by scaling R, G and B with the same ratio.
//!
//! The ratio is computed from an edge-preserving smoothed luminance (a self-guided filter on
//! the gamma-coded luminance), not from the pixel's own: above SDR code ~238 one 8-bit step
//! becomes 6-13 PQ codes (5-13 % in nits), so a smooth highlight would turn into hard bands.
//! The filter evens out those steps in smooth areas and keeps edges; below the knee the
//! ratio is exactly 1 either way, so midtones and darks are unchanged.

use image::{ImageBuffer, Rgb, RgbImage};

use super::measure::nits_to_pq;
use crate::settings::HdrMode;

/// Default SDR reference white in nits (BT.2408).
pub const SDR_WHITE: u32 = 203;
/// Fraction of SDR white luminance below which the picture is left as it is.
const KNEE: f32 = 0.8;
/// Guided filter radius (pixels) and regularisation (gamma-coded luminance, 0..1).
const GUIDE_RADIUS: usize = 8;
const GUIDE_EPS: f32 = (1.5 / 255.0) * (1.5 / 255.0);
/// Nominal peak of an HLG signal, used to normalise before the OOTF.
pub const HLG_NOMINAL_PEAK: f32 = 1000.0;
/// System gamma of HLG at the nominal peak (BT.2100).
const HLG_GAMMA: f32 = 1.2;
/// HLG OETF constants (BT.2100).
const HLG_A: f32 = 0.178_832_77;
const HLG_B: f32 = 0.284_668_92;
const HLG_C: f32 = 0.559_910_73;

/// BT.709 to BT.2020 primaries (linear light).
const M709_TO_2020: [[f32; 3]; 3] = [
    [0.6274, 0.3293, 0.0433],
    [0.0691, 0.9195, 0.0114],
    [0.0164, 0.0880, 0.8956],
];

/// A 16-bit RGB frame of HDR code values (PQ or HLG, BT.2020).
pub type Frame16 = ImageBuffer<Rgb<u16>, Vec<u16>>;

/// Converter for one mode, peak and SDR white; holds the decode and PQ tables.
pub struct Itm {
    /// BT.1886 decode of the 8-bit codes (linear, 1.0 = SDR white).
    lin: [f32; 256],
    mode: HdrMode,
    peak: f32,
    white: f32,
    a: f32,
    /// The knee in gamma-coded luminance: smoothed values at or below it give ratio 1.
    knee_gamma: f32,
    /// `x^(1/2.4)` by table, indexed by `sqrt(x)` (the filter smooths it anyway).
    gamma: Vec<f32>,
    pq: PqTable,
}

impl Itm {
    pub fn new(mode: HdrMode, peak_nits: u32, sdr_white_nits: u32) -> Itm {
        let white = (sdr_white_nits as f32).max(1.0);
        let peak = (peak_nits as f32).max(white + 1.0);
        let mut lin = [0.0f32; 256];
        for (c, v) in lin.iter_mut().enumerate() {
            *v = (c as f32 / 255.0).powf(2.4);
        }
        Itm {
            lin,
            mode,
            peak,
            white,
            a: (peak - white) / (1.0 - KNEE).powi(3),
            knee_gamma: KNEE.powf(1.0 / 2.4),
            gamma: (0..=GAMMA_TABLE_SIZE)
                .map(|i| (i as f32 / GAMMA_TABLE_SIZE as f32).powf(2.0 / 2.4))
                .collect(),
            pq: PqTable::new(peak),
        }
    }

    /// Luminance ratio (output nits over `x * white`) for linear luminance `x` (1.0 = white).
    fn ratio(&self, x: f32) -> f32 {
        if x <= KNEE || x <= 1e-9 {
            1.0
        } else {
            let x = x.min(1.0);
            (x * self.white + self.a * (x - KNEE).powi(3)) / (x * self.white)
        }
    }

    /// Linear BT.709 light (1.0 = SDR white) scaled by `ratio`, to BT.2020 nits,
    /// hue-preserving clipped at the peak.
    pub fn nits_2020(&self, lin: [f32; 3], ratio: f32) -> [f32; 3] {
        let k = self.white * ratio;
        let n709 = [lin[0] * k, lin[1] * k, lin[2] * k];
        let mut n = [0.0f32; 3];
        for (o, row) in n.iter_mut().zip(M709_TO_2020) {
            *o = (row[0] * n709[0] + row[1] * n709[1] + row[2] * n709[2]).max(0.0);
        }
        let mx = n[0].max(n[1]).max(n[2]);
        if mx > self.peak {
            let s = self.peak / mx;
            n.iter_mut().for_each(|v| *v *= s);
        }
        n
    }

    fn encode(&self, n: [f32; 3]) -> [u16; 3] {
        if self.mode == HdrMode::Hlg {
            hlg_signal(n).map(code)
        } else {
            n.map(|v| self.pq.code(v))
        }
    }

    /// One 8-bit SDR frame to 16-bit HDR code values.
    pub fn convert(&self, img: &RgbImage) -> Frame16 {
        let (w, h) = img.dimensions();
        let raw = img.as_raw();
        let lum = |px: &[u8]| {
            (0.2126 * self.lin[px[0] as usize]
                + 0.7152 * self.lin[px[1] as usize]
                + 0.0722 * self.lin[px[2] as usize])
                .clamp(0.0, 1.0)
        };
        // Gamma-coded luminance, the guide and input of the filter.
        let yg: Vec<f32> = raw
            .chunks_exact(3)
            .map(|px| {
                let x = lum(px).sqrt() * GAMMA_TABLE_SIZE as f32;
                let i = (x as usize).min(GAMMA_TABLE_SIZE - 1);
                self.gamma[i] + (self.gamma[i + 1] - self.gamma[i]) * (x - i as f32)
            })
            .collect();
        // The filter's output is locally between the window mean and the pixel, so it never
        // exceeds the frame's maximum: a frame with nothing above the knee needs no filter.
        let bright = yg.iter().any(|&v| v > self.knee_gamma);
        let smooth = bright.then(|| guided_self(&yg, w as usize, h as usize));
        let mut out = Vec::with_capacity(raw.len());
        for (i, px) in raw.chunks_exact(3).enumerate() {
            let lin = [
                self.lin[px[0] as usize],
                self.lin[px[1] as usize],
                self.lin[px[2] as usize],
            ];
            let ratio = match &smooth {
                Some(s) if s[i] > self.knee_gamma && yg[i] > 0.0 => {
                    self.ratio(s[i].min(1.0).powf(2.4))
                }
                _ => 1.0,
            };
            out.extend_from_slice(&self.encode(self.nits_2020(lin, ratio)));
        }
        ImageBuffer::from_raw(w, h, out).expect("frame size")
    }
}

const GAMMA_TABLE_SIZE: usize = 4096;

fn code(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}

/// PQ encoding of 0..=peak nits to 16-bit codes by table: the exact curve needs two `powf`
/// in f64 per channel. Indexed by `(n / peak)^(1/8)`, where the curve is smooth down to black,
/// and interpolated linearly (within one 16-bit code of the exact value, tested).
struct PqTable {
    peak: f32,
    table: Vec<f32>,
}

const PQ_TABLE_SIZE: usize = 8192;

impl PqTable {
    fn new(peak: f32) -> PqTable {
        let table = (0..=PQ_TABLE_SIZE)
            .map(|i| {
                let t = i as f64 / PQ_TABLE_SIZE as f64;
                nits_to_pq((t.powi(8) * peak as f64) as f32) * 65535.0
            })
            .collect();
        PqTable { peak, table }
    }

    fn code(&self, nits: f32) -> u16 {
        let t = (nits / self.peak).clamp(0.0, 1.0).sqrt().sqrt().sqrt();
        let x = t * PQ_TABLE_SIZE as f32;
        let i = (x as usize).min(PQ_TABLE_SIZE - 1);
        let f = x - i as f32;
        let v = self.table[i] + (self.table[i + 1] - self.table[i]) * f;
        v.round().clamp(0.0, 65535.0) as u16
    }
}

/// `i` mirrored into `0..n` (edge pixel repeated: `cba|abc|cba`), for any `i`.
fn reflect(i: isize, n: usize) -> usize {
    let n = n as isize;
    let m = i.rem_euclid(2 * n);
    (if m < n { m } else { 2 * n - 1 - m }) as usize
}

/// Mean over a `(2r+1)²` box of `src` (`w`×`h`) into `dst`, separable running sums in f64.
fn box_mean(src: &[f32], w: usize, h: usize, r: usize, dst: &mut [f32], tmp: &mut [f32]) {
    let ri = r as isize;
    let norm = 1.0 / ((2 * r + 1) * (2 * r + 1)) as f64;
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        let mut sum: f64 = (-ri..=ri).map(|d| row[reflect(d, w)] as f64).sum();
        for x in 0..w {
            tmp[y * w + x] = sum as f32;
            // Mirror only at the edges (`reflect` divides).
            let (add, sub) = (x + r + 1, x as isize - ri);
            let add = if add < w {
                add
            } else {
                reflect(add as isize, w)
            };
            let sub = if sub >= 0 {
                sub as usize
            } else {
                reflect(sub, w)
            };
            sum += row[add] as f64 - row[sub] as f64;
        }
    }
    let mut sums = vec![0.0f64; w];
    for d in -ri..=ri {
        let row = &tmp[reflect(d, h) * w..][..w];
        sums.iter_mut().zip(row).for_each(|(s, v)| *s += *v as f64);
    }
    for y in 0..h {
        for x in 0..w {
            dst[y * w + x] = (sums[x] * norm) as f32;
        }
        let yi = y as isize;
        let (add, sub) = (reflect(yi + ri + 1, h), reflect(yi - ri, h));
        for x in 0..w {
            sums[x] += tmp[add * w + x] as f64 - tmp[sub * w + x] as f64;
        }
    }
}

/// He et al.'s guided filter with the picture as its own guide: edge-preserving smoothing.
fn guided_self(p: &[f32], w: usize, h: usize) -> Vec<f32> {
    let n = p.len();
    let mut tmp = vec![0.0f32; n];
    let mut m = vec![0.0f32; n];
    box_mean(p, w, h, GUIDE_RADIUS, &mut m, &mut tmp);
    let mut pp: Vec<f32> = p.iter().map(|v| v * v).collect();
    let mut mpp = vec![0.0f32; n];
    box_mean(&pp, w, h, GUIDE_RADIUS, &mut mpp, &mut tmp);
    // a into pp, b into m.
    for i in 0..n {
        let var = (mpp[i] - m[i] * m[i]).max(0.0);
        let a = var / (var + GUIDE_EPS);
        pp[i] = a;
        m[i] -= a * m[i];
    }
    box_mean(&pp, w, h, GUIDE_RADIUS, &mut mpp, &mut tmp);
    box_mean(&m, w, h, GUIDE_RADIUS, &mut pp, &mut tmp);
    for i in 0..n {
        mpp[i] = mpp[i] * p[i] + pp[i];
    }
    mpp
}

/// HLG signal (BT.2020) to display light in nits: inverse OETF, then the OOTF at the nominal
/// peak. The inverse of `hlg_signal`.
pub fn hlg_to_nits(e: [f32; 3]) -> [f32; 3] {
    let es = e.map(|v| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.5 {
            v * v / 3.0
        } else {
            (((v - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
        }
    });
    let ys = 0.2627 * es[0] + 0.6780 * es[1] + 0.0593 * es[2];
    let g = if ys > 1e-9 {
        ys.powf(HLG_GAMMA - 1.0)
    } else {
        0.0
    };
    es.map(|v| v * g * HLG_NOMINAL_PEAK)
}

/// BT.2020 display light in nits to an HLG signal: normalise by the nominal peak, undo the
/// OOTF (system gamma 1.2) and apply the HLG OETF.
fn hlg_signal(nits: [f32; 3]) -> [f32; 3] {
    let d = nits.map(|v| v / HLG_NOMINAL_PEAK);
    let yd = 0.2627 * d[0] + 0.6780 * d[1] + 0.0593 * d[2];
    // Scene light Es = Fd * Yd^((1 - gamma) / gamma).
    let g = if yd > 1e-9 {
        yd.powf((1.0 - HLG_GAMMA) / HLG_GAMMA)
    } else {
        0.0
    };
    d.map(|v| {
        let e = (v * g).max(0.0);
        if e <= 1.0 / 12.0 {
            (3.0 * e).sqrt()
        } else {
            HLG_A * (12.0 * e - HLG_B).ln() + HLG_C
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hdr::measure::pq_to_nits;

    fn gray_code(lin: f32) -> u8 {
        (lin.powf(1.0 / 2.4) * 255.0).round() as u8
    }

    fn uniform(c: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(24, 20, Rgb(c))
    }

    fn nits_at(img: &Frame16, x: u32, y: u32) -> f32 {
        pq_to_nits(img.get_pixel(x, y).0[1] as f32 / 65535.0)
    }

    /// The old per-pixel conversion: the ratio from the pixel's own luminance.
    fn per_pixel(itm: &Itm, img: &RgbImage) -> Frame16 {
        let mut out = Frame16::new(img.width(), img.height());
        for (x, y, p) in img.enumerate_pixels() {
            let lin = p.0.map(|c| itm.lin[c as usize]);
            let lum = (0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2]).clamp(0.0, 1.0);
            out.put_pixel(x, y, Rgb(itm.encode(itm.nits_2020(lin, itm.ratio(lum)))));
        }
        out
    }

    #[test]
    fn hlg_to_nits_inverts_hlg_signal() {
        let n = [120.0, 400.0, 60.0];
        let back = hlg_to_nits(hlg_signal(n));
        for (a, b) in n.iter().zip(back) {
            assert!((a - b).abs() / a < 0.01, "{n:?} {back:?}");
        }
    }

    #[test]
    fn itm_keeps_midtones() {
        for white in [203u32, 300, 400] {
            let itm = Itm::new(HdrMode::Hdr10, 1000, white);
            let c = gray_code(0.18);
            let lin = (c as f32 / 255.0).powf(2.4);
            let nits = nits_at(&itm.convert(&uniform([c; 3])), 3, 3);
            let want = lin * white as f32;
            assert!(
                (nits - want).abs() / want < 0.01,
                "{white}: {nits} vs {want}"
            );
        }
    }

    #[test]
    fn itm_white_reaches_peak() {
        for white in [203u32, 400] {
            let itm = Itm::new(HdrMode::Hdr10, 1000, white);
            let nits = nits_at(&itm.convert(&uniform([255; 3])), 5, 5);
            assert!((nits - 1000.0).abs() < 15.0, "{white}: {nits}");
        }
    }

    #[test]
    fn itm_keeps_saturation() {
        // A saturated bright colour: R:G:B of the BT.709 light survives the expansion, i.e.
        // the 2020 triple is the matrix applied to the input times one factor.
        let itm = Itm::new(HdrMode::Hdr10, 1000, 203);
        let lin = [1.0f32, 0.90, 0.40];
        let lum = 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
        let ratio = itm.ratio(lum);
        assert!(ratio > 1.2, "highlight should be expanded, ratio {ratio}");
        let n = itm.nits_2020(lin, ratio);
        let m: Vec<f32> = M709_TO_2020
            .iter()
            .map(|r| r[0] * lin[0] + r[1] * lin[1] + r[2] * lin[2])
            .collect();
        let k = n[0] / m[0];
        for i in 0..3 {
            assert!((n[i] / m[i] - k).abs() / k < 1e-4, "{n:?} vs {m:?}");
        }
    }

    #[test]
    fn hlg_reference_white() {
        // 203 nits (reference white) is an HLG signal of about 0.75 (BT.2408).
        let s = hlg_signal([203.0; 3]);
        assert!((s[1] - 0.75).abs() < 0.03, "{}", s[1]);
        // Mid grey passes through the whole frame path in 16-bit range.
        let itm = Itm::new(HdrMode::Hlg, 1000, 203);
        let p = itm
            .convert(&uniform([gray_code(0.18); 3]))
            .get_pixel(0, 0)
            .0[1];
        assert!(p > 20000 && p < 40000, "{p}");
    }

    #[test]
    fn pq_table_matches_the_curve() {
        for peak in [600.0f32, 1000.0, 1500.0] {
            let t = PqTable::new(peak);
            let mut worst = 0i32;
            for i in 0..=200_000 {
                // Dense near black (where PQ is steep) and over the whole range.
                let u = i as f32 / 200_000.0;
                for n in [u * peak, u * u * u * u * peak] {
                    let exact = (nits_to_pq(n) * 65535.0).round() as i32;
                    worst = worst.max((t.code(n) as i32 - exact).abs());
                }
            }
            assert!(worst <= 1, "peak {peak}: off by {worst} codes");
        }
    }

    #[test]
    fn box_mean_is_exact_on_small_frames() {
        // Brute force with the same mirroring, also for a frame smaller than the box.
        for (w, h) in [(5usize, 3usize), (40, 23)] {
            let src: Vec<f32> = (0..w * h)
                .map(|i| ((i * 37) % 101) as f32 / 100.0)
                .collect();
            let (mut dst, mut tmp) = (vec![0.0; w * h], vec![0.0; w * h]);
            box_mean(&src, w, h, 2, &mut dst, &mut tmp);
            for y in 0..h {
                for x in 0..w {
                    let mut s = 0.0f64;
                    for dy in -2isize..=2 {
                        for dx in -2isize..=2 {
                            let (xx, yy) =
                                (reflect(x as isize + dx, w), reflect(y as isize + dy, h));
                            s += src[yy * w + xx] as f64;
                        }
                    }
                    let want = (s / 25.0) as f32;
                    assert!((dst[y * w + x] - want).abs() < 1e-5, "{w}x{h} at {x},{y}");
                }
            }
        }
    }

    /// A smooth highlight in 8-bit steps (one code per 16 pixels, codes 240..=255): the
    /// per-pixel ratio turns each step into a hard jump of many PQ codes; the smoothed ratio
    /// spreads it out.
    #[test]
    fn smoothed_gain_removes_highlight_bands() {
        let itm = Itm::new(HdrMode::Hdr10, 1000, 203);
        let img = RgbImage::from_fn(256, 32, |x, _| {
            let c = 240 + (x / 16) as u8;
            Rgb([c, c, c])
        });
        let jump = |f: &Frame16| {
            (64..192u32)
                .map(|x| f.get_pixel(x + 1, 16).0[1] as i32 - f.get_pixel(x, 16).0[1] as i32)
                .map(i32::abs)
                .max()
                .unwrap()
        };
        let (old, new) = (jump(&per_pixel(&itm, &img)), jump(&itm.convert(&img)));
        assert!(new * 3 < old, "largest step {new} codes vs {old} per pixel");
    }

    /// Below the knee nothing changes, even right next to a highlight, and a dark pixel next
    /// to white gets no halo.
    #[test]
    fn darks_and_midtones_are_bit_identical() {
        let itm = Itm::new(HdrMode::Hdr10, 1000, 203);
        let img = RgbImage::from_fn(64, 32, |x, y| match x {
            0..32 => Rgb([30 + (y as u8), 90, 150]),
            _ => Rgb([255, 255, 250]),
        });
        let (old, new) = (per_pixel(&itm, &img), itm.convert(&img));
        for y in 0..32 {
            for x in 0..32 {
                assert_eq!(old.get_pixel(x, y), new.get_pixel(x, y), "at {x},{y}");
            }
        }
        // The highlight itself is still expanded.
        assert!(nits_at(&new, 48, 16) > 900.0, "{}", nits_at(&new, 48, 16));
    }
}
