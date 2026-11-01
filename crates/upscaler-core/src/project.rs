//! Projects: one folder per video holding `project.json` (the video, marks, position and
//! the settings that shape the result) and everything made for it: previews, the input and
//! output frames of exports and temp files. The default folder is
//! `<video folder>/AI Video Upscaler/<video name>/`, so the frames end up next to the video
//! instead of on the system drive.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::settings::{
    AiRate, AspectChoice, AudioMode, HdrMode, LfeLevel, OutputSize, ProviderKind, Settings,
    SurroundEngine, VideoCodec,
};

pub const PROJECT_FILE: &str = "project.json";
pub const APP_FOLDER: &str = "AI Video Upscaler";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    pub version: u32,
    pub video: PathBuf,
    pub current: u64,
    pub in_point: Option<u64>,
    pub out_point: Option<u64>,
    /// Frame shape chosen by hand instead of the file's flag.
    pub frame_aspect: Option<(u32, u32)>,
    pub last_output: Option<PathBuf>,
    pub settings: ProjectSettings,
}

impl Default for Project {
    fn default() -> Self {
        Project {
            version: 1,
            video: PathBuf::new(),
            current: 0,
            in_point: None,
            out_point: None,
            frame_aspect: None,
            last_output: None,
            settings: ProjectSettings::from(&Settings::default()),
        }
    }
}

/// The settings a project keeps: everything that shapes the result. API keys, program paths
/// and the look of the app stay in the app's own settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectSettings {
    pub provider: ProviderKind,
    pub prompt: String,
    pub codex_model: String,
    pub codex_reasoning: String,
    pub openai_model: String,
    pub openai_size: String,
    pub openai_quality: String,
    pub openai_input_fidelity: bool,
    pub gemini_model: String,
    pub gemini_image_size: String,
    pub bfl_model: String,
    pub xai_model: String,
    pub realesrgan_model: String,
    pub output_size: OutputSize,
    pub pad_to_model_aspect: bool,
    pub keep_anamorphic: bool,
    pub remove_black_bars: bool,
    pub change_aspect: bool,
    pub target_aspect: AspectChoice,
    pub use_previous_frame: bool,
    pub scene_cut_threshold: f32,
    pub skip_duplicates: bool,
    pub ai_rate: AiRate,
    pub parallel_requests: u32,
    pub reference_max_side: u32,
    pub review_frames: bool,
    pub review_max_attempts: u32,
    pub video_codec: VideoCodec,
    pub crf: u8,
    pub hdr_mode: HdrMode,
    pub hdr_peak_nits: u32,
    pub hdr_delete_frames_after: bool,
    pub audio_mode: AudioMode,
    pub surround_engine: SurroundEngine,
    pub lfe_level: LfeLevel,
    pub audio_enhance: bool,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        ProjectSettings::from(&Settings::default())
    }
}

macro_rules! project_fields {
    ($m:ident) => {
        $m!(
            provider,
            prompt,
            codex_model,
            codex_reasoning,
            openai_model,
            openai_size,
            openai_quality,
            openai_input_fidelity,
            gemini_model,
            gemini_image_size,
            bfl_model,
            xai_model,
            realesrgan_model,
            output_size,
            pad_to_model_aspect,
            keep_anamorphic,
            remove_black_bars,
            change_aspect,
            target_aspect,
            use_previous_frame,
            scene_cut_threshold,
            skip_duplicates,
            ai_rate,
            parallel_requests,
            reference_max_side,
            review_frames,
            review_max_attempts,
            video_codec,
            crf,
            hdr_mode,
            hdr_peak_nits,
            hdr_delete_frames_after,
            audio_mode,
            surround_engine,
            lfe_level,
            audio_enhance
        )
    };
}

impl From<&Settings> for ProjectSettings {
    fn from(s: &Settings) -> Self {
        macro_rules! take {
            ($($f:ident),*) => { ProjectSettings { $($f: s.$f.clone()),* } };
        }
        project_fields!(take)
    }
}

impl ProjectSettings {
    /// Copies the project's settings into the app's settings.
    pub fn apply(&self, s: &mut Settings) {
        macro_rules! put {
            ($($f:ident),*) => {{ $(s.$f = self.$f.clone();)* }};
        }
        project_fields!(put);
    }
}

impl Project {
    pub fn new(video: &Path, settings: &Settings) -> Project {
        Project {
            video: video.to_path_buf(),
            settings: ProjectSettings::from(settings),
            ..Project::default()
        }
    }

    pub fn load(dir: &Path) -> Result<Project> {
        let file = dir.join(PROJECT_FILE);
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("could not read {}", file.display()))?;
        let mut p: Project = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a project file", file.display()))?;
        // The video and its project folder were moved together: find the video again.
        if !p.video.is_file() {
            if let (Some(name), Some(base)) =
                (p.video.file_name(), dir.parent().and_then(Path::parent))
            {
                let moved = base.join(name);
                if moved.is_file() {
                    p.video = moved;
                }
            }
        }
        Ok(p)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let file = dir.join(PROJECT_FILE);
        let tmp = dir.join("project.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &file)?;
        Ok(())
    }

    /// A short name for menus: the video's file name.
    pub fn name(&self) -> String {
        self.video
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn stem(video: &Path) -> String {
    video
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".into())
}

/// `<video folder>/AI Video Upscaler/<video name>`.
pub fn default_dir(video: &Path) -> PathBuf {
    video
        .parent()
        .unwrap_or(Path::new("."))
        .join(APP_FOLDER)
        .join(stem(video))
}

/// Used when the video's folder cannot be written to (a DVD, a read-only share).
pub fn fallback_dir(settings: &Settings, video: &Path) -> PathBuf {
    let key = crate::cache::key(&[&video.to_string_lossy()]);
    settings.global_work_dir().join("projects").join(format!(
        "{}-{}",
        stem(video),
        &key[..8.min(key.len())]
    ))
}

/// Whether files can be created in `dir` (creating it if needed).
pub fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-test");
    let ok = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The project folder already made for `video`: the default folder next to it, then the
/// recent projects.
pub fn find_for_video(video: &Path, recent: &[PathBuf], settings: &Settings) -> Option<PathBuf> {
    let same = |dir: &Path| {
        Project::load(dir)
            .map(|p| same_file(&p.video, video))
            .unwrap_or(false)
    };
    [default_dir(video), fallback_dir(settings, video)]
        .into_iter()
        .chain(recent.iter().cloned())
        .find(|d| d.join(PROJECT_FILE).is_file() && same(d))
}

/// The folder for a new project: next to the video if possible.
pub fn new_dir(video: &Path, settings: &Settings) -> PathBuf {
    let d = default_dir(video);
    if writable(&d) {
        d
    } else {
        fallback_dir(settings, video)
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Moves a project folder (previews, frames, project file) to `to`, which must be empty or
/// missing.
pub fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if to.exists() && std::fs::read_dir(to)?.next().is_some() {
        anyhow::bail!("{} is not empty", to.display());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_dir(to);
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // Another drive: copy, then remove the old folder.
    copy_tree(from, to)?;
    std::fs::remove_dir_all(from)
        .with_context(|| format!("copied, but {} could not be removed", from.display()))?;
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_folder_sits_next_to_the_video() {
        let d = default_dir(Path::new("/media/Voyager/S01E01.avi"));
        assert_eq!(
            d,
            Path::new("/media/Voyager").join(APP_FOLDER).join("S01E01")
        );
    }

    #[test]
    fn project_keeps_result_settings_but_not_keys() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("clip.mp4");
        std::fs::write(&video, b"x").unwrap();
        let mut s = Settings::default();
        s.prompt = "my prompt".into();
        s.change_aspect = true;
        s.target_aspect = AspectChoice::Scope235;
        s.openai_api_key = "secret".into();
        let mut p = Project::new(&video, &s);
        p.in_point = Some(10);
        let pdir = default_dir(&video);
        p.save(&pdir).unwrap();
        let text = std::fs::read_to_string(pdir.join(PROJECT_FILE)).unwrap();
        assert!(!text.contains("secret"));

        let loaded = Project::load(&pdir).unwrap();
        assert_eq!(loaded, p);
        let mut other = Settings::default();
        other.openai_api_key = "mine".into();
        loaded.settings.apply(&mut other);
        assert_eq!(other.prompt, "my prompt");
        assert_eq!(other.target_aspect, AspectChoice::Scope235);
        assert_eq!(other.openai_api_key, "mine");

        assert_eq!(find_for_video(&video, &[], &other), Some(pdir));
    }

    #[test]
    fn project_keeps_hdr() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("clip.mp4");
        std::fs::write(&video, b"x").unwrap();
        let mut s = Settings::default();
        s.hdr_mode = crate::settings::HdrMode::Hdr10Plus;
        s.hdr_peak_nits = 600;
        s.hdr_delete_frames_after = false;
        let p = Project::new(&video, &s);
        let pdir = default_dir(&video);
        p.save(&pdir).unwrap();
        let loaded = Project::load(&pdir).unwrap();
        let mut other = Settings::default();
        loaded.settings.apply(&mut other);
        assert_eq!(other.hdr_mode, crate::settings::HdrMode::Hdr10Plus);
        assert_eq!(other.hdr_peak_nits, 600);
        assert!(!other.hdr_delete_frames_after);
    }

    #[test]
    fn project_keeps_audio_choices() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("clip.mp4");
        std::fs::write(&video, b"x").unwrap();
        let mut s = Settings::default();
        s.audio_mode = AudioMode::Surround;
        s.surround_engine = SurroundEngine::Classic;
        s.lfe_level = LfeLevel::Cinema;
        s.audio_enhance = true;
        let p = Project::new(&video, &s);
        let pdir = default_dir(&video);
        p.save(&pdir).unwrap();
        let loaded = Project::load(&pdir).unwrap();
        let mut other = Settings::default();
        loaded.settings.apply(&mut other);
        assert_eq!(other.audio_mode, AudioMode::Surround);
        assert_eq!(other.surround_engine, SurroundEngine::Classic);
        assert_eq!(other.lfe_level, LfeLevel::Cinema);
        assert!(other.audio_enhance);
    }

    #[test]
    fn moved_folder_finds_its_video_and_move_dir_moves_files() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("a").join("clip.mp4");
        std::fs::create_dir_all(video.parent().unwrap()).unwrap();
        std::fs::write(&video, b"x").unwrap();
        let p = Project::new(&video, &Settings::default());
        let pdir = default_dir(&video);
        p.save(&pdir).unwrap();
        std::fs::create_dir_all(pdir.join("previews")).unwrap();
        std::fs::write(pdir.join("previews").join("f.png"), b"png").unwrap();

        // The whole folder "a" is moved to "b".
        let b = dir.path().join("b");
        std::fs::rename(dir.path().join("a"), &b).unwrap();
        let loaded = Project::load(&default_dir(&b.join("clip.mp4"))).unwrap();
        assert_eq!(loaded.video, b.join("clip.mp4"));

        let to = dir.path().join("elsewhere").join("proj");
        move_dir(&default_dir(&b.join("clip.mp4")), &to).unwrap();
        assert!(to.join("previews").join("f.png").is_file());
        assert!(to.join(PROJECT_FILE).is_file());
    }
}
