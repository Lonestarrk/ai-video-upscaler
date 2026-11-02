//! HDR frames for an SDR screen: the frame browser shows what an HDR file looks like when it
//! is tone mapped down to 100 nits (PQ or HLG to nits, BT.2390-style roll-off, BT.2020 to
//! BT.709, sRGB).

use image::{ImageBuffer, Rgb, RgbImage};

use super::itm::hlg_to_nits;
use super::measure::{nits_to_pq, pq_to_nits};
use crate::settings::HdrMode;

/// Peak of the SDR display the picture is mapped to, in nits.
const SDR_PEAK: f32 = 100.0;

/// BT.2020 to BT.709 primaries (linear light).
const M2020_TO_709: [[f32; 3]; 3] = [
    [1.6605, -0.5876, -0.0728],
    [-0.1246, 1.1329, -0.0083],
    [-0.0182, -0.1006, 1.1187],
];

/// BT.2390 EETF in the PQ domain: identity up to the knee, then a Hermite spline that ends at
/// the target peak. `nits` in, nits out; `src_peak` is the mastering peak.
fn roll_off(nits: f32, src_peak: f32) -> f32 {
    if src_peak <= SDR_PEAK {
        return nits;
    }
    let pq_src = nits_to_pq(src_peak);
    let e1 = nits_to_pq(nits) / pq_src;
    let max_lum = nits_to_pq(SDR_PEAK) / pq_src;
    let ks = 1.5 * max_lum - 0.5;
    let e2 = if e1 < ks {
        e1
    } else {
        let t = (e1 - ks) / (1.0 - ks);
        let (t2, t3) = (t * t, t * t * t);
        (2.0 * t3 - 3.0 * t2 + 1.0) * ks
            + (t3 - 2.0 * t2 + t) * (1.0 - ks)
            + (-2.0 * t3 + 3.0 * t2) * max_lum
    };
    pq_to_nits(e2 * pq_src)
}

fn srgb_code(lin: f32) -> u8 {
    let l = lin.clamp(0.0, 1.0);
    let v = if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    (v * 255.0).round() as u8
}

/// A 16-bit HDR frame (PQ or HLG, BT.2020) as an 8-bit sRGB picture of 100-nit peak.
pub fn to_sdr_preview(
    img16: &ImageBuffer<Rgb<u16>, Vec<u16>>,
    mode: HdrMode,
    peak_nits: u32,
) -> RgbImage {
    let peak = (peak_nits as f32).max(SDR_PEAK);
    let mut out = RgbImage::new(img16.width(), img16.height());
    for (x, y, px) in img16.enumerate_pixels() {
        let e = px.0.map(|v| v as f32 / 65535.0);
        let mut n = if mode == HdrMode::Hlg {
            hlg_to_nits(e)
        } else {
            e.map(pq_to_nits)
        };
        // Roll off on the brightest channel and scale all three, which keeps the hue.
        let mx = n[0].max(n[1]).max(n[2]);
        if mx > 1e-6 {
            let k = roll_off(mx, peak) / mx;
            n = n.map(|v| v * k);
        }
        let mut o = [0u8; 3];
        for (c, row) in o.iter_mut().zip(M2020_TO_709) {
            *c = srgb_code((row[0] * n[0] + row[1] * n[1] + row[2] * n[2]) / SDR_PEAK);
        }
        out.put_pixel(x, y, Rgb(o));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(nits: f32) -> ImageBuffer<Rgb<u16>, Vec<u16>> {
        let v = (nits_to_pq(nits) * 65535.0).round() as u16;
        ImageBuffer::from_pixel(2, 2, Rgb([v, v, v]))
    }

    #[test]
    fn preview_maps_203_nits_to_near_white_gray() {
        let p = to_sdr_preview(&gray(203.0), HdrMode::Hdr10, 1000);
        for c in p.get_pixel(0, 0).0 {
            assert!((230..=255).contains(&c), "{:?}", p.get_pixel(0, 0));
        }
    }

    #[test]
    fn preview_keeps_dark_dark() {
        let p = to_sdr_preview(&gray(1.0), HdrMode::Hdr10, 1000);
        for c in p.get_pixel(0, 0).0 {
            assert!(c < 40, "{:?}", p.get_pixel(0, 0));
        }
    }

    #[test]
    fn hlg_preview_is_monotonic_and_bounded() {
        let mk = |v: u16| ImageBuffer::from_pixel(1, 1, Rgb([v, v, v]));
        let lo = to_sdr_preview(&mk(20000), HdrMode::Hlg, 1000)
            .get_pixel(0, 0)
            .0[1];
        let hi = to_sdr_preview(&mk(60000), HdrMode::Hlg, 1000)
            .get_pixel(0, 0)
            .0[1];
        assert!(lo < hi && hi > 200, "{lo} {hi}");
    }
}
