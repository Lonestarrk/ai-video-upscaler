//! HDR10+ dynamic metadata (SMPTE ST 2094-40) as the JSON x265's `dhdr10-info` reads.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::measure::Measurement;

/// Writes the per-frame HDR10+ metadata of `m` to `path`, in the layout `hdr10plus_tool
/// extract` writes (profile A, one window per frame).
///
/// `cuts[i]` is true when a new scene starts at frame `i` of `m.frames` (frame 0 always does;
/// a missing entry counts as no cut). Every frame carries its own measured statistics, so
/// duplicate frames (which reuse their source's pixels) get that source's stats by
/// construction.
///
/// Units, as in the ST 2094-40 payload and `hdr10plus_tool`: MaxScl, AverageRGB and the
/// distribution values are integers in 0.1 cd/m^2, i.e. nits x 10 (10000 nits = 100000).
/// `peak_nits` is the mastering peak; metadata is not tone-mapped to it
/// (`TargetedSystemDisplayMaximumLuminance` is 0), values only stay inside the 17-bit range.
pub fn write_json(m: &Measurement, cuts: &[bool], peak_nits: u32, path: &Path) -> Result<()> {
    let _ = peak_nits;
    let units = |nits: f32| (nits as f64 * 10.0).round().clamp(0.0, 100_000.0) as u64;
    let n = m.frames.len();
    let mut first: Vec<usize> = Vec::new();
    let mut lengths: Vec<usize> = Vec::new();
    let mut frames: Vec<Value> = Vec::with_capacity(n);
    for (i, f) in m.frames.iter().enumerate() {
        if i == 0 || cuts.get(i).copied().unwrap_or(false) {
            first.push(i);
            lengths.push(0);
        }
        let scene = first.len() - 1;
        *lengths.last_mut().unwrap() += 1;
        frames.push(json!({
            "SceneFrameIndex": i - first[scene],
            "SceneId": scene,
            "SequenceFrameIndex": i,
            "NumberOfWindows": 1,
            "TargetedSystemDisplayMaximumLuminance": 0,
            "LuminanceParameters": {
                "AverageRGB": units(f.avg_max_rgb),
                "MaxScl": f.max_scl.map(units),
                "LuminanceDistributions": {
                    "DistributionIndex": [1, 5, 10, 25, 50, 75, 90, 95, 99],
                    "DistributionValues": f.percentiles.map(units),
                },
            },
        }));
    }
    let doc = json!({
        "JSONInfo": { "HDR10plusProfile": "A", "Version": "1.0" },
        "SceneInfo": frames,
        "SceneInfoSummary": { "SceneFirstFrameIndex": first, "SceneFrameNumbers": lengths },
        "ToolInfo": { "Tool": "AI Video Upscaler", "Version": env!("CARGO_PKG_VERSION") },
    });
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// The encoded (full-rate) frame, 1-based, that belongs to `out/` frame `k` (1-based). With
/// "every 2nd/3rd frame" the SDR frames are interpolated to the full rate first, so AI frame
/// `k` sits at full-rate frame `(k - 1) * divisor + 1`.
pub fn hdr_frame_for_out(k: u64, divisor: u32) -> u64 {
    (k.max(1) - 1) * divisor.max(1) as u64 + 1
}

/// Scene starts per *encoded* frame (0-based) from the export's cut list.
///
/// `ai_cuts` is indexed by `out/` frame number (1-based, entry 0 unused) like the job's cut
/// list. Without frame analysis that list is all true, which is no information, so then the
/// clip is one scene. When the SDR frames were interpolated to the full rate (`divisor` > 1),
/// AI frame `k` is the full-rate frame `(k - 1) * divisor` (minterpolate keeps the input
/// frames at their own timestamps), and the interpolated frames after it belong to its
/// scene. Per-frame statistics are always measured on the encoded frames themselves, so
/// duplicates and interpolated frames need no special handling here.
pub fn encoded_cuts(ai_cuts: &[bool], analysed: bool, divisor: u32, encoded: usize) -> Vec<bool> {
    let mut cuts = vec![false; encoded];
    if let Some(first) = cuts.first_mut() {
        *first = true;
    }
    if analysed {
        for (k, _) in ai_cuts.iter().enumerate().skip(1).filter(|(_, c)| **c) {
            if let Some(c) = cuts.get_mut(hdr_frame_for_out(k as u64, divisor) as usize - 1) {
                *c = true;
            }
        }
    }
    cuts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hdr::measure::FrameLight;

    fn meas(n: usize) -> Measurement {
        Measurement {
            max_cll: 900,
            max_fall: 200,
            frames: (0..n)
                .map(|i| FrameLight {
                    max_scl: [500.0 + i as f32, 400.0, 300.0],
                    avg_max_rgb: 50.5,
                    percentiles: [1.0, 5.0, 10.0, 20.0, 40.0, 80.0, 160.0, 320.0, 640.0],
                })
                .collect(),
        }
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn single_scene_json_shape() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h.json");
        let mut cuts = vec![false; 5];
        cuts[0] = true;
        write_json(&meas(5), &cuts, 1000, &p).unwrap();
        let j = read(&p);
        assert_eq!(j["JSONInfo"]["HDR10plusProfile"], "A");
        assert_eq!(j["JSONInfo"]["Version"], "1.0");
        assert_eq!(j["ToolInfo"]["Tool"], "AI Video Upscaler");
        assert!(j["ToolInfo"]["Version"].is_string());
        let f = j["SceneInfo"].as_array().unwrap();
        assert_eq!(f.len(), 5);
        for (i, s) in f.iter().enumerate() {
            assert_eq!(s["SequenceFrameIndex"], i);
            assert_eq!(s["SceneFrameIndex"], i);
            assert_eq!(s["SceneId"], 0);
            assert_eq!(s["NumberOfWindows"], 1);
            assert_eq!(s["TargetedSystemDisplayMaximumLuminance"], 0);
            let l = &s["LuminanceParameters"];
            assert_eq!(l["AverageRGB"], 505);
            assert_eq!(l["MaxScl"], serde_json::json!([5000 + 10 * i, 4000, 3000]));
            let d = &l["LuminanceDistributions"];
            assert_eq!(
                d["DistributionIndex"],
                serde_json::json!([1, 5, 10, 25, 50, 75, 90, 95, 99])
            );
            assert_eq!(
                d["DistributionValues"],
                serde_json::json!([10, 50, 100, 200, 400, 800, 1600, 3200, 6400])
            );
        }
        assert_eq!(
            j["SceneInfoSummary"]["SceneFirstFrameIndex"],
            serde_json::json!([0])
        );
        assert_eq!(
            j["SceneInfoSummary"]["SceneFrameNumbers"],
            serde_json::json!([5])
        );
    }

    #[test]
    fn hdr_frame_for_out_maps_to_full_rate() {
        assert_eq!(hdr_frame_for_out(1, 1), 1);
        assert_eq!(hdr_frame_for_out(5, 1), 5);
        assert_eq!(hdr_frame_for_out(1, 3), 1);
        assert_eq!(hdr_frame_for_out(3, 3), 7);
        assert_eq!(hdr_frame_for_out(4, 2), 7);
    }

    #[test]
    fn encoded_cuts_map_ai_frames_to_full_rate() {
        // AI frames 1..=4, cuts at 1 (always) and 3; divisor 3 -> 12 encoded frames.
        let ai = [false, true, false, true, false];
        let c = encoded_cuts(&ai, true, 3, 12);
        let at: Vec<usize> = (0..12).filter(|&i| c[i]).collect();
        assert_eq!(at, [0, 6]);
        // Not analysed: one scene.
        let c = encoded_cuts(&[false, true, true, true], false, 1, 3);
        assert_eq!(c, [true, false, false]);
        // No interpolation.
        let c = encoded_cuts(&ai, true, 1, 4);
        assert_eq!(c, [true, false, true, false]);
    }

    #[test]
    fn scenes_follow_cuts() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h.json");
        let mut cuts = vec![false; 10];
        cuts[3] = true;
        cuts[7] = true;
        write_json(&meas(10), &cuts, 1000, &p).unwrap();
        let j = read(&p);
        let s = &j["SceneInfoSummary"];
        assert_eq!(s["SceneFirstFrameIndex"], serde_json::json!([0, 3, 7]));
        assert_eq!(s["SceneFrameNumbers"], serde_json::json!([3, 4, 3]));
        let f = j["SceneInfo"].as_array().unwrap();
        let ids: Vec<u64> = f.iter().map(|x| x["SceneId"].as_u64().unwrap()).collect();
        assert_eq!(ids, [0, 0, 0, 1, 1, 1, 1, 2, 2, 2]);
        assert_eq!(f[3]["SceneFrameIndex"], 0);
        assert_eq!(f[6]["SceneFrameIndex"], 3);
        assert_eq!(f[9]["SceneFrameIndex"], 2);
    }
}
