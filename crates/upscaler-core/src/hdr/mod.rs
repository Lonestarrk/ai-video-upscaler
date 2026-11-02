//! HDR export: PQ math, frame measurement, inverse tone mapping, the two conversion passes
//! (measure, then stream into the encoder) and the encoder parameters.

pub mod convert;
pub mod hdr10plus;
pub mod itm;
pub mod measure;
pub mod preview;

use std::path::PathBuf;

use crate::settings::HdrMode;

/// Everything the video encoder needs to write HDR: the mode, the mastering peak and the
/// light levels measured from the HDR frames.
#[derive(Clone, Debug, PartialEq)]
pub struct HdrEncode {
    pub mode: HdrMode,
    pub peak_nits: u32,
    pub max_cll: u32,
    pub max_fall: u32,
    /// Per-scene HDR10+ metadata (a JSON file for x265's `dhdr10-info`).
    pub hdr10plus_json: Option<PathBuf>,
}

/// HDR is encoded this many CRF steps below the user's CRF (16 gives 14). PQ spends its code
/// values on a much wider range than SDR, so the same CRF leaves too few bits for smooth
/// highlights and dark textures (planet test: blocky flash at CRF 16, clean at 14 with
/// aq-mode 3, still well under the SDR H.264 file's bitrate).
pub const HDR_CRF_OFFSET: u8 = 2;

/// The CRF x265 gets for HDR when the user chose `crf`.
pub fn hdr_crf(crf: u8) -> u8 {
    crf.saturating_sub(HDR_CRF_OFFSET)
}

/// The value of ffmpeg's `-x265-params` for HDR output at the user's `crf` (encoded at
/// `hdr_crf(crf)`, with aq-mode 3, which keeps texture in dark flat areas).
///
/// PQ modes carry the BT.2020 mastering display with the peak as its maximum luminance (in
/// 0.0001 nit units) and the measured MaxCLL/MaxFALL; HLG carries neither.
pub fn x265_params(h: &HdrEncode, crf: u8) -> String {
    let mut p = format!(
        "crf={}:aq-mode=3:repeat-headers=1:colorprim=bt2020:colormatrix=bt2020nc",
        hdr_crf(crf)
    );
    if h.mode.is_pq() {
        p.push_str(&format!(
            ":hdr10=1:hdr10-opt=1:transfer=smpte2084\
             :master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L({},1)\
             :max-cll={},{}",
            h.peak_nits as u64 * 10000,
            h.max_cll,
            h.max_fall
        ));
        if let Some(json) = &h.hdr10plus_json {
            p.push_str(&format!(":dhdr10-info={}", escape_param(json)));
        }
    } else {
        p.push_str(":transfer=arib-std-b67");
    }
    p
}

/// A path as a value inside ffmpeg's `-x265-params key=value:key=value`. ffmpeg splits that
/// string on `:` and honours backslash escapes, so the `:` of a Windows drive letter and the
/// backslashes of the path would break it. Separators become `/` (accepted by Windows) and
/// `:` and `'` are backslash-escaped for ffmpeg's parser.
fn escape_param(path: &std::path::Path) -> String {
    let mut out = String::new();
    for c in path.to_string_lossy().chars() {
        match c {
            '\\' => out.push('/'),
            ':' | '\'' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// The `-color_trc` name of the mode.
pub fn transfer_name(mode: HdrMode) -> &'static str {
    if mode.is_pq() {
        "smpte2084"
    } else {
        "arib-std-b67"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(mode: HdrMode) -> HdrEncode {
        HdrEncode {
            mode,
            peak_nits: 1000,
            max_cll: 812,
            max_fall: 143,
            hdr10plus_json: None,
        }
    }

    #[test]
    fn x265_params_hdr10() {
        let p = x265_params(&enc(HdrMode::Hdr10), 18);
        assert!(p.contains("L(10000000,1)"), "{p}");
        assert!(p.contains("max-cll=812,143"), "{p}");
        assert!(p.contains("G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)"));
        assert!(p.contains("hdr10=1") && p.contains("transfer=smpte2084"));
        assert!(p.contains("colorprim=bt2020") && p.contains("colormatrix=bt2020nc"));
        assert!(!p.contains("dhdr10-info"));
        // Two steps below the user's CRF, and aq-mode 3.
        assert!(p.starts_with("crf=16:aq-mode=3:"), "{p}");
        assert_eq!(hdr_crf(1), 0);
    }

    #[test]
    fn x265_params_hdr10plus_adds_json() {
        let mut h = enc(HdrMode::Hdr10Plus);
        h.hdr10plus_json = Some(PathBuf::from("m.json"));
        assert!(x265_params(&h, 18).contains("dhdr10-info=m.json"));
    }

    #[test]
    fn x265_params_escapes_windows_path() {
        let mut h = enc(HdrMode::Hdr10Plus);
        h.hdr10plus_json = Some(PathBuf::from(r"H:\My Jobs\a'b\hdr10plus.json"));
        let p = x265_params(&h, 18);
        assert!(
            p.contains(r"dhdr10-info=H\:/My Jobs/a\'b/hdr10plus.json"),
            "{p}"
        );
    }

    #[test]
    fn x265_params_hlg_has_no_mastering() {
        let p = x265_params(&enc(HdrMode::Hlg), 18);
        assert!(
            !p.contains("master-display") && !p.contains("max-cll"),
            "{p}"
        );
        assert!(
            p.contains("transfer=arib-std-b67") && !p.contains("hdr10=1"),
            "{p}"
        );
    }
}
