//! Is a video flagged with the wrong aspect ratio?
//!
//! Films are made in a handful of shapes. When black bars show where the picture is, its
//! shape can be measured; if that is no standard shape with the file's own flag, but becomes
//! one with another common flag (4:3 or 16:9), the file is probably flagged wrongly. Without
//! bars the whole frame is the picture, and a frame shape that no video standard uses (720x480
//! with square pixels is 3:2) is suspicious too.

use crate::ffmpeg::VideoInfo;

/// Picture shapes films and TV are made in.
pub const STANDARD: [(f64, &str); 10] = [
    (4.0 / 3.0, "4:3"),
    (1.375, "1.37:1"),
    (1.66, "1.66:1"),
    (16.0 / 9.0, "16:9"),
    (1.85, "1.85:1"),
    (2.0, "2:1"),
    (2.2, "2.20:1"),
    (2.35, "2.35:1"),
    (2.39, "2.39:1"),
    (2.55, "2.55:1"),
];

/// Frame shapes a video file is commonly flagged with.
pub const FRAME_FLAGS: [((u32, u32), &str); 2] = [((4, 3), "4:3"), ((16, 9), "16:9")];

const TOLERANCE: f64 = 0.025;

/// The standard shape `ratio` is, if any.
pub fn standard_name(ratio: f64) -> Option<&'static str> {
    STANDARD
        .iter()
        .find(|(r, _)| (ratio / r - 1.0).abs() <= TOLERANCE)
        .map(|(_, n)| *n)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    /// Frame shape (width:height) to show the whole stored frame at.
    pub frame: (u32, u32),
    pub frame_name: &'static str,
    /// The picture's shape with that frame shape.
    pub picture: f64,
    pub picture_name: &'static str,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    /// The picture's shape (after removing black bars) as the file declares it.
    pub picture: f64,
    /// Whether that is a standard shape.
    pub standard: Option<&'static str>,
    /// Frame shapes that would make the picture a standard shape; empty when it already is.
    pub suggestions: Vec<Suggestion>,
}

/// Checks the shape of `info`'s picture with the file's own flag, using the black bars it
/// found (`detected_crop`).
pub fn check(info: &VideoInfo) -> Check {
    let declared = info.declared_frame_aspect();
    let (pw, ph) = info
        .detected_crop
        .map(|c| (c.w as f64, c.h as f64))
        .unwrap_or((info.source_width as f64, info.source_height as f64));
    // Picture shape as a fraction of the frame shape, so it scales with any frame flag.
    let picture_fraction = (pw / info.source_width as f64) / (ph / info.source_height as f64);
    let picture = declared * picture_fraction;
    let standard = standard_name(picture);
    let mut suggestions = Vec::new();
    if standard.is_none() && !info.rotated {
        for (frame, frame_name) in FRAME_FLAGS {
            let f = frame.0 as f64 / frame.1 as f64;
            if (f / declared - 1.0).abs() < 0.03 {
                continue; // that is the flag it already has
            }
            let p = f * picture_fraction;
            if let Some(picture_name) = standard_name(p) {
                suggestions.push(Suggestion {
                    frame,
                    frame_name,
                    picture: p,
                    picture_name,
                });
            }
        }
    }
    Check {
        picture,
        standard,
        suggestions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::{parse_probe, Crop};
    use std::path::Path;

    fn info(w: u32, h: u32, sar: &str, crop: Option<Crop>) -> VideoInfo {
        let json = format!(
            r#"{{"streams":[{{"codec_type":"video","width":{w},"height":{h},"sample_aspect_ratio":"{sar}","avg_frame_rate":"25/1"}}],"format":{{"duration":"1"}}}}"#
        );
        let mut i = parse_probe(Path::new("x"), &json).unwrap();
        i.detected_crop = crop;
        i.with_crop(crop)
    }

    #[test]
    fn sintel_trailer_is_fine() {
        // 854x480 square pixels (16:9) with a 2.35:1 picture inside.
        let c = check(&info(
            854,
            480,
            "1:1",
            Some(Crop {
                w: 854,
                h: 364,
                x: 0,
                y: 58,
            }),
        ));
        assert_eq!(c.standard, Some("2.35:1"));
        assert!(c.suggestions.is_empty());
    }

    #[test]
    fn widescreen_flagged_as_16_9_but_really_4_3_frame() {
        // A 4:3 DVD frame with a 2.35:1 letterbox, wrongly flagged 16:9: the picture measures
        // 3.13:1, which no film is; flagged 4:3 it is 2.35:1.
        let c = check(&info(
            720,
            576,
            "64:45",
            Some(Crop {
                w: 720,
                h: 326,
                x: 0,
                y: 125,
            }),
        ));
        assert!(c.standard.is_none(), "{c:?}");
        assert_eq!(c.suggestions.len(), 1);
        assert_eq!(
            (c.suggestions[0].frame, c.suggestions[0].picture_name),
            ((4, 3), "2.35:1")
        );
    }

    #[test]
    fn dvd_without_flag_gets_both_suggestions() {
        // 720x480 with square pixels is 3:2: no standard. 4:3 and 16:9 both fit.
        let c = check(&info(720, 480, "1:1", None));
        assert!(c.standard.is_none());
        let frames: Vec<_> = c.suggestions.iter().map(|s| s.frame).collect();
        assert_eq!(frames, vec![(4, 3), (16, 9)]);
    }

    #[test]
    fn ambiguous_case_is_left_alone() {
        // 1.76:1 is standard 16:9 as declared, even if it could be a wrongly flagged 2.35:1.
        let c = check(&info(
            720,
            576,
            "16:15",
            Some(Crop {
                w: 720,
                h: 436,
                x: 0,
                y: 70,
            }),
        ));
        assert_eq!(c.standard, Some("16:9"));
        assert!(c.suggestions.is_empty());
    }

    #[test]
    fn override_changes_pixel_aspect() {
        let i = info(
            720,
            576,
            "64:45",
            Some(Crop {
                w: 720,
                h: 326,
                x: 0,
                y: 125,
            }),
        );
        let fixed = i.with_frame_aspect(Some((4, 3)));
        assert_eq!(fixed.sar, (16, 15));
        assert_eq!((fixed.width, fixed.height), (768, 326));
        assert_ne!(fixed.identity(), i.identity());
        let square = i.with_frame_aspect(Some((720, 576)));
        assert_eq!(square.sar, (1, 1));
        assert_eq!(i.with_frame_aspect(None).sar, (64, 45));
    }
}
