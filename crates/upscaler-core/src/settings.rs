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
    /// Real-ESRGAN (realesrgan-ncnn-vulkan), free and local, needs a GPU with Vulkan.
    RealEsrgan,
    /// Plain Lanczos resize with a little sharpening: instant, for testing the workflow.
    Basic,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 4] = [
        ProviderKind::ChatGptSubscription,
        ProviderKind::OpenAiApi,
        ProviderKind::RealEsrgan,
        ProviderKind::Basic,
    ];

    /// Generative providers take a prompt and benefit from padding and a reference frame.
    pub fn is_generative(self) -> bool {
        matches!(
            self,
            ProviderKind::ChatGptSubscription | ProviderKind::OpenAiApi
        )
    }

    pub fn id(self) -> &'static str {
        match self {
            ProviderKind::ChatGptSubscription => "chatgpt-codex",
            ProviderKind::OpenAiApi => "openai-api",
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

pub const DEFAULT_PROMPT: &str = "Restore and upscale this video frame to a clean, high-resolution image. \
Keep it photorealistic and natural: recover fine detail and texture, sharpen soft edges, and remove \
compression artifacts, blocking, banding, noise and blur. Do not add, remove or move anything. \
Preserve the composition, faces, identities, expressions, text, colors, lighting and the overall \
look of the original exactly. The result should look like the same shot filmed with a better camera.";

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
            presets: vec![PromptPreset {
                name: "Standard".into(),
                text: DEFAULT_PROMPT.into(),
            }],
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
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("settings.json could not be read ({e}), using defaults");
                Settings::default()
            }),
            Err(_) => Settings::default(),
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
        self.provider.is_generative() && self.use_previous_frame
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
