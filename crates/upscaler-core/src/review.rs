//! Quality control: the chosen service's own model looks at every finished frame once, next
//! to the original and the previous restored frame, and approves or rejects it. Rejected
//! frames are made again with the reasons added to the prompt.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub approved: bool,
    /// Short reasons for a rejection (empty when approved).
    #[serde(default)]
    pub problems: Vec<String>,
}

impl Verdict {
    pub fn summary(&self) -> String {
        self.problems.join("; ")
    }
}

/// JSON schema for the reviewer's answer: every problem with its severity. Only major
/// problems reject a frame.
pub const SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "problems": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "severity": { "type": "string", "enum": ["minor", "major"] },
          "description": { "type": "string" }
        },
        "required": ["severity", "description"],
        "additionalProperties": false
      }
    }
  },
  "required": ["problems"],
  "additionalProperties": false
}"#;

#[derive(Deserialize)]
struct Answer {
    problems: Vec<Problem>,
}

#[derive(Deserialize)]
struct Problem {
    severity: String,
    description: String,
}

impl From<Answer> for Verdict {
    fn from(a: Answer) -> Verdict {
        let major: Vec<String> = a
            .problems
            .into_iter()
            .filter(|p| p.severity.eq_ignore_ascii_case("major"))
            .map(|p| p.description)
            .collect();
        Verdict {
            approved: major.is_empty(),
            problems: major,
        }
    }
}

/// Two reviews of the same frame: a rejection stands only when both reject.
pub fn confirm(first: Verdict, second: Verdict) -> Verdict {
    if second.approved {
        return second;
    }
    let mut problems = first.problems;
    for p in second.problems {
        if !problems.contains(&p) {
            problems.push(p);
        }
    }
    Verdict {
        approved: false,
        problems,
    }
}

pub struct Context<'a> {
    pub has_previous: bool,
    /// The picture was extended to this aspect ratio (name), new area at the sides or not.
    pub extend: Option<(&'a str, bool)>,
    pub user_prompt: &'a str,
}

/// What the reviewer is told. Images: ORIGINAL, RESULT and, if given, PREVIOUS.
pub fn instruction(c: &Context) -> String {
    let mut s = String::from(
        "You are the quality inspector of a video restoration app. Judge ONE restored video frame. \
         Do not edit or create images and do not run any commands; only look and answer.\n\n\
         Attached images:\n\
         1. `original.png`: the frame as it is in the source video (low quality).\n\
         2. `result.png`: the restored and upscaled frame to judge.\n",
    );
    let mut i = 3;
    if let Some((name, sides)) = c.extend {
        let where_ = if sides {
            "left and right"
        } else {
            "above and below"
        };
        s.push_str(&format!("{i}. `result-source-area.png`: the part of `result.png` where the source picture sits, cut out and scaled to exactly the size of `original.png`.\n"));
        i += 1;
        s.push_str(&format!("\nIMPORTANT: this frame was deliberately changed to the {name} aspect ratio, so `result.png` is wider/taller than `original.png` on purpose: new, invented picture was added {where_} the source picture. That is expected and correct; never reject the result for being extended or for containing new scenery there. \
            For framing, zoom, position and faithfulness compare `original.png` with `result-source-area.png` only: they cover the same area at the same size, so objects should line up. Look at `result.png` as a whole only to judge the added areas and the seams.\n"));
    }
    if c.has_previous {
        s.push_str(&format!("{i}. `previous.png`: the already approved restored frame just before it in the same shot.\n"));
    }
    s.push_str("\nReject the result only for real, visible problems:\n");
    s.push_str("- Content changed compared with the original picture: objects, people or text added, removed, moved or resized; different framing, zoom, crop or camera angle (inside the source picture area); a different pose or expression.\n");
    s.push_str("- Faces or bodies changed: different identity, face shape, eyes, nose, mouth, age, hair or makeup; distorted anatomy; extra or missing fingers or limbs.\n");
    s.push_str("- Invented detail that the original does not support, or details reinterpreted (for example a shadow turned into anatomy, stars painted on the dark side of a planet, a dark object turned into empty space).\n");
    s.push_str("- Technical faults: stretched or squashed picture, borders or bars, leftover grey canvas, visible seams, watermarks, text overlays, heavy over-sharpening, painted or plastic look, strong color or brightness shift.\n");
    if let Some((name, sides)) = c.extend {
        let where_ = if sides {
            "left and right"
        } else {
            "top and bottom"
        };
        s.push_str(&format!("- Extension to {name} (new picture {where_} of the source picture): reject only if things cut off by the original edge do not continue naturally into the new area (wrong shape, wrong perspective, missing parts, a planet's night side showing stars), if there is a seam or a change in sharpness or color at the old edge, or if the new area contains eye-catching new objects or people.\n"));
    }
    if c.has_previous {
        s.push_str("- Consistency with `previous.png`: the same faces, hair, skin, clothing, textures, colors and brightness, and (if extended) the same added scenery, apart from the real motion between the two frames. Reject clear flicker-worthy differences, not tiny ones.\n");
    }
    s.push_str("\nThe restoration was asked to do this:\n=== RESTORATION PROMPT ===\n");
    s.push_str(c.user_prompt.trim());
    s.push_str("\n=== END ===\n\n");
    s.push_str("How to judge:\n\
                - A restored frame is supposed to be sharper and cleaner than the original. Better detail, less noise and slightly richer color are not problems.\n\
                - Before you say something moved, grew or shrank, check its position and size against the image edges and nearby objects in both images. A shift of a few pixels is not a problem.\n\
                - major: a viewer would notice it while watching (changed content, face or identity, a clear reframing, an invented object, a seam, stars on a dark object, clear flicker against the previous frame).\n\
                - minor: anything you are not sure about or that only shows when comparing closely.\n\
                Answer only with JSON: {\"problems\": [{\"severity\": \"minor\" or \"major\", \"description\": \"short and concrete, with where in the picture it is\"}]}. \
                Use an empty list when there is nothing to report.\n");
    s
}

/// Reads the verdict from a model answer that should be JSON but may have text around it.
pub fn parse(text: &str) -> Result<Verdict> {
    let t = text.trim();
    if let Ok(a) = serde_json::from_str::<Answer>(t) {
        return Ok(a.into());
    }
    let start = t.find('{');
    let end = t.rfind('}');
    if let (Some(a), Some(b)) = (start, end) {
        if a < b {
            if let Ok(a) = serde_json::from_str::<Answer>(&t[a..=b]) {
                return Ok(a.into());
            }
        }
    }
    Err(anyhow!(
        "the reviewer's answer was not understood: {}",
        t.chars().take(300).collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_wrapped_answers() {
        let v = parse(r#"{"problems": []}"#).unwrap();
        assert!(v.approved && v.problems.is_empty());
        let v = parse(r#"{"problems": [{"severity": "minor", "description": "slightly warmer"}]}"#)
            .unwrap();
        assert!(v.approved, "minor problems do not reject");
        let v = parse("Here you go:\n```json\n{\"problems\": [{\"severity\": \"major\", \"description\": \"stars on the night side, right edge\"}, {\"severity\": \"minor\", \"description\": \"grain\"}]}\n```").unwrap();
        assert!(!v.approved);
        assert_eq!(v.summary(), "stars on the night side, right edge");
        assert!(parse("no idea").is_err());

        let no = |p: &str| Verdict {
            approved: false,
            problems: vec![p.into()],
        };
        let yes = Verdict {
            approved: true,
            problems: vec![],
        };
        assert!(confirm(no("a"), yes.clone()).approved);
        assert_eq!(confirm(no("a"), no("b")).problems, vec!["a", "b"]);
    }

    #[test]
    fn instruction_mentions_only_what_applies() {
        let plain = instruction(&Context {
            has_previous: false,
            extend: None,
            user_prompt: "Restore.",
        });
        assert!(!plain.contains("previous.png") && !plain.contains("extended to"));
        let full = instruction(&Context {
            has_previous: true,
            extend: Some(("2.35:1", true)),
            user_prompt: "Restore.",
        });
        assert!(full.contains("4. `previous.png`") && full.contains("3. `result-source-area.png`"));
        assert!(full.contains("Extension to 2.35:1") && full.contains("left and right"));
    }
}
