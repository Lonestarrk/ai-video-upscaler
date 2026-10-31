//! Builds the instruction sent with each frame: the user's prompt plus the technical rules
//! that keep a video frame a video frame (no reframing, consistent with the previous frame).

pub struct PromptParts<'a> {
    pub user_prompt: &'a str,
    pub padded: bool,
    pub has_reference: bool,
    /// The picture is to be extended to this aspect ratio (name, and whether the new area is
    /// at the sides rather than above and below).
    pub extend: Option<(&'a str, bool)>,
}

pub fn build(p: &PromptParts) -> String {
    let mut s = String::new();
    s.push_str(p.user_prompt.trim());
    s.push_str("\n\nTechnical requirements (this is one frame of a video):\n");
    if let Some((name, sides)) = p.extend {
        let where_ = if sides {
            "left and right of"
        } else {
            "above and below"
        };
        s.push_str(&format!("- Change the aspect ratio to {name}. The FIRST image already has the {name} canvas: the plain mid-grey areas {where_} the picture are empty canvas. Fill them by naturally continuing the scene (same perspective, lighting, colors, style, depth of field and film grain), as if the shot had been filmed in {name}. No grey may remain, and there must be no visible seam or border where the original picture ends.\n"));
        s.push_str("- Inside the original picture area keep the framing, camera angle and composition exactly as they are. Do not crop, zoom, rotate, mirror or reframe.\n");
    } else {
        s.push_str("- Edit the FIRST image. Keep exactly the same framing, camera angle, aspect ratio and composition. Do not crop, zoom, rotate, mirror or reframe.\n");
    }
    s.push_str("- Every object, person and edge must stay in exactly the same position and size so the frame lines up with the rest of the video.\n");
    s.push_str("- Do not add text, watermarks, borders or new objects.\n");
    if p.padded {
        s.push_str("- The image has solid black bars added around the picture to fit the canvas. Keep those bars pure black and unchanged; restore only the picture inside them.\n");
    }
    if p.has_reference {
        s.push_str("- The SECOND image is the previous frame of the same shot, already restored. Use it only as a reference so details, textures, faces, skin, hair and colors look identical between the frames. The content and positions must follow the FIRST image, never the second.\n");
        if p.extend.is_some() {
            s.push_str("- The SECOND image also shows how the extended areas were filled in the previous frame. Continue them the same way so the added scenery stays stable from frame to frame, only following the camera and subject motion of the FIRST image.\n");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_reference_and_padding_only_when_used() {
        let plain = build(&PromptParts {
            user_prompt: "Restore.",
            padded: false,
            has_reference: false,
            extend: None,
        });
        assert!(plain.starts_with("Restore."));
        assert!(!plain.contains("SECOND") && !plain.contains("black bars"));
        let full = build(&PromptParts {
            user_prompt: "Restore.",
            padded: true,
            has_reference: true,
            extend: None,
        });
        assert!(full.contains("SECOND") && full.contains("black bars"));
        assert!(!full.contains("mid-grey"));
        let ext = build(&PromptParts {
            user_prompt: "Restore.",
            padded: false,
            has_reference: true,
            extend: Some(("2.39:1", true)),
        });
        assert!(
            ext.contains("Change the aspect ratio to 2.39:1") && ext.contains("left and right")
        );
        assert!(ext.contains("extended areas were filled in the previous frame"));
        assert!(!ext.contains("Keep exactly the same framing, camera angle, aspect ratio"));
    }
}
