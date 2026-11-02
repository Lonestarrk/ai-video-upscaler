//! The HDR stages of the export. No 16-bit frame is ever stored (they were 6.5 MB each at
//! 1440x1080, ~440 GB for an episode):
//!
//! - Pass 1 converts every SDR frame in memory, measures its light levels and records them in
//!   `hdr/stats.txt`, one line per frame with the size and modification time of the SDR file it
//!   came from. A line counts only while that file is unchanged, so a resumed or repeated
//!   export converts only new or remade frames. MaxCLL/MaxFALL and the HDR10+ metadata come
//!   from these lines.
//! - Pass 2 converts the frames again (the conversion is deterministic, so the pixels are the
//!   ones measured) and streams them as raw 16-bit RGB into the encoder's stdin.
//!
//! `hdr/settings.txt` holds the mode, peak, SDR white and `HDR_VERSION`; `stats.txt` starts
//! with the same header and is discarded when it differs.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;

use super::itm::{Itm, SDR_WHITE};
use super::measure::{measure_frame, FrameLight};
use crate::process::Cancelled;
use crate::settings::{HdrMode, Settings};

/// Bump when the curve or the encoding changes, so earlier measurements and videos are redone.
pub const HDR_VERSION: u32 = 2;
pub const HDR_DIR: &str = "hdr";
const SETTINGS_FILE: &str = "settings.txt";
const STATS_FILE: &str = "stats.txt";
/// Stats lines between flushes to disk.
const FLUSH_EVERY: u64 = 100;
/// Memory the conversion workers may use together (frames in flight and their buffers).
const MEMORY_BUDGET: u64 = 2 << 30;
/// Bytes per pixel one conversion needs at its peak (8-bit input, the filter's f32 planes,
/// the 16-bit result and its byte copy for the pipe).
const BYTES_PER_PIXEL: u64 = 40;

pub fn hdr_dir(job_dir: &Path) -> PathBuf {
    job_dir.join(HDR_DIR)
}

/// What the conversion depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdrParams {
    pub mode: HdrMode,
    /// Mastering peak; always 1000 for HLG (its nominal peak).
    pub peak_nits: u32,
    pub sdr_white_nits: u32,
}

impl HdrParams {
    /// The settings' HDR output, or `None` when HDR is off.
    pub fn from_settings(s: &Settings) -> Option<HdrParams> {
        (s.hdr_mode != HdrMode::Off).then(|| HdrParams {
            mode: s.hdr_mode,
            peak_nits: if s.hdr_mode == HdrMode::Hlg {
                1000
            } else {
                s.hdr_peak_nits
            },
            sdr_white_nits: s.hdr_sdr_white_nits,
        })
    }

    /// The header of `settings.txt` and `stats.txt`.
    pub fn header(&self) -> String {
        format!(
            "mode: {}\npeak: {}\nwhite: {}\nversion: {HDR_VERSION}\n",
            self.mode.name(),
            self.peak_nits,
            self.sdr_white_nits
        )
    }

    pub fn itm(&self) -> Itm {
        Itm::new(self.mode, self.peak_nits, self.sdr_white_nits)
    }
}

/// How a job's HDR output was made, read from its `hdr/settings.txt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobHdr {
    pub params: HdrParams,
    /// Full-rate frames per `out/` frame (2/3 with "every 2nd/3rd frame").
    pub divisor: u32,
}

fn parse_header(text: &str) -> Option<HdrParams> {
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().to_string())
    };
    let name = value("mode:")?;
    let mode = [HdrMode::Hdr10Plus, HdrMode::Hdr10, HdrMode::Hlg]
        .into_iter()
        .find(|m| m.name() == name)?;
    Some(HdrParams {
        mode,
        peak_nits: value("peak:")?.parse().ok()?,
        sdr_white_nits: value("white:")
            .and_then(|v| v.parse().ok())
            .unwrap_or(SDR_WHITE),
    })
}

fn interp_divisor(job_dir: &Path) -> u32 {
    // The interpolation stage's marker says how many full-rate frames one AI frame covers.
    std::fs::read_to_string(job_dir.join("interp").join(SETTINGS_FILE))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("divisor:"))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(1)
}

/// The HDR settings of the job in `job_dir`, or `None` when it was never exported as HDR.
pub fn job_hdr(job_dir: &Path) -> Option<JobHdr> {
    let text = std::fs::read_to_string(hdr_dir(job_dir).join(SETTINGS_FILE)).ok()?;
    Some(JobHdr {
        params: parse_header(&text)?,
        divisor: interp_divisor(job_dir),
    })
}

/// Size and modification time (ns since the epoch) of a file, what a stats line is valid for.
type Stamp = (u64, u128);

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    let t = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((m.len(), t))
}

fn stats_line(n: u64, s: Stamp, f: &FrameLight) -> String {
    let mut line = format!("{n} {} {}", s.0, s.1);
    for v in f
        .max_scl
        .iter()
        .chain([f.avg_max_rgb].iter())
        .chain(f.percentiles.iter())
    {
        // `{}` prints the shortest text that reads back to the same f32.
        line.push_str(&format!(" {v}"));
    }
    line
}

fn parse_stats_line(line: &str) -> Option<(u64, Stamp, FrameLight)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() != 16 {
        return None;
    }
    let n = f[0].parse().ok()?;
    let s = (f[1].parse().ok()?, f[2].parse().ok()?);
    let v: Vec<f32> = f[3..]
        .iter()
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    let mut percentiles = [0.0f32; 9];
    percentiles.copy_from_slice(&v[4..]);
    Some((
        n,
        s,
        FrameLight {
            max_scl: [v[0], v[1], v[2]],
            avg_max_rgb: v[3],
            percentiles,
        },
    ))
}

/// The stats lines of `hdr_dir` made with `header` (the last line per frame wins).
fn read_stats(hdr_dir: &Path, header: &str) -> HashMap<u64, (Stamp, FrameLight)> {
    let Ok(text) = std::fs::read_to_string(hdr_dir.join(STATS_FILE)) else {
        return HashMap::new();
    };
    let Some(body) = text.strip_prefix(header) else {
        return HashMap::new();
    };
    body.lines()
        .filter_map(parse_stats_line)
        .map(|(n, s, f)| (n, (s, f)))
        .collect()
}

/// Whether the HDR version of every `out/` frame `1..=total` has been made (measured) and
/// still matches its SDR frame. With interpolation the measured frames are the full-rate ones
/// in `interp/`; once that folder is deleted, a frame counts when the stats were written
/// after its `out/` frame.
pub fn frame_statuses(job_dir: &Path, total: u64, job: &JobHdr) -> HashMap<u64, bool> {
    let dir = hdr_dir(job_dir);
    let stats = read_stats(&dir, &job.params.header());
    let stats_time = stamp(&dir.join(STATS_FILE)).map(|s| s.1);
    let interp = job_dir.join("interp");
    (1..=total)
        .map(|k| {
            let n = super::hdr10plus::hdr_frame_for_out(k, job.divisor);
            let out = stamp(&crate::ffmpeg::frame_file(&job_dir.join("out"), k));
            let ready = stats.get(&n).is_some_and(|(s, _)| {
                if job.divisor <= 1 {
                    out == Some(*s)
                } else {
                    match stamp(&crate::ffmpeg::frame_file(&interp, n)) {
                        Some(i) => i == *s,
                        None => matches!((out, stats_time), (Some(o), Some(t)) if o.1 <= t),
                    }
                }
            });
            (k, ready)
        })
        .collect()
}

/// `out/` frame `k` of a job converted to HDR the way the export does it and tone mapped back
/// to SDR for the frame browser.
pub fn tone_mapped_frame(job_dir: &Path, k: u64, job: &JobHdr) -> Option<RgbImage> {
    let img = image::open(crate::ffmpeg::frame_file(&job_dir.join("out"), k))
        .ok()?
        .to_rgb8();
    let hdr = job.params.itm().convert(&img);
    Some(super::preview::to_sdr_preview(
        &hdr,
        job.params.mode,
        job.params.peak_nits,
    ))
}

/// Conversion workers and frames in flight for frames of `pixels` pixels.
fn parallelism(pixels: u64) -> (usize, u64) {
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get());
    let workers = (MEMORY_BUDGET / (pixels.max(1) * BYTES_PER_PIXEL)).clamp(1, threads as u64);
    (workers as usize, workers * 2)
}

fn read_frame(path: &Path) -> Result<RgbImage> {
    Ok(image::open(path)
        .with_context(|| format!("could not read frame {}", path.display()))?
        .to_rgb8())
}

/// Pass 1: measures the HDR version of frames `1..=total` of `src_dir` (out/ or interp/),
/// reusing valid lines of `hdr/stats.txt`. Returns the light levels of every frame in order.
/// `progress` gets the number of frames done.
pub fn measure_pass(
    src_dir: &Path,
    hdr_dir: &Path,
    total: u64,
    params: &HdrParams,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<Vec<FrameLight>> {
    let header = params.header();
    std::fs::create_dir_all(hdr_dir)?;
    let settings = hdr_dir.join(SETTINGS_FILE);
    if std::fs::read_to_string(&settings).ok().as_deref() != Some(header.as_str()) {
        let _ = std::fs::remove_file(hdr_dir.join(STATS_FILE));
        std::fs::write(&settings, &header)?;
    }
    let mut known = read_stats(hdr_dir, &header);
    known.retain(|n, (s, _)| {
        *n >= 1 && *n <= total && stamp(&crate::ffmpeg::frame_file(src_dir, *n)) == Some(*s)
    });
    // Rewrite the file with only the valid lines, then append.
    let stats = hdr_dir.join(STATS_FILE);
    let mut text = header.clone();
    let mut kept: Vec<_> = known.iter().collect();
    kept.sort_by_key(|(n, _)| **n);
    for (n, (s, f)) in kept {
        text.push_str(&stats_line(*n, *s, f));
        text.push('\n');
    }
    let part = hdr_dir.join("stats.part.txt");
    std::fs::write(&part, text)?;
    std::fs::rename(&part, &stats)?;

    let todo: Vec<u64> = (1..=total).filter(|n| !known.contains_key(n)).collect();
    let already = total - todo.len() as u64;
    progress(already);
    if !todo.is_empty() {
        let (w, h) = image::image_dimensions(crate::ffmpeg::frame_file(src_dir, todo[0]))
            .context("could not read the first frame to convert")?;
        let (workers, window) = parallelism(w as u64 * h as u64);
        let itm = params.itm();
        let pq = params.mode.is_pq();
        let mut file = std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&stats)
                .context("could not open hdr/stats.txt")?,
        );
        let mut done = 0;
        let result = ordered_parallel(
            todo.len() as u64,
            workers,
            window,
            cancel,
            |i| {
                let n = todo[i as usize - 1];
                let path = crate::ffmpeg::frame_file(src_dir, n);
                let s = stamp(&path).ok_or_else(|| anyhow!("frame {} missing", path.display()))?;
                // HLG has no light-level metadata: nothing to measure.
                let light = if pq {
                    measure_frame(&itm.convert(&read_frame(&path)?))
                } else {
                    FrameLight {
                        max_scl: [0.0; 3],
                        avg_max_rgb: 0.0,
                        percentiles: [0.0; 9],
                    }
                };
                Ok((n, s, light))
            },
            |_, (n, s, light)| {
                writeln!(file, "{}", stats_line(n, s, &light))?;
                known.insert(n, (s, light));
                done += 1;
                if done % FLUSH_EVERY == 0 {
                    file.flush()?;
                }
                progress(already + done);
                Ok(())
            },
        );
        file.flush()?;
        result?;
    }
    (1..=total)
        .map(|n| {
            known
                .remove(&n)
                .map(|(_, f)| f)
                .ok_or_else(|| anyhow!("no light levels for frame {n}"))
        })
        .collect()
}

/// Size of the frames pass 2 streams (all frames of a job have the size of the first).
pub fn frame_size(src_dir: &Path) -> Result<(u32, u32)> {
    let first = crate::ffmpeg::frame_file(src_dir, 1);
    image::image_dimensions(&first).with_context(|| format!("could not read {}", first.display()))
}

/// Pass 2: converts frames `1..=total` of `src_dir` again, in parallel, and writes them in
/// order to `w` as raw `rgb48le`. Stops with an error when `cancel` is set or `w` fails.
pub fn stream_frames(
    src_dir: &Path,
    total: u64,
    params: &HdrParams,
    cancel: &AtomicBool,
    w: &mut dyn Write,
) -> Result<()> {
    let (fw, fh) = frame_size(src_dir)?;
    let (workers, window) = parallelism(fw as u64 * fh as u64);
    let itm = params.itm();
    ordered_parallel(
        total,
        workers,
        window,
        cancel,
        |n| {
            let img = read_frame(&crate::ffmpeg::frame_file(src_dir, n))?;
            if img.dimensions() != (fw, fh) {
                bail!(
                    "frame {n} is {}x{}, the others {fw}x{fh}",
                    img.width(),
                    img.height()
                );
            }
            let hdr = itm.convert(&img);
            let mut bytes = Vec::with_capacity(hdr.as_raw().len() * 2);
            for v in hdr.as_raw() {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            Ok(bytes)
        },
        |_, bytes| {
            w.write_all(&bytes)
                .context("the encoder stopped taking frames")
        },
    )
}

/// Runs `make(1..=count)` on `workers` threads and hands the results to `sink` in order on
/// this thread. At most `window` results wait ahead of the sink, which bounds the memory.
/// Stops at the first error (of `make` or `sink`) or when `cancel` is set.
fn ordered_parallel<T: Send>(
    count: u64,
    workers: usize,
    window: u64,
    cancel: &AtomicBool,
    make: impl Fn(u64) -> Result<T> + Sync,
    mut sink: impl FnMut(u64, T) -> Result<()>,
) -> Result<()> {
    struct State<T> {
        ready: BTreeMap<u64, Result<T>>,
        /// The next index the sink takes.
        next: u64,
        stop: bool,
    }
    let state = Mutex::new(State {
        ready: BTreeMap::new(),
        next: 1,
        stop: false,
    });
    let changed = Condvar::new();
    let next_job = AtomicU64::new(1);
    let window = window.max(1);
    std::thread::scope(|scope| {
        for _ in 0..workers.clamp(1, count.max(1) as usize) {
            scope.spawn(|| loop {
                let n = next_job.fetch_add(1, Ordering::Relaxed);
                if n > count {
                    return;
                }
                {
                    let mut st = state.lock().unwrap();
                    while !st.stop && n >= st.next + window {
                        st = changed.wait(st).unwrap();
                    }
                    if st.stop {
                        return;
                    }
                }
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let r = make(n);
                state.lock().unwrap().ready.insert(n, r);
                changed.notify_all();
            });
        }
        let result = (|| {
            for n in 1..=count {
                let item = {
                    let mut st = state.lock().unwrap();
                    loop {
                        if let Some(r) = st.ready.remove(&n) {
                            st.next = n + 1;
                            break r;
                        }
                        if cancel.load(Ordering::Relaxed) {
                            bail!(Cancelled);
                        }
                        st = changed
                            .wait_timeout(st, Duration::from_millis(100))
                            .unwrap()
                            .0;
                    }
                };
                changed.notify_all();
                sink(n, item?)?;
            }
            Ok(())
        })();
        state.lock().unwrap().stop = true;
        changed.notify_all();
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn write_sdr(dir: &Path, n: u64, bright: u8) {
        std::fs::create_dir_all(dir).unwrap();
        let img = RgbImage::from_fn(16, 8, |x, _| match x {
            0..4 => Rgb([bright, bright, bright]),
            4..8 => Rgb([118, 118, 118]),
            _ => Rgb([60, 60, 60]),
        });
        img.save(crate::ffmpeg::frame_file(dir, n)).unwrap();
    }

    fn params(peak: u32) -> HdrParams {
        HdrParams {
            mode: HdrMode::Hdr10,
            peak_nits: peak,
            sdr_white_nits: 203,
        }
    }

    fn setup(total: u64) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let (o, h) = (t.path().join("out"), t.path().join("hdr"));
        for n in 1..=total {
            write_sdr(&o, n, 255);
        }
        (t, o, h)
    }

    fn measure(o: &Path, h: &Path, total: u64, p: &HdrParams) -> Vec<FrameLight> {
        measure_pass(o, h, total, p, &AtomicBool::new(false), |_| {}).unwrap()
    }

    /// Frame numbers the last pass converted, from the order of the stats lines after the
    /// header: lines appended by a pass come after the kept ones.
    fn lines(h: &Path) -> Vec<u64> {
        std::fs::read_to_string(h.join(STATS_FILE))
            .unwrap()
            .lines()
            .filter_map(parse_stats_line)
            .map(|(n, _, _)| n)
            .collect()
    }

    #[test]
    fn hlg_and_settings_use_fixed_peak_and_white() {
        let mut s = Settings::default();
        s.hdr_mode = HdrMode::Hlg;
        s.hdr_peak_nits = 1500;
        s.hdr_sdr_white_nits = 300;
        let p = HdrParams::from_settings(&s).unwrap();
        assert_eq!((p.peak_nits, p.sdr_white_nits), (1000, 300));
        s.hdr_mode = HdrMode::Hdr10;
        assert_eq!(HdrParams::from_settings(&s).unwrap().peak_nits, 1500);
        s.hdr_mode = HdrMode::Off;
        assert_eq!(HdrParams::from_settings(&s), None);
        assert_eq!(parse_header(&p.header()), Some(p));
    }

    #[test]
    fn measures_without_storing_frames() {
        let (t, o, h) = setup(3);
        let mut last = 0;
        let m = measure_pass(&o, &h, 3, &params(1000), &AtomicBool::new(false), |d| {
            last = d
        })
        .unwrap();
        assert_eq!(last, 3);
        assert_eq!(m.len(), 3);
        // White expands to the peak, the brightest channel reaches ~1000 nits.
        assert!(m[0].max_scl[1] > 900.0, "{:?}", m[0]);
        // Nothing but the two text files in hdr/.
        let mut names: Vec<String> = std::fs::read_dir(&h)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["settings.txt", "stats.txt"]);
        let job = job_hdr(t.path()).unwrap();
        assert_eq!(job.params, params(1000));
        assert_eq!(job.divisor, 1);
    }

    #[test]
    fn stats_round_trip_exactly() {
        let f = FrameLight {
            max_scl: [999.123_4, 0.000_123, 203.0],
            avg_max_rgb: 12.345_678,
            percentiles: [0.0, 0.1, 0.2, 1.0, 2.5, 7.0, 13.333_333, 50.0, 1000.0],
        };
        let (n, s, back) = parse_stats_line(&stats_line(7, (123, 456), &f)).unwrap();
        assert_eq!((n, s), (7, (123, 456)));
        assert_eq!(back, f);
        assert!(parse_stats_line("7 123 45").is_none());
    }

    #[test]
    fn resumes_and_redoes_only_changed_frames() {
        let (t, o, h) = setup(4);
        let first = measure(&o, &h, 4, &params(1000));
        assert_eq!(lines(&h), [1, 2, 3, 4]);
        // Same settings: nothing converted again, same values.
        assert_eq!(measure(&o, &h, 4, &params(1000)), first);
        assert_eq!(lines(&h), [1, 2, 3, 4]);
        // A remade SDR frame (rejected and made again): only it is converted again.
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_sdr(&o, 2, 200);
        let again = measure(&o, &h, 4, &params(1000));
        assert_eq!(lines(&h), [1, 3, 4, 2]);
        assert!(again[1].max_scl[1] < first[1].max_scl[1]);
        let job = job_hdr(t.path()).unwrap();
        assert!(frame_statuses(t.path(), 4, &job).values().all(|r| *r));
        // A frame newer than its line is not ready in the browser.
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_sdr(&o, 3, 250);
        let st = frame_statuses(t.path(), 4, &job);
        assert!(!st[&3] && st[&4]);
        // A half-written last line (a crash) is ignored.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(h.join(STATS_FILE))
            .unwrap();
        write!(f, "4 12 3").unwrap();
        drop(f);
        measure(&o, &h, 4, &params(1000));
        assert_eq!(lines(&h), [1, 2, 4, 3]);
    }

    #[test]
    fn changed_settings_measure_again() {
        let (_t, o, h) = setup(2);
        let a = measure(&o, &h, 2, &params(1000));
        let b = measure(&o, &h, 2, &params(1500));
        assert!(b[0].max_scl[1] > a[0].max_scl[1] * 1.3, "{a:?} {b:?}");
        let c = measure(
            &o,
            &h,
            2,
            &HdrParams {
                sdr_white_nits: 300,
                ..params(1000)
            },
        );
        // Mid grey sits higher with a brighter SDR white.
        assert!(
            c[0].percentiles[4] > a[0].percentiles[4] * 1.3,
            "{a:?} {c:?}"
        );
    }

    #[test]
    fn interpolated_frames_status_after_interp_is_deleted() {
        let (t, o, h) = setup(2);
        // Full-rate frames 1..=4 in interp/ for out/ frames 1, 2 (divisor 2).
        let interp = t.path().join("interp");
        for n in 1..=4 {
            write_sdr(&interp, n, 255);
        }
        std::fs::write(
            interp.join(SETTINGS_FILE),
            "fps: 24/1\ndivisor: 2\nframes: 4\nversion: 1\n",
        )
        .unwrap();
        measure(&interp, &h, 4, &params(1000));
        let job = job_hdr(t.path()).unwrap();
        assert_eq!(job.divisor, 2);
        assert!(frame_statuses(t.path(), 2, &job).values().all(|r| *r));
        std::fs::remove_dir_all(&interp).unwrap();
        assert!(frame_statuses(t.path(), 2, &job).values().all(|r| *r));
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_sdr(&o, 2, 200);
        let st = frame_statuses(t.path(), 2, &job);
        assert!(st[&1] && !st[&2], "{st:?}");
    }

    #[test]
    fn stream_matches_the_measured_frames() {
        let (t, o, h) = setup(3);
        let p = params(1000);
        let m = measure(&o, &h, 3, &p);
        let mut bytes = Vec::new();
        stream_frames(&o, 3, &p, &AtomicBool::new(false), &mut bytes).unwrap();
        assert_eq!(bytes.len(), 3 * 16 * 8 * 6);
        for (i, chunk) in bytes.chunks_exact(16 * 8 * 6).enumerate() {
            let raw: Vec<u16> = chunk
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            let img = super::super::itm::Frame16::from_raw(16, 8, raw).unwrap();
            assert_eq!(measure_frame(&img), m[i]);
        }
        // The browser's picture is the same conversion, tone mapped.
        let job = job_hdr(t.path()).unwrap();
        assert!(tone_mapped_frame(t.path(), 1, &job).is_some());
    }

    #[test]
    fn ordered_parallel_keeps_order_and_bounds_the_window() {
        let ahead = AtomicU64::new(0);
        let taken = AtomicU64::new(0);
        let mut seen = Vec::new();
        ordered_parallel(
            200,
            8,
            4,
            &AtomicBool::new(false),
            |n| {
                // Never more than `window` results beyond what the sink took.
                let t = taken.load(Ordering::SeqCst);
                ahead.fetch_max(n.saturating_sub(t), Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_micros((n * 7919 % 13) * 50));
                Ok(n * 10)
            },
            |n, v| {
                assert_eq!(v, n * 10);
                seen.push(n);
                taken.store(n, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, (1..=200).collect::<Vec<_>>());
        assert!(
            ahead.load(Ordering::SeqCst) <= 5,
            "{}",
            ahead.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn ordered_parallel_stops_on_error_and_cancel() {
        let r = ordered_parallel(
            50,
            4,
            8,
            &AtomicBool::new(false),
            |n| if n == 20 { bail!("bad frame") } else { Ok(n) },
            |_, _| Ok(()),
        );
        assert!(r.unwrap_err().to_string().contains("bad frame"));
        let r = ordered_parallel(50, 4, 8, &AtomicBool::new(false), Ok, |n, _| {
            if n == 3 {
                bail!("pipe closed")
            }
            Ok(())
        });
        assert!(r.is_err());
        let r = ordered_parallel(50, 4, 8, &AtomicBool::new(true), Ok, |_, _| Ok(()));
        assert!(crate::process::is_cancelled(&r.unwrap_err()));
    }
}
