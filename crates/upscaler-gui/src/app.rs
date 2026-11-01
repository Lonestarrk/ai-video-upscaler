//! Application state, event handling and the window layout.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use image::RgbImage;
use upscaler_core::audio::ai::Device;
use upscaler_core::audio::job::{AudioEvent, AudioStage};
use upscaler_core::audio::pack::PackStatus;
use upscaler_core::audio::source::SourceAudio;
use upscaler_core::ffmpeg::{Tools, VideoInfo};
use upscaler_core::job::ReviewOutcome;
use upscaler_core::job::{ExportEvent, ExportSpec, Stage};
use upscaler_core::project::{self, Project, ProjectSettings};
use upscaler_core::provider::codex::LoginState;
use upscaler_core::settings::Settings;

use crate::i18n::tr;
use crate::ui;
use crate::worker::{self, Bus, Event, FrameFetcher, FrameRequest, PackOp};

pub const VIDEO_EXTENSIONS: [&str; 20] = [
    "mp4", "mov", "mkv", "avi", "webm", "m4v", "wmv", "flv", "mpg", "mpeg", "ts", "3gp", "vob",
    "m2ts", "mts", "m2v", "dv", "divx", "ogv", "mxf",
];
pub const THUMB_COUNT: usize = 40;

/// The loaded video and everything shown about it.
pub struct Video {
    pub info: VideoInfo,
    /// Frame under the playhead.
    pub current: u64,
    /// Decoded original of `shown_index`.
    pub frame: Option<(u64, Arc<RgbImage>, egui::TextureHandle)>,
    /// Upscaled result shown for `result.0`.
    pub result: Option<(u64, egui::TextureHandle, (u32, u32))>,
    pub thumbs: Vec<Option<egui::TextureHandle>>,
    /// Frames that have a preview with the current settings (as far as this session knows).
    pub previewed: BTreeSet<u64>,
    pub in_point: Option<u64>,
    pub out_point: Option<u64>,
    /// Frame requested from the fetcher and not yet shown.
    pub requested: Option<u64>,
    pub frame_error: Option<String>,
    /// Frame shape chosen by the user instead of the file's flag (`None`: from the file).
    pub frame_aspect: Option<(u32, u32)>,
    /// Whether the picture's shape looks like a standard one, measured when the video opened.
    pub aspect_check: upscaler_core::aspect::Check,
    /// The user answered the "flagged wrongly?" warning (chose or kept a shape).
    pub aspect_hint_done: bool,
    /// The source's audio: `None` while it is being read, `Ok(None)` when there is none.
    pub audio: Option<Result<Option<SourceAudio>, String>>,
}

impl Video {
    /// The export range: in/out points, else the whole video. `end` is exclusive.
    pub fn range(&self) -> (u64, u64) {
        let start = self.in_point.unwrap_or(0);
        let end = self
            .out_point
            .map(|o| o + 1)
            .unwrap_or(self.info.frame_count);
        if end > start {
            (start, end)
        } else {
            (0, self.info.frame_count)
        }
    }
}

pub struct PreviewJob {
    pub index: u64,
    pub started: Instant,
    pub cancel: Arc<AtomicBool>,
}

pub struct ExportState {
    pub spec: ExportSpec,
    pub cancel: Arc<AtomicBool>,
    pub started: Instant,
    pub stage: Stage,
    pub done: u64,
    pub total: u64,
    /// Frames finished by the AI in this run, and the seconds they took (for the ETA).
    pub ai_frames: u64,
    pub ai_seconds: f64,
    pub already_done: u64,
    pub last_frame: Option<egui::TextureHandle>,
    pub retries: Vec<String>,
    pub outcome: Option<Result<PathBuf, String>>,
    pub cancelled: bool,
    /// Quality control in this run: approved, rejected (made again), kept despite problems,
    /// and unchecked (the review itself failed).
    pub approved: u64,
    pub rejected: u64,
    pub kept: u64,
    pub unchecked: u64,
    /// Latest rejections, newest last.
    pub review_notes: Vec<String>,
    /// The export changes the audio (video encoded without audio, audio job, MKV mux).
    pub audio_path: bool,
    /// The audio job's current step.
    pub audio_stage: Option<AudioStage>,
    /// A section count has arrived (before that the audio progress is a placeholder 0/1).
    pub audio_sections_known: bool,
    /// The audio job's warnings (English from core; translated when shown).
    pub audio_warnings: Vec<String>,
}

/// The open project: its folder, its saved state and what is on screen now.
pub struct OpenProject {
    pub dir: PathBuf,
    pub data: Project,
    saved: Project,
    /// Marks and position still to be put back once the video has loaded.
    restore: bool,
    last_save: Instant,
}

#[derive(Default)]
pub struct SettingsUi {
    pub open: bool,
    pub tab: usize,
    pub codex: Option<LoginState>,
    pub codex_busy: Option<(String, Arc<AtomicBool>)>,
    pub codex_message: Option<Result<String, String>>,
    pub ffmpeg_download: Option<(u64, Option<u64>, Arc<AtomicBool>)>,
    pub ffmpeg_message: Option<Result<String, String>>,
    pub work_dir_size: Option<u64>,
    pub show_key: bool,
    /// The tab shown in the previous frame (`None` while the window is closed), to notice
    /// when a tab is opened.
    pub last_tab: Option<usize>,
}

/// The AI audio pack as the UI knows it. Its status walks a 5.7 GB folder, so it is read in
/// a worker at start, after install/remove and when the Ljud-AI tab opens, never per frame.
#[derive(Default)]
pub struct AudioPackUi {
    /// `None` until the first status arrives.
    pub status: Option<PackStatus>,
    /// A status request is running.
    pub checking: bool,
    /// An install is running: stage name, overall fraction, cancel flag.
    pub installing: Option<(String, f32, Arc<AtomicBool>)>,
    /// A remove is running, since when.
    pub removing: Option<Instant>,
    pub message: Option<Result<String, String>>,
    /// The device probe is running.
    pub probing: bool,
    pub probe: Option<Result<Device, String>>,
    /// "Remove…" was clicked and waits for "Yes, remove".
    pub confirm_remove: bool,
}

impl AudioPackUi {
    pub fn busy(&self) -> bool {
        self.installing.is_some() || self.removing.is_some()
    }

    /// Asks a worker for the pack's status unless a request is already running.
    pub fn refresh(&mut self, bus: &Bus) {
        if !self.checking && !self.busy() {
            self.checking = true;
            worker::pack_status(bus.clone());
        }
    }
}

/// Zoom and pan shared by both halves of the comparison.
#[derive(Clone, Copy, Debug)]
pub struct ViewTransform {
    /// 1.0 = fit to the panel.
    pub zoom: f32,
    /// Image point (0..1) shown at the panel centre.
    pub center: egui::Vec2,
    /// Swipe line position across the view (0..1).
    pub swipe: f32,
}

impl Default for ViewTransform {
    fn default() -> Self {
        ViewTransform {
            zoom: 1.0,
            center: egui::vec2(0.5, 0.5),
            swipe: 0.5,
        }
    }
}

pub struct UpscalerApp {
    pub settings: Settings,
    saved_settings: Settings,
    last_save: Instant,
    pub tools: Option<Tools>,
    rx: Receiver<Event>,
    pub bus: Bus,
    fetcher: FrameFetcher,
    pub gen: u64,
    pub loading: Option<PathBuf>,
    pub video: Option<Video>,
    pub view: ViewTransform,
    pub preview: Option<PreviewJob>,
    pub preview_error: Option<String>,
    pub export: Option<ExportState>,
    pub project: Option<OpenProject>,
    /// The project folder is being moved.
    pub project_moving: bool,
    pub frame_browser: ui::frames::FrameBrowser,
    pub settings_ui: SettingsUi,
    pub audio_pack: AudioPackUi,
    pub error: Option<String>,
    /// An export target the save dialog did not ask about (the forced `.mkv` of a typed
    /// `.mp4`) that exists: waits for the user's yes before it is overwritten.
    pub confirm_overwrite: Option<PathBuf>,
    /// Settings that change the result: when they change, the shown result is looked up again.
    result_settings_key: String,
    pub new_preset_name: String,
}

impl UpscalerApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        settings: Settings,
        initial: Option<PathBuf>,
    ) -> Self {
        let (tx, rx) = channel();
        let bus = Bus::new(tx, cc.egui_ctx.clone());
        cc.egui_ctx
            .set_zoom_factor(settings.ui_scale.clamp(0.75, 2.5));
        ui::style::apply(&cc.egui_ctx);
        let tools = Tools::locate(&settings.ffmpeg_dir);
        if tools.is_none() {
            log::warn!("ffmpeg was not found");
        }
        worker::codex_status(bus.clone(), settings.codex_path.clone());
        let mut audio_pack = AudioPackUi::default();
        audio_pack.refresh(&bus);
        let fetcher = FrameFetcher::start(bus.clone());
        let key = result_key(&settings);
        let mut app = UpscalerApp {
            saved_settings: settings.clone(),
            settings,
            last_save: Instant::now(),
            tools,
            rx,
            bus,
            fetcher,
            gen: 0,
            loading: None,
            video: None,
            view: ViewTransform::default(),
            preview: None,
            preview_error: None,
            export: None,
            project: None,
            project_moving: false,
            frame_browser: Default::default(),
            settings_ui: SettingsUi::default(),
            audio_pack,
            error: None,
            confirm_overwrite: None,
            result_settings_key: key,
            new_preset_name: String::new(),
        };
        if let Some(path) = initial {
            app.open_video(path);
        }
        app
    }

    pub fn exporting(&self) -> bool {
        self.export
            .as_ref()
            .is_some_and(|e| e.outcome.is_none() && !e.cancelled)
    }

    // ------------------------------------------------------------ actions

    pub fn open_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new().add_filter(tr("Video", "Video"), &VIDEO_EXTENSIONS);
        if let Some(v) = &self.video {
            if let Some(dir) = v.info.path.parent() {
                dialog = dialog.set_directory(dir);
            }
        }
        if let Some(path) = dialog.pick_file() {
            self.open_video(path);
        }
    }

    /// Opens a video in its project: the existing one (found next to the video or among the
    /// recent projects), else a new one in `<video folder>/AI Video Upscaler/<name>`.
    pub fn open_video(&mut self, path: PathBuf) {
        self.open_video_in(path, None);
    }

    /// Lets the user pick a project folder and opens it.
    pub fn open_project_dialog(&mut self) {
        let mut dialog =
            rfd::FileDialog::new().set_title(tr("Välj projektmapp", "Choose a project folder"));
        if let Some(p) = &self.project {
            if let Some(parent) = p.dir.parent() {
                dialog = dialog.set_directory(parent);
            }
        }
        if let Some(dir) = dialog.pick_folder() {
            self.open_project(dir);
        }
    }

    pub fn open_project(&mut self, dir: PathBuf) {
        match Project::load(&dir) {
            Ok(p) if p.video.is_file() => self.open_video_in(p.video.clone(), Some(dir)),
            Ok(p) => {
                self.error = Some(format!(
                    "{}\n{}",
                    tr(
                        "Projektets video hittades inte:",
                        "The project's video was not found:"
                    ),
                    p.video.display()
                ))
            }
            Err(e) => {
                self.error = Some(format!(
                    "{}\n\n{e:#}",
                    tr(
                        "Mappen är inte en projektmapp (project.json saknas).",
                        "The folder is not a project folder (project.json is missing)."
                    )
                ))
            }
        }
    }

    /// Moves the open project's folder (previews, frames, project file) somewhere else.
    pub fn move_project_dialog(&mut self) {
        let Some(p) = &self.project else { return };
        if self.exporting() || self.preview.is_some() || self.project_moving {
            self.error = Some(
                tr(
                    "Vänta tills exporten och förhandsvisningen är klara.",
                    "Wait for the export and the preview to finish.",
                )
                .into(),
            );
            return;
        }
        let Some(chosen) = rfd::FileDialog::new()
            .set_title(tr(
                "Var ska projektets bildrutor ligga?",
                "Where should the project's frames go?",
            ))
            .pick_folder()
        else {
            return;
        };
        let name = p
            .dir
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        let to = if chosen.file_name() == Some(name.as_os_str()) {
            chosen
        } else {
            chosen.join(&name)
        };
        if to == p.dir {
            return;
        }
        self.project_moving = true;
        worker::move_project(self.bus.clone(), p.dir.clone(), to);
    }

    fn open_video_in(&mut self, path: PathBuf, dir: Option<PathBuf>) {
        if self.exporting() {
            self.error = Some(
                tr(
                    "Vänta tills exporten är klar eller avbryt den först.",
                    "Wait for the export to finish or cancel it first.",
                )
                .into(),
            );
            return;
        }
        let Some(tools) = self.tools.clone() else {
            self.error = Some(tr(
                "FFmpeg behövs för att läsa video. Öppna Inställningar ➡ FFmpeg och klicka \"Hämta FFmpeg\".",
                "FFmpeg is needed to read video. Open Settings ➡ FFmpeg and click \"Download FFmpeg\".",
            ).into());
            self.settings_ui.open = true;
            self.settings_ui.tab = crate::ui::settings_window::TAB_FFMPEG;
            return;
        };
        self.save_project();
        let dir = dir
            .or_else(|| {
                project::find_for_video(&path, &self.settings.recent_projects, &self.settings)
            })
            .unwrap_or_else(|| project::new_dir(&path, &self.settings));
        let data = match Project::load(&dir) {
            Ok(p) => {
                log::info!("project {} opened", dir.display());
                p.settings.apply(&mut self.settings);
                Project {
                    video: path.clone(),
                    ..p
                }
            }
            Err(_) => {
                log::info!("new project in {}", dir.display());
                Project::new(&path, &self.settings)
            }
        };
        if let Err(e) = data.save(&dir) {
            log::warn!("could not save the project in {}: {e:#}", dir.display());
        }
        self.settings.project_dir = Some(dir.clone());
        self.settings.remember_project(&dir);
        self.frame_browser.reset();
        self.project = Some(OpenProject {
            dir,
            saved: data.clone(),
            data,
            restore: true,
            last_save: Instant::now(),
        });
        self.gen += 1;
        self.loading = Some(path.clone());
        self.cancel_preview();
        worker::probe(self.bus.clone(), tools, self.gen, path);
    }

    pub fn seek(&mut self, index: u64) {
        let Some(v) = &mut self.video else { return };
        let index = index.min(v.info.frame_count.saturating_sub(1));
        let shown = v.frame.as_ref().map(|(i, _, _)| *i);
        if v.current == index
            && (v.requested == Some(index) || (v.requested.is_none() && shown == Some(index)))
        {
            return;
        }
        v.current = index;
        self.request_frame();
    }

    fn request_frame(&mut self) {
        let (Some(v), Some(tools)) = (&mut self.video, &self.tools) else {
            return;
        };
        v.requested = Some(v.current);
        self.fetcher.request(FrameRequest {
            gen: self.gen,
            index: v.current,
            tools: tools.clone(),
            info: v.info.clone(),
            settings: self.settings.clone(),
        });
    }

    pub fn start_preview(&mut self) {
        let Some(v) = &self.video else { return };
        let Some((idx, src, _)) = &v.frame else {
            return;
        };
        if *idx != v.current {
            return;
        }
        let (idx, src, info) = (*idx, src.clone(), v.info.clone());
        self.cancel_preview();
        let cancel = Arc::new(AtomicBool::new(false));
        self.preview_error = None;
        self.preview = Some(PreviewJob {
            index: idx,
            started: Instant::now(),
            cancel: cancel.clone(),
        });
        worker::preview(
            self.bus.clone(),
            self.gen,
            idx,
            src,
            self.settings.clone(),
            info,
            cancel,
        );
    }

    pub fn cancel_preview(&mut self) {
        if let Some(p) = self.preview.take() {
            p.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn start_export(&mut self) {
        let (Some(v), true) = (&self.video, self.tools.is_some()) else {
            return;
        };
        let stem = v
            .info
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "video".into());
        let (_, h) = upscaler_core::job::geometry(&self.settings, &v.info).stored;
        let last = self
            .project
            .as_ref()
            .and_then(|p| p.data.last_output.clone());
        // New audio is muxed into an MKV (core writes `<name>.mkv`): offer and force that
        // name here, so the dialog's overwrite prompt is about the file really written.
        let audio_path = ui::audio::export_makes_audio(self);
        let (filter, ext) = if audio_path {
            ("MKV", "mkv")
        } else {
            ("MP4", "mp4")
        };
        let mut dialog = rfd::FileDialog::new().add_filter(filter, &[ext]);
        match last
            .as_ref()
            .and_then(|l| Some((l.parent()?, l.file_stem()?)))
        {
            Some((dir, name)) => {
                dialog = dialog
                    .set_directory(dir)
                    .set_file_name(format!("{}.{ext}", name.to_string_lossy()));
            }
            None => {
                dialog = dialog.set_file_name(format!("{stem}_upscaled_{h}p.{ext}"));
                if let Some(dir) = v.info.path.parent() {
                    dialog = dialog.set_directory(dir);
                }
            }
        }
        let Some(chosen) = dialog.save_file() else {
            return;
        };
        let mut output = chosen.clone();
        if audio_path || output.extension().is_none() {
            output.set_extension(ext);
        }
        // The dialog only asked about the name typed (say `x.mp4`); the file really written
        // (`x.mkv`) is asked about here before it is replaced.
        if output != chosen && output.exists() && output != v.info.path {
            self.confirm_overwrite = Some(output);
            return;
        }
        self.launch_export(output);
    }

    /// Starts the export to `output` (already confirmed by the user).
    pub fn launch_export(&mut self, output: PathBuf) {
        let audio_path = ui::audio::export_makes_audio(self);
        let Some(v) = &self.video else { return };
        if output == v.info.path {
            self.error = Some(
                tr(
                    "Välj ett annat filnamn än originalet.",
                    "Pick a different file name than the original.",
                )
                .into(),
            );
            return;
        }
        if let Some(p) = &mut self.project {
            p.data.last_output = Some(output.clone());
        }
        let Some(v) = &self.video else { return };
        let Some(tools) = &self.tools else { return };
        let (start, end) = v.range();
        let spec = ExportSpec {
            info: v.info.clone(),
            start,
            end,
            output,
            settings: self.settings.clone(),
        };
        let already_done = upscaler_core::job::frames_already_done(&spec);
        let cancel = Arc::new(AtomicBool::new(false));
        log::info!(
            "export {} frames {start}..{end} to {}",
            v.info.path.display(),
            spec.output.display()
        );
        worker::export(
            self.bus.clone(),
            tools.clone(),
            spec.clone(),
            cancel.clone(),
        );
        self.export = Some(ExportState {
            spec,
            cancel,
            started: Instant::now(),
            stage: Stage::Extracting,
            done: 0,
            total: 0,
            ai_frames: 0,
            ai_seconds: 0.0,
            already_done,
            last_frame: None,
            retries: Vec::new(),
            outcome: None,
            cancelled: false,
            approved: 0,
            rejected: 0,
            kept: 0,
            unchecked: 0,
            review_notes: Vec::new(),
            audio_path,
            audio_stage: None,
            audio_sections_known: false,
            audio_warnings: Vec::new(),
        });
    }

    pub fn reload_tools(&mut self) {
        self.tools = Tools::locate(&self.settings.ffmpeg_dir);
    }

    // ------------------------------------------------------------ events

    fn handle_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.rx.try_recv() {
            self.handle(ctx, event);
        }
    }

    fn handle(&mut self, ctx: &egui::Context, event: Event) {
        if self.frame_browser.handle(ctx, &event) {
            return;
        }
        match event {
            Event::Probed { gen, result } if gen == self.gen => {
                self.loading = None;
                match result {
                    Ok(info) => {
                        let crop = if self.settings.remove_black_bars {
                            info.detected_crop
                        } else {
                            None
                        };
                        let info = info.with_crop(crop);
                        let aspect_check = upscaler_core::aspect::check(&info);
                        log::info!("aspect check: {aspect_check:?}");
                        log::info!(
                            "opened {} ({}x{}, {:.3} fps, {} frames)",
                            info.path.display(),
                            info.width,
                            info.height,
                            info.fps(),
                            info.frame_count
                        );
                        if let Some(tools) = &self.tools {
                            worker::thumbnails(
                                self.bus.clone(),
                                tools.clone(),
                                gen,
                                info.clone(),
                                THUMB_COUNT,
                            );
                            worker::audio_source(
                                self.bus.clone(),
                                tools.clone(),
                                gen,
                                info.clone(),
                            );
                        }
                        self.video = Some(Video {
                            info,
                            current: 0,
                            frame: None,
                            result: None,
                            thumbs: vec![None; THUMB_COUNT],
                            previewed: BTreeSet::new(),
                            in_point: None,
                            out_point: None,
                            requested: None,
                            frame_error: None,
                            frame_aspect: None,
                            aspect_check,
                            aspect_hint_done: false,
                            audio: None,
                        });
                        if let (Some(p), Some(v)) = (&mut self.project, &mut self.video) {
                            if p.restore {
                                p.restore = false;
                                let last = v.info.frame_count.saturating_sub(1);
                                v.current = p.data.current.min(last);
                                v.in_point = p.data.in_point.filter(|&i| i <= last);
                                v.out_point = p.data.out_point.filter(|&o| o <= last);
                                if p.data.frame_aspect.is_some() {
                                    v.frame_aspect = p.data.frame_aspect;
                                    v.aspect_hint_done = true;
                                }
                            }
                        }
                        self.view = ViewTransform::default();
                        self.preview_error = None;
                        self.request_frame();
                    }
                    Err(e) => {
                        self.error = Some(format!(
                            "{}\n\n{e}",
                            tr(
                                "Videon kunde inte öppnas.",
                                "The video could not be opened."
                            )
                        ))
                    }
                }
            }
            Event::Thumb { gen, slot, image } if gen == self.gen => {
                if let Some(v) = &mut self.video {
                    let tex = ctx.load_texture(
                        format!("thumb{slot}"),
                        image,
                        egui::TextureOptions::LINEAR,
                    );
                    if let Some(s) = v.thumbs.get_mut(slot) {
                        *s = Some(tex);
                    }
                }
            }
            Event::Frame { gen, index, image } if gen == self.gen => {
                if let Some(v) = &mut self.video {
                    let tex = load_big_texture(ctx, "frame", &image);
                    v.frame = Some((index, image, tex));
                    v.frame_error = None;
                    if v.requested == Some(index) {
                        v.requested = None;
                    }
                }
            }
            Event::FrameFailed { gen, index, error } if gen == self.gen => {
                if let Some(v) = &mut self.video {
                    v.frame_error = Some(error);
                    if v.requested == Some(index) {
                        v.requested = None;
                    }
                }
            }
            Event::CachedResult {
                gen,
                index,
                key,
                image,
            } if gen == self.gen => {
                let current_key = self
                    .video
                    .as_ref()
                    .map(|v| upscaler_core::job::preview_path(&self.settings, &v.info, index));
                if let Some(v) = &mut self.video {
                    if v.current != index || current_key.as_ref() != Some(&key) {
                        return;
                    }
                    match image {
                        Some(img) => {
                            v.previewed.insert(index);
                            v.result = Some((
                                index,
                                load_big_texture(ctx, "result", &img),
                                img.dimensions(),
                            ));
                        }
                        None => {
                            v.previewed.remove(&index);
                            v.result = None;
                        }
                    }
                }
            }
            Event::PreviewDone {
                gen,
                index,
                key,
                result,
                seconds,
            } if gen == self.gen => {
                if self.preview.as_ref().is_some_and(|p| p.index == index) {
                    self.preview = None;
                }
                match result {
                    Ok(img) => {
                        if self.settings.provider.is_generative() || seconds > 0.5 {
                            self.settings
                                .record_seconds_per_frame(self.settings.provider, seconds);
                        }
                        let still_current = self.video.as_ref().is_some_and(|v| {
                            upscaler_core::job::preview_path(&self.settings, &v.info, index) == key
                        });
                        if let Some(v) = &mut self.video {
                            if still_current {
                                v.previewed.insert(index);
                                if v.current == index {
                                    v.result = Some((
                                        index,
                                        load_big_texture(ctx, "result", &img),
                                        img.dimensions(),
                                    ));
                                }
                            }
                        }
                    }
                    Err(e) if e == "cancelled" => {}
                    Err(e) => self.preview_error = Some(e),
                }
            }
            Event::Export(e) => self.handle_export(e),
            Event::ExportThumb(img) => {
                if let Some(x) = &mut self.export {
                    x.last_frame =
                        Some(ctx.load_texture("export-last", img, egui::TextureOptions::LINEAR));
                }
            }
            Event::CodexStatus(s) => self.settings_ui.codex = Some(s),
            Event::CodexAction(r) => {
                self.settings_ui.codex_busy = None;
                self.settings_ui.codex_message = Some(r);
            }
            Event::FfmpegDownload { done, total } => {
                if let Some(d) = &mut self.settings_ui.ffmpeg_download {
                    d.0 = done;
                    d.1 = total;
                }
            }
            Event::FfmpegDownloaded(r) => {
                self.settings_ui.ffmpeg_download = None;
                self.reload_tools();
                self.settings_ui.ffmpeg_message = Some(match r {
                    Ok(()) if self.tools.is_some() => {
                        Ok(tr("FFmpeg är installerat.", "FFmpeg is installed.").into())
                    }
                    Ok(()) => Err(tr(
                        "FFmpeg hämtades men hittas inte.",
                        "FFmpeg was downloaded but cannot be found.",
                    )
                    .into()),
                    Err(e) => Err(e),
                });
            }
            Event::WorkDirSize(b) => self.settings_ui.work_dir_size = Some(b),
            Event::AudioSource { gen, result } if gen == self.gen => {
                if let Err(e) = &result {
                    log::warn!("could not read the source audio: {e}");
                }
                if let Some(v) = &mut self.video {
                    v.audio = Some(result);
                }
            }
            Event::PackStatus(s) => {
                log::info!("AI audio pack: {s:?}");
                self.audio_pack.checking = false;
                self.audio_pack.status = Some(s);
            }
            Event::PackProgress { stage, fraction } => {
                if let Some(i) = &mut self.audio_pack.installing {
                    i.0 = stage;
                    i.1 = fraction;
                }
            }
            Event::PackDone(r) => {
                let p = &mut self.audio_pack;
                p.installing = None;
                p.removing = None;
                p.probe = None;
                // The worker sends the new status right after this.
                p.checking = true;
                p.message = Some(match r {
                    Ok(PackOp::Install) => {
                        Ok(tr("Ljud-AI är installerat.", "The audio AI is installed.").into())
                    }
                    Ok(PackOp::Remove) => {
                        Ok(tr("Ljud-AI är borttaget.", "The audio AI is removed.").into())
                    }
                    Err(e) if e == "cancelled" => Err(tr(
                        "Avbrutet. Klicka Installera igen för att fortsätta där det slutade.",
                        "Cancelled. Click Install again to continue where it stopped.",
                    )
                    .into()),
                    Err(e) => Err(e),
                });
            }
            Event::DeviceProbed(r) => {
                self.audio_pack.probing = false;
                self.audio_pack.probe = Some(r);
            }
            Event::ProjectMoved(r) => {
                self.project_moving = false;
                match r {
                    Ok(to) => {
                        log::info!("project moved to {}", to.display());
                        if let Some(p) = &mut self.project {
                            let old = std::mem::replace(&mut p.dir, to.clone());
                            self.settings.recent_projects.retain(|d| d != &old);
                        }
                        self.settings.project_dir = Some(to.clone());
                        self.settings.remember_project(&to);
                        self.settings_ui.work_dir_size = None;
                    }
                    Err(e) => {
                        self.error = Some(format!(
                            "{}\n\n{e}",
                            tr(
                                "Projektmappen kunde inte flyttas.",
                                "The project folder could not be moved."
                            )
                        ))
                    }
                }
            }
            _ => {} // stale events from an earlier video
        }
    }

    fn handle_export(&mut self, e: ExportEvent) {
        let Some(x) = &mut self.export else { return };
        match e {
            ExportEvent::Progress { stage, done, total } => {
                x.stage = stage;
                x.done = done;
                x.total = total;
            }
            ExportEvent::FrameDone {
                seconds: Some(s), ..
            } => {
                x.ai_frames += 1;
                x.ai_seconds += s as f64;
                let provider = x.spec.settings.provider;
                // Parallel requests overlap: the wall-clock time per frame is what estimates need.
                let per = if x.spec.settings.sequential() {
                    s
                } else {
                    s / x.spec.settings.parallel_requests.max(1) as f32
                };
                self.settings.record_seconds_per_frame(provider, per);
            }
            ExportEvent::FrameDone { .. } => {}
            ExportEvent::Retry { n, error } => {
                x.retries
                    .push(format!("{} {n}: {error}", tr("Bildruta", "Frame")));
            }
            ExportEvent::Reviewed {
                n,
                outcome,
                problems,
                attempt,
            } => {
                match outcome {
                    ReviewOutcome::Approved => x.approved += 1,
                    ReviewOutcome::Rejected => x.rejected += 1,
                    ReviewOutcome::Kept => x.kept += 1,
                    ReviewOutcome::Unchecked => x.unchecked += 1,
                }
                if outcome != ReviewOutcome::Approved {
                    let what = match outcome {
                        ReviewOutcome::Rejected => tr("underkänd, görs om", "rejected, made again"),
                        ReviewOutcome::Kept => {
                            tr("underkänd igen, behålls", "rejected again, kept")
                        }
                        _ => tr("kunde inte granskas", "could not be reviewed"),
                    };
                    x.review_notes.push(format!(
                        "{} {n} ({} {attempt}), {what}: {problems}",
                        tr("Bildruta", "Frame"),
                        tr("försök", "attempt"),
                    ));
                    if x.review_notes.len() > 50 {
                        x.review_notes.remove(0);
                    }
                }
            }
            ExportEvent::Audio(a) => {
                log::info!("export audio: {a:?}");
                match a {
                    AudioEvent::Stage(s) => x.audio_stage = Some(s),
                    AudioEvent::Warning(w) => {
                        if !x.audio_warnings.contains(&w) {
                            x.audio_warnings.push(w);
                        }
                    }
                    // The counts come as Progress; this only says they are real now.
                    AudioEvent::Section { .. } => x.audio_sections_known = true,
                }
            }
            ExportEvent::Finished(p) => {
                log::info!("export finished: {}", p.display());
                x.outcome = Some(Ok(p));
            }
            ExportEvent::Failed(err) => x.outcome = Some(Err(err)),
            ExportEvent::Cancelled => x.cancelled = true,
        }
    }

    /// The open project as it is on screen now.
    fn refresh_project(&mut self) {
        let (Some(p), Some(v)) = (&mut self.project, &self.video) else {
            return;
        };
        if p.restore {
            return; // the saved marks are not on screen yet
        }
        p.data.settings = ProjectSettings::from(&self.settings);
        p.data.current = v.current;
        p.data.in_point = v.in_point;
        p.data.out_point = v.out_point;
        p.data.frame_aspect = v.frame_aspect;
    }

    /// Writes the open project's file now if it changed.
    pub fn save_project(&mut self) {
        self.refresh_project();
        if let Some(p) = &mut self.project {
            if p.data != p.saved {
                match p.data.save(&p.dir) {
                    Ok(()) => p.saved = p.data.clone(),
                    Err(e) => log::warn!("could not save the project: {e:#}"),
                }
            }
            p.last_save = Instant::now();
        }
    }

    /// Saves changed settings (at most every half second) and refreshes the shown result
    /// when a setting that changes it was edited.
    fn persist(&mut self) {
        self.refresh_project();
        if self
            .project
            .as_ref()
            .is_some_and(|p| p.data != p.saved && p.last_save.elapsed().as_secs() >= 2)
        {
            self.save_project();
        }
        if self.settings != self.saved_settings && self.last_save.elapsed().as_millis() > 500 {
            if let Err(e) = self.settings.save() {
                log::warn!("could not save settings: {e}");
            }
            self.saved_settings = self.settings.clone();
            self.last_save = Instant::now();
        }
        // "Remove black bars" toggled or the source's shape changed: the picture itself
        // changes, so it is decoded again.
        let mut refetch = false;
        if let Some(v) = &mut self.video {
            let wanted = if self.settings.remove_black_bars {
                v.info.detected_crop
            } else {
                None
            };
            let wanted_sar = v.info.with_frame_aspect(v.frame_aspect).sar;
            if v.info.crop != wanted || v.info.sar != wanted_sar {
                v.info = v.info.with_frame_aspect(v.frame_aspect).with_crop(wanted);
                v.frame = None;
                v.result = None;
                v.previewed.clear();
                v.thumbs = vec![None; THUMB_COUNT];
                if let Some(tools) = &self.tools {
                    worker::thumbnails(
                        self.bus.clone(),
                        tools.clone(),
                        self.gen,
                        v.info.clone(),
                        THUMB_COUNT,
                    );
                }
                self.view = ViewTransform::default();
                refetch = true;
            }
        }
        if refetch {
            self.request_frame();
        }
        let key = result_key(&self.settings);
        if key != self.result_settings_key {
            self.result_settings_key = key;
            if let Some(v) = &mut self.video {
                v.previewed.clear();
                v.result = None;
                worker::check_cached(
                    self.bus.clone(),
                    self.gen,
                    v.current,
                    self.settings.clone(),
                    v.info.clone(),
                );
            }
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if let Some(path) = dropped.into_iter().find(|p| is_video(p) || p.is_file()) {
            self.open_video(path);
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let Some(v) = &self.video else { return };
        let (cur, fps, last) = (
            v.current,
            v.info.fps().round().max(1.0) as u64,
            v.info.frame_count.saturating_sub(1),
        );
        let mut target: Option<u64> = None;
        ctx.input(|i| {
            let step = if i.modifiers.shift { fps } else { 1 };
            if i.key_pressed(egui::Key::ArrowRight) {
                target = Some((cur + step).min(last));
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                target = Some(cur.saturating_sub(step));
            }
            if i.key_pressed(egui::Key::Home) {
                target = Some(0);
            }
            if i.key_pressed(egui::Key::End) {
                target = Some(last);
            }
        });
        let (set_in, set_out, preview) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::I),
                i.key_pressed(egui::Key::O),
                i.key_pressed(egui::Key::P),
            )
        });
        if let Some(t) = target {
            self.seek(t);
        }
        if let Some(v) = &mut self.video {
            if set_in {
                v.in_point = Some(v.current);
            }
            if set_out {
                v.out_point = Some(v.current);
            }
        }
        if preview && self.preview.is_none() {
            self.start_preview();
        }
    }
}

fn result_key(s: &Settings) -> String {
    let mut s = s.clone();
    // Only fields that change the pixels matter; zero the rest.
    s.compare_mode = upscaler_core::settings::CompareMode::SideBySide;
    s.ui_scale = 1.0;
    s.seconds_per_frame.clear();
    s.presets.clear();
    s.language.clear();
    s.video_codec = upscaler_core::settings::VideoCodec::H264;
    s.crf = 0;
    s.ai_rate = upscaler_core::settings::AiRate::All;
    s.parallel_requests = 0;
    s.use_previous_frame = false;
    s.skip_duplicates = false;
    s.scene_cut_threshold = 0.0;
    s.audio_mode = Default::default();
    s.surround_engine = Default::default();
    s.lfe_level = Default::default();
    s.audio_enhance = false;
    format!("{s:?}")
}

pub fn is_video(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Uploads a frame, shrinking it first if it exceeds the GPU's texture limit.
fn load_big_texture(ctx: &egui::Context, name: &str, img: &RgbImage) -> egui::TextureHandle {
    let max = ctx.input(|i| i.max_texture_side).max(2048) as u32;
    let color = if img.width() > max || img.height() > max {
        let small = upscaler_core::frame::limit_size(img, max);
        worker::to_color_image(&small)
    } else {
        worker::to_color_image(img)
    };
    ctx.load_texture(name, color, egui::TextureOptions::LINEAR)
}

impl eframe::App for UpscalerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_events(ctx);
        self.handle_drops(ctx);
        self.handle_keys(ctx);
        self.persist();
        if self.preview.is_some()
            || self.exporting()
            || self.settings_ui.codex_busy.is_some()
            || self.audio_pack.busy()
            || self.audio_pack.probing
            || self.loading.is_some()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let zoom = self.settings.ui_scale.clamp(0.75, 2.5);
        if (ui.ctx().zoom_factor() - zoom).abs() > 0.001 {
            ui.ctx().set_zoom_factor(zoom);
        }
        egui::Panel::top("toolbar").show(ui, |ui| ui::toolbar::show(ui, self));
        egui::Panel::right("side")
            .resizable(true)
            .default_size(360.0)
            .min_size(300.0)
            .show(ui, |ui| ui::side_panel::show(ui, self));
        if self.video.is_some() {
            egui::Panel::bottom("timeline")
                .resizable(false)
                .show(ui, |ui| ui::timeline::show(ui, self));
        }
        egui::CentralPanel::default().show(ui, |ui| ui::viewer::show(ui, self));
        ui::settings_window::show(ui.ctx(), self);
        ui::frames::show(ui.ctx(), self);
        ui::dialogs::show(ui.ctx(), self);
        ui::dialogs::confirm_overwrite(ui.ctx(), self);
        ui::dialogs::drop_overlay(ui.ctx());
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(x) = &self.export {
            x.cancel.store(true, Ordering::Relaxed);
        }
        self.cancel_preview();
        self.save_project();
        let _ = self.settings.save();
    }
}
