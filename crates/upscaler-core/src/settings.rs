//! User settings, saved as JSON in the config folder.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    /// ChatGPT through the official Codex CLI, signed in with a ChatGPT plan (no API cost).
    ChatGptSubscription,
    /// OpenAI Images API with an API key (pay as you go).
    OpenAiApi,
    /// Google Gemini image models ("Nano Banana") with an API key from Google AI Studio.
    Gemini,
    /// Black Forest Labs FLUX (Kontext / FLUX.2) with a BFL API key.
    Flux,
    /// xAI Grok Imagine with an xAI API key.
    Grok,
    /// Real-ESRGAN (realesrgan-ncnn-vulkan), free and local, needs a GPU with Vulkan.
    RealEsrgan,
    /// Plain Lanczos resize with a little sharpening: instant, for testing the workflow.
    Basic,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 7] = [
        ProviderKind::ChatGptSubscription,
        ProviderKind::OpenAiApi,
        ProviderKind::Gemini,
        ProviderKind::Flux,
        ProviderKind::Grok,
        ProviderKind::RealEsrgan,
        ProviderKind::Basic,
    ];

    /// Generative providers take a prompt and benefit from padding and a reference frame.
    pub fn is_generative(self) -> bool {
        matches!(
            self,
            ProviderKind::ChatGptSubscription
                | ProviderKind::OpenAiApi
                | ProviderKind::Gemini
                | ProviderKind::Flux
                | ProviderKind::Grok
        )
    }

    pub fn id(self) -> &'static str {
        match self {
            ProviderKind::ChatGptSubscription => "chatgpt-codex",
            ProviderKind::OpenAiApi => "openai-api",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Flux => "flux",
            ProviderKind::Grok => "grok",
            ProviderKind::RealEsrgan => "realesrgan",
            ProviderKind::Basic => "basic",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputSize {
    Scale2,
    Scale3,
    Scale4,
    Height1080,
    Height1440,
    Height2160,
}

impl OutputSize {
    pub const ALL: [OutputSize; 6] = [
        OutputSize::Scale2,
        OutputSize::Scale3,
        OutputSize::Scale4,
        OutputSize::Height1080,
        OutputSize::Height1440,
        OutputSize::Height2160,
    ];

    pub fn label(self) -> &'static str {
        match self {
            OutputSize::Scale2 => "2×",
            OutputSize::Scale3 => "3×",
            OutputSize::Scale4 => "4×",
            OutputSize::Height1080 => "1080p",
            OutputSize::Height1440 => "1440p",
            OutputSize::Height2160 => "2160p (4K)",
        }
    }

    /// The output size for a `w`×`h` source, rounded to even numbers (needed by yuv420p).
    pub fn target(self, w: u32, h: u32) -> (u32, u32) {
        let (tw, th) = match self {
            OutputSize::Scale2 => (w as f64 * 2.0, h as f64 * 2.0),
            OutputSize::Scale3 => (w as f64 * 3.0, h as f64 * 3.0),
            OutputSize::Scale4 => (w as f64 * 4.0, h as f64 * 4.0),
            OutputSize::Height1080 | OutputSize::Height1440 | OutputSize::Height2160 => {
                let th = match self {
                    OutputSize::Height1080 => 1080.0,
                    OutputSize::Height1440 => 1440.0,
                    _ => 2160.0,
                };
                (w as f64 * th / h.max(1) as f64, th)
            }
        };
        let even = |v: f64| ((v / 2.0).round() as u32).max(1) * 2;
        (even(tw), even(th))
    }
}

/// Aspect ratios the AI can extend the picture to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AspectChoice {
    Wide16x9,
    Tv4x3,
    Scope239,
    Flat185,
    Photo3x2,
    Square1x1,
    Portrait4x5,
    Vertical9x16,
}

impl AspectChoice {
    pub const ALL: [AspectChoice; 8] = [
        AspectChoice::Wide16x9,
        AspectChoice::Tv4x3,
        AspectChoice::Scope239,
        AspectChoice::Flat185,
        AspectChoice::Photo3x2,
        AspectChoice::Square1x1,
        AspectChoice::Portrait4x5,
        AspectChoice::Vertical9x16,
    ];

    /// Width / height.
    pub fn ratio(self) -> f64 {
        match self {
            AspectChoice::Wide16x9 => 16.0 / 9.0,
            AspectChoice::Tv4x3 => 4.0 / 3.0,
            AspectChoice::Scope239 => 2.39,
            AspectChoice::Flat185 => 1.85,
            AspectChoice::Photo3x2 => 1.5,
            AspectChoice::Square1x1 => 1.0,
            AspectChoice::Portrait4x5 => 0.8,
            AspectChoice::Vertical9x16 => 9.0 / 16.0,
        }
    }

    /// The ratio as written in prompts and menus.
    pub fn name(self) -> &'static str {
        match self {
            AspectChoice::Wide16x9 => "16:9",
            AspectChoice::Tv4x3 => "4:3",
            AspectChoice::Scope239 => "2.39:1",
            AspectChoice::Flat185 => "1.85:1",
            AspectChoice::Photo3x2 => "3:2",
            AspectChoice::Square1x1 => "1:1",
            AspectChoice::Portrait4x5 => "4:5",
            AspectChoice::Vertical9x16 => "9:16",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    H265,
}

/// How many of the source frames go through the AI. Skipped frames are rebuilt with
/// motion interpolation, which cuts cost and time at some loss of quality in fast motion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AiRate {
    All,
    Half,
    Third,
}

impl AiRate {
    pub const ALL: [AiRate; 3] = [AiRate::All, AiRate::Half, AiRate::Third];

    pub fn divisor(self) -> u32 {
        match self {
            AiRate::All => 1,
            AiRate::Half => 2,
            AiRate::Third => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareMode {
    SideBySide,
    Swipe,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptPreset {
    pub name: String,
    pub text: String,
}

/// The default prompt: faithful restoration in one pass. Structure before detail, nothing
/// invented, identity first.
pub const DEFAULT_PROMPT: &str = "Restore and upscale this video frame faithfully, in a single pass. Recover the picture that is actually there; do not invent a new one.

Work in this order within the one result:
1. Structure first: restore the shapes, silhouettes, edges, tonal transitions, shadows, highlights and colors that are present in the source. Remove blur, compression artifacts, blocking, banding, noise, fading and damage without changing what lies underneath. Keep every face's head shape, the positions and proportions of the eyes, eyebrows, nose, mouth, cheeks, chin, jawline, ears and forehead, the expression and the pose exactly as they are.
2. Then detail, only where that structure supports it: iris and pupil definition, eyelids and eyelashes, hair strands, skin texture, fabric, small objects. Detail must refine what is there, never add new physical features. Stop where more detail would require guessing.

No invention: do not create facial or other details just to make the picture look more realistic. A blurred eye becomes a clearer version of the same eye, not a new eye. Do not redesign noses or mouths; keep their original width, position and shape. Never interpret a cast shadow, for example below the chin, as anatomy such as a jaw, chin or neck contour. Where information is genuinely missing, leave it naturally soft rather than guessing. Do not beautify, slim, smooth or reshape anyone, and do not change age, makeup or hairstyle.

Priority when accuracy and detail conflict: 1) identity and facial geometry, 2) original positions and proportions, 3) original expression, 4) faithful photographic look (colors, lighting, grain, depth of field), 5) supported fine detail, 6) apparent resolution. Never sacrifice a higher priority for a lower one.

The result should look like the same frame filmed with a better camera: natural, not over-sharpened, not painted, not airbrushed.";

/// The first default prompt: a short, general clean-up. Kept as a preset.
pub const SIMPLE_PROMPT: &str = "Restore and upscale this video frame to a clean, high-resolution image. \
Keep it photorealistic and natural: recover fine detail and texture, sharpen soft edges, and remove \
compression artifacts, blocking, banding, noise and blur. Do not add, remove or move anything. \
Preserve the composition, faces, identities, expressions, text, colors, lighting and the overall \
look of the original exactly. The result should look like the same shot filmed with a better camera.";

/// Preset names, as shown in the prompt menu.
pub const FAITHFUL_PRESET: &str = "Trogen restaurering";
pub const SIMPLE_PRESET: &str = "Enkel uppstädning";

fn default_presets() -> Vec<PromptPreset> {
    vec![
        PromptPreset {
            name: FAITHFUL_PRESET.into(),
            text: DEFAULT_PROMPT.into(),
        },
        PromptPreset {
            name: SIMPLE_PRESET.into(),
            text: SIMPLE_PROMPT.into(),
        },
    ]
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "sv" or "en"; empty follows the system language.
    pub language: String,
    pub provider: ProviderKind,
    pub prompt: String,
    pub presets: Vec<PromptPreset>,

    // ChatGPT through Codex CLI
    /// Path to `codex`; empty looks it up on PATH and in the usual npm folders.
    pub codex_path: String,
    /// Codex model override; empty uses Codex's default.
    pub codex_model: String,
    /// Reasoning effort for the Codex agent turn ("low" saves plan quota).
    pub codex_reasoning: String,
    /// "workspace-write" or "danger-full-access" (if the sandbox does not work on this machine).
    pub codex_sandbox: String,
    pub codex_timeout_secs: u64,

    // OpenAI API
    pub openai_api_key: String,
    pub openai_model: String,
    /// "auto" or e.g. "1536x1024".
    pub openai_size: String,
    /// "high", "medium", "low" or "auto".
    pub openai_quality: String,
    pub openai_input_fidelity: bool,

    // Google Gemini
    pub gemini_api_key: String,
    pub gemini_model: String,
    /// "1K", "2K" or "4K".
    pub gemini_image_size: String,

    // Black Forest Labs FLUX
    pub bfl_api_key: String,
    /// Endpoint name: "flux-kontext-pro", "flux-kontext-max", "flux-2-pro", …
    pub bfl_model: String,

    // xAI Grok
    pub xai_api_key: String,
    pub xai_model: String,

    // Real-ESRGAN
    pub realesrgan_path: String,
    pub realesrgan_model: String,

    // Tools and folders
    /// Folder with ffmpeg and ffprobe; empty looks next to the app, in the downloaded tools and on PATH.
    pub ffmpeg_dir: String,
    /// Previews and export jobs; empty uses the default in local app data.
    pub work_dir: String,

    // Frame pipeline
    pub output_size: OutputSize,
    /// Pad frames to the classic image-model aspect ratios (1:1, 3:2, 2:3) and crop back afterwards,
    /// so the model never stretches or reframes a 16:9 frame.
    pub pad_to_model_aspect: bool,
    /// Anamorphic sources (non-square pixels, e.g. DVD) are restored with square pixels and
    /// squeezed back to the original pixel aspect ratio in the exported video.
    pub keep_anamorphic: bool,
    /// Cut away black bars (letterbox/pillarbox) found in the video before anything else.
    pub remove_black_bars: bool,
    /// Let the AI extend the picture to `target_aspect` (generative providers only).
    pub change_aspect: bool,
    pub target_aspect: AspectChoice,
    /// Send the previous upscaled frame as a reference image (sequential processing).
    pub use_previous_frame: bool,
    /// Mean difference (0-255) above which a frame starts a new shot and gets no reference.
    pub scene_cut_threshold: f32,
    /// Reuse the previous result for frames that are (almost) identical to the previous one.
    pub skip_duplicates: bool,
    pub ai_rate: AiRate,
    /// Requests in flight at once when no reference frame is used.
    pub parallel_requests: u32,
    /// Longest side of the reference frame sent to the model.
    pub reference_max_side: u32,

    // Output video
    pub video_codec: VideoCodec,
    pub crf: u8,

    // View
    pub compare_mode: CompareMode,
    pub ui_scale: f32,
    /// Measured average seconds per AI frame, per provider id, for time estimates.
    pub seconds_per_frame: Vec<(String, f32)>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            language: String::new(),
            provider: ProviderKind::ChatGptSubscription,
            prompt: DEFAULT_PROMPT.to_string(),
            presets: default_presets(),
            codex_path: String::new(),
            codex_model: String::new(),
            codex_reasoning: "low".into(),
            codex_sandbox: "workspace-write".into(),
            codex_timeout_secs: 600,
            openai_api_key: String::new(),
            openai_model: "gpt-image-2".into(),
            openai_size: "auto".into(),
            openai_quality: "high".into(),
            openai_input_fidelity: false,
            gemini_api_key: String::new(),
            gemini_model: "gemini-3-pro-image".into(),
            gemini_image_size: "2K".into(),
            bfl_api_key: String::new(),
            bfl_model: "flux-kontext-pro".into(),
            xai_api_key: String::new(),
            xai_model: "grok-imagine-image-2.0".into(),
            realesrgan_path: String::new(),
            realesrgan_model: "realesrgan-x4plus".into(),
            ffmpeg_dir: String::new(),
            work_dir: String::new(),
            output_size: OutputSize::Scale2,
            pad_to_model_aspect: true,
            keep_anamorphic: true,
            remove_black_bars: true,
            change_aspect: false,
            target_aspect: AspectChoice::Wide16x9,
            use_previous_frame: true,
            scene_cut_threshold: 28.0,
            skip_duplicates: true,
            ai_rate: AiRate::All,
            parallel_requests: 2,
            reference_max_side: 1536,
            video_codec: VideoCodec::H264,
            crf: 16,
            compare_mode: CompareMode::SideBySide,
            ui_scale: 1.0,
            seconds_per_frame: Vec::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        Self::load_from(&paths::settings_file())
    }

    pub fn load_from(path: &Path) -> Settings {
        let mut s = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("settings.json could not be read ({e}), using defaults");
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
        s.migrate();
        s
    }

    /// Settings saved by an earlier version: the first default prompt becomes the new one
    /// (unless it was edited), and the built-in presets are added.
    fn migrate(&mut self) {
        if self.prompt == SIMPLE_PROMPT {
            self.prompt = DEFAULT_PROMPT.into();
        }
        self.presets
            .retain(|p| !(p.name == "Standard" && p.text == SIMPLE_PROMPT));
        for (i, preset) in default_presets().into_iter().enumerate() {
            if !self.presets.iter().any(|p| p.name == preset.name) {
                self.presets.insert(i.min(self.presets.len()), preset);
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&paths::settings_file())
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default())?;
        std::fs::rename(&tmp, path)
    }

    pub fn work_dir(&self) -> PathBuf {
        if self.work_dir.trim().is_empty() {
            paths::default_work_dir()
        } else {
            PathBuf::from(self.work_dir.trim())
        }
    }

    /// The measured seconds per frame for the current provider, else a rough default.
    pub fn estimate_seconds_per_frame(&self) -> f32 {
        let id = self.provider.id();
        self.seconds_per_frame
            .iter()
            .find(|(p, _)| p == id)
            .map(|(_, s)| *s)
            .unwrap_or(match self.provider {
                ProviderKind::ChatGptSubscription => 90.0,
                ProviderKind::OpenAiApi => 45.0,
                ProviderKind::Gemini => 25.0,
                ProviderKind::Flux => 15.0,
                ProviderKind::Grok => 10.0,
                ProviderKind::RealEsrgan => 2.0,
                ProviderKind::Basic => 0.2,
            })
    }

    /// Folds one measurement into the running average for `provider`.
    pub fn record_seconds_per_frame(&mut self, provider: ProviderKind, secs: f32) {
        let id = provider.id();
        match self.seconds_per_frame.iter_mut().find(|(p, _)| p == id) {
            Some((_, avg)) => *avg = *avg * 0.7 + secs * 0.3,
            None => self.seconds_per_frame.push((id.to_string(), secs)),
        }
    }

    /// The aspect ratio the AI extends the picture to, when that option is on and the
    /// provider can do it.
    pub fn aspect_change(&self) -> Option<AspectChoice> {
        (self.change_aspect && self.provider.is_generative()).then_some(self.target_aspect)
    }

    /// Whether frames must run one after another (the previous result feeds the next request).
    pub fn sequential(&self) -> bool {
        self.use_previous_frame && self.reference_supported()
    }

    /// Whether the current service can take the previous frame as a second image.
    /// FLUX.1 Kontext and Grok edit one image at a time.
    pub fn reference_supported(&self) -> bool {
        match self.provider {
            ProviderKind::ChatGptSubscription | ProviderKind::OpenAiApi | ProviderKind::Gemini => {
                true
            }
            ProviderKind::Flux => !self.bfl_model.contains("kontext"),
            ProviderKind::Grok | ProviderKind::RealEsrgan | ProviderKind::Basic => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_even() {
        assert_eq!(OutputSize::Scale2.target(640, 360), (1280, 720));
        assert_eq!(OutputSize::Height1080.target(720, 480), (1620, 1080));
        assert_eq!(OutputSize::Height2160.target(853, 480), (3838, 2160));
        let (w, h) = OutputSize::Scale3.target(333, 111);
        assert!(w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn old_default_prompt_is_upgraded_but_edits_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let old = serde_json::json!({
            "prompt": SIMPLE_PROMPT,
            "presets": [{"name": "Standard", "text": SIMPLE_PROMPT}, {"name": "Mine", "text": "x"}]
        });
        std::fs::write(&path, old.to_string()).unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.prompt, DEFAULT_PROMPT);
        let names: Vec<&str> = s.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec![FAITHFUL_PRESET, SIMPLE_PRESET, "Mine"]);
        std::fs::write(&path, r#"{"prompt":"my own"}"#).unwrap();
        assert_eq!(Settings::load_from(&path).prompt, "my own");
    }

    #[test]
    fn settings_round_trip_and_tolerate_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let mut s = Settings::default();
        s.prompt = "hej".into();
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
        std::fs::write(&path, r#"{"prompt":"x"}"#).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.prompt, "x");
        assert_eq!(loaded.provider, ProviderKind::ChatGptSubscription);
    }
}
