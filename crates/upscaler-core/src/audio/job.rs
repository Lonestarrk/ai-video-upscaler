//! The audio job: turns the export range's audio into the tracks the export muxes next to
//! the untouched original — FLAC 5.1 + AC-3 5.1, or FLAC stereo when only the quality is
//! improved — in resumable sections.
//!
//! Flow:
//! 1. Decide what to make from the settings and the source ([`plan_job`]); impossible
//!    requests (mono to 5.1, local AI without the pack) fail here, before any work.
//! 2. Extract exactly the exported frames' audio ([`range`]) as headerless 48 kHz float.
//! 3. For each section not yet in `sections/NNN.wav` (complete files only; every write is
//!    `*.part` then rename): AI separation + placement, or the classic upmix, or the
//!    source's own channels; then the optional enhancement (FL/FR as a pair, FC alone,
//!    SL/SR as a pair, LFE never). The AI's stems are kept in a stem cache
//!    (`<work>/audio/stems/<key>/NNN/`) whose key leaves out the LFE level and the engine,
//!    so trying another LFE level places the same stems again instead of separating anew.
//! 4. Stitch the sections with the Hann crossfade (which also smooths the per-section
//!    enhancement gain steps) straight into the FLAC and, for 5.1, the AC-3 encoder.
//! 5. Checks that never fail the job: loudness against the source (±1 LU, else a static
//!    gain and a re-encode), true peak ≤ -1 dBTP (a static cut up to 1 dB, beyond that
//!    FFmpeg's `alimiter`), and sync (cross-correlation of the downmix against the source).
//!
//! The finished result is remembered in `result.json`; the big intermediates (the
//! extracted source and the sections) are removed once it is written.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use super::ai::{pack_missing_message, AudioAi, Stems};
use super::classic::upmix_classic;
use super::dsp::rms_db;
use super::enhance::enhance_pcm;
use super::io::{
    encode_stream_filtered, extract, extract_raw, frames_for, read_raw_frames, read_wav, write_wav,
    AudioCodec, Pcm, RATE,
};
use super::pack;
use super::place::{place, DOWNMIX_FC};
use super::sections::{self, Section};
use super::source::{self, SourceAudio};
use crate::cache;
use crate::ffmpeg::{Tools, VideoInfo};
use crate::process::{self, is_cancelled, Cancelled};
use crate::settings::{AudioMode, LfeLevel, Settings, SurroundEngine};

/// Bumped whenever the job's output for the same inputs changes; part of the cache key.
pub const AUDIO_VERSION: u32 = 6;
/// Bumped whenever the separated stems for the same input change (how the range is
/// extracted or cut into sections, or what the worker is asked); part of the stem cache key.
pub const SEPARATION_VERSION: u32 = 1;
/// The stem cache's folder in `<work>/audio`.
pub const STEMS_DIR: &str = "stems";
/// Frames of silence the AC-3 encoder puts in front of the signal (one block).
const AC3_PRIMING: usize = 256;
/// Channel layout of every 5.1 track: FL FR FC LFE SL SR.
pub const LAYOUT_51: &str = "5.1(side)";
/// AC-3 dialogue normalisation when the source has none (the AC-3 default).
pub const DEFAULT_DIALNORM: i8 = -27;
/// FFmpeg's look-ahead limiter for true-peak control: a sample-peak ceiling of -1.5 dBFS
/// (0.5 dB of room for inter-sample peaks under -1 dBTP), no auto-levelling (`level=0`),
/// and its look-ahead delay compensated (`latency=1`) so sync and length are kept.
pub const LIMITER: &str = "alimiter=limit=0.8414:level=0:latency=1:attack=5:release=50";

const LOUDNESS_TOLERANCE_LU: f64 = 1.0;
const TRUE_PEAK_MAX_DBTP: f64 = -1.0;
/// Inter-sample allowance added to the f32 sample peak when the first encode clipped.
const CLIPPED_PEAK_MARGIN_DB: f64 = 0.5;
/// Most the loudness may drop for a static peak cut; beyond this the limiter is used.
const MAX_PEAK_CUT_DB: f64 = 1.0;
/// Integrated loudness at or under this is silence: no loudness correction.
const SILENT_LUFS: f64 = -69.0;
/// Corrections smaller than this are not worth a re-encode.
const MIN_GAIN_DB: f64 = 0.05;
/// Sync check: a 2 s window, lags searched within ±5 ms, ±1 sample allowed.
const SYNC_WINDOW: u64 = 2 * RATE as u64;
const SYNC_MAX_LAG: u64 = 240;
const SYNC_TOLERANCE: i64 = 1;
/// Frames per write into the encoders (1 s).
const CHUNK_FRAMES: usize = RATE as usize;

/// One export range's audio job.
#[derive(Clone, Debug)]
pub struct AudioSpec {
    pub info: VideoInfo,
    /// First exported frame.
    pub start: u64,
    /// One past the last exported frame.
    pub end: u64,
    pub settings: Settings,
}

/// The file's identity for the audio: path, size and modification time. Unlike
/// [`VideoInfo::identity`] it leaves out the crop and the SAR override, which change only
/// the pixels, so they never throw away a finished audio job.
fn audio_identity(info: &VideoInfo) -> String {
    format!(
        "{}|{}|{}",
        info.path.display(),
        info.file_size,
        info.modified
    )
}

/// Whether these settings may use the AI audio pack (separation or enhancement).
pub fn may_use_ai(s: &Settings) -> bool {
    s.audio_enhance
        || (s.audio_mode == AudioMode::Surround && s.surround_engine == SurroundEngine::LocalAi)
}

/// The cache key: the video's audio identity, the range, the audio settings that shape this
/// result, [`AUDIO_VERSION`] and, when the AI may be used, the AI pack's identity. The engine
/// and the LFE level only count when a 5.1 is made, so they never split a stereo job.
pub fn key(spec: &AudioSpec) -> String {
    key_with(spec, &pack::identity())
}

fn key_with(spec: &AudioSpec, pack_identity: &str) -> String {
    let s = &spec.settings;
    let mut parts = vec![
        audio_identity(&spec.info),
        spec.start.to_string(),
        spec.end.to_string(),
        format!("{:?}", s.audio_mode),
    ];
    if s.audio_mode == AudioMode::Surround {
        parts.push(format!("{:?}", s.surround_engine));
        parts.push(format!("{:?}", s.lfe_level));
    }
    parts.push(format!("enhance={}", s.audio_enhance));
    parts.push(format!("audio-v{AUDIO_VERSION}"));
    if may_use_ai(s) {
        parts.push(pack_identity.to_string());
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    cache::key(&refs)
}

/// `<work dir>/audio`: the audio jobs and, in [`STEMS_DIR`], the stem cache.
fn audio_root(settings: &Settings) -> PathBuf {
    settings.work_dir().join("audio")
}

/// `<work dir>/audio/<key>`.
pub fn job_dir(spec: &AudioSpec) -> PathBuf {
    audio_root(&spec.settings).join(key(spec))
}

/// The stem cache key: what the separated stems depend on, and nothing else (not the LFE
/// level, the engine or the enhancement): the audio identity, the range, how it is cut into
/// sections, the AI pack's identity and [`SEPARATION_VERSION`].
fn stems_key(spec: &AudioSpec, opts: &JobOptions, pack_identity: &str) -> String {
    let (_, _, n) = range(&spec.info, spec.start, spec.end);
    cache::key(&[
        &audio_identity(&spec.info),
        &spec.start.to_string(),
        &spec.end.to_string(),
        &format!(
            "sections {n} {} {}",
            opts.section_frames, opts.overlap_frames
        ),
        pack_identity,
        &format!("separation-v{SEPARATION_VERSION}"),
    ])
}

/// `<work dir>/audio/stems/<stem key>`: one folder per section (`NNN/`) with the three stems.
pub fn stems_dir(spec: &AudioSpec, opts: &JobOptions) -> PathBuf {
    stems_dir_with(spec, opts, &pack::identity())
}

fn stems_dir_with(spec: &AudioSpec, opts: &JobOptions, pack_identity: &str) -> PathBuf {
    audio_root(&spec.settings)
        .join(STEMS_DIR)
        .join(stems_key(spec, opts, pack_identity))
}

/// In each job and stem folder: which video and range it was made from (see [`marker`]), so
/// [`prune`] can tell a superseded folder from one another range still needs.
const VIDEO_MARKER: &str = "video.txt";
/// Audio job folders not touched for this long are removed by [`prune_after`].
pub const JOB_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// The marker text: the video's path, its audio identity and the range, one per line.
fn marker(spec: &AudioSpec) -> String {
    format!(
        "{}\n{}\n{} {}",
        spec.info.path.display(),
        audio_identity(&spec.info),
        spec.start,
        spec.end
    )
}

/// Whether a folder marked `old` is superseded by the job marked `current`: the same video
/// and either the same range (other settings) or a changed file (other identity). Another
/// range of the same unchanged video is kept, so a short sample never throws away the full
/// episode. A marker from before the range was recorded counts as superseded.
fn superseded(old: &str, current: &str) -> bool {
    let old: Vec<&str> = old.lines().map(str::trim).collect();
    let cur: Vec<&str> = current.lines().map(str::trim).collect();
    if old.first() != cur.first() {
        return false;
    }
    match (old.get(1), old.get(2)) {
        (Some(identity), Some(range)) => cur.get(1) != Some(identity) || cur.get(2) == Some(range),
        _ => true,
    }
}

/// Removes the folders under `root` except `keep` and the stem cache: those [`superseded`]
/// by `current` (the marker of `keep`'s job) and those whose newest entry is older than
/// `max_age` at `now`. Best effort; returns what it removed.
pub fn prune(
    root: &Path,
    keep: &Path,
    current: &str,
    max_age: Duration,
    now: SystemTime,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for e in entries.flatten() {
        let dir = e.path();
        if !e.file_type().is_ok_and(|t| t.is_dir()) || dir == keep || e.file_name() == STEMS_DIR {
            continue;
        }
        let old = std::fs::read_to_string(dir.join(VIDEO_MARKER)).ok();
        let replaced = old.is_some_and(|m| superseded(&m, current));
        let stale = newest_mtime(&dir)
            .is_some_and(|t| now.duration_since(t).is_ok_and(|age| age > max_age));
        if (replaced || stale) && std::fs::remove_dir_all(&dir).is_ok() {
            removed.push(dir);
        }
    }
    removed
}

/// After `spec`'s export finished: [`prune`] the audio jobs and the stem cache with
/// [`JOB_MAX_AGE`], keeping this job and the stems of its range (whatever engine it used, so
/// switching back to the AI later separates nothing again).
pub fn prune_after(spec: &AudioSpec) -> Vec<PathBuf> {
    let current = marker(spec);
    let now = SystemTime::now();
    let root = audio_root(&spec.settings);
    let mut removed = prune(&root, &job_dir(spec), &current, JOB_MAX_AGE, now);
    removed.extend(prune(
        &root.join(STEMS_DIR),
        &stems_dir(spec, &JobOptions::default()),
        &current,
        JOB_MAX_AGE,
        now,
    ));
    removed
}

/// Removes the big intermediates (extracted source, sections, temp files) of every audio
/// job under `work_dir`, keeping the finished tracks and the stem cache.
pub fn clean_intermediates(work_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(work_dir.join("audio")) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name() != STEMS_DIR && e.file_type().is_ok_and(|t| t.is_dir()) {
            remove_intermediates(&e.path());
        }
    }
}

/// The newest modification time of `dir` and its direct entries.
fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let own = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().and_then(|m| m.modified()).ok())
        .chain(own)
        .max()
}

/// A finished track. The variant names avoid underscores between digits (`Ac3_51`) to keep
/// to the Rust naming lint, as `AudioCodec::Ac3At640` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioTrack {
    /// FLAC 24 bit, 5.1(side).
    Flac51(PathBuf),
    /// AC-3 640 kbit/s, 5.1(side), made from the same samples as the FLAC. AC-3 codes whole
    /// 1536-sample frames plus encoder delay, so the decoded track can be up to 1792 samples
    /// longer than the range; the mux must cut it to the video's length.
    Ac351(PathBuf),
    /// FLAC 24 bit, stereo (quality improvement without 5.1).
    FlacStereo(PathBuf),
}

impl AudioTrack {
    pub fn path(&self) -> &Path {
        match self {
            AudioTrack::Flac51(p) | AudioTrack::Ac351(p) | AudioTrack::FlacStereo(p) => p,
        }
    }

    fn tag(&self) -> &'static str {
        match self {
            AudioTrack::Flac51(_) => "flac51",
            AudioTrack::Ac351(_) => "ac351",
            AudioTrack::FlacStereo(_) => "flac-stereo",
        }
    }

    fn from_tag(tag: &str, path: PathBuf) -> Option<AudioTrack> {
        match tag {
            "flac51" => Some(AudioTrack::Flac51(path)),
            "ac351" => Some(AudioTrack::Ac351(path)),
            "flac-stereo" => Some(AudioTrack::FlacStereo(path)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioResult {
    pub tracks: Vec<AudioTrack>,
    /// From the checks; the job still succeeded.
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioStage {
    Extracting,
    Separating,
    Enhancing,
    Placing,
    /// Stitching the sections straight into the encoders (the first encode).
    Joining,
    Checking,
    /// Encoding again with a loudness or peak correction (only when one is needed).
    Encoding,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AudioEvent {
    Stage(AudioStage),
    /// `done` of `total` sections are finished (resumed ones included).
    Section {
        done: usize,
        total: usize,
    },
    Warning(String),
}

/// Section length and overlap in frames. The default (300 s, 2 s) is what the app uses;
/// tests use shorter sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobOptions {
    pub section_frames: u64,
    pub overlap_frames: u64,
}

impl Default for JobOptions {
    fn default() -> Self {
        JobOptions {
            section_frames: sections::SECTION_SECONDS * RATE as u64,
            overlap_frames: sections::OVERLAP_SECONDS * RATE as u64,
        }
    }
}

/// The audio of exported frames `start..end`: start = `info.frame_time(start)`, length
/// `(end - start) * fps_den / fps_num` seconds, and `round(length * 48000)` samples.
pub fn range(info: &VideoInfo, start: u64, end: u64) -> (f64, f64, usize) {
    let t0 = info.frame_time(start);
    let dur = end.saturating_sub(start) as f64 * info.fps_den as f64 / info.fps_num.max(1) as f64;
    (t0, dur, frames_for(dur))
}

/// What the job makes of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Stereo to 5.1: AI separation, then placement.
    UpmixAi,
    /// Stereo to 5.1 with the classic filter.
    UpmixClassic,
    /// The source is already 5.1: its own channels, never upmixed.
    Keep51,
    /// Stereo stays stereo (enhancement only).
    Stereo,
    /// Mono, enhanced, written as stereo with both channels the same.
    MonoToStereo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobPlan {
    pub kind: Kind,
    pub enhance: bool,
    pub source_channels: usize,
    /// The source is a Dolby Surround matrixed stereo (AC-3 flag).
    pub dolby_surround: bool,
    /// For the AC-3 track: the source's, else [`DEFAULT_DIALNORM`].
    pub dialnorm: i8,
}

impl JobPlan {
    /// Channels of the job's result.
    pub fn channels(&self) -> usize {
        match self.kind {
            Kind::Stereo | Kind::MonoToStereo => 2,
            _ => 6,
        }
    }

    fn layout(&self) -> &'static str {
        if self.channels() == 6 {
            LAYOUT_51
        } else {
            "stereo"
        }
    }

    /// The job uses the AI audio pack (separation or enhancement).
    pub fn needs_ai(&self) -> bool {
        self.kind == Kind::UpmixAi || self.enhance
    }

    /// The "pack missing" message for what this plan uses the AI for.
    pub fn pack_missing(&self) -> &'static str {
        pack_missing_message(self.kind == Kind::UpmixAi, self.enhance)
    }
}

/// The export's warning when the pack runs on the CPU and the job separates.
pub const CPU_SLOW_SEPARATION: &str =
    "The local AI audio pack runs on the CPU: audio separation will be slow.";
/// The export's warning when the pack runs on the CPU and the job only enhances.
pub const CPU_SLOW_ENHANCE: &str =
    "The local AI audio pack runs on the CPU: the audio enhancement will be slow.";

impl JobPlan {
    /// The warning for this plan when the pack runs on the CPU.
    pub fn cpu_warning(&self) -> &'static str {
        if self.kind == Kind::UpmixAi {
            CPU_SLOW_SEPARATION
        } else {
            CPU_SLOW_ENHANCE
        }
    }
}

/// The refusal of [`plan_job`] for a 5.1 source with 5.1 chosen and no enhancement.
pub const ALREADY_51: &str =
    "the source is already 5.1 and the audio is not enhanced: the audio is kept as it is";

/// Whether these settings leave `source`'s audio as it is: no 5.1 to make (none chosen, or
/// the source is 5.1 already) and no enhancement. The export then copies the original audio
/// into an MP4, as with "Unchanged", instead of transcoding it to the same thing.
pub fn keeps_original(settings: &Settings, source: &SourceAudio) -> bool {
    !settings.audio_enhance && (settings.audio_mode != AudioMode::Surround || source.channels == 6)
}

/// Decides what the job makes, or why it cannot run; no work is done here.
///
/// The AI is required when it would be used: for AI separation, and for enhancement
/// (whichever engine). A 5.1 source with 5.1 chosen and no enhancement is nothing to do
/// ([`keeps_original`]).
pub fn plan_job(
    settings: &Settings,
    source: Option<&SourceAudio>,
    has_ai: bool,
) -> Result<JobPlan> {
    let Some(src) = source else {
        bail!("the video has no audio track");
    };
    let surround = settings.audio_mode == AudioMode::Surround;
    let ch = src.channels as usize;
    if surround && ch == 1 {
        bail!("mono source cannot be turned into 5.1");
    }
    if !surround && !settings.audio_enhance {
        bail!("the audio settings keep the original audio: there is no audio job to run");
    }
    if keeps_original(settings, src) {
        bail!(ALREADY_51);
    }
    let kind = match ch {
        6 => Kind::Keep51,
        2 if surround => match settings.surround_engine {
            SurroundEngine::LocalAi => Kind::UpmixAi,
            SurroundEngine::Classic => Kind::UpmixClassic,
        },
        2 => Kind::Stereo,
        1 => Kind::MonoToStereo,
        n => bail!("{n}-channel audio is not supported (mono, stereo and 5.1 are)"),
    };
    let plan = JobPlan {
        kind,
        enhance: settings.audio_enhance,
        source_channels: ch,
        dolby_surround: src.dolby_surround,
        dialnorm: src.dialnorm.unwrap_or(DEFAULT_DIALNORM).clamp(-31, -1),
    };
    if plan.needs_ai() && !has_ai {
        bail!(plan.pack_missing());
    }
    Ok(plan)
}

/// Runs the audio job for `spec` (see the module docs). `ai` is the opened AI audio pack;
/// it is needed for local-AI 5.1 and for enhancement.
pub fn run(
    tools: &Tools,
    spec: &AudioSpec,
    ai: Option<&dyn AudioAi>,
    cancel: &AtomicBool,
    emit: &(dyn Fn(AudioEvent) + Sync),
) -> Result<AudioResult> {
    run_with(tools, spec, ai, cancel, emit, &JobOptions::default())
}

/// [`run`] with explicit section sizes.
pub fn run_with(
    tools: &Tools,
    spec: &AudioSpec,
    ai: Option<&dyn AudioAi>,
    cancel: &AtomicBool,
    emit: &(dyn Fn(AudioEvent) + Sync),
    opts: &JobOptions,
) -> Result<AudioResult> {
    if spec.end <= spec.start {
        bail!("the export range is empty");
    }
    let src = source::analyse(tools, &spec.info)?;
    let plan = plan_job(&spec.settings, src.as_ref(), ai.is_some())?;
    let dir = job_dir(spec);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(VIDEO_MARKER), marker(spec))?;
    if let Some(done) = load_result(&dir) {
        // A crash right after the result was saved may have left the intermediates.
        remove_intermediates(&dir);
        return Ok(done);
    }
    let stems = if plan.kind == Kind::UpmixAi {
        let stems = stems_dir(spec, opts);
        std::fs::create_dir_all(&stems)?;
        std::fs::write(stems.join(VIDEO_MARKER), marker(spec))?;
        Some(stems)
    } else {
        None
    };
    let (t0, dur, n) = range(&spec.info, spec.start, spec.end);
    let total = n as u64;
    let job = Job {
        tools,
        plan,
        lfe: spec.settings.lfe_level,
        dir: dir.clone(),
        stems,
        total,
        opts: *opts,
        sections: sections::plan_with(total, opts.section_frames, opts.overlap_frames),
    };

    emit(AudioEvent::Stage(AudioStage::Extracting));
    let raw = job.source_path();
    let want = total * plan.source_channels as u64 * 4;
    if std::fs::metadata(&raw).map(|m| m.len()).ok() != Some(want) {
        extract_raw(
            tools,
            &spec.info.path,
            t0,
            dur,
            plan.source_channels,
            &raw,
            cancel,
        )?;
    }

    job.render(ai, cancel, emit)?;

    emit(AudioEvent::Stage(AudioStage::Joining));
    let (mut tracks, f32_peak) = job.encode(0.0, false, cancel)?;

    emit(AudioEvent::Stage(AudioStage::Checking));
    let mut warnings = Vec::new();
    let mut warn = |w: String| {
        emit(AudioEvent::Warning(w.clone()));
        warnings.push(w);
    };
    let first = tracks[0].path().to_path_buf();
    match job.check_levels(&first, cancel) {
        Ok((src_level, mut res_level)) => {
            // The FLAC clips at 0 dBFS, so its measured peak understates a hotter f32
            // result (FC is written +3 dB). Then the f32 sample peak, plus the same 0.5 dB
            // inter-sample allowance the limiter keeps, stands in for the true peak.
            if f32_peak >= 1.0 {
                let f32_peak_db = 20.0 * (f32_peak as f64).log10() + CLIPPED_PEAK_MARGIN_DB;
                res_level.true_peak = res_level.true_peak.max(f32_peak_db);
            }
            let (gain, limiter) = correction(&src_level, &res_level);
            if gain.abs() >= MIN_GAIN_DB || limiter {
                emit(AudioEvent::Stage(AudioStage::Encoding));
                tracks = job.encode(gain, limiter, cancel)?.0;
                emit(AudioEvent::Stage(AudioStage::Checking));
                match measure(tools, &file_input(tracks[0].path()), false, cancel) {
                    Ok(l) => res_level = l,
                    Err(e) if is_cancelled(&e) => return Err(e),
                    Err(e) => warn(format!("The loudness check could not run: {e:#}")),
                }
            }
            if src_level.integrated > SILENT_LUFS {
                let diff = res_level.integrated - src_level.integrated;
                if diff.abs() > LOUDNESS_TOLERANCE_LU + 0.05 {
                    warn(format!(
                        "The new audio's loudness differs from the source by {diff:+.1} LU."
                    ));
                }
            }
            if res_level.true_peak > TRUE_PEAK_MAX_DBTP + 0.05 {
                warn(format!(
                    "The new audio peaks at {:.1} dBTP, above -1 dBTP.",
                    res_level.true_peak
                ));
            }
        }
        Err(e) if is_cancelled(&e) => return Err(e),
        Err(e) => warn(format!("The loudness check could not run: {e:#}")),
    }
    match job.sync_lag(tracks[0].path(), cancel) {
        Ok(Some(lag)) if lag.abs() > SYNC_TOLERANCE => warn(format!(
            "The new audio may be out of sync with the source: it is {lag} samples ({:.1} ms) off.",
            lag as f64 * 1000.0 / RATE as f64
        )),
        Ok(_) => {}
        Err(e) if is_cancelled(&e) => return Err(e),
        Err(e) => warn(format!("The sync check could not run: {e:#}")),
    }

    let result = AudioResult { tracks, warnings };
    save_result(&dir, &result)?;
    // The result is cached; the intermediates are big (several GB for a film).
    remove_intermediates(&dir);
    Ok(result)
}

/// Removes the extracted source, the sections and the temp files (best effort).
fn remove_intermediates(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("source.f32"));
    let _ = std::fs::remove_dir_all(dir.join("sections"));
    let _ = std::fs::remove_dir_all(dir.join("tmp"));
}

fn load_result(dir: &Path) -> Option<AudioResult> {
    let text = std::fs::read_to_string(dir.join("result.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let mut tracks = Vec::new();
    for t in v["tracks"].as_array()? {
        let path = dir.join(t["file"].as_str()?);
        if !path.is_file() {
            return None;
        }
        tracks.push(AudioTrack::from_tag(t["kind"].as_str()?, path)?);
    }
    if tracks.is_empty() {
        return None;
    }
    let warnings = v["warnings"]
        .as_array()?
        .iter()
        .filter_map(|w| w.as_str().map(str::to_string))
        .collect();
    Some(AudioResult { tracks, warnings })
}

fn save_result(dir: &Path, r: &AudioResult) -> Result<()> {
    let tracks: Vec<Value> = r
        .tracks
        .iter()
        .map(|t| {
            let file = t
                .path()
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            json!({"kind": t.tag(), "file": file})
        })
        .collect();
    let text = serde_json::to_string_pretty(&json!({
        "tracks": tracks,
        "warnings": r.warnings,
    }))?;
    let part = dir.join("result.json.part");
    std::fs::write(&part, text)?;
    std::fs::rename(&part, dir.join("result.json")).context("could not save the audio result")?;
    Ok(())
}

struct Job<'a> {
    tools: &'a Tools,
    plan: JobPlan,
    lfe: LfeLevel,
    dir: PathBuf,
    /// The stem cache folder of this range ([`stems_dir`]) when the AI separates.
    stems: Option<PathBuf>,
    /// Frames of the whole range.
    total: u64,
    opts: JobOptions,
    sections: Vec<Section>,
}

/// Whether `path` is a finished section: a WAV whose header says `len` frames of
/// `channels` channels at 48 kHz. Writes are atomic, so a complete header means a
/// complete file.
fn section_complete(path: &Path, len: u64, channels: usize) -> bool {
    hound::WavReader::open(path).is_ok_and(|r| {
        let s = r.spec();
        s.channels as usize == channels && s.sample_rate == RATE && r.duration() as u64 == len
    })
}

/// The three stems in a stem cache section folder.
fn stems_in(dir: &Path) -> Stems {
    Stems {
        speech: dir.join("speech.wav"),
        music: dir.join("music.wav"),
        effects: dir.join("effects.wav"),
    }
}

/// The cached stems in `dir`, when all three are there. The folder only ever appears
/// complete ([`store_stems`] renames it into place), so existing files are finished files.
fn cached_stems(dir: &Path) -> Option<Stems> {
    let stems = stems_in(dir);
    [&stems.speech, &stems.music, &stems.effects]
        .iter()
        .all(|p| p.is_file())
        .then_some(stems)
}

/// Moves `stems` into the cache folder `dir` atomically: into `NNN.part` first, then the
/// folder is renamed to `NNN`.
fn store_stems(stems: &Stems, dir: &Path) -> Result<Stems> {
    let part = dir.with_extension("part");
    let _ = std::fs::remove_dir_all(&part);
    std::fs::create_dir_all(&part)?;
    let target = stems_in(&part);
    for (from, to) in [
        (&stems.speech, &target.speech),
        (&stems.music, &target.music),
        (&stems.effects, &target.effects),
    ] {
        if std::fs::rename(from, to).is_err() {
            std::fs::copy(from, to)
                .with_context(|| format!("could not copy {}", from.display()))?;
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&part, dir).context("could not finish the stem cache folder")?;
    Ok(stems_in(dir))
}

/// `p` padded with silence or trimmed to `len` frames.
fn fit(p: Pcm, len: u64) -> Pcm {
    if p.frames() as u64 == len {
        return p;
    }
    let chans: Vec<Vec<f32>> = (0..p.channels)
        .map(|c| {
            let mut v = p.channel(c);
            v.resize(len as usize, 0.0);
            v
        })
        .collect();
    Pcm::from_channels(p.rate, &chans)
}

fn mono(samples: Vec<f32>) -> Pcm {
    Pcm {
        rate: RATE,
        channels: 1,
        samples,
    }
}

/// Enhancement of a section's result: 5.1 as FL/FR (pair), FC (mono) and SL/SR (pair) with
/// LFE passed through untouched; stereo as a pair (M/S when Dolby Surround); mono as mono.
fn enhance(
    ai: &dyn AudioAi,
    pcm: &Pcm,
    dolby_surround: bool,
    tmp: &Path,
    cancel: &AtomicBool,
) -> Result<Pcm> {
    let mut quiet = |_: f32| {};
    if pcm.channels != 6 {
        return enhance_pcm(ai, pcm, dolby_surround, tmp, cancel, &mut quiet);
    }
    let ch = |i: usize| pcm.channel(i);
    let pair = |a: usize, b: usize| Pcm::from_channels(RATE, &[ch(a), ch(b)]);
    let front = enhance_pcm(
        ai,
        &pair(0, 1),
        false,
        &tmp.join("front"),
        cancel,
        &mut quiet,
    )?;
    let centre = enhance_pcm(
        ai,
        &mono(ch(2)),
        false,
        &tmp.join("centre"),
        cancel,
        &mut quiet,
    )?;
    let back = enhance_pcm(
        ai,
        &pair(4, 5),
        false,
        &tmp.join("surround"),
        cancel,
        &mut quiet,
    )?;
    Ok(Pcm::from_channels(
        RATE,
        &[
            front.channel(0),
            front.channel(1),
            centre.samples,
            ch(3),
            back.channel(0),
            back.channel(1),
        ],
    ))
}

/// The mono downmix the sync check compares, on the scale of L + R: for 5.1 the ITU
/// stereo downmix (L = FL + 0.7071 FC + 0.7071 SL, R likewise) summed, so
/// FL + FR + 2 · 0.7071 FC + 0.7071 (SL + SR), LFE left out; L + R for stereo. With FC
/// written at +3 dB this rebuilds the source's L + R plus the surrounds.
fn downmix(p: &Pcm) -> Vec<f32> {
    p.samples
        .chunks_exact(p.channels.max(1))
        .map(|f| match f.len() {
            6 => f[0] + f[1] + 2.0 * DOWNMIX_FC * f[2] + DOWNMIX_FC * (f[4] + f[5]),
            _ => f.iter().sum(),
        })
        .collect()
}

fn file_input(path: &Path) -> Vec<OsString> {
    vec!["-i".into(), path.as_os_str().to_owned()]
}

impl Job<'_> {
    fn source_path(&self) -> PathBuf {
        self.dir.join("source.f32")
    }

    fn sections_dir(&self) -> PathBuf {
        self.dir.join("sections")
    }

    fn section_path(&self, index: usize) -> PathBuf {
        self.sections_dir().join(format!("{index:03}.wav"))
    }

    fn tmp_dir(&self, index: usize) -> PathBuf {
        self.dir.join("tmp").join(format!("{index:03}"))
    }

    fn source_layout(&self) -> &'static str {
        match self.plan.source_channels {
            1 => "mono",
            2 => "stereo",
            _ => LAYOUT_51,
        }
    }

    /// Makes every section that is not finished yet. Sections from a different layout
    /// (another section length) are thrown away first.
    fn render(
        &self,
        ai: Option<&dyn AudioAi>,
        cancel: &AtomicBool,
        emit: &(dyn Fn(AudioEvent) + Sync),
    ) -> Result<()> {
        let dir = self.sections_dir();
        let layout = format!(
            "{} {} {} {}",
            self.total,
            self.opts.section_frames,
            self.opts.overlap_frames,
            self.plan.channels()
        );
        let marker = dir.join("layout.txt");
        if std::fs::read_to_string(&marker).ok().as_deref() != Some(layout.as_str()) {
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&marker, &layout)?;
        }
        let count = self.sections.len();
        for s in &self.sections {
            if cancel.load(Ordering::Relaxed) {
                bail!(Cancelled);
            }
            let path = self.section_path(s.index);
            if !section_complete(&path, s.len, self.plan.channels()) {
                let pcm = self.make_section(s, ai, cancel, emit)?;
                write_wav(&path, &pcm)?;
                let _ = std::fs::remove_dir_all(self.tmp_dir(s.index));
            }
            emit(AudioEvent::Section {
                done: s.index + 1,
                total: count,
            });
        }
        Ok(())
    }

    fn make_section(
        &self,
        s: &Section,
        ai: Option<&dyn AudioAi>,
        cancel: &AtomicBool,
        emit: &(dyn Fn(AudioEvent) + Sync),
    ) -> Result<Pcm> {
        let tmp = self.tmp_dir(s.index);
        std::fs::create_dir_all(&tmp)?;
        let input = read_raw_frames(
            &self.source_path(),
            self.plan.source_channels,
            s.start,
            s.len,
        )?;
        let need_ai = || ai.ok_or_else(|| anyhow!(self.plan.pack_missing()));
        let mut pcm = match self.plan.kind {
            Kind::UpmixAi => {
                let stems = self.stems_for(s, &input, &tmp, ai, cancel, emit)?;
                let stem = |p: &Path| -> Result<Pcm> {
                    let x = read_wav(p)?;
                    if x.channels != 2 || x.rate != RATE {
                        bail!(
                            "the AI separation returned {} channels at {} Hz, expected stereo at {RATE} Hz",
                            x.channels,
                            x.rate
                        );
                    }
                    Ok(x)
                };
                let (speech, music, effects) = (
                    stem(&stems.speech)?,
                    stem(&stems.music)?,
                    stem(&stems.effects)?,
                );
                emit(AudioEvent::Stage(AudioStage::Placing));
                place(&input, &speech, &music, &effects, self.lfe)
            }
            Kind::UpmixClassic => {
                emit(AudioEvent::Stage(AudioStage::Placing));
                upmix_classic(&input, self.lfe, cancel)?
            }
            Kind::Keep51 | Kind::Stereo | Kind::MonoToStereo => input,
        };
        if self.plan.enhance {
            let ai = need_ai()?;
            emit(AudioEvent::Stage(AudioStage::Enhancing));
            // Dolby Surround matters only for a stereo result made from the stereo source.
            let ds = self.plan.dolby_surround && self.plan.kind == Kind::Stereo;
            pcm = enhance(ai, &pcm, ds, &tmp.join("enhance"), cancel)?;
        }
        if self.plan.kind == Kind::MonoToStereo {
            let m = pcm.channel(0);
            pcm = Pcm::from_channels(RATE, &[m.clone(), m]);
        }
        if pcm.channels != self.plan.channels() {
            bail!(
                "section {} has {} channels, expected {}",
                s.index,
                pcm.channels,
                self.plan.channels()
            );
        }
        Ok(fit(pcm, s.len))
    }

    /// Section `s`'s separated stems: from the stem cache when it has them, else from the AI,
    /// and then stored in the cache for the next run of the same range.
    fn stems_for(
        &self,
        s: &Section,
        input: &Pcm,
        tmp: &Path,
        ai: Option<&dyn AudioAi>,
        cancel: &AtomicBool,
        emit: &(dyn Fn(AudioEvent) + Sync),
    ) -> Result<Stems> {
        let cached = self
            .stems
            .as_ref()
            .map(|d| d.join(format!("{:03}", s.index)));
        if let Some(stems) = cached.as_deref().and_then(cached_stems) {
            return Ok(stems);
        }
        let ai = ai.ok_or_else(|| anyhow!(self.plan.pack_missing()))?;
        emit(AudioEvent::Stage(AudioStage::Separating));
        let wav = tmp.join("input.wav");
        write_wav(&wav, input)?;
        let out = tmp.join("stems");
        let _ = std::fs::remove_dir_all(&out);
        let stems = ai.separate(&wav, &out, cancel, &mut |_| {})?;
        let Some(cached) = cached else {
            return Ok(stems);
        };
        // A failed store costs only a separation next time.
        match store_stems(&stems, &cached) {
            Ok(kept) => Ok(kept),
            Err(e) => {
                log::warn!("could not keep the stems in {}: {e:#}", cached.display());
                Ok(stems)
            }
        }
    }

    /// The finished sections, crossfaded, as one stream of `total` frames.
    fn stitch(&self, write: impl FnMut(&[f32]) -> Result<()>) -> Result<()> {
        let ch = self.plan.channels();
        sections::stitch(
            &self.sections,
            self.total,
            ch,
            |s| {
                let p = read_wav(&self.section_path(s.index))?;
                if p.channels != ch {
                    bail!(
                        "section {} has {} channels, expected {ch}",
                        s.index,
                        p.channels
                    );
                }
                Ok(p.samples)
            },
            write,
        )
    }

    /// Stitches into the FLAC (and for 5.1 the AC-3) encoder at once, with `gain_db` applied
    /// and optionally the limiter. Files are finished under a temporary name and renamed.
    /// Also returns the f32 sample peak written (after the gain, before the limiter and the
    /// encoders' clipping at 0 dBFS).
    fn encode(
        &self,
        gain_db: f64,
        limiter: bool,
        cancel: &AtomicBool,
    ) -> Result<(Vec<AudioTrack>, f32)> {
        let ch = self.plan.channels();
        let layout = self.plan.layout();
        let filter = limiter.then_some(LIMITER);
        let mut outputs: Vec<(AudioTrack, PathBuf, AudioCodec)> = Vec::new();
        if ch == 6 {
            outputs.push((
                AudioTrack::Flac51(self.dir.join("surround.flac")),
                self.dir.join("surround.part.flac"),
                AudioCodec::Flac24,
            ));
            outputs.push((
                AudioTrack::Ac351(self.dir.join("surround.ac3")),
                self.dir.join("surround.part.ac3"),
                AudioCodec::Ac3At640 {
                    dialnorm: self.plan.dialnorm,
                },
            ));
        } else {
            outputs.push((
                AudioTrack::FlacStereo(self.dir.join("stereo.flac")),
                self.dir.join("stereo.part.flac"),
                AudioCodec::Flac24,
            ));
        }
        // The AC-3 encoder puts AC3_PRIMING frames of silence in front of the signal and
        // nothing in the stream says so: it gets the signal without its first frames and
        // silence at the end instead, so decoded it lines up with the FLAC sample for sample
        // and keeps its length.
        let mut encoders = Vec::new();
        for (_, part, codec) in &outputs {
            let skip = match codec {
                AudioCodec::Ac3At640 { .. } => AC3_PRIMING,
                _ => 0,
            };
            let enc = encode_stream_filtered(self.tools, ch, layout, *codec, filter, part, cancel)?;
            encoders.push((enc, skip));
        }
        let gain = 10f32.powf(gain_db as f32 / 20.0);
        let mut scaled = Vec::new();
        let mut peak = 0.0f32;
        self.stitch(|block| {
            for chunk in block.chunks(CHUNK_FRAMES * ch) {
                let data = if gain_db == 0.0 {
                    chunk
                } else {
                    scaled.clear();
                    scaled.extend(chunk.iter().map(|v| v * gain));
                    &scaled[..]
                };
                peak = data.iter().fold(peak, |m, v| m.max(v.abs()));
                for (e, skip) in encoders.iter_mut() {
                    let drop = (*skip).min(data.len() / ch);
                    *skip -= drop;
                    if drop * ch < data.len() {
                        e.write(&data[drop * ch..])?;
                    }
                }
            }
            Ok(())
        })?;
        for ((mut e, skip), (_, _, codec)) in encoders.into_iter().zip(&outputs) {
            // As much silence as was dropped (less when the whole signal was shorter).
            let padding = match codec {
                AudioCodec::Ac3At640 { .. } => AC3_PRIMING - skip,
                _ => 0,
            };
            if padding > 0 {
                e.write(&vec![0.0; padding * ch])?;
            }
            e.finish()?;
        }
        let mut tracks = Vec::new();
        for (track, part, _) in outputs {
            std::fs::rename(&part, track.path()).context("could not finish the audio track")?;
            tracks.push(track);
        }
        Ok((tracks, peak))
    }

    /// Integrated loudness and true peak of the source range and of `result`.
    fn check_levels(&self, result: &Path, cancel: &AtomicBool) -> Result<(Loudness, Loudness)> {
        let raw: Vec<OsString> = vec![
            "-f".into(),
            "f32le".into(),
            "-ar".into(),
            RATE.to_string().into(),
            "-ac".into(),
            self.plan.source_channels.to_string().into(),
            "-channel_layout".into(),
            self.source_layout().into(),
            "-i".into(),
            self.source_path().into_os_string(),
        ];
        // A mono source becomes L = R stereo (or sits in a 5.1 as a pair): BS.1770 counts
        // mono played on two speakers as dual mono, +3 dB against a single channel.
        let dual_mono = self.plan.source_channels == 1;
        let src = measure(self.tools, &raw, dual_mono, cancel)?;
        let res = measure(self.tools, &file_input(result), false, cancel)?;
        Ok((src, res))
    }

    /// The lag of the result against the source where their downmixes correlate best, over
    /// the loudest of a few 2 s windows; `None` when the range is too short or too quiet.
    fn sync_lag(&self, result: &Path, cancel: &AtomicBool) -> Result<Option<i64>> {
        let lag = SYNC_MAX_LAG;
        // Window starts on 10 ms boundaries, so the seek in seconds is exact.
        let step = RATE as u64 / 100;
        let w = SYNC_WINDOW.min(self.total.saturating_sub(4 * step));
        if w < step {
            return Ok(None);
        }
        let mut best: Option<(u64, Vec<f32>, f32)> = None;
        for f in [0.1, 0.3, 0.5, 0.7, 0.9] {
            let p = ((self.total - w) as f64 * f) as u64 / step * step;
            let p = p.max(step);
            if p + w + lag > self.total {
                continue;
            }
            let src = read_raw_frames(&self.source_path(), self.plan.source_channels, p, w)?;
            let mix = downmix(&src);
            let level = rms_db(&mix);
            if best.as_ref().is_none_or(|b| level > b.2) {
                best = Some((p, mix, level));
            }
        }
        let Some((p, src, level)) = best else {
            return Ok(None);
        };
        if level < -60.0 {
            return Ok(None);
        }
        let wav = self.dir.join("tmp").join("sync.wav");
        extract(
            self.tools,
            result,
            (p - lag) as f64 / RATE as f64,
            (w + 2 * lag) as f64 / RATE as f64,
            &wav,
            cancel,
        )?;
        let res = read_wav(&wav)?;
        let _ = std::fs::remove_file(&wav);
        Ok(Some(best_lag(&src, &downmix(&res), lag as usize)))
    }
}

/// Integrated loudness (LUFS) and true peak (dBTP), BS.1770 as FFmpeg's `ebur128` measures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loudness {
    pub integrated: f64,
    pub true_peak: f64,
}

/// Runs `ebur128` over the input given by `input` (FFmpeg input arguments). `dual_mono`
/// measures a mono input as if played on two speakers (`dualmono=true`).
fn measure(
    tools: &Tools,
    input: &[OsString],
    dual_mono: bool,
    cancel: &AtomicBool,
) -> Result<Loudness> {
    let filter = format!("ebur128=peak=true:dualmono={dual_mono}:framelog=verbose");
    let mut cmd = process::command(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-nostats"])
        .args(input)
        .arg("-af")
        .arg(filter)
        .args(["-f", "null", "-"]);
    let out = process::run(cmd, None, cancel, None)?;
    if !out.success {
        bail!("ffmpeg could not measure the loudness: {}", out.tail(4));
    }
    parse_ebur128(&out.stderr_text())
        .ok_or_else(|| anyhow!("ffmpeg gave no loudness summary: {}", out.tail(4)))
}

/// The integrated loudness (`I:`) and true peak (`Peak:`) from `ebur128`'s summary.
pub fn parse_ebur128(stderr: &str) -> Option<Loudness> {
    let summary = &stderr[stderr.rfind("Summary:")?..];
    let value = |label: &str| -> Option<f64> {
        summary
            .lines()
            .map(str::trim)
            .find_map(|l| l.strip_prefix(label))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|v| v.parse().ok())
    };
    Some(Loudness {
        integrated: value("I:")?,
        true_peak: value("Peak:")?,
    })
}

/// The static gain (dB) and whether the limiter is needed to bring the result within
/// ±1 LU of the source and under -1 dBTP. A peak over by up to 1 dB (after the loudness
/// gain) is cut statically; more than that goes to the limiter instead, as a static cut
/// would cost more than 1 dB of loudness.
pub(crate) fn correction(src: &Loudness, res: &Loudness) -> (f64, bool) {
    let mut gain = 0.0;
    if src.integrated > SILENT_LUFS
        && res.integrated > SILENT_LUFS
        && (res.integrated - src.integrated).abs() > LOUDNESS_TOLERANCE_LU
    {
        gain = src.integrated - res.integrated;
    }
    let over = res.true_peak + gain - TRUE_PEAK_MAX_DBTP;
    if over <= 0.0 {
        (gain, false)
    } else if over <= MAX_PEAK_CUT_DB {
        (gain - over, false)
    } else {
        (gain, true)
    }
}

/// The lag (samples, positive = the result is late) where `res` correlates best with
/// `src`. `res` holds `max_lag` extra samples before and after `src`'s window.
pub(crate) fn best_lag(src: &[f32], res: &[f32], max_lag: usize) -> i64 {
    let mut best = (0i64, f64::MIN);
    for lag in -(max_lag as i64)..=max_lag as i64 {
        let off = (max_lag as i64 + lag) as usize;
        let c: f64 = src
            .iter()
            .zip(res.iter().skip(off))
            .map(|(a, b)| *a as f64 * *b as f64)
            .sum();
        if c > best.1 {
            best = (lag, c);
        }
    }
    best.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::ai::{PACK_MISSING_BOTH, PACK_MISSING_ENHANCE, PACK_MISSING_SEPARATION};
    use crate::audio::io::{read_wav, write_raw, write_wav};
    use crate::audio::sections;
    use crate::process::is_cancelled;
    use crate::settings::{AudioMode, SurroundEngine};
    use anyhow::bail;
    use rustfft::{num_complex::Complex, FftPlanner};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn info(fps_num: u32, fps_den: u32) -> VideoInfo {
        VideoInfo {
            path: PathBuf::from("movie.mkv"),
            width: 640,
            height: 360,
            coded_width: 640,
            coded_height: 360,
            source_width: 640,
            source_height: 360,
            detected_crop: None,
            crop: None,
            rotated: false,
            interlaced: false,
            sar: (1, 1),
            declared_sar: (1, 1),
            start_time: 0.0,
            fps_num,
            fps_den,
            duration: 3600.0,
            frame_count: 86_400,
            video_codec: "h264".into(),
            has_audio: true,
            audio_codec: Some("ac3".into()),
            file_size: 1234,
            modified: 5678,
        }
    }

    fn settings(mode: AudioMode, engine: SurroundEngine, enhance: bool) -> Settings {
        Settings {
            audio_mode: mode,
            surround_engine: engine,
            audio_enhance: enhance,
            ..Settings::default()
        }
    }

    fn source(channels: u32) -> SourceAudio {
        SourceAudio {
            channels,
            codec: "ac3".into(),
            dolby_surround: channels == 2,
            dialnorm: Some(-24),
        }
    }

    fn tools() -> Tools {
        // Never started by these tests: they use the AI and pure-Rust paths only.
        Tools {
            ffmpeg: PathBuf::from("ffmpeg-not-used"),
            ffprobe: PathBuf::from("ffprobe-not-used"),
        }
    }

    fn noise(n: usize, mut s: u32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                ((s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// `x` with everything above `f` removed, like a lossy codec's cutoff.
    fn lowpassed(x: &[f32], f: f32) -> Vec<f32> {
        let n = x.len();
        let mut planner = FftPlanner::<f32>::new();
        let mut buf: Vec<Complex<f32>> = x.iter().map(|&v| Complex::new(v, 0.0)).collect();
        planner.plan_fft_forward(n).process(&mut buf);
        for (k, b) in buf.iter_mut().enumerate() {
            if k.min(n - k) as f32 * RATE as f32 / n as f32 > f {
                *b = Complex::new(0.0, 0.0);
            }
        }
        planner.plan_fft_inverse(n).process(&mut buf);
        buf.iter().map(|c| c.re / n as f32).collect()
    }

    fn no_events(_: AudioEvent) {}

    #[test]
    fn prune_removes_superseded_and_stale_jobs() {
        let root = tempfile::tempdir().unwrap();
        let job = |name: &str, marker: Option<&str>| {
            let d = root.path().join(name);
            std::fs::create_dir_all(&d).unwrap();
            if let Some(m) = marker {
                std::fs::write(d.join(VIDEO_MARKER), m).unwrap();
            }
            std::fs::write(d.join("result.json"), "{}").unwrap();
            d
        };
        let current = "C:\\v\\a.mkv\nC:\\v\\a.mkv|10|20\n0 1000";
        let keep = job("keep", Some(current));
        // Same video and range, other settings: superseded.
        let older = job("older", Some(current));
        // Same video, another range (a sample after the full episode): kept.
        let sample = job("sample", Some("C:\\v\\a.mkv\nC:\\v\\a.mkv|10|20\n100 200"));
        // Same path, the file changed since: useless whatever the range.
        let changed = job("changed", Some("C:\\v\\a.mkv\nC:\\v\\a.mkv|10|19\n100 200"));
        // A marker from before ranges were recorded: superseded.
        let legacy = job("legacy", Some("C:\\v\\a.mkv"));
        let other = job("other", Some("C:\\v\\b.mkv\nC:\\v\\b.mkv|1|2\n0 1000"));
        let unmarked = job("unmarked", None);
        let stems = job(STEMS_DIR, None);
        std::fs::write(root.path().join("loose.txt"), "x").unwrap();

        let mut removed = prune(root.path(), &keep, current, JOB_MAX_AGE, SystemTime::now());
        removed.sort();
        assert_eq!(removed, vec![changed, legacy, older]);
        assert!(keep.is_dir() && sample.is_dir() && other.is_dir() && unmarked.is_dir());

        // 31 days later: every other folder is stale, but `keep`, the stem cache (pruned on
        // its own) and loose files stay.
        let later = SystemTime::now() + Duration::from_secs(31 * 24 * 3600);
        let mut removed = prune(root.path(), &keep, current, JOB_MAX_AGE, later);
        removed.sort();
        assert_eq!(removed, vec![other, sample, unmarked]);
        assert!(keep.is_dir() && stems.is_dir());
        assert!(root.path().join("loose.txt").is_file());

        // A missing root is fine.
        assert!(prune(
            &root.path().join("nope"),
            &keep,
            current,
            JOB_MAX_AGE,
            later
        )
        .is_empty());
    }

    #[test]
    fn prune_after_keeps_stems_of_the_range() {
        let work = tempfile::tempdir().unwrap();
        let spec = |start: u64, end: u64, lfe: LfeLevel| AudioSpec {
            info: info(24, 1),
            start,
            end,
            settings: Settings {
                audio_mode: AudioMode::Surround,
                lfe_level: lfe,
                project_dir: Some(work.path().to_path_buf()),
                ..Settings::default()
            },
        };
        let make = |dir: PathBuf, s: &AudioSpec| {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(VIDEO_MARKER), marker(s)).unwrap();
            dir
        };
        let full = spec(0, 1000, LfeLevel::Normal);
        let full_job = make(job_dir(&full), &full);
        let full_stems = make(stems_dir(&full, &JobOptions::default()), &full);
        // Stems of the same range made with another pack: superseded.
        let old_stems = make(
            stems_dir_with(&full, &JobOptions::default(), "old pack"),
            &full,
        );

        // Another LFE level of the full range: its job replaces the Normal one, the stems stay.
        let strong = spec(0, 1000, LfeLevel::Strong);
        let strong_job = make(job_dir(&strong), &strong);
        let mut removed = prune_after(&strong);
        removed.sort();
        assert_eq!(removed, vec![full_job.clone(), old_stems]);
        assert!(strong_job.is_dir() && full_stems.is_dir());

        // A short sample afterwards removes neither the full range's job nor its stems.
        let sample = spec(100, 200, LfeLevel::Normal);
        let sample_job = make(job_dir(&sample), &sample);
        assert!(prune_after(&sample).is_empty());
        assert!(strong_job.is_dir() && full_stems.is_dir() && sample_job.is_dir());
    }

    #[test]
    fn light_clean_keeps_tracks_and_stems() {
        let work = tempfile::tempdir().unwrap();
        let audio = work.path().join("audio");
        let job = audio.join("somekey");
        std::fs::create_dir_all(job.join("sections")).unwrap();
        std::fs::create_dir_all(job.join("tmp").join("000")).unwrap();
        std::fs::write(job.join("source.f32"), b"x").unwrap();
        std::fs::write(job.join("surround.flac"), b"x").unwrap();
        std::fs::write(job.join("result.json"), b"{}").unwrap();
        let stem = audio.join(STEMS_DIR).join("k").join("000");
        std::fs::create_dir_all(&stem).unwrap();
        std::fs::write(stem.join("speech.wav"), b"x").unwrap();

        clean_intermediates(work.path());
        assert!(!job.join("source.f32").exists());
        assert!(!job.join("sections").exists() && !job.join("tmp").exists());
        assert!(job.join("surround.flac").is_file() && job.join("result.json").is_file());
        assert!(stem.join("speech.wav").is_file());
        // No audio folder at all is fine.
        clean_intermediates(&work.path().join("nope"));
    }

    /// Fake AI: stems are fixed fractions of the input; enhancement adds a 17 kHz tone.
    #[derive(Default)]
    struct FakeAi {
        separations: AtomicUsize,
        enhancements: Mutex<Vec<usize>>,
        /// Returns `Cancelled` on the given separation call (1-based), as the real worker
        /// does when it is stopped mid-section.
        cancel_on: Option<usize>,
    }

    impl AudioAi for FakeAi {
        fn separate(
            &self,
            wav: &Path,
            out_dir: &Path,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(f32),
        ) -> Result<Stems> {
            let call = self.separations.fetch_add(1, Ordering::SeqCst) + 1;
            if self.cancel_on == Some(call) {
                bail!(Cancelled);
            }
            let x = read_wav(wav)?;
            std::fs::create_dir_all(out_dir)?;
            let stem = |name: &str, g: f32| -> Result<PathBuf> {
                let p = out_dir.join(name);
                let samples = x.samples.iter().map(|v| v * g).collect();
                write_wav(
                    &p,
                    &Pcm {
                        rate: x.rate,
                        channels: x.channels,
                        samples,
                    },
                )?;
                Ok(p)
            };
            let stems = Stems {
                speech: stem("speech.wav", 0.5)?,
                music: stem("music.wav", 0.3)?,
                effects: stem("effects.wav", 0.2)?,
            };
            Ok(stems)
        }

        fn enhance(
            &self,
            wav: &Path,
            out: &Path,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(f32),
        ) -> Result<()> {
            let x = read_wav(wav)?;
            self.enhancements.lock().unwrap().push(x.channels);
            if x.channels > 2 {
                bail!("the AI takes mono or stereo only");
            }
            let mut y = x.clone();
            for (i, v) in y.samples.iter_mut().enumerate() {
                let t = (i / x.channels) as f32 / RATE as f32;
                *v += 0.05 * (std::f32::consts::TAU * 17_000.0 * t).sin();
            }
            write_wav(out, &y)
        }
    }

    fn job<'a>(
        tools: &'a Tools,
        plan: JobPlan,
        dir: &Path,
        total: u64,
        opts: JobOptions,
    ) -> Job<'a> {
        Job {
            tools,
            plan,
            lfe: LfeLevel::Normal,
            dir: dir.to_path_buf(),
            stems: None,
            total,
            opts,
            sections: sections::plan_with(total, opts.section_frames, opts.overlap_frames),
        }
    }

    fn stitched(job: &Job) -> Vec<f32> {
        let mut out = Vec::new();
        job.stitch(|b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn key_changes_with_lfe_level() {
        let s = settings(AudioMode::Surround, SurroundEngine::LocalAi, false);
        let spec = AudioSpec {
            info: info(24, 1),
            start: 0,
            end: 100,
            settings: s.clone(),
        };
        let base = key(&spec);
        assert_eq!(base, key(&spec.clone()));
        let mut other = spec.clone();
        other.settings.lfe_level = LfeLevel::Cinema;
        assert_ne!(key(&other), base);
        for change in [
            |s: &mut AudioSpec| s.settings.surround_engine = SurroundEngine::Classic,
            |s: &mut AudioSpec| s.settings.audio_enhance = true,
            |s: &mut AudioSpec| s.settings.audio_mode = AudioMode::Original,
            |s: &mut AudioSpec| s.start = 1,
            |s: &mut AudioSpec| s.end = 101,
            |s: &mut AudioSpec| s.info.modified += 1,
        ] {
            let mut c = spec.clone();
            change(&mut c);
            assert_ne!(key(&c), base, "{c:?}");
        }
        // Pixel settings and pixel-only state (crop, SAR override) do not touch the audio.
        let mut prompt = spec.clone();
        prompt.settings.prompt = "something else".into();
        assert_eq!(key(&prompt), base);
        let mut cropped = spec.clone();
        cropped.info.crop = Some(crate::ffmpeg::Crop {
            w: 640,
            h: 272,
            x: 0,
            y: 44,
        });
        assert_ne!(cropped.info.identity(), spec.info.identity());
        assert_eq!(key(&cropped), base);
        let mut sar = spec.clone();
        sar.info.sar = (4, 3);
        assert_ne!(sar.info.identity(), spec.info.identity());
        assert_eq!(key(&sar), base);
        // The AI pack's identity is part of the key when the AI may be used, not otherwise.
        assert_ne!(base, key_with(&spec, "another pack"));
        let mut classic = spec.clone();
        classic.settings.surround_engine = SurroundEngine::Classic;
        assert_eq!(key(&classic), key_with(&classic, "another pack"));
        let mut enhanced = classic.clone();
        enhanced.settings.audio_enhance = true;
        assert_ne!(key(&enhanced), key_with(&enhanced, "another pack"));
        let mut s2 = spec.clone();
        s2.settings.project_dir = Some(PathBuf::from("proj"));
        assert_eq!(
            job_dir(&s2),
            PathBuf::from("proj").join("audio").join(&base)
        );

        // Without 5.1 the engine and the LFE level shape nothing: one key for all of them,
        // and no pack identity when the AI is not used.
        let mut stereo = spec.clone();
        stereo.settings.audio_mode = AudioMode::Original;
        stereo.settings.audio_enhance = true;
        let stereo_key = key(&stereo);
        for change in [
            |s: &mut AudioSpec| s.settings.surround_engine = SurroundEngine::Classic,
            |s: &mut AudioSpec| s.settings.lfe_level = LfeLevel::Strong,
        ] {
            let mut c = stereo.clone();
            change(&mut c);
            assert_eq!(key(&c), stereo_key, "{c:?}");
        }
        assert_ne!(stereo_key, key_with(&stereo, "another pack"));
        let mut original = stereo.clone();
        original.settings.audio_enhance = false;
        assert_eq!(key(&original), key_with(&original, "another pack"));
    }

    #[test]
    fn stems_key_leaves_out_lfe_engine_and_enhance() {
        let spec = AudioSpec {
            info: info(24, 1),
            start: 0,
            end: 100,
            settings: settings(AudioMode::Surround, SurroundEngine::LocalAi, false),
        };
        let opts = JobOptions::default();
        let base = stems_dir(&spec, &opts);
        for change in [
            |s: &mut AudioSpec| s.settings.lfe_level = LfeLevel::Cinema,
            |s: &mut AudioSpec| s.settings.surround_engine = SurroundEngine::Classic,
            |s: &mut AudioSpec| s.settings.audio_enhance = true,
        ] {
            let mut c = spec.clone();
            change(&mut c);
            assert_eq!(stems_dir(&c, &opts), base, "{c:?}");
            assert_ne!(job_dir(&c), job_dir(&spec), "{c:?}");
        }
        for change in [
            |s: &mut AudioSpec| s.start = 1,
            |s: &mut AudioSpec| s.end = 101,
            |s: &mut AudioSpec| s.info.modified += 1,
        ] {
            let mut c = spec.clone();
            change(&mut c);
            assert_ne!(stems_dir(&c, &opts), base, "{c:?}");
        }
        assert_ne!(stems_dir_with(&spec, &opts, "another pack"), base);
        let short = JobOptions {
            section_frames: 48_000,
            overlap_frames: 4_800,
        };
        assert_ne!(stems_dir(&spec, &short), base);
        assert_eq!(
            base.parent().unwrap(),
            job_dir(&spec).parent().unwrap().join(STEMS_DIR)
        );
    }

    /// Another LFE level of the same range places the cached stems again: no separation.
    #[test]
    fn lfe_change_reuses_cached_stems() {
        let dir = tempfile::tempdir().unwrap();
        let tools = tools();
        let plan = plan_job(
            &settings(AudioMode::Surround, SurroundEngine::LocalAi, false),
            Some(&source(2)),
            true,
        )
        .unwrap();
        let opts = JobOptions {
            section_frames: 48_000,
            overlap_frames: 4_800,
        };
        let total = 120_000u64;
        // 50 Hz bass hits over quiet noise: effects bass that rises above its background.
        let x: Vec<f32> = noise(total as usize, 5, 0.02)
            .into_iter()
            .enumerate()
            .map(|(i, v)| {
                let t = i as f32 / RATE as f32;
                let hit = if (t * 2.0).fract() < 0.3 { 0.5 } else { 0.0 };
                v + hit * (std::f32::consts::TAU * 50.0 * t).sin()
            })
            .collect();
        let src = Pcm::from_channels(RATE, &[x.clone(), x]);
        let stems = dir.path().join("stems").join("key");
        let make = |name: &str, lfe: LfeLevel, ai: &FakeAi| -> Vec<f32> {
            let mut j = job(&tools, plan, &dir.path().join(name), total, opts);
            j.lfe = lfe;
            j.stems = Some(stems.clone());
            write_raw(&j.source_path(), &src).unwrap();
            j.render(Some(ai), &AtomicBool::new(false), &no_events)
                .unwrap();
            let out = Pcm {
                rate: RATE,
                channels: 6,
                samples: stitched(&j),
            };
            out.channel(3)
        };
        let first = FakeAi::default();
        let normal = make("normal", LfeLevel::Normal, &first);
        assert_eq!(first.separations.load(Ordering::SeqCst), 3);
        assert!(stems.join("002").join("effects.wav").is_file());
        assert!(!stems.join("002.part").exists());

        let second = FakeAi::default();
        let strong = make("strong", LfeLevel::Strong, &second);
        assert_eq!(second.separations.load(Ordering::SeqCst), 0);
        assert_ne!(normal, strong);
        assert!(
            rms_db(&strong) > rms_db(&normal) + 1.0,
            "LFE Strong {} dB, Normal {} dB",
            rms_db(&strong),
            rms_db(&normal)
        );

        // Without the AI, a range whose stems are all cached still renders.
        let mut j = job(&tools, plan, &dir.path().join("no-ai"), total, opts);
        j.stems = Some(stems.clone());
        write_raw(&j.source_path(), &src).unwrap();
        j.render(None, &AtomicBool::new(false), &no_events).unwrap();
    }

    #[test]
    fn range_covers_exact_frames() {
        // start = frame_time(start), length (end - start) * fps_den / fps_num seconds,
        // round(seconds * 48000) samples.
        let ntsc = info(24_000, 1001);
        let (start, dur, n) = range(&ntsc, 0, 1000);
        assert_eq!(start, 0.0);
        assert!((dur - 1000.0 * 1001.0 / 24_000.0).abs() < 1e-12);
        assert_eq!(n, 2_002_000);
        let (start, _, n) = range(&ntsc, 24, 48);
        assert_eq!(start, ntsc.frame_time(24));
        assert_eq!(n, 48_048);
        // 1001 / 30000 s = 1601.6 samples: rounded.
        let (_, _, n) = range(&info(30_000, 1001), 7, 8);
        assert_eq!(n, 1602);
        assert_eq!(range(&info(25, 1), 0, 3).2, 5760);
        assert_eq!(range(&info(25, 1), 5, 5).2, 0);
    }

    #[test]
    fn mono_surround_rejected_early() {
        let s = settings(AudioMode::Surround, SurroundEngine::Classic, false);
        let err = plan_job(&s, Some(&source(1)), true).unwrap_err();
        assert!(
            err.to_string()
                .contains("mono source cannot be turned into 5.1"),
            "{err}"
        );
    }

    #[test]
    fn plan_follows_source_and_settings() {
        let ai_51 = settings(AudioMode::Surround, SurroundEngine::LocalAi, false);
        let classic = settings(AudioMode::Surround, SurroundEngine::Classic, false);
        let enhance = settings(AudioMode::Original, SurroundEngine::LocalAi, true);
        let p = plan_job(&ai_51, Some(&source(2)), true).unwrap();
        assert_eq!((p.kind, p.enhance, p.dialnorm), (Kind::UpmixAi, false, -24));
        assert_eq!(
            plan_job(&classic, Some(&source(2)), false).unwrap().kind,
            Kind::UpmixClassic
        );
        // A 5.1 source is never upmixed; without enhancement there is nothing to do, so the
        // export keeps the original audio (and needs no pack).
        let enhanced_51 = settings(AudioMode::Surround, SurroundEngine::LocalAi, true);
        assert_eq!(
            plan_job(&enhanced_51, Some(&source(6)), true).unwrap().kind,
            Kind::Keep51
        );
        let err = plan_job(&ai_51, Some(&source(6)), false).unwrap_err();
        assert_eq!(err.to_string(), ALREADY_51);
        assert!(keeps_original(&ai_51, &source(6)));
        assert!(keeps_original(&classic, &source(6)));
        assert!(!keeps_original(&enhanced_51, &source(6)));
        assert!(!keeps_original(&ai_51, &source(2)));
        assert!(!keeps_original(&enhance, &source(2)));
        let original = settings(AudioMode::Original, SurroundEngine::LocalAi, false);
        assert!(keeps_original(&original, &source(2)));
        let p = plan_job(&enhance, Some(&source(2)), true).unwrap();
        assert_eq!(
            (p.kind, p.enhance, p.dolby_surround),
            (Kind::Stereo, true, true)
        );
        assert_eq!(
            plan_job(&enhance, Some(&source(1)), true).unwrap().kind,
            Kind::MonoToStereo
        );
        // Without the AI: an error before any work, suggesting what fits the plan.
        let err = plan_job(&ai_51, Some(&source(2)), false).unwrap_err();
        assert_eq!(err.to_string(), PACK_MISSING_SEPARATION);
        let err = plan_job(&enhance, Some(&source(2)), false).unwrap_err();
        assert_eq!(err.to_string(), PACK_MISSING_ENHANCE);
        let err = plan_job(&enhanced_51, Some(&source(6)), false).unwrap_err();
        assert_eq!(err.to_string(), PACK_MISSING_ENHANCE);
        let both = settings(AudioMode::Surround, SurroundEngine::LocalAi, true);
        let err = plan_job(&both, Some(&source(2)), false).unwrap_err();
        assert_eq!(err.to_string(), PACK_MISSING_BOTH);
        // No audio, odd layouts and nothing to do are errors too.
        assert!(plan_job(&ai_51, None, true).is_err());
        assert!(plan_job(&ai_51, Some(&source(8)), true).is_err());
        assert!(plan_job(&original, Some(&source(2)), true).is_err());
        // Without a dialnorm from the source, AC-3 gets -27.
        let mut aac = source(2);
        aac.dialnorm = None;
        assert_eq!(plan_job(&classic, Some(&aac), false).unwrap().dialnorm, -27);
    }

    #[test]
    fn resumes_after_partial_section() {
        let dir = tempfile::tempdir().unwrap();
        let tools = tools();
        let plan = plan_job(
            &settings(AudioMode::Surround, SurroundEngine::LocalAi, false),
            Some(&source(2)),
            true,
        )
        .unwrap();
        let opts = JobOptions {
            section_frames: 48_000,
            overlap_frames: 4_800,
        };
        let total = 120_000u64;
        let job = job(&tools, plan, dir.path(), total, opts);
        assert_eq!(job.sections.len(), 3);
        let src = Pcm::from_channels(
            RATE,
            &[noise(total as usize, 1, 0.3), noise(total as usize, 2, 0.3)],
        );
        write_raw(&job.source_path(), &src).unwrap();

        // First run: the AI is cancelled while separating section 3, so 2 of 3 are written.
        let first = FakeAi {
            cancel_on: Some(3),
            ..FakeAi::default()
        };
        let err = job
            .render(Some(&first), &AtomicBool::new(false), &no_events)
            .unwrap_err();
        assert!(is_cancelled(&err), "{err}");
        assert_eq!(first.separations.load(Ordering::SeqCst), 3);
        assert!(job.section_path(0).is_file() && job.section_path(1).is_file());
        assert!(!job.section_path(2).exists());
        // A crash left a partial file behind for section 3: it must not count.
        let part = job.dir.join("sections").join("002.wav.part");
        std::fs::write(&part, b"RIFF junk").unwrap();

        let fresh = AtomicBool::new(false);
        let second = FakeAi::default();
        let events = Mutex::new(Vec::new());
        job.render(Some(&second), &fresh, &|e| events.lock().unwrap().push(e))
            .unwrap();
        assert_eq!(second.separations.load(Ordering::SeqCst), 1);
        assert!(job.section_path(2).is_file());
        assert!(!part.exists());
        assert!(events
            .lock()
            .unwrap()
            .contains(&AudioEvent::Section { done: 3, total: 3 }));

        let out = stitched(&job);
        assert_eq!(out.len() as u64, total * 6);
        assert!(out.iter().all(|v| v.is_finite()));
        // The ITU downmix FL + 0.7071 FC is the source's left channel (placement loses
        // nothing), across the joins.
        let worst = (0..total as usize)
            .map(|i| (out[6 * i] + DOWNMIX_FC * out[6 * i + 2] - src.samples[2 * i]).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst < 1e-3,
            "FL + 0.7071 FC differs from the source by {worst}"
        );
    }

    #[test]
    fn five_one_source_keeps_lfe() {
        let dir = tempfile::tempdir().unwrap();
        let tools = tools();
        let plan = plan_job(
            &settings(AudioMode::Surround, SurroundEngine::LocalAi, true),
            Some(&source(6)),
            true,
        )
        .unwrap();
        assert_eq!((plan.kind, plan.enhance), (Kind::Keep51, true));
        let n = 96_000usize;
        let chans: Vec<Vec<f32>> = (0..6)
            .map(|c| lowpassed(&noise(n, 10 + c as u32, 0.3), 14_000.0))
            .collect();
        let src = Pcm::from_channels(RATE, &chans);
        let job = job(&tools, plan, dir.path(), n as u64, JobOptions::default());
        write_raw(&job.source_path(), &src).unwrap();
        let ai = FakeAi::default();
        job.render(Some(&ai), &AtomicBool::new(false), &no_events)
            .unwrap();
        // FL/FR as a pair, FC alone, SL/SR as a pair; never LFE, never upmixed.
        assert_eq!(*ai.enhancements.lock().unwrap(), vec![2, 1, 2]);
        assert_eq!(ai.separations.load(Ordering::SeqCst), 0);
        let out = Pcm {
            rate: RATE,
            channels: 6,
            samples: stitched(&job),
        };
        assert_eq!(out.frames(), n);
        assert_eq!(out.channel(3), src.channel(3), "LFE changed");
        for c in [0, 1, 2, 4, 5] {
            assert_ne!(out.channel(c), src.channel(c), "channel {c} not enhanced");
        }
    }

    #[test]
    fn parses_ebur128_summary() {
        let text = "[Parsed_ebur128_0 @ 0000] Summary:\n\
            \n  Integrated loudness:\n    I:         -21.1 LUFS\n    Threshold: -31.1 LUFS\n\
            \n  Loudness range:\n    LRA:        20.0 LU\n    Threshold: -41.1 LUFS\n\
            \n  True peak:\n    Peak:      -18.1 dBFS\n";
        assert_eq!(
            parse_ebur128(text),
            Some(Loudness {
                integrated: -21.1,
                true_peak: -18.1
            })
        );
        assert_eq!(parse_ebur128("no summary"), None);
    }

    #[test]
    fn loudness_and_peak_corrections() {
        let l = |i: f64, p: f64| Loudness {
            integrated: i,
            true_peak: p,
        };
        // Within 1 LU and under -1 dBTP: nothing.
        assert_eq!(correction(&l(-23.0, -5.0), &l(-23.5, -4.0)), (0.0, false));
        // 3 LU too quiet: +3 dB, peak still fine.
        assert_eq!(correction(&l(-23.0, -5.0), &l(-26.0, -8.0)), (3.0, false));
        // Peak 0.5 dB over: a static cut of 0.5 dB.
        let (g, lim) = correction(&l(-23.0, -5.0), &l(-23.0, -0.5));
        assert!((g + 0.5).abs() < 1e-9 && !lim, "{g}");
        // Peak 3 dB over: a cut would lose more than 1 dB of loudness, so the limiter.
        assert_eq!(correction(&l(-23.0, -5.0), &l(-23.0, 2.0)), (0.0, true));
        // Silence is not corrected.
        assert_eq!(correction(&l(-70.0, -90.0), &l(-40.0, -30.0)), (0.0, false));
    }

    #[test]
    fn best_lag_finds_the_shift() {
        let max = 240usize;
        let x = noise(20_000, 3, 0.5);
        let src = &x[max..x.len() - max];
        assert_eq!(best_lag(src, &x, max), 0);
        // The result is 5 samples late.
        let mut late = vec![0.0; 5];
        late.extend_from_slice(&x[..x.len() - 5]);
        assert_eq!(best_lag(src, &late, max), 5);
        let early = x[3..].to_vec();
        let mut early = early;
        early.extend_from_slice(&[0.0; 3]);
        assert_eq!(best_lag(src, &early, max), -3);
    }
}
