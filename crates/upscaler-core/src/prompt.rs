//! Builds the instruction sent with each frame: the user's prompt plus the technical rules
//! that keep a video frame a video frame (no reframing, consistent with the previous frame).

pub struct PromptParts<'a> {
    pub user_prompt: &'a str,
    pub padded: bool,
    pub has_reference: bool,
    /// The picture is to be extended to this aspect ratio (name, and whether the new area is
    /// at the sides rather than above and below).
    pub extend: Option<(&'a str, bool)>,
    /// Why quality control rejected the previous attempt at this frame.
    pub feedback: Option<&'a str>,
}

/// Text that appears in the instructions only when the picture is to be extended.
const EXTEND_MARKER: &str = "EXTENSION TASK";

/// Whether `instructions` (from [`build`]) contain the aspect-ratio extension task.
pub fn has_extension_task(instructions: &str) -> bool {
    instructions.contains(EXTEND_MARKER)
}

pub fn build(p: &PromptParts) -> String {
    let mut s = String::new();
    if let Some((name, sides)) = p.extend {
        let where_ = if sides {
            "left and right of"
        } else {
            "above and below"
        };
        s.push_str(&format!("{EXTEND_MARKER}: this frame must be changed to the aspect ratio {name}. The plain mid-grey areas {where_} the picture are empty canvas and MUST be completely replaced by newly painted scene; leaving any of that grey in the result is a failure. The restoration rules below (recover what is there, do not invent) apply only inside the original picture area, never to the grey canvas.\n\n"));
    }
    s.push_str(p.user_prompt.trim());
    s.push_str("\n\nTechnical requirements (this is one frame of a video):\n");
    if let Some((name, sides)) = p.extend {
        let where_ = if sides {
            "left and right of"
        } else {
            "above and below"
        };
        let edges = if sides {
            "left and right edges"
        } else {
            "top and bottom edges"
        };
        s.push_str(&format!("- Change the aspect ratio to {name}. The FIRST image already has the {name} canvas: the plain mid-grey areas {where_} the picture are empty canvas to fill, as if the shot had been filmed in {name}.\n"));
        s.push_str(&format!("- Before drawing, study the strips of the original picture along its {edges}, where they meet the grey. List to yourself everything that is cut off there: objects, people, surfaces, walls, floor, ceiling, lights, consoles, furniture, set pieces, planets, ships, horizons and lines. Note each one's shape, size, color, texture, lighting direction, which side is lit and which is in shadow, and its perspective.\n"));
        s.push_str("- Continue every one of them across the edge, starting exactly where it is cut: edges, lines and curves keep their direction and perspective (a planet's outline completes its circle, a wall keeps its vanishing lines, a table or console keeps its edge, a light strip keeps its spacing), and partly visible objects are completed with their plausible full shape at the same scale. Lighting and shadows continue the same way.\n");
        s.push_str("- Objects cut off by the edge keep their unlit or shadowed parts in the new area. A dark object still hides what is behind it: no stars or background may show through the night side of a planet or a dark hull.\n");
        s.push_str("- Fill what remains with what the set or scene would naturally contain just outside the frame, in the same perspective, lighting, colors, style, depth of field and film grain. No new characters and nothing that draws attention.\n");
        s.push_str("- No grey may remain, and there must be no visible seam, border or change in sharpness where the original picture ends.\n");
        s.push_str("- Inside the original picture area keep the framing, camera angle and composition exactly as they are. Do not crop, zoom, rotate, mirror or reframe.\n");
    } else {
        s.push_str("- Edit the FIRST image. Keep exactly the same framing, camera angle, aspect ratio and composition. Do not crop, zoom, rotate, mirror or reframe.\n");
    }
    s.push_str("- Every object, person and edge must stay in exactly the same position and size so the frame lines up with the rest of the video.\n");
    s.push_str("- Do not add text, watermarks, borders or new objects.\n");
    s.push_str("- Dark areas that belong to an object (the night side of a planet, shadows, unlit surfaces) stay dark and solid. Do not read them as empty space: never add stars, lights or invented detail there.\n");
    if p.padded {
        s.push_str("- The image has solid black bars added around the picture to fit the canvas. Keep those bars pure black and unchanged; restore only the picture inside them.\n");
    }
    if let Some(f) = p.feedback.filter(|f| !f.trim().is_empty()) {
        s.push_str(&format!("- An earlier attempt at this frame was rejected by quality control because: {}. Make sure the new result does not have these problems.\n", f.trim().trim_end_matches('.')));
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
            feedback: None,
        });
        assert!(plain.starts_with("Restore."));
        assert!(!plain.contains("rejected by quality control"));
        assert!(!plain.contains("SECOND") && !plain.contains("black bars"));
        let full = build(&PromptParts {
            user_prompt: "Restore.",
            padded: true,
            has_reference: true,
            extend: None,
            feedback: Some("the face changed shape."),
        });
        assert!(full.contains("rejected by quality control because: the face changed shape. Make"));
        assert!(full.contains("SECOND") && full.contains("black bars"));
        assert!(!full.contains("mid-grey"));
        let ext = build(&PromptParts {
            user_prompt: "Restore.",
            padded: false,
            has_reference: true,
            extend: Some(("2.39:1", true)),
            feedback: None,
        });
        assert!(
            ext.contains("Change the aspect ratio to 2.39:1") && ext.contains("left and right")
        );
        assert!(ext.contains("extended areas were filled in the previous frame"));
        assert!(ext.contains("night side of a planet") && plain.contains("never add stars"));
        assert!(
            ext.contains("study the strips of the original picture along its left and right edges")
        );
        assert!(!plain.contains("study the strips"));
        assert!(!ext.contains("Keep exactly the same framing, camera angle, aspect ratio"));
        let task = ext.find("MUST be completely replaced").expect("task");
        assert!(task < ext.find("Restore.").unwrap());
        assert!(ext.contains("apply only inside the original picture"));
        assert!(has_extension_task(&ext) && !has_extension_task(&plain));
    }
}
