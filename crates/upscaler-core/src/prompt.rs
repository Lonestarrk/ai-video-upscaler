//! Builds the instruction sent with each frame: the user's prompt plus the technical rules
//! that keep a video frame a video frame (no reframing, consistent with the previous frame).

pub struct PromptParts<'a> {
    pub user_prompt: &'a str,
    pub padded: bool,
    pub has_reference: bool,
}

pub fn build(p: &PromptParts) -> String {
    let mut s = String::new();
    s.push_str(p.user_prompt.trim());
    s.push_str("\n\nTechnical requirements (this is one frame of a video):\n");
    s.push_str("- Edit the FIRST image. Keep exactly the same framing, camera angle, aspect ratio and composition. Do not crop, zoom, rotate, mirror or reframe.\n");
    s.push_str("- Every object, person and edge must stay in exactly the same position and size so the frame lines up with the rest of the video.\n");
    s.push_str("- Do not add text, watermarks, borders or new objects.\n");
    if p.padded {
        s.push_str("- The image has solid black bars added around the picture to fit the canvas. Keep those bars pure black and unchanged; restore only the picture inside them.\n");
    }
    if p.has_reference {
        s.push_str("- The SECOND image is the previous frame of the same shot, already restored. Use it only as a reference so details, textures, faces, skin, hair and colors look identical between the frames. The content and positions must follow the FIRST image, never the second.\n");
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
        });
        assert!(plain.starts_with("Restore."));
        assert!(!plain.contains("SECOND") && !plain.contains("black bars"));
        let full = build(&PromptParts {
            user_prompt: "Restore.",
            padded: true,
            has_reference: true,
        });
        assert!(full.contains("SECOND") && full.contains("black bars"));
    }
}
