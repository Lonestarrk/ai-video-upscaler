//! Background work. Every result comes back to the UI as an [`Event`] on one channel, and
//! each sender wakes the UI with `request_repaint`.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui;
use image::RgbImage;
use upscaler_core::ffmpeg::{self, Tools, VideoInfo};
use upscaler_core::job::{self, ExportEvent, ExportSpec};
use upscaler_core::provider::codex::{self, LoginState};
use upscaler_core::settings::Settings;

pub enum Event {
    Probed {
        gen: u64,
        result: Result<VideoInfo, String>,
    },
    Thumb {
        gen: u64,
        slot: usize,
        image: egui::ColorImage,
    },
    Frame {
        gen: u64,
        index: u64,
        image: Arc<RgbImage>,
    },
    FrameFailed {
        gen: u64,
        index: u64,
        error: String,
    },
    /// A cached preview for `index` (or `None`: there is none for the current settings).
    CachedResult {
        gen: u64,
        index: u64,
        key: PathBuf,
        image: Option<Arc<RgbImage>>,
    },
    PreviewDone {
        gen: u64,
        index: u64,
        key: PathBuf,
        result: Result<Arc<RgbImage>, String>,
        seconds: f32,
    },
    Export(ExportEvent),
    /// A small copy of the newest exported frame.
    ExportThumb(egui::ColorImage),
    CodexStatus(LoginState),
    /// Result of login/logout/install, as a message.
    CodexAction(Result<String, String>),
    FfmpegDownload {
        done: u64,
        total: Option<u64>,
    },
    FfmpegDownloaded(Result<(), String>),
    WorkDirSize(u64),
}

#[derive(Clone)]
pub struct Bus {
    tx: Sender<Event>,
    ctx: egui::Context,
}

impl Bus {
    pub fn new(tx: Sender<Event>, ctx: egui::Context) -> Self {
        Bus { tx, ctx }
    }

    pub fn send(&self, e: Event) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        log::error!("could not start thread {name}: {e}");
    }
}

pub fn to_color_image(img: &RgbImage) -> egui::ColorImage {
    egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw())
}

pub fn probe(bus: Bus, tools: Tools, gen: u64, path: PathBuf) {
    spawn("probe", move || {
        let result = ffmpeg::probe(&tools, &path).map_err(|e| format!("{e:#}"));
        bus.send(Event::Probed { gen, result });
    });
}

/// Fetches `count` evenly spaced thumbnails, nearest keyframe each (fast).
pub fn thumbnails(bus: Bus, tools: Tools, gen: u64, info: VideoInfo, count: usize) {
    spawn("thumbs", move || {
        for slot in 0..count {
            let index = ((slot as f64 + 0.5) / count as f64 * info.frame_count as f64) as u64;
            match ffmpeg::grab_frame(&tools, &info, index, Some(72), true) {
                Ok(img) => bus.send(Event::Thumb {
                    gen,
                    slot,
                    image: to_color_image(&img),
                }),
                Err(e) => log::warn!("thumbnail {slot}: {e:#}"),
            }
        }
    });
}

/// One frame request: always the newest wins; older ones still waiting are dropped.
pub struct FrameRequest {
    pub gen: u64,
    pub index: u64,
    pub tools: Tools,
    pub info: VideoInfo,
    pub settings: Settings,
}

/// A single long-lived thread that decodes the frame under the playhead.
pub struct FrameFetcher {
    slot: Arc<(Mutex<Option<FrameRequest>>, Condvar)>,
}

impl FrameFetcher {
    pub fn start(bus: Bus) -> Self {
        let slot: Arc<(Mutex<Option<FrameRequest>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let s = slot.clone();
        spawn("frames", move || loop {
            let req = {
                let (lock, cv) = &*s;
                let mut g = lock.lock().unwrap();
                while g.is_none() {
                    g = cv.wait(g).unwrap();
                }
                g.take().unwrap()
            };
            match ffmpeg::grab_frame(&req.tools, &req.info, req.index, None, false) {
                Ok(img) => bus.send(Event::Frame {
                    gen: req.gen,
                    index: req.index,
                    image: Arc::new(img),
                }),
                Err(e) => bus.send(Event::FrameFailed {
                    gen: req.gen,
                    index: req.index,
                    error: format!("{e:#}"),
                }),
            }
            load_cached(&bus, req.gen, req.index, &req.settings, &req.info);
        });
        FrameFetcher { slot }
    }

    pub fn request(&self, req: FrameRequest) {
        let (lock, cv) = &*self.slot;
        *lock.lock().unwrap() = Some(req);
        cv.notify_one();
    }
}

fn load_cached(bus: &Bus, gen: u64, index: u64, settings: &Settings, info: &VideoInfo) {
    let key = job::preview_path(settings, info, index);
    let image = if key.is_file() {
        image::open(&key).ok().map(|i| Arc::new(i.to_rgb8()))
    } else {
        None
    };
    bus.send(Event::CachedResult {
        gen,
        index,
        key,
        image,
    });
}

/// Looks up the cached preview for `index` with the current settings.
pub fn check_cached(bus: Bus, gen: u64, index: u64, settings: Settings, info: VideoInfo) {
    spawn("cache", move || {
        load_cached(&bus, gen, index, &settings, &info)
    });
}

pub fn preview(
    bus: Bus,
    gen: u64,
    index: u64,
    source: Arc<RgbImage>,
    settings: Settings,
    info: VideoInfo,
    cancel: Arc<AtomicBool>,
) {
    spawn("preview", move || {
        let start = std::time::Instant::now();
        let key = job::preview_path(&settings, &info, index);
        let result = job::run_preview(&settings, &info, index, &source, &cancel)
            .map(Arc::new)
            .map_err(|e| format!("{e:#}"));
        bus.send(Event::PreviewDone {
            gen,
            index,
            key,
            result,
            seconds: start.elapsed().as_secs_f32(),
        });
    });
}

pub fn export(bus: Bus, tools: Tools, spec: ExportSpec, cancel: Arc<AtomicBool>) {
    spawn("export", move || {
        let last_thumb = Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(10));
        let emit = |e: ExportEvent| {
            if let ExportEvent::FrameDone { path, .. } = &e {
                // At most one small preview per second.
                let mut last = last_thumb.lock().unwrap();
                if last.elapsed().as_secs_f32() > 1.0 {
                    *last = std::time::Instant::now();
                    if let Ok(img) = image::open(path) {
                        let small = img.thumbnail(480, 270).to_rgb8();
                        bus.send(Event::ExportThumb(to_color_image(&small)));
                    }
                }
            }
            bus.send(Event::Export(e));
        };
        job::run_export(&tools, &spec, &cancel, &emit);
    });
}

pub fn codex_status(bus: Bus, path: String) {
    spawn("codex-status", move || {
        bus.send(Event::CodexStatus(codex::login_status(&path)))
    });
}

pub enum CodexOp {
    Login,
    Logout,
    Install,
}

pub fn codex_action(bus: Bus, path: String, op: CodexOp, cancel: Arc<AtomicBool>) {
    spawn("codex-action", move || {
        let r = match op {
            CodexOp::Login => codex::login(&path, &cancel),
            CodexOp::Logout => codex::logout(&path).map(|_| String::new()),
            CodexOp::Install => codex::install_with_npm(&cancel),
        };
        bus.send(Event::CodexAction(r.map_err(|e| format!("{e:#}"))));
        bus.send(Event::CodexStatus(codex::login_status(&path)));
    });
}

pub fn download_ffmpeg(bus: Bus, cancel: Arc<AtomicBool>) {
    spawn("ffmpeg-download", move || {
        let b = bus.clone();
        let mut last = std::time::Instant::now();
        let r = upscaler_core::ffmpeg_download::download(&cancel, |done, total| {
            if last.elapsed().as_millis() > 100 {
                last = std::time::Instant::now();
                b.send(Event::FfmpegDownload { done, total });
            }
        });
        bus.send(Event::FfmpegDownloaded(r.map_err(|e| format!("{e:#}"))));
    });
}

pub fn work_dir_size(bus: Bus, dir: PathBuf) {
    spawn("du", move || {
        bus.send(Event::WorkDirSize(job::dir_size(&dir)))
    });
}

/// Shows `path` in Explorer / Finder.
pub fn reveal(path: &std::path::Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = path.parent().unwrap_or(path);
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

/// Opens `path` with its default program.
pub fn open(path: &std::path::Path) {
    #[cfg(windows)]
    {
        let _ = upscaler_core::process::command("cmd")
            .arg("/C")
            .arg("start")
            .arg("")
            .arg(path)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}
