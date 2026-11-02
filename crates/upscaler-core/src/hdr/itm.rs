//! Inverse tone mapping of SDR frames to HDR, in pure Rust. The curve is the owner-approved
//! "hdr10-v2" look: SDR white sits at 203 nits (BT.2408), everything up to 80 % of SDR white
//! luminance is unchanged, and above that a cubic expands the highlights towards the peak.
//! Colour is kept by scaling R, G and B with the same ratio as the luminance.

use super::measure::nits_to_pq;
use crate::settings::HdrMode;

/// SDR reference white in nits.
pub const SDR_WHITE: f32 = 203.0;
/// Fraction of SDR white luminance below which the picture is left as it is.
const KNEE: f32 = 0.8;
/// Nominal peak of an HLG signal, used to normalise before the OOTF.
const HLG_NOMINAL_PEAK: f32 = 1000.0;
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

/// Converter for one mode and peak; holds the BT.1886 decode table.
pub struct Itm {
    lut: Vec<f32>,
    mode: HdrMode,
    peak: f32,
    a: f32,
}

impl Itm {
    pub fn new(mode: HdrMode, peak_nits: u32) -> Itm {
        let peak = (peak_nits as f32).max(SDR_WHITE + 1.0);
        let lut = (0..=65535u32)
            .map(|c| (c as f32 / 65535.0).powf(2.4))
            .collect();
        Itm {
            lut,
            mode,
            peak,
            a: (peak - SDR_WHITE) / (1.0 - KNEE).powi(3),
        }
    }

    /// Linear BT.709 light (1.0 = SDR white) to BT.2020 nits, hue-preserving clipped at the peak.
    pub fn nits_2020(&self, lin: [f32; 3]) -> [f32; 3] {
        let x = (0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2]).clamp(0.0, 1.0);
        let ratio = if x > 1e-9 {
            let y_out = if x <= KNEE {
                x * SDR_WHITE
            } else {
                x * SDR_WHITE + self.a * (x - KNEE).powi(3)
            };
            y_out / (x * SDR_WHITE)
        } else {
            1.0
        };
        let k = SDR_WHITE * ratio;
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

    /// One pixel of 16-bit SDR (gamma coded) to 16-bit HDR code values.
    pub fn pixel(&self, px: [u16; 3]) -> [u16; 3] {
        let lin = [
            self.lut[px[0] as usize],
            self.lut[px[1] as usize],
            self.lut[px[2] as usize],
        ];
        let n = self.nits_2020(lin);
        let e = if self.mode == HdrMode::Hlg {
            hlg_signal(n)
        } else {
            [nits_to_pq(n[0]), nits_to_pq(n[1]), nits_to_pq(n[2])]
        };
        [code(e[0]), code(e[1]), code(e[2])]
    }
}

fn code(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
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

    fn gray_code(lin: f32) -> u16 {
        (lin.powf(1.0 / 2.4) * 65535.0).round() as u16
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
        let itm = Itm::new(HdrMode::Hdr10, 1000);
        let c = gray_code(0.18);
        let out = itm.pixel([c, c, c]);
        let nits = pq_to_nits(out[1] as f32 / 65535.0);
        let want = 0.18 * SDR_WHITE;
        assert!((nits - want).abs() / want < 0.05, "{nits} vs {want}");
    }

    #[test]
    fn itm_white_reaches_peak() {
        let itm = Itm::new(HdrMode::Hdr10, 1000);
        let out = itm.pixel([65535; 3]);
        let nits = pq_to_nits(out[1] as f32 / 65535.0);
        assert!((nits - 1000.0).abs() < 15.0, "{nits}");
    }

    #[test]
    fn itm_keeps_saturation() {
        // A saturated bright red: the R:G:B ratio of the BT.709 light must survive the
        // expansion (checked before the primaries matrix, which is the same for all).
        let itm = Itm::new(HdrMode::Hdr10, 1000);
        let lin = [1.0f32, 0.90, 0.40];
        let n = itm.nits_2020(lin);
        // Undo the matrix by solving via the unscaled path: the 709 input scaled by one
        // factor must give the same 2020 triple up to that factor.
        let k = n[0]
            / (M709_TO_2020[0][0] * lin[0]
                + M709_TO_2020[0][1] * lin[1]
                + M709_TO_2020[0][2] * lin[2]);
        for i in 0..3 {
            let want = k
                * (M709_TO_2020[i][0] * lin[0]
                    + M709_TO_2020[i][1] * lin[1]
                    + M709_TO_2020[i][2] * lin[2]);
            assert!(
                (n[i] - want).abs() / want.max(1e-3) < 1e-3,
                "{i}: {} vs {want}",
                n[i]
            );
        }
        assert!(k > SDR_WHITE, "highlight should be expanded, k = {k}");
    }

    #[test]
    fn hlg_reference_white() {
        // 203 nits (reference white) is an HLG signal of about 0.75 (BT.2408).
        let s = hlg_signal([203.0; 3]);
        assert!((s[1] - 0.75).abs() < 0.03, "{}", s[1]);
        // Mid grey passes through the full pixel path in 16-bit range.
        let c = (0.18f32.powf(1.0 / 2.4) * 65535.0).round() as u16;
        let itm = Itm::new(HdrMode::Hlg, 1000);
        let p = itm.pixel([c, c, c]);
        assert!(p[1] > 20000 && p[1] < 40000, "{}", p[1]);
    }
}
