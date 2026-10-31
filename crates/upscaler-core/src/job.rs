//! Previews and the export job.
//!
//! Export: the chosen range is extracted to PNG frames, every frame is upscaled into
//! `out/`, and the sequence is encoded with the original audio. A job lives in a folder
//! named after the video and every setting that changes the pixels, so a stopped or failed
//! export picks up where it left off (frames already done are never paid for twice).
//!
//! Consistency: with "use previous frame" on, frames run in order and each request carries
//! the previous restored frame as a reference, except after a scene cut. Frames that are
//! (almost) identical to the previous one reuse its result.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use image::RgbImage;

use crate::cache;
use crate::ffmpeg::{self, EncodeOptions, Range, Tools, VideoInfo};
use crate::frame;
use crate::process::{self, Cancelled};
use crate::provider;
use crate::settings::Settings;

/// Everything about the settings that changes the resulting pixels.
fn pixel_settings_key(s: &Settings, info: &VideoInfo) -> Vec<String> {
    let mut v = vec![s.provider.id().to_string(), format!("{:?}", s.output_size)];
    if s.provider.is_generative() {
        v.push(s.prompt.clone());
        v.push(s.pad_to_model_aspect.to_string());
        if let Some(a) = extension(s, info) {
            v.push(format!("aspect {}", a.name()));
        }
    }
    match s.provider {
        crate::settings::ProviderKind::ChatGptSubscription => v.push(s.codex_model.clone()),
        crate::settings::ProviderKind::OpenAiApi => v.extend([
            s.openai_model.clone(),
            s.openai_size.clone(),
            s.openai_quality.clone(),
            s.openai_input_fidelity.to_string(),
        ]),
        crate::settings::ProviderKind::RealEsrgan => v.push(s.realesrgan_model.clone()),
        crate::settings::ProviderKind::Basic => {}
    }
    v
}

// ---------------------------------------------------------------- geometry

/// Sizes for one video and the current settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Square-pixel size the frames are restored at (previews, AI, consistency reference).
    pub work: (u32, u32),
    /// Size stored in the exported file.
    pub stored: (u32, u32),
    /// Pixel aspect ratio of the exported file when it is anamorphic.
    pub sar: Option<(u32, u32)>,
}

/// The aspect ratio the AI extends this video to: the chosen one, unless the picture
/// already has it (within 1 %).
pub fn extension(settings: &Settings, info: &VideoInfo) -> Option<crate::settings::AspectChoice> {
    let source = info.width as f64 / info.height.max(1) as f64;
    settings
        .aspect_change()
        .filter(|a| (a.ratio() / source - 1.0).abs() > 0.01)
}

pub fn geometry(settings: &Settings, info: &VideoInfo) -> Geometry {
    // The same square-pixel size either way, so previews and finished frames are shared.
    let work = settings.output_size.target(info.width, info.height);
    if let Some(a) = extension(settings, info) {
        // The picture keeps its size and the canvas grows: wider adds width, taller adds height.
        let (w, h) = (work.0 as f64, work.1 as f64);
        let even = |v: f64| ((v / 2.0).round() as u32).max(1) * 2;
        let size = if a.ratio() > w / h {
            (even(h * a.ratio()), work.1)
        } else {
            (work.0, even(w / a.ratio()))
        };
        return Geometry {
            work: size,
            stored: size,
            sar: None,
        };
    }
    if info.anamorphic() && settings.keep_anamorphic {
        let stored = settings
            .output_size
            .target(info.coded_width, info.coded_height);
        Geometry {
            work,
            stored,
            sar: Some(info.sar),
        }
    } else {
        Geometry {
            work,
            stored: work,
            sar: None,
        }
    }
}

// ---------------------------------------------------------------- previews

/// Where the preview of `frame` with the current settings is cached.
pub fn preview_path(settings: &Settings, info: &VideoInfo, frame: u64) -> PathBuf {
    let mut parts = pixel_settings_key(settings, info);
    parts.push(info.identity());
    parts.push(frame.to_string());
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    settings
        .work_dir()
        .join("previews")
        .join(format!("{}.png", cache::key(&refs)))
}

/// Upscales one frame for the preview and caches it.
pub fn run_preview(
    settings: &Settings,
    info: &VideoInfo,
    frame: u64,
    source: &RgbImage,
    cancel: &AtomicBool,
) -> Result<RgbImage> {
    let target = geometry(settings, info).work;
    let provider = provider::create(settings);
    let tmp = temp_dir(settings);
    let result = provider::upscale_frame(
        provider.as_ref(),
        settings,
        source,
        None,
        target,
        &tmp,
        cancel,
    );
    let _ = std::fs::remove_dir_all(&tmp);
    let img = result?;
    let path = preview_path(settings, info, frame);
    save_atomic(&img, &path)?;
    Ok(img)
}

fn temp_dir(settings: &Settings) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let id = format!(
        "{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    settings.work_dir().join("tmp").join(id)
}

fn save_atomic(img: &RgbImage, path: &Path) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("part.png");
    img.save_with_format(&tmp, image::ImageFormat::Png)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------- export

#[derive(Clone, Debug)]
pub struct ExportSpec {
    pub info: VideoInfo,
    /// First source frame and one past the last.
    pub start: u64,
    pub end: u64,
    pub output: PathBuf,
    pub settings: Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Extracting,
    Analysing,
    Upscaling,
    Encoding,
}

#[derive(Clone, Debug)]
pub enum ExportEvent {
    Progress {
        stage: Stage,
        done: u64,
        total: u64,
    },
    /// Frame `n` (1-based within the job) is done; `seconds` is `None` when it was reused.
    FrameDone {
        n: u64,
        path: PathBuf,
        seconds: Option<f32>,
    },
    /// A frame failed and is retried.
    Retry {
        n: u64,
        error: String,
    },
    Finished(PathBuf),
    Failed(String),
    Cancelled,
}

impl ExportSpec {
    pub fn divisor(&self) -> u32 {
        self.settings.ai_rate.divisor()
    }

    /// Frames that go through the upscaler.
    pub fn job_frames(&self) -> u64 {
        (self.end - self.start).div_ceil(self.divisor() as u64)
    }

    pub fn job_dir(&self) -> PathBuf {
        let mut parts = pixel_settings_key(&self.settings, &self.info);
        parts.extend([
            self.info.identity(),
            self.start.to_string(),
            self.end.to_string(),
            self.divisor().to_string(),
            self.settings.use_previous_frame.to_string(),
            self.settings.skip_duplicates.to_string(),
            self.settings.scene_cut_threshold.to_string(),
        ]);
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        self.settings
            .work_dir()
            .join("jobs")
            .join(cache::key(&refs))
    }
}

/// Frames already finished in an earlier run of the same job.
pub fn frames_already_done(spec: &ExportSpec) -> u64 {
    ffmpeg::count_frames(&spec.job_dir().join("out"))
}

/// Runs the whole export, reporting through `emit`. Never panics on errors: they end in
/// `Failed` (or `Cancelled`).
pub fn run_export(
    tools: &Tools,
    spec: &ExportSpec,
    cancel: &AtomicBool,
    emit: &(dyn Fn(ExportEvent) + Sync),
) {
    match export(tools, spec, cancel, emit) {
        Ok(()) => emit(ExportEvent::Finished(spec.output.clone())),
        Err(e) if process::is_cancelled(&e) => emit(ExportEvent::Cancelled),
        Err(e) => {
            log::error!("export failed: {e:#}");
            emit(ExportEvent::Failed(format!("{e:#}")))
        }
    }
}

fn check(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!(Cancelled);
    }
    Ok(())
}

fn export(
    tools: &Tools,
    spec: &ExportSpec,
    cancel: &AtomicBool,
    emit: &(dyn Fn(ExportEvent) + Sync),
) -> Result<()> {
    let dir = spec.job_dir();
    let src_dir = dir.join("src");
    let out_dir = dir.join("out");
    std::fs::create_dir_all(&out_dir)?;
    std::fs::write(
        dir.join("job.txt"),
        format!(
            "video: {}\nframes: {}..{}\noutput: {}\nprovider: {:?}\n",
            spec.info.path.display(),
            spec.start,
            spec.end,
            spec.output.display(),
            spec.settings.provider
        ),
    )?;

    // 1. Extract
    let expected = spec.job_frames();
    let marker = dir.join("extracted");
    let total = if let Some(n) = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        n
    } else {
        let _ = std::fs::remove_dir_all(&src_dir);
        let range = Range {
            start: spec.start,
            count: spec.end - spec.start,
        };
        let n = ffmpeg::extract_range(
            tools,
            &spec.info,
            &range,
            spec.divisor(),
            &src_dir,
            cancel,
            |f| {
                emit(ExportEvent::Progress {
                    stage: Stage::Extracting,
                    done: f.min(expected),
                    total: expected,
                })
            },
        )?;
        std::fs::write(&marker, n.to_string())?;
        n
    };
    check(cancel)?;

    // 2. Analyse: duplicates and scene cuts.
    let s = &spec.settings;
    let mut dup_of: Vec<Option<u64>> = vec![None; total as usize + 1];
    let mut cut: Vec<bool> = vec![true; total as usize + 1];
    if s.skip_duplicates || s.sequential() {
        let mut prev: Option<image::GrayImage> = None;
        for n in 1..=total {
            check(cancel)?;
            let img = image::open(ffmpeg::frame_file(&src_dir, n))
                .with_context(|| format!("frame {n}"))?
                .to_rgb8();
            let fp = frame::fingerprint(&img);
            if let Some(p) = &prev {
                let d = frame::difference(p, &fp);
                if s.skip_duplicates && d < 0.6 {
                    let i = n as usize;
                    dup_of[i] = Some(dup_of[i - 1].unwrap_or(n - 1));
                }
                cut[n as usize] = d > s.scene_cut_threshold;
            }
            prev = Some(fp);
            if n % 10 == 0 || n == total {
                emit(ExportEvent::Progress {
                    stage: Stage::Analysing,
                    done: n,
                    total,
                });
            }
        }
    }

    // 3. Upscale
    let geo = geometry(s, &spec.info);
    let target = geo.work;
    let provider = provider::create(s);
    let done = AtomicU64::new(0);
    let report = |n: u64, path: PathBuf, seconds: Option<f32>| {
        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
        emit(ExportEvent::FrameDone { n, path, seconds });
        emit(ExportEvent::Progress {
            stage: Stage::Upscaling,
            done: d,
            total,
        });
    };
    // Frames from an earlier run count as done right away.
    for n in 1..=total {
        let p = ffmpeg::frame_file(&out_dir, n);
        if p.is_file() {
            done.fetch_add(1, Ordering::Relaxed);
        }
    }
    emit(ExportEvent::Progress {
        stage: Stage::Upscaling,
        done: done.load(Ordering::Relaxed),
        total,
    });

    let work = |n: u64, reference: Option<&RgbImage>| -> Result<RgbImage> {
        let src = image::open(ffmpeg::frame_file(&src_dir, n))?.to_rgb8();
        let mut attempt = 0;
        loop {
            check(cancel)?;
            let tmp = temp_dir(s);
            let started = Instant::now();
            let r = provider::upscale_frame(
                provider.as_ref(),
                s,
                &src,
                reference,
                target,
                &tmp,
                cancel,
            );
            match r {
                Ok(img) => {
                    let _ = std::fs::remove_dir_all(&tmp);
                    let path = ffmpeg::frame_file(&out_dir, n);
                    save_atomic(&img, &path)?;
                    report(n, path, Some(started.elapsed().as_secs_f32()));
                    return Ok(img);
                }
                Err(e) if process::is_cancelled(&e) => return Err(e),
                Err(e) if attempt < 2 => {
                    attempt += 1;
                    log::warn!("frame {n} failed (attempt {attempt}): {e:#}");
                    emit(ExportEvent::Retry {
                        n,
                        error: format!("{e:#}"),
                    });
                    std::thread::sleep(std::time::Duration::from_secs(5 * attempt));
                }
                Err(e) => return Err(e.context(format!("frame {n} (kept: {})", tmp.display()))),
            }
        }
    };
    let copy_dup = |n: u64, of: u64| -> Result<()> {
        let path = ffmpeg::frame_file(&out_dir, n);
        std::fs::copy(ffmpeg::frame_file(&out_dir, of), &path)?;
        report(n, path, None);
        Ok(())
    };

    if s.sequential() {
        let mut prev_out: Option<RgbImage> = None;
        for n in 1..=total {
            check(cancel)?;
            let path = ffmpeg::frame_file(&out_dir, n);
            if path.is_file() {
                prev_out = None; // loaded lazily below if the next frame needs it
                continue;
            }
            if let Some(of) = dup_of[n as usize] {
                copy_dup(n, of)?;
                prev_out = None;
                continue;
            }
            let reference = if n > 1 && !cut[n as usize] {
                if prev_out.is_none() {
                    prev_out = image::open(ffmpeg::frame_file(&out_dir, n - 1))
                        .ok()
                        .map(|i| i.to_rgb8());
                }
                prev_out.as_ref()
            } else {
                None
            };
            let img = work(n, reference)?;
            prev_out = Some(img);
        }
    } else {
        let todo: Vec<u64> = (1..=total)
            .filter(|n| {
                dup_of[*n as usize].is_none() && !ffmpeg::frame_file(&out_dir, *n).is_file()
            })
            .collect();
        let next = AtomicU64::new(0);
        let first_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
        let workers = s.parallel_requests.clamp(1, 16) as usize;
        std::thread::scope(|scope| {
            for _ in 0..workers.min(todo.len().max(1)) {
                scope.spawn(|| loop {
                    if first_error.lock().unwrap().is_some() || cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                    let Some(&n) = todo.get(i) else { return };
                    if let Err(e) = work(n, None) {
                        let mut slot = first_error.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        return;
                    }
                });
            }
        });
        if let Some(e) = first_error.into_inner().unwrap() {
            return Err(e);
        }
        check(cancel)?;
        for n in 1..=total {
            if let Some(of) = dup_of[n as usize] {
                if !ffmpeg::frame_file(&out_dir, n).is_file() {
                    copy_dup(n, of)?;
                }
            }
        }
    }
    check(cancel)?;

    // 4. Encode
    let info = &spec.info;
    let divisor = spec.divisor();
    let out_frames_total = if divisor > 1 {
        spec.end - spec.start
    } else {
        total
    };
    let opts = EncodeOptions {
        in_fps_num: info.fps_num,
        in_fps_den: info.fps_den as u64 * divisor as u64,
        interpolate_to: (divisor > 1).then_some((info.fps_num, info.fps_den)),
        anamorphic: geo.sar.map(|sar| (geo.stored, sar)),
        codec: s.video_codec,
        crf: s.crf,
        audio_from: info.has_audio.then(|| {
            (
                info.path.clone(),
                info.frame_time(spec.start),
                (spec.end - spec.start) as f64 / info.fps(),
            )
        }),
    };
    if let Some(d) = spec.output.parent() {
        std::fs::create_dir_all(d)?;
    }
    ffmpeg::encode(tools, &out_dir, total, &opts, &spec.output, cancel, |f| {
        emit(ExportEvent::Progress {
            stage: Stage::Encoding,
            done: f.min(out_frames_total),
            total: out_frames_total,
        })
    })?;
    Ok(())
}

/// Removes the source frames of finished jobs; with `everything` also previews, results and temp files.
pub fn clean_work_dir(settings: &Settings, everything: bool) -> Result<()> {
    let w = settings.work_dir();
    if everything {
        for sub in ["previews", "jobs", "tmp"] {
            let _ = std::fs::remove_dir_all(w.join(sub));
        }
    } else {
        let _ = std::fs::remove_dir_all(w.join("tmp"));
    }
    Ok(())
}

/// Total bytes below `dir`.
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}
