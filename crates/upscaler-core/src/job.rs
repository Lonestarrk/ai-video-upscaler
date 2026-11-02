//! Previews and the export job.
//!
//! Export: the chosen range is extracted to PNG frames, every frame is upscaled into
//! `out/`, and the sequence is encoded with the original audio. A job lives in a folder
//! named after the video and every setting that changes the pixels, so a stopped or failed
//! export picks up where it left off (frames already done are never paid for twice).
//!
//! When the audio settings change the sound (5.1, better quality), the frames are encoded
//! without audio to `video.mkv` in the job folder, the audio job runs, and both are muxed
//! with the untouched original into an MKV. The AI audio pack is checked before any frame
//! work, so a missing pack fails the export at once.
//!
//! Consistency: with "use previous frame" on, frames run in order and each request carries
//! the previous restored frame as a reference, except after a scene cut. Frames that are
//! (almost) identical to the previous one reuse its result.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use image::RgbImage;

use crate::audio::ai::{AudioAi, Device, LocalAi, PACK_MISSING};
use crate::audio::job::{self as audio_job, AudioEvent, AudioSpec};
use crate::cache;
use crate::ffmpeg::{self, EncodeOptions, Range, Tools, VideoInfo, VideoInput};
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
        crate::settings::ProviderKind::Gemini => {
            v.extend([s.gemini_model.clone(), s.gemini_image_size.clone()])
        }
        crate::settings::ProviderKind::Flux => v.push(s.bfl_model.clone()),
        crate::settings::ProviderKind::Grok => v.push(s.xai_model.clone()),
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
        None,
        target,
        &tmp,
        cancel,
    );
    // A failed request keeps its folder (input.png, codex-output.txt …) for troubleshooting.
    let img = match result {
        Ok(img) => {
            let _ = std::fs::remove_dir_all(&tmp);
            img
        }
        Err(e) if crate::process::is_cancelled(&e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        Err(e) => {
            log::warn!(
                "preview of frame {frame} failed, files kept in {}: {e:#}",
                tmp.display()
            );
            return Err(e.context(format!("files kept in {}", tmp.display())));
        }
    };
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
    /// SDR to HDR conversion of the finished frames (only when HDR output is on).
    Hdr,
    Encoding,
    /// The audio job (5.1 / better quality); `done`/`total` count its sections.
    Audio,
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
    /// Quality control judged frame `n` (its `attempt`th result).
    Reviewed {
        n: u64,
        outcome: ReviewOutcome,
        problems: String,
        attempt: u32,
    },
    /// From the audio job; its warnings are English (the GUI translates them).
    Audio(AudioEvent),
    /// The export is done; the path is the file written (`.mkv` when the audio was changed).
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
    ffmpeg::count_frames(&spec.job_dir().join(OUTPUT_DIR))
}

/// Folders of an export job: the frames taken from the video and the finished frames.
pub const INPUT_DIR: &str = "src";
pub const OUTPUT_DIR: &str = "out";

/// Runs the whole export, reporting through `emit`. Never panics on errors: they end in
/// `Failed` (or `Cancelled`).
pub fn run_export(
    tools: &Tools,
    spec: &ExportSpec,
    cancel: &AtomicBool,
    emit: &(dyn Fn(ExportEvent) + Sync),
) {
    match export(tools, spec, cancel, emit) {
        Ok(written) => emit(ExportEvent::Finished(written)),
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

/// What the export does with the audio when the settings change it: the AI pack, opened
/// before any frame work when the job will use it.
struct AudioWork {
    ai: Option<LocalAi>,
}

/// Decides, before any frame work, whether the export runs the audio job, and fails at once
/// when it cannot (AI pack missing while it is needed, mono source to 5.1, …) instead of
/// after hours of frames. `None`: the audio is not changed (or there is none), so the export
/// takes the plain MP4 path with the original audio.
fn prepare_audio(
    tools: &Tools,
    spec: &ExportSpec,
    emit: &(dyn Fn(ExportEvent) + Sync),
) -> Result<Option<AudioWork>> {
    let s = &spec.settings;
    if !s.audio_active() || !spec.info.has_audio {
        return Ok(None);
    }
    let Some(source) = crate::audio::source::analyse(tools, &spec.info)? else {
        return Ok(None);
    };
    // A 5.1 source with 5.1 chosen and no enhancement: nothing to change, so the original
    // audio is copied as with "Unchanged" instead of transcoded to the same thing.
    if audio_job::keeps_original(s, &source) {
        log::info!("export: the source is already 5.1, its audio is kept as it is");
        return Ok(None);
    }
    let (ai, open_error) = if audio_job::may_use_ai(s) {
        match LocalAi::open() {
            Ok(ai) => (Some(ai), None),
            Err(e) => (None, Some(e)),
        }
    } else {
        (None, None)
    };
    let plan = match audio_job::plan_job(s, Some(&source), ai.is_some()) {
        Ok(plan) => plan,
        Err(e) => {
            // The plan's own "pack missing" text says what to change for this job; any other
            // failure to open the pack says why.
            if let Some(open_error) = open_error {
                if audio_job::plan_job(s, Some(&source), true).is_ok()
                    && open_error.to_string() != PACK_MISSING
                {
                    return Err(open_error);
                }
            }
            return Err(e);
        }
    };
    // The device recorded when the pack was installed: free to read, unlike asking the
    // worker (which starts Python and loads torch). A CPU never fails the export.
    if plan.needs_ai() && ai.as_ref().is_some_and(|a| a.device == Device::Cpu) {
        let w = plan.cpu_warning();
        log::warn!("{w}");
        emit(ExportEvent::Audio(AudioEvent::Warning(w.to_string())));
    }
    Ok(Some(AudioWork {
        ai: ai.filter(|_| plan.needs_ai()),
    }))
}

/// What the encoded intermediate video depends on: the encode options, `extra` (the HDR
/// settings and how the encoder converts them) and every finished frame (size and
/// modification time), so a frame made again means a new encode.
///
/// Everything HDR adds (the interpolated frames, the measured light levels, the HDR10+
/// metadata) is computed deterministically from exactly these, so the key is checked before
/// any HDR work: reusing `video.mkv` costs nothing.
fn video_key(
    opts: &EncodeOptions,
    extra: &str,
    total: u64,
    frames: impl Iterator<Item = PathBuf>,
) -> String {
    // The HDR curve and the interpolation stage change pixels without changing `opts`.
    let mut parts = vec![
        format!("{opts:?}"),
        extra.to_string(),
        total.to_string(),
        format!(
            "hdr v{} interp v{INTERP_VERSION}",
            crate::hdr::convert::HDR_VERSION
        ),
    ];
    for path in frames {
        let (len, modified) = std::fs::metadata(&path)
            .map(|m| {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                (m.len(), t)
            })
            .unwrap_or((0, 0));
        parts.push(format!("{len}:{modified}"));
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    format!("frames {total}\n{}", cache::key(&refs))
}

/// Interpolated SDR frames for HDR export, and the version of how they are made.
const INTERP_DIR: &str = "interp";
const INTERP_VERSION: u32 = 1;

/// Motion-interpolates the SDR frames of `out_dir` to the source rate into `interp_dir`
/// (8-bit, like the SDR export does), reusing an earlier run made with the same settings
/// from `out/` frames that have not changed since. Returns the number of frames.
fn interpolate_sdr(
    tools: &Tools,
    spec: &ExportSpec,
    out_dir: &Path,
    interp_dir: &Path,
    total: u64,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<u64> {
    let info = &spec.info;
    let want = format!(
        "fps: {}/{}
divisor: {}
frames: {total}
version: {INTERP_VERSION}
",
        info.fps_num,
        info.fps_den,
        spec.divisor()
    );
    let marker = interp_dir.join("settings.txt");
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let fresh = std::fs::read_to_string(&marker).is_ok_and(|h| h == want)
        && modified(&marker).is_some_and(|m| {
            (1..=total).all(|n| modified(&ffmpeg::frame_file(out_dir, n)).is_none_or(|o| o <= m))
        });
    if fresh {
        let n = ffmpeg::count_frames(interp_dir);
        if n > 0 {
            progress(n);
            return Ok(n);
        }
    }
    let _ = std::fs::remove_dir_all(interp_dir);
    let n = ffmpeg::interpolate_frames(
        tools,
        out_dir,
        (info.fps_num, info.fps_den as u64 * spec.divisor() as u64),
        (info.fps_num, info.fps_den),
        interp_dir,
        cancel,
        progress,
    )?;
    std::fs::write(&marker, want)?;
    Ok(n)
}

/// What the HDR encode needs from the first pass.
struct HdrWork {
    encode: crate::hdr::HdrEncode,
    params: crate::hdr::convert::HdrParams,
    /// The SDR frames that are converted (`out/`, or the full-rate `interp/`) and their number.
    src: PathBuf,
    frames: u64,
}

/// HDR pass 1: interpolates the SDR frames to the full rate when frames were skipped, then
/// measures every frame's HDR version (`hdr::convert::measure_pass`) and writes the HDR10+
/// metadata. One progress bar (`Stage::Hdr`) over both.
#[allow(clippy::too_many_arguments)]
fn hdr_first_pass(
    tools: &Tools,
    spec: &ExportSpec,
    params: &crate::hdr::convert::HdrParams,
    dir: &Path,
    out_dir: &Path,
    total: u64,
    cut: &[bool],
    cancel: &AtomicBool,
    emit: &(dyn Fn(ExportEvent) + Sync),
) -> Result<HdrWork> {
    use crate::settings::HdrMode;
    let s = &spec.settings;
    let divisor = spec.divisor();
    let interp_on = divisor > 1;
    let weight = if interp_on {
        spec.end - spec.start
    } else {
        total
    }
    .max(1);
    let units = (interp_on as u64 + 1) * weight;
    let report = |base: u64, f: u64| {
        emit(ExportEvent::Progress {
            stage: Stage::Hdr,
            done: base + f.min(weight),
            total: units,
        })
    };
    report(0, 0);
    let (src, frames, base) = if interp_on {
        let interp = dir.join(INTERP_DIR);
        let n = interpolate_sdr(tools, spec, out_dir, &interp, total, cancel, |f| {
            report(0, f)
        });
        check(cancel)?;
        (interp, n?, weight)
    } else {
        (out_dir.to_path_buf(), total, 0)
    };
    let lights = crate::hdr::convert::measure_pass(
        &src,
        &crate::hdr::convert::hdr_dir(dir),
        frames,
        params,
        cancel,
        |f| report(base, f),
    );
    check(cancel)?;
    let m = crate::hdr::measure::Measurement::from_frames(lights?);
    // HLG carries no light levels.
    let mut hdr10plus_json = None;
    let (max_cll, max_fall) = if params.mode.is_pq() {
        if params.mode == HdrMode::Hdr10Plus {
            // Scene ids follow the export's cut list, mapped to the encoded frames.
            let cuts = crate::hdr::hdr10plus::encoded_cuts(
                cut,
                s.skip_duplicates || s.sequential(),
                divisor,
                m.frames.len(),
            );
            let path = dir.join("hdr10plus.json");
            crate::hdr::hdr10plus::write_json(&m, &cuts, params.peak_nits, &path)?;
            hdr10plus_json = Some(path);
        }
        (m.max_cll, m.max_fall)
    } else {
        (0, 0)
    };
    Ok(HdrWork {
        encode: crate::hdr::HdrEncode {
            mode: params.mode,
            peak_nits: params.peak_nits,
            max_cll,
            max_fall,
            hdr10plus_json,
        },
        params: *params,
        src,
        frames,
    })
}

/// The intermediate video (no audio) and its marker in the job folder.
pub const VIDEO_FILE: &str = "video.mkv";
const VIDEO_DONE: &str = "video.done";

/// The new-audio path writes `spec.output` with the extension `.mkv`, a file the user may not
/// have chosen: refuses when that is the source video itself (`film.mkv` exported as
/// `film.mp4`).
fn check_mkv_output(spec: &ExportSpec) -> Result<()> {
    let output = spec.output.with_extension("mkv");
    if let (Ok(out), Ok(src)) = (
        std::fs::canonicalize(&output),
        std::fs::canonicalize(&spec.info.path),
    ) {
        if out == src {
            bail!(
                "The export with new audio is saved as {}, which is the source video. \
                 Choose another name or folder.",
                output.display()
            );
        }
    }
    Ok(())
}

fn export(
    tools: &Tools,
    spec: &ExportSpec,
    cancel: &AtomicBool,
    emit: &(dyn Fn(ExportEvent) + Sync),
) -> Result<PathBuf> {
    let audio = prepare_audio(tools, spec, emit)?;
    if audio.is_some() {
        check_mkv_output(spec)?;
    }
    let dir = spec.job_dir();
    let src_dir = dir.join(INPUT_DIR);
    let out_dir = dir.join(OUTPUT_DIR);
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

    // 3. Upscale (and review)
    let geo = geometry(s, &spec.info);
    let provider = provider::create(s);
    let plan = Plan {
        total,
        dup_of: &dup_of,
        cut: &cut,
        src_dir: &src_dir,
        out_dir: &out_dir,
        review_file: &dir.join(REVIEW_FILE),
        rejects_dir: &dir.join(REJECTS_DIR),
        rejected_dir: &dir.join(REJECTED_DIR),
        target: geo.work,
    };
    upscale_all(&plan, s, provider.as_ref(), cancel, emit)?;
    check(cancel)?;

    // 3b. HDR (`hdr::convert`): nothing 16-bit is stored. The first pass converts and
    // measures every frame in memory (cached per frame in `hdr/stats.txt`), the encoder then
    // gets the frames converted again through a pipe. With skipped frames the SDR frames are
    // motion-interpolated first (`interp/`, 8-bit, as for SDR) and the HDR stages work at the
    // full rate: interpolating HDR itself would be 8-bit PQ.
    let info = &spec.info;
    let divisor = spec.divisor();
    let hdr_params = crate::hdr::convert::HdrParams::from_settings(s);
    let interp_frames = dir.join(INTERP_DIR);
    let first_pass = |p: &crate::hdr::convert::HdrParams| {
        hdr_first_pass(tools, spec, p, &dir, &out_dir, total, &cut, cancel, emit)
    };
    // The interpolated frames are only an intermediate: gone once the output exists.
    let finish = |output: PathBuf| -> PathBuf {
        if hdr_params.is_some() && s.hdr_delete_interp_after && interp_frames.exists() {
            if let Err(e) = std::fs::remove_dir_all(&interp_frames) {
                log::warn!("could not delete {}: {e}", interp_frames.display());
            }
        }
        output
    };

    // 4. Encode
    let out_frames_total = if divisor > 1 {
        spec.end - spec.start
    } else {
        total
    };
    let (in_fps_num, in_fps_den, interpolate_to) = if hdr_params.is_some() {
        // Already at the full rate.
        (info.fps_num, info.fps_den as u64, None)
    } else {
        (
            info.fps_num,
            info.fps_den as u64 * divisor as u64,
            (divisor > 1).then_some((info.fps_num, info.fps_den)),
        )
    };
    let opts = EncodeOptions {
        in_fps_num,
        in_fps_den,
        interpolate_to,
        anamorphic: geo.sar.map(|sar| (geo.stored, sar)),
        codec: s.effective_codec(),
        crf: s.crf,
        audio_from: info.has_audio.then(|| {
            (
                info.path.clone(),
                info.frame_time(spec.start),
                (spec.end - spec.start) as f64 / info.fps(),
            )
        }),
        hdr: None,
    };
    let progress = |f: u64| {
        emit(ExportEvent::Progress {
            stage: Stage::Encoding,
            done: f.min(out_frames_total),
            total: out_frames_total,
        })
    };
    // SDR reads `out/`; HDR gets the second pass's frames on stdin.
    let run_encode = |work: Option<&HdrWork>, opts: &EncodeOptions, out: &Path| -> Result<()> {
        let Some(w) = work else {
            return ffmpeg::encode(
                tools,
                &VideoInput::Frames(&out_dir),
                opts,
                out,
                cancel,
                progress,
            );
        };
        let (width, height) = crate::hdr::convert::frame_size(&w.src)?;
        let feed = |pipe: &mut dyn std::io::Write| {
            crate::hdr::convert::stream_frames(&w.src, w.frames, &w.params, cancel, pipe)
        };
        let input = VideoInput::Rgb48 {
            width,
            height,
            feed: &feed,
        };
        ffmpeg::encode(tools, &input, opts, out, cancel, progress)
    };
    let Some(audio) = audio else {
        if let Some(d) = spec.output.parent() {
            std::fs::create_dir_all(d)?;
        }
        let work = hdr_params.as_ref().map(first_pass).transpose()?;
        let opts = EncodeOptions {
            hdr: work.as_ref().map(|w| w.encode.clone()),
            ..opts
        };
        run_encode(work.as_ref(), &opts, &spec.output)?;
        return Ok(finish(spec.output.clone()));
    };

    // 4b. With new audio: the video without audio to an intermediate MKV (kept, so changing
    // only the audio settings re-runs the audio and the mux, not the frames, the HDR passes
    // or the encode), then the audio job, then the mux.
    let opts = EncodeOptions {
        audio_from: None,
        ..opts
    };
    let video = dir.join(VIDEO_FILE);
    let done = dir.join(VIDEO_DONE);
    let extra = hdr_params.as_ref().map_or(String::new(), |p| {
        format!("{}zscale: {}\n", p.header(), tools.has_zscale())
    });
    let key = video_key(
        &opts,
        &extra,
        total,
        (1..=total)
            .filter(|n| dup_of[*n as usize].is_none())
            .map(|n| ffmpeg::frame_file(&out_dir, n)),
    );
    let reuse = video.is_file() && std::fs::read_to_string(&done).is_ok_and(|k| k == key);
    if reuse {
        log::info!("export: reusing {}", video.display());
    } else {
        let _ = std::fs::remove_file(&done);
        let work = hdr_params.as_ref().map(first_pass).transpose()?;
        let opts = EncodeOptions {
            hdr: work.as_ref().map(|w| w.encode.clone()),
            ..opts
        };
        let part = dir.join("video.part.mkv");
        run_encode(work.as_ref(), &opts, &part)?;
        std::fs::rename(&part, &video).context("could not keep the encoded video")?;
        std::fs::write(&done, &key)?;
    }
    check(cancel)?;
    emit(ExportEvent::Progress {
        stage: Stage::Audio,
        done: 0,
        total: 1,
    });
    let audio_spec = AudioSpec {
        info: info.clone(),
        start: spec.start,
        end: spec.end,
        settings: s.clone(),
    };
    let warned = Mutex::new(Vec::<String>::new());
    let result = audio_job::run(
        tools,
        &audio_spec,
        audio.ai.as_ref().map(|a| a as &dyn AudioAi),
        cancel,
        &|e| {
            match &e {
                AudioEvent::Warning(w) => warned.lock().unwrap().push(w.clone()),
                AudioEvent::Section { done, total } => emit(ExportEvent::Progress {
                    stage: Stage::Audio,
                    done: *done as u64,
                    total: *total as u64,
                }),
                AudioEvent::Stage(_) => {}
            }
            emit(ExportEvent::Audio(e));
        },
    )?;
    // A finished job loaded from its cache reports its warnings only in the result.
    let warned = warned.into_inner().unwrap();
    for w in result.warnings.iter().filter(|w| !warned.contains(w)) {
        emit(ExportEvent::Audio(AudioEvent::Warning(w.clone())));
    }
    check(cancel)?;

    let output = spec.output.with_extension("mkv");
    if let Some(d) = output.parent() {
        std::fs::create_dir_all(d)?;
    }
    let (start, duration, _) = audio_job::range(info, spec.start, spec.end);
    ffmpeg::mux(
        tools,
        &video,
        &result.tracks,
        Some((info.path.clone(), start, duration)),
        &output,
        cancel,
    )?;
    // Earlier audio jobs of this video (other settings or ranges) and month-old ones are
    // not needed any more; this one stays so a re-export reuses it.
    for d in audio_job::prune_after(&audio_spec) {
        log::info!("removed old audio job {}", d.display());
    }
    Ok(finish(output))
}

const REVIEW_FILE: &str = "review.json";
const REJECTS_DIR: &str = "rejects";
pub const REJECTED_DIR: &str = "rejected";

/// An export job found in a work folder.
#[derive(Clone, Debug, PartialEq)]
pub struct JobSummary {
    pub dir: PathBuf,
    /// "frames: a..b" from the job's description (source frame numbers).
    pub range: (u64, u64),
    /// Frames the job works on (after "every 2nd/3rd").
    pub total: u64,
    pub modified: std::time::SystemTime,
}

/// The export jobs in `work_dir`, newest first.
pub fn list_jobs(work_dir: &Path) -> Vec<JobSummary> {
    let Ok(rd) = std::fs::read_dir(work_dir.join("jobs")) else {
        return Vec::new();
    };
    let mut jobs: Vec<JobSummary> = rd
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            let total = std::fs::read_to_string(dir.join("extracted"))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            let desc = std::fs::read_to_string(dir.join("job.txt")).unwrap_or_default();
            let range = desc
                .lines()
                .find_map(|l| l.strip_prefix("frames: "))
                .and_then(|r| r.split_once(".."))
                .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
                .unwrap_or((0, total));
            let modified = std::fs::metadata(dir.join(REVIEW_FILE))
                .or_else(|_| std::fs::metadata(dir.join("extracted")))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some(JobSummary {
                dir,
                range,
                total,
                modified,
            })
        })
        .collect();
    jobs.sort_by_key(|j| std::cmp::Reverse(j.modified));
    jobs
}

/// One frame of a job as the frame browser shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameStatus {
    pub n: u64,
    /// The finished frame exists.
    pub done: bool,
    pub record: Option<Record>,
    /// Rejected by hand and waiting for the export to make it again.
    pub pending_reject: bool,
}

/// Every frame of the job in `dir` with what is known about it.
pub fn job_frames(dir: &Path, total: u64) -> Vec<FrameStatus> {
    let records: std::collections::BTreeMap<u64, Record> =
        std::fs::read_to_string(dir.join(REVIEW_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
    let names = |sub: &str| -> std::collections::HashSet<String> {
        std::fs::read_dir(dir.join(sub))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    let out = names(OUTPUT_DIR);
    let rejects = names(REJECTS_DIR);
    (1..=total)
        .map(|n| FrameStatus {
            n,
            done: out.contains(&format!("{n:06}.png")),
            record: records.get(&n).cloned(),
            pending_reject: rejects.contains(&format!("{n}.txt")),
        })
        .collect()
}

/// The finished and the source frame `n` of the job in `dir`.
pub fn job_frame_files(dir: &Path, n: u64) -> (PathBuf, PathBuf) {
    (
        ffmpeg::frame_file(&dir.join(INPUT_DIR), n),
        ffmpeg::frame_file(&dir.join(OUTPUT_DIR), n),
    )
}

/// Rejects frame `n` by hand: the export (running now, or the next time it is started)
/// makes it again with `note` as the reason, and quality control looks at it again.
pub fn reject_frame(dir: &Path, n: u64, note: &str) -> Result<()> {
    let inbox = dir.join(REJECTS_DIR);
    std::fs::create_dir_all(&inbox)?;
    let tmp = inbox.join(format!("{n}.tmp"));
    std::fs::write(&tmp, note.trim())?;
    std::fs::rename(&tmp, inbox.join(format!("{n}.txt")))?;
    Ok(())
}

/// Takes back a hand rejection that the export has not acted on yet.
pub fn undo_reject(dir: &Path, n: u64) -> bool {
    std::fs::remove_file(dir.join(REJECTS_DIR).join(format!("{n}.txt"))).is_ok()
}

/// The frames of one export and where they live.
struct Plan<'a> {
    total: u64,
    /// Frames that reuse an earlier frame's result (index 1..=total).
    dup_of: &'a [Option<u64>],
    /// Frames that start a new shot.
    cut: &'a [bool],
    src_dir: &'a Path,
    out_dir: &'a Path,
    review_file: &'a Path,
    /// Frames rejected by hand, one `<n>.txt` (with the reason) per frame.
    rejects_dir: &'a Path,
    /// Where rejected results are kept (`<n>_try<k>.png` + `.json`).
    rejected_dir: &'a Path,
    target: (u32, u32),
}

impl Plan<'_> {
    fn base(&self, n: u64) -> u64 {
        self.dup_of[n as usize].unwrap_or(n)
    }

    /// The frame before `n` in the same shot (whose result is `n`'s reference).
    fn previous_of(&self, n: u64) -> Option<u64> {
        (n > 1 && !self.cut[n as usize]).then(|| self.base(n - 1))
    }

    fn out(&self, n: u64) -> PathBuf {
        ffmpeg::frame_file(self.out_dir, n)
    }

    fn src(&self, n: u64) -> PathBuf {
        ffmpeg::frame_file(self.src_dir, n)
    }
}

/// Quality control's word on one frame, saved so a resumed export does not ask again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    Approved,
    /// Rejected; the frame is made again.
    Rejected,
    /// Rejected on the last allowed attempt and kept anyway.
    Kept,
    /// The review itself failed; the frame is kept unchecked.
    Unchecked,
}

impl ReviewOutcome {
    fn is_final(self) -> bool {
        self != ReviewOutcome::Rejected
    }
}

/// What is known about one frame of an export (saved in `review.json`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Record {
    /// Results made since the last manual rejection.
    pub attempts: u32,
    pub outcome: Option<ReviewOutcome>,
    /// Why it was last rejected or kept.
    pub problems: String,
    /// Times it was rejected, by quality control or by hand.
    pub rejections: u32,
    /// The last rejection was made by hand.
    pub manual: bool,
    /// Every rejection so far, oldest first.
    pub history: Vec<Rejection>,
}

/// Who rejected a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectedBy {
    Ai,
    User,
}

impl RejectedBy {
    fn id(self) -> &'static str {
        match self {
            RejectedBy::Ai => "ai",
            RejectedBy::User => "user",
        }
    }
}

/// One rejected result of a frame.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Rejection {
    /// The attempt (1-based) that made the result.
    pub attempt: u32,
    pub by: RejectedBy,
    pub problems: String,
    /// File name inside `rejected/`; `None` when the frame was kept in `out/`.
    pub file: Option<String>,
    /// RFC 3339, UTC.
    pub time: String,
}

/// Seconds since the epoch as an RFC 3339 UTC timestamp.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Moves the rejected result of frame `n` from `out/` to `rejected/` with a description
/// next to it. The result leaves `out/` even if keeping it fails. Returns the file name.
fn archive(
    plan: &Plan,
    n: u64,
    attempt: u32,
    by: RejectedBy,
    problems: &str,
    prompt: &str,
) -> Option<String> {
    let from = plan.out(n);
    if !from.is_file() {
        return None;
    }
    let result = (|| -> Result<String> {
        std::fs::create_dir_all(plan.rejected_dir)?;
        let base = format!("{n:06}_try{attempt}");
        let mut stem = base.clone();
        let mut i = 2;
        while plan.rejected_dir.join(format!("{stem}.png")).exists()
            || plan.rejected_dir.join(format!("{stem}.json")).exists()
        {
            stem = format!("{base}_{i}");
            i += 1;
        }
        let png = plan.rejected_dir.join(format!("{stem}.png"));
        if std::fs::rename(&from, &png).is_err() {
            std::fs::copy(&from, &png)?;
        }
        let meta = serde_json::json!({
            "frame": n,
            "attempt": attempt,
            "by": by.id(),
            "problems": problems,
            "prompt": prompt,
            "time": now_rfc3339(),
        });
        std::fs::write(
            plan.rejected_dir.join(format!("{stem}.json")),
            serde_json::to_string_pretty(&meta)?,
        )?;
        Ok(format!("{stem}.png"))
    })();
    let _ = std::fs::remove_file(&from);
    match result {
        Ok(name) => Some(name),
        Err(e) => {
            log::warn!("could not keep the rejected frame {n}: {e:#}");
            None
        }
    }
}

/// The earlier, rejected results of frame `n` with their records, oldest first.
pub fn rejected_versions(job_dir: &Path, n: u64) -> Vec<(PathBuf, Rejection)> {
    let records: std::collections::BTreeMap<u64, Record> =
        std::fs::read_to_string(job_dir.join(REVIEW_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
    records
        .get(&n)
        .into_iter()
        .flat_map(|r| r.history.iter())
        .filter_map(|h| {
            let path = job_dir.join(REJECTED_DIR).join(h.file.as_ref()?);
            path.is_file().then(|| (path, h.clone()))
        })
        .collect()
}

#[derive(Default)]
struct Slot {
    generating: bool,
    reviewing: bool,
    /// Has a result that needs no (further) review.
    settled: bool,
    /// Bumped whenever a new result is written, so a late verdict on an old one is ignored.
    version: u64,
    /// The prompt of the latest result, for the history if it is rejected.
    prompt: String,
}

struct Board {
    slots: Vec<Slot>,
    records: std::collections::BTreeMap<u64, Record>,
    /// Frames without a result, not being made.
    missing: BTreeSet<u64>,
    /// Frames with a result waiting for review.
    unreviewed: BTreeSet<u64>,
    /// Frames (counting their duplicates) that are finished, for the progress bar.
    settled_weight: u64,
    error: Option<anyhow::Error>,
    /// When the folder of hand-rejected frames was last read.
    inbox_checked: Option<Instant>,
}

impl Board {
    fn record(&mut self, n: u64) -> &mut Record {
        self.records.entry(n).or_default()
    }

    fn save(&self, path: &Path) {
        if let Ok(text) = serde_json::to_string(&self.records) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

/// Makes every missing frame and, when quality control is on, has each one reviewed once in
/// a background thread. Rejected frames are deleted and made again (with the reasons in the
/// prompt); the frame after one that was made again is reviewed again, since it was judged
/// (and, in order, made) against the old one.
fn upscale_all(
    plan: &Plan,
    s: &Settings,
    provider: &dyn provider::Provider,
    cancel: &AtomicBool,
    emit: &(dyn Fn(ExportEvent) + Sync),
) -> Result<()> {
    let total = plan.total;
    let review = s.review_frames && provider.can_review();
    let max_attempts = s.review_max_attempts.max(1);
    let mut weight = vec![0u64; total as usize + 1];
    for n in 1..=total {
        weight[plan.base(n) as usize] += 1;
    }
    let records: std::collections::BTreeMap<u64, Record> =
        std::fs::read_to_string(plan.review_file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
    let mut board = Board {
        slots: (0..=total).map(|_| Slot::default()).collect(),
        records,
        missing: BTreeSet::new(),
        unreviewed: BTreeSet::new(),
        settled_weight: 0,
        error: None,
        inbox_checked: None,
    };
    for n in 1..=total {
        if plan.dup_of[n as usize].is_some() {
            continue;
        }
        if plan.out(n).is_file() {
            let settled = !review
                || board
                    .records
                    .get(&n)
                    .and_then(|r| r.outcome)
                    .is_some_and(ReviewOutcome::is_final);
            if settled {
                board.slots[n as usize].settled = true;
                board.settled_weight += weight[n as usize];
            } else {
                board.unreviewed.insert(n);
            }
        } else {
            board.missing.insert(n);
        }
    }
    let board = Mutex::new(board);
    let progress = |b: &Board| {
        emit(ExportEvent::Progress {
            stage: Stage::Upscaling,
            done: b.settled_weight,
            total,
        })
    };
    progress(&board.lock().unwrap());

    let finished = |b: &Board| {
        b.missing.is_empty()
            && b.unreviewed.is_empty()
            && b.slots.iter().all(|x| !x.generating && !x.reviewing)
    };
    let stop = |b: &Board| b.error.is_some() || cancel.load(Ordering::Relaxed);
    let idle = || std::thread::sleep(std::time::Duration::from_millis(150));

    // Marks `n` as having a new result; the next frame of the shot must be judged again.
    let new_result = |b: &mut Board, n: u64| {
        let slot = &mut b.slots[n as usize];
        slot.version += 1;
        if slot.settled {
            slot.settled = false;
            b.settled_weight -= weight[n as usize];
        }
        if review {
            b.unreviewed.insert(n);
        } else {
            b.slots[n as usize].settled = true;
            b.settled_weight += weight[n as usize];
        }
        if let Some(m) = ((n + 1)..=total)
            .take_while(|m| plan.base(*m) == n || plan.previous_of(*m) == Some(n))
            .find(|m| plan.dup_of[*m as usize].is_none())
        {
            let next = &mut b.slots[m as usize];
            if review && next.settled && plan.out(m).is_file() {
                next.settled = false;
                b.settled_weight -= weight[m as usize];
                b.unreviewed.insert(m);
            }
        }
    };

    // Frames rejected by hand (in the frame browser) are dropped and made again.
    let take_rejects = |b: &mut Board| {
        if b.inbox_checked
            .is_some_and(|t| t.elapsed().as_millis() < 1000)
        {
            return;
        }
        b.inbox_checked = Some(Instant::now());
        let Ok(rd) = std::fs::read_dir(plan.rejects_dir) else {
            return;
        };
        let mut changed = false;
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("txt") {
                continue;
            }
            let Some(n) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| (1..=total).contains(n))
            else {
                continue;
            };
            let n = plan.base(n);
            if b.slots[n as usize].generating {
                continue; // picked up once the frame in progress is saved
            }
            let note = std::fs::read_to_string(&path).unwrap_or_default();
            let slot = &mut b.slots[n as usize];
            slot.version += 1; // a review in progress no longer counts
            if slot.settled {
                slot.settled = false;
                b.settled_weight -= weight[n as usize];
            }
            b.unreviewed.remove(&n);
            b.missing.insert(n);
            let problems = if note.trim().is_empty() {
                "rejected by the user".to_string()
            } else {
                format!("rejected by the user: {}", note.trim())
            };
            let had_result = plan.out(n).is_file();
            let attempt = b.record(n).attempts.max(1);
            let prompt = b.slots[n as usize].prompt.clone();
            let file = archive(plan, n, attempt, RejectedBy::User, &problems, &prompt);
            {
                let r = b.record(n);
                r.attempts = 0;
                r.outcome = Some(ReviewOutcome::Rejected);
                r.problems = problems.clone();
                r.rejections += 1;
                r.manual = true;
                if had_result {
                    r.history.push(Rejection {
                        attempt,
                        by: RejectedBy::User,
                        problems: problems.clone(),
                        file,
                        time: now_rfc3339(),
                    });
                }
            }
            let _ = std::fs::remove_file(&path);
            log::info!("frame {n} {problems}");
            emit(ExportEvent::Reviewed {
                n,
                outcome: ReviewOutcome::Rejected,
                problems,
                attempt: 0,
            });
            changed = true;
        }
        if changed {
            b.save(plan.review_file);
            progress(b);
        }
    };

    take_rejects(&mut board.lock().unwrap());
    let workers = if s.sequential() {
        1
    } else {
        s.parallel_requests.clamp(1, 16) as usize
    };
    // The reviewer stops only after the generators: they are the ones that pick up frames
    // rejected by hand, which may turn a finished job into an unfinished one.
    let generators_left = AtomicU64::new(workers as u64);

    let generator = || loop {
        let claimed = {
            let mut b = board.lock().unwrap();
            take_rejects(&mut b);
            if stop(&b) || finished(&b) {
                generators_left.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            let pick = b.missing.iter().copied().find(|&n| {
                !s.sequential()
                    || plan
                        .previous_of(n)
                        .is_none_or(|r| !b.slots[r as usize].generating && plan.out(r).is_file())
            });
            match pick {
                Some(n) => {
                    b.missing.remove(&n);
                    b.slots[n as usize].generating = true;
                    let reference = if s.sequential() {
                        plan.previous_of(n)
                            .and_then(|r| image::open(plan.out(r)).ok())
                            .map(|i| i.to_rgb8())
                    } else {
                        None
                    };
                    let rec = b.record(n).clone();
                    let feedback =
                        (rec.outcome == Some(ReviewOutcome::Rejected)).then_some(rec.problems);
                    Some((n, reference, feedback))
                }
                None => None,
            }
        };
        let Some((n, reference, feedback)) = claimed else {
            idle();
            continue;
        };
        let started = Instant::now();
        let result = make_frame(
            plan,
            s,
            provider,
            n,
            reference.as_ref(),
            feedback.as_deref(),
            cancel,
            emit,
        );
        let mut b = board.lock().unwrap();
        b.slots[n as usize].generating = false;
        match result {
            Ok(prompt) => {
                b.slots[n as usize].prompt = prompt;
                b.record(n).attempts += 1;
                new_result(&mut b, n);
                b.save(plan.review_file);
                emit(ExportEvent::FrameDone {
                    n,
                    path: plan.out(n),
                    seconds: Some(started.elapsed().as_secs_f32()),
                });
                progress(&b);
            }
            Err(e) => {
                b.missing.insert(n);
                if b.error.is_none() {
                    b.error = Some(e);
                }
                generators_left.fetch_sub(1, Ordering::SeqCst);
                return;
            }
        }
    };

    let reviewer = || loop {
        let claimed = {
            let mut b = board.lock().unwrap();
            if stop(&b) || (generators_left.load(Ordering::SeqCst) == 0 && b.unreviewed.is_empty())
            {
                return;
            }
            let pick = b.unreviewed.iter().copied().find(|&n| {
                !b.slots[n as usize].generating
                    && plan
                        .previous_of(n)
                        .is_none_or(|r| b.slots[r as usize].settled)
            });
            pick.map(|n| {
                b.unreviewed.remove(&n);
                let slot = &mut b.slots[n as usize];
                slot.reviewing = true;
                (n, slot.version)
            })
        };
        let Some((n, version)) = claimed else {
            idle();
            continue;
        };
        let verdict = review_frame(plan, s, provider, n, cancel);
        let mut b = board.lock().unwrap();
        b.slots[n as usize].reviewing = false;
        if b.slots[n as usize].version != version {
            continue; // made again meanwhile; the new result gets its own review
        }
        let attempts = b.record(n).attempts.max(1);
        let (outcome, problems) = match verdict {
            Ok(v) if v.approved => (ReviewOutcome::Approved, String::new()),
            Ok(v) if attempts >= max_attempts => (ReviewOutcome::Kept, v.summary()),
            Ok(v) => (ReviewOutcome::Rejected, v.summary()),
            Err(e) if process::is_cancelled(&e) => {
                b.unreviewed.insert(n);
                return;
            }
            Err(e) => (ReviewOutcome::Unchecked, format!("{e:#}")),
        };
        log::info!("review of frame {n} (attempt {attempts}): {outcome:?} {problems}");
        let rejected = outcome != ReviewOutcome::Approved && outcome != ReviewOutcome::Unchecked;
        // A rejected result is moved to the history; a kept one stays but is noted there.
        let entry = rejected.then(|| {
            let prompt = b.slots[n as usize].prompt.clone();
            let file = (outcome == ReviewOutcome::Rejected)
                .then(|| archive(plan, n, attempts, RejectedBy::Ai, &problems, &prompt))
                .flatten();
            Rejection {
                attempt: attempts,
                by: RejectedBy::Ai,
                problems: problems.clone(),
                file,
                time: now_rfc3339(),
            }
        });
        {
            let r = b.record(n);
            r.outcome = Some(outcome);
            r.problems = problems.clone();
            if rejected {
                r.rejections += 1;
                r.manual = false;
            }
            r.history.extend(entry);
        }
        b.save(plan.review_file);
        if outcome.is_final() {
            b.slots[n as usize].settled = true;
            b.settled_weight += weight[n as usize];
        } else {
            b.missing.insert(n);
        }
        emit(ExportEvent::Reviewed {
            n,
            outcome,
            problems,
            attempt: attempts,
        });
        progress(&b);
    };

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(generator);
        }
        if review {
            scope.spawn(reviewer);
        }
    });
    let mut b = board.into_inner().unwrap();
    if let Some(e) = b.error.take() {
        return Err(e);
    }
    check(cancel)?;
    // Duplicates get their frame's final result.
    for n in 1..=total {
        if let Some(of) = plan.dup_of[n as usize] {
            std::fs::copy(plan.out(of), plan.out(n))?;
        }
    }
    emit(ExportEvent::Progress {
        stage: Stage::Upscaling,
        done: total,
        total,
    });
    Ok(())
}

/// The feedback for an attempt: quality control's problems, plus a note when the previous
/// attempt left the grey extension canvas unfilled.
fn retry_feedback(feedback: Option<&str>, grey_left: bool) -> Option<String> {
    const GREY: &str = "the grey canvas areas were left unfilled; paint the full new scene there";
    let base = feedback.map(str::trim).filter(|f| !f.is_empty());
    match (base, grey_left) {
        (Some(f), true) => Some(format!("{}; {GREY}", f.trim_end_matches('.'))),
        (Some(f), false) => Some(f.to_string()),
        (None, true) => Some(GREY.to_string()),
        (None, false) => None,
    }
}

/// Makes frame `n` (retrying failed requests twice) and saves it.
#[allow(clippy::too_many_arguments)]
fn make_frame(
    plan: &Plan,
    s: &Settings,
    provider: &dyn provider::Provider,
    n: u64,
    reference: Option<&RgbImage>,
    feedback: Option<&str>,
    cancel: &AtomicBool,
    emit: &(dyn Fn(ExportEvent) + Sync),
) -> Result<String> {
    let src = image::open(plan.src(n))?.to_rgb8();
    let mut attempt = 0;
    let mut grey = false;
    loop {
        check(cancel)?;
        let tmp = temp_dir(s);
        let retry = retry_feedback(feedback, grey);
        let r = provider::upscale_frame_with_prompt(
            provider,
            s,
            &src,
            reference,
            retry.as_deref(),
            plan.target,
            &tmp,
            cancel,
        );
        match r {
            Ok((img, prompt)) => {
                let _ = std::fs::remove_dir_all(&tmp);
                save_atomic(&img, &plan.out(n))?;
                return Ok(prompt);
            }
            Err(e) if process::is_cancelled(&e) => return Err(e),
            Err(e) if attempt >= 2 && provider::grey_canvas_result(&e).is_some() => {
                // Out of retries and still grey: the scene may really be flat grey at the
                // edges. Keep the result and let quality control or the owner judge it.
                let kept = provider::grey_canvas_result(&e).expect("checked above");
                let _ = std::fs::remove_dir_all(&tmp);
                save_atomic(&kept.image, &plan.out(n))?;
                log::warn!("frame {n}: grey canvas may still be unfilled after 3 attempts; kept");
                emit(ExportEvent::Retry {
                    n,
                    error: format!(
                        "{e:#} (kept after 3 attempts; the grey canvas may still be unfilled)"
                    ),
                });
                return Ok(kept.prompt.clone());
            }
            Err(e) if attempt < 2 => {
                attempt += 1;
                grey = provider::is_grey_canvas_error(&e);
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
}

/// Quality control for frame `n`; a failing request is tried twice more.
fn review_frame(
    plan: &Plan,
    s: &Settings,
    provider: &dyn provider::Provider,
    n: u64,
    cancel: &AtomicBool,
) -> Result<crate::review::Verdict> {
    let source = image::open(plan.src(n))?.to_rgb8();
    let result = image::open(plan.out(n))?.to_rgb8();
    let previous = plan
        .previous_of(n)
        .and_then(|r| image::open(plan.out(r)).ok())
        .map(|i| i.to_rgb8());
    let extend = s.aspect_change().filter(|a| {
        (a.ratio() / (source.width() as f64 / source.height() as f64) - 1.0).abs() > 0.01
    });
    let result_area = extend.map(|_| frame::source_area(&result, source.dimensions()));
    let instruction = crate::review::instruction(&crate::review::Context {
        has_previous: previous.is_some(),
        extend: extend.map(|a| {
            (
                a.name(),
                a.ratio() > source.width() as f64 / source.height() as f64,
            )
        }),
        user_prompt: &s.prompt,
    });
    let ask = || -> Result<crate::review::Verdict> {
        let mut attempt = 0;
        loop {
            check(cancel)?;
            let tmp = temp_dir(s);
            let r = provider.review(
                &provider::ReviewRequest {
                    source: &source,
                    result: &result,
                    result_area: result_area.as_ref(),
                    previous: previous.as_ref(),
                    instruction: &instruction,
                    work_dir: &tmp,
                },
                cancel,
            );
            match r {
                Ok(v) => {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Ok(v);
                }
                Err(e) if process::is_cancelled(&e) => return Err(e),
                Err(e) if attempt < 2 => {
                    attempt += 1;
                    log::warn!("review of frame {n} failed (attempt {attempt}): {e:#}");
                    std::thread::sleep(std::time::Duration::from_secs(3 * attempt));
                }
                Err(e) => return Err(e.context(format!("kept: {}", tmp.display()))),
            }
        }
    };
    // Single reviews are noisy; a rejection needs a second, independent opinion. That costs
    // one more look, which is far cheaper than making the frame again for nothing.
    let first = ask()?;
    if first.approved {
        return Ok(first);
    }
    let second = ask()?;
    if second.approved {
        log::info!("frame {n}: rejection not confirmed ({})", first.summary());
    }
    Ok(crate::review::confirm(first, second))
}

/// Empties the work folder. With `everything`: previews, export jobs, audio jobs (with the
/// stem cache) and temp files. Otherwise only what can be made again cheaply: the temp files
/// and the audio jobs' big intermediates (extracted source, sections, temp files), keeping
/// finished audio tracks and the stem cache, which costs an AI separation to make again.
pub fn clean_work_dir(settings: &Settings, everything: bool) -> Result<()> {
    let w = settings.work_dir();
    if everything {
        for sub in ["previews", "jobs", "audio", "tmp"] {
            let _ = std::fs::remove_dir_all(w.join(sub));
        }
    } else {
        let _ = std::fs::remove_dir_all(w.join("tmp"));
        audio_job::clean_intermediates(&w);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Provider, Request, ReviewRequest};
    use crate::review::Verdict;
    use image::Rgb;

    #[test]
    fn grey_canvas_retry_feedback_is_combined() {
        let grey = anyhow::Error::new(provider::GreyCanvasLeft {
            image: RgbImage::new(1, 1),
            prompt: String::new(),
        });
        assert!(provider::is_grey_canvas_error(&grey));
        assert!(!provider::is_grey_canvas_error(&anyhow::anyhow!("other")));
        assert_eq!(
            retry_feedback(Some("a face"), false).as_deref(),
            Some("a face")
        );
        assert_eq!(retry_feedback(None, false), None);
        let both = retry_feedback(Some("a face"), true).unwrap();
        assert!(
            both.starts_with("a face") && both.contains("grey canvas areas were left unfilled")
        );
        assert!(retry_feedback(None, true)
            .unwrap()
            .contains("paint the full new scene"));
    }

    /// Always answers with flat grey, as a scene with a really grey edge would look.
    struct AlwaysGrey {
        calls: Mutex<u32>,
    }

    impl Provider for AlwaysGrey {
        fn upscale(&self, req: &Request, _: &AtomicBool) -> Result<RgbImage> {
            *self.calls.lock().unwrap() += 1;
            let (w, h) = req.image.dimensions();
            Ok(RgbImage::from_pixel(w, h, frame::EXTEND_FILL))
        }

        fn canvas_aspects(&self) -> &'static [f64] {
            &[]
        }
    }

    #[test]
    fn grey_result_is_kept_after_three_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let (src_dir, out_dir) = (dir.path().join("in"), dir.path().join("out"));
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();
        RgbImage::from_pixel(160, 120, Rgb([200, 30, 30]))
            .save(ffmpeg::frame_file(&src_dir, 1))
            .unwrap();
        let (review_file, rejects) = (dir.path().join(REVIEW_FILE), dir.path().join(REJECTS_DIR));
        let rejected = dir.path().join(REJECTED_DIR);
        let plan = Plan {
            total: 1,
            dup_of: &[None, None],
            cut: &[false, true],
            src_dir: &src_dir,
            out_dir: &out_dir,
            review_file: &review_file,
            rejects_dir: &rejects,
            rejected_dir: &rejected,
            target: (320, 180),
        };
        let mut s = Settings::default();
        s.project_dir = Some(dir.path().to_path_buf());
        s.change_aspect = true;
        s.target_aspect = crate::settings::AspectChoice::Wide16x9;
        let fake = AlwaysGrey {
            calls: Mutex::new(0),
        };
        let retries = Mutex::new(Vec::new());
        let prompt = make_frame(
            &plan,
            &s,
            &fake,
            1,
            None,
            None,
            &AtomicBool::new(false),
            &|e| {
                if let ExportEvent::Retry { error, .. } = e {
                    retries.lock().unwrap().push(error);
                }
            },
        )
        .unwrap();
        assert!(prompt.contains("EXTENSION TASK"));
        assert_eq!(*fake.calls.lock().unwrap(), 3);
        assert!(plan.out(1).is_file());
        let retries = retries.into_inner().unwrap();
        assert_eq!(retries.len(), 3);
        assert!(retries[2].contains("may still be unfilled"));
    }

    /// Paints results grey, or white when the prompt carries quality-control feedback, and
    /// rejects the first (grey) result of the frame whose source is 30.
    struct Fake {
        log: Mutex<Vec<String>>,
    }

    impl Provider for Fake {
        fn upscale(&self, req: &Request, _: &AtomicBool) -> Result<RgbImage> {
            let id = req.image.get_pixel(0, 0).0[0];
            let retry = req
                .prompt
                .contains("rejected by quality control because: face changed");
            self.log.lock().unwrap().push(format!(
                "make {id} ref={} retry={retry} user={}",
                req.reference.is_some(),
                req.prompt.contains("rejected by the user: too dark")
            ));
            let v = if retry { 255 } else { 128 };
            Ok(RgbImage::from_pixel(8, 6, Rgb([v, v, id])))
        }

        fn canvas_aspects(&self) -> &'static [f64] {
            &[]
        }

        fn can_review(&self) -> bool {
            true
        }

        fn review(&self, req: &ReviewRequest, _: &AtomicBool) -> Result<Verdict> {
            let id = req.source.get_pixel(0, 0).0[0];
            let grey = req.result.get_pixel(0, 0).0[0] == 128;
            self.log
                .lock()
                .unwrap()
                .push(format!("review {id} prev={}", req.previous.is_some()));
            Ok(if id == 30 && grey {
                Verdict {
                    approved: false,
                    problems: vec!["face changed".into()],
                }
            } else {
                Verdict {
                    approved: true,
                    problems: vec![],
                }
            })
        }
    }

    #[test]
    fn rejected_frames_are_made_again_and_the_next_one_rechecked() {
        let dir = tempfile::tempdir().unwrap();
        let (src_dir, out_dir) = (dir.path().join("in"), dir.path().join("out"));
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();
        let total = 6u64;
        for n in 1..=total {
            let id = (n.min(5) * 10) as u8; // frame 6 is a copy of frame 5
            RgbImage::from_pixel(4, 3, Rgb([id, id, id]))
                .save(ffmpeg::frame_file(&src_dir, n))
                .unwrap();
        }
        let mut dup_of = vec![None; total as usize + 1];
        dup_of[6] = Some(5);
        let mut cut = vec![false; total as usize + 1];
        cut[1] = true;
        let review_file = dir.path().join(REVIEW_FILE);
        let rejects_dir = dir.path().join(REJECTS_DIR);
        let plan = Plan {
            total,
            dup_of: &dup_of,
            cut: &cut,
            src_dir: &src_dir,
            out_dir: &out_dir,
            review_file: &review_file,
            rejects_dir: &rejects_dir,
            rejected_dir: &dir.path().join(REJECTED_DIR),
            target: (8, 6),
        };
        let mut s = Settings::default();
        s.project_dir = Some(dir.path().to_path_buf());
        let fake = Fake {
            log: Mutex::new(Vec::new()),
        };
        let events = Mutex::new(Vec::new());
        upscale_all(&plan, &s, &fake, &AtomicBool::new(false), &|e| {
            if let ExportEvent::Reviewed { n, outcome, .. } = e {
                events.lock().unwrap().push((n, outcome));
            }
        })
        .unwrap();

        let log = fake.log.into_inner().unwrap();
        let makes: Vec<&String> = log.iter().filter(|l| l.starts_with("make 30")).collect();
        assert_eq!(makes.len(), 2, "{log:#?}");
        assert!(makes[1].contains("retry=true") && makes[1].contains("ref=true"));
        assert!(log.iter().any(|l| l == "review 10 prev=false"));
        let reviews_of_4 = log.iter().filter(|l| l.starts_with("review 40")).count();
        assert!(reviews_of_4 >= 1, "{log:#?}");
        let ev = events.into_inner().unwrap();
        assert!(ev.contains(&(3, ReviewOutcome::Rejected)));
        assert!(ev.contains(&(3, ReviewOutcome::Approved)));
        // Final frame 3 is the white retry; frame 6 copies frame 5.
        let f3 = image::open(ffmpeg::frame_file(&out_dir, 3))
            .unwrap()
            .to_rgb8();
        assert_eq!(f3.get_pixel(0, 0).0[0], 255);
        let f6 = image::open(ffmpeg::frame_file(&out_dir, 6))
            .unwrap()
            .to_rgb8();
        assert_eq!(f6.get_pixel(0, 0).0[2], 50);
        let saved = std::fs::read_to_string(&review_file).unwrap();
        assert!(saved.contains("approved"));

        // A second run finds everything approved and asks nobody.
        let again = Fake {
            log: Mutex::new(Vec::new()),
        };
        upscale_all(&plan, &s, &again, &AtomicBool::new(false), &|_| {}).unwrap();
        assert!(again.log.into_inner().unwrap().is_empty());

        // Rejected by hand: made again with the note, checked again, and so is the next one.
        reject_frame(dir.path(), 2, "too dark").unwrap();
        let st = job_frames(dir.path(), total);
        assert!(st[1].pending_reject && st[1].done);
        let manual = Fake {
            log: Mutex::new(Vec::new()),
        };
        upscale_all(&plan, &s, &manual, &AtomicBool::new(false), &|_| {}).unwrap();
        let log = manual.log.into_inner().unwrap();
        assert!(
            log.iter()
                .any(|l| l.starts_with("make 20") && l.ends_with("user=true")),
            "{log:#?}"
        );
        assert!(log.iter().any(|l| l.starts_with("review 20")));
        assert!(log.iter().any(|l| l.starts_with("review 30")), "{log:#?}");
        let st = job_frames(dir.path(), total);
        let r2 = st[1].record.clone().unwrap();
        assert!(!st[1].pending_reject && st[1].done);
        assert_eq!(r2.outcome, Some(ReviewOutcome::Approved));
        assert_eq!(r2.rejections, 1);
        assert!(r2.manual);
        assert_eq!(st[2].record.as_ref().unwrap().rejections, 1); // frame 3 from the AI

        // Rejected results are kept with their history.
        let rej = dir.path().join(REJECTED_DIR);
        assert!(rej.join("000003_try1.png").is_file());
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(rej.join("000003_try1.json")).unwrap())
                .unwrap();
        assert_eq!(meta["frame"], 3);
        assert_eq!(meta["attempt"], 1);
        assert_eq!(meta["by"], "ai");
        assert!(meta["problems"].as_str().unwrap().contains("face changed"));
        assert!(!meta["prompt"].as_str().unwrap().is_empty());
        assert!(meta["time"].as_str().unwrap().ends_with('Z'));
        let kept3 = image::open(rej.join("000003_try1.png")).unwrap().to_rgb8();
        assert_eq!(kept3.get_pixel(0, 0).0[0], 128); // the grey, rejected result
        assert!(rej.join("000002_try1.png").is_file());
        let h3 = &st[2].record.as_ref().unwrap().history;
        assert_eq!(h3.len(), 1);
        assert_eq!(h3[0].by, RejectedBy::Ai);
        assert_eq!(h3[0].attempt, 1);
        assert_eq!(h3[0].file.as_deref(), Some("000003_try1.png"));
        let h2 = &r2.history;
        assert_eq!(h2.len(), 1);
        assert_eq!(h2[0].by, RejectedBy::User);
        assert_eq!(h2[0].file.as_deref(), Some("000002_try1.png"));
        let v3 = rejected_versions(dir.path(), 3);
        assert_eq!(v3.len(), 1);
        assert_eq!(v3[0].0, rej.join("000003_try1.png"));
        assert_eq!(rejected_versions(dir.path(), 2).len(), 1);
        assert!(rejected_versions(dir.path(), 1).is_empty());

        // A second hand rejection of the same attempt number does not overwrite the first.
        reject_frame(dir.path(), 2, "too dark").unwrap();
        let third = Fake {
            log: Mutex::new(Vec::new()),
        };
        upscale_all(&plan, &s, &third, &AtomicBool::new(false), &|_| {}).unwrap();
        assert!(rej.join("000002_try1_2.png").is_file());
        assert_eq!(rejected_versions(dir.path(), 2).len(), 2);
    }

    #[test]
    fn old_review_files_without_history_still_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(REVIEW_FILE),
            r#"{"1":{"attempts":2,"outcome":"approved","problems":"","rejections":1,"manual":false}}"#,
        )
        .unwrap();
        let st = job_frames(dir.path(), 1);
        let r = st[0].record.clone().unwrap();
        assert_eq!(r.attempts, 2);
        assert!(r.history.is_empty());
    }

    #[test]
    fn clean_work_dir_handles_audio() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            project_dir: Some(dir.path().to_path_buf()),
            ..Settings::default()
        };
        let w = settings.work_dir();
        let ajob = w.join("audio").join("key");
        let stems = w
            .join("audio")
            .join(audio_job::STEMS_DIR)
            .join("k")
            .join("000");
        for d in [
            w.join("previews"),
            w.join("jobs").join("j"),
            w.join("tmp").join("t"),
            ajob.join("sections"),
            stems.clone(),
        ] {
            std::fs::create_dir_all(&d).unwrap();
        }
        std::fs::write(ajob.join("source.f32"), b"x").unwrap();
        std::fs::write(ajob.join("surround.flac"), b"x").unwrap();
        std::fs::write(stems.join("music.wav"), b"x").unwrap();

        // Light: temp files and audio intermediates go; tracks, stems and jobs stay.
        clean_work_dir(&settings, false).unwrap();
        assert!(!w.join("tmp").exists());
        assert!(!ajob.join("source.f32").exists() && !ajob.join("sections").exists());
        assert!(ajob.join("surround.flac").is_file() && stems.join("music.wav").is_file());
        assert!(w.join("jobs").join("j").is_dir() && w.join("previews").is_dir());

        // Everything: the audio folder goes too.
        clean_work_dir(&settings, true).unwrap();
        for sub in ["previews", "jobs", "audio", "tmp"] {
            assert!(!w.join(sub).exists(), "{sub}");
        }
    }
}
