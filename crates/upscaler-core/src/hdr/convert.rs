//! The HDR conversion stage: `out/%06d.png` (SDR) to `hdr/%06d.png` (16-bit PQ or HLG),
//! parallel over frames, cached by `hdr/settings.txt`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;

use anyhow::{bail, Context, Result};
use image::{ImageBuffer, Rgb};

use super::itm::Itm;
use crate::settings::HdrMode;

/// Bump when the curve or the encoding changes, so old conversions are redone.
pub const HDR_VERSION: u32 = 1;
pub const HDR_DIR: &str = "hdr";
const SETTINGS_FILE: &str = "settings.txt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrStatus {
    Missing,
    Converting,
    Ready,
}

pub fn hdr_dir(job_dir: &Path) -> PathBuf {
    job_dir.join(HDR_DIR)
}

fn settings_text(mode: HdrMode, peak_nits: u32) -> String {
    format!(
        "mode: {}\npeak: {peak_nits}\nversion: {HDR_VERSION}\n",
        mode.name()
    )
}

fn frame_path(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{n:06}.png"))
}

/// True when `hdr` is missing or older than its SDR source `out`.
fn is_stale(out: &Path, hdr: &Path) -> bool {
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (modified(hdr), modified(out)) {
        (Some(h), Some(o)) => o > h,
        (None, _) => true,
        (Some(_), None) => false,
    }
}

fn all_present(dir: &Path, total: u64) -> bool {
    (1..=total).all(|n| frame_path(dir, n).is_file())
}

/// Whether the job folder has converted frames: `Ready` when `settings.txt` exists and all
/// `n` frames are there, `Converting` when only some are, otherwise `Missing`.
pub fn hdr_status(job_dir: &Path, n: u64) -> HdrStatus {
    let dir = hdr_dir(job_dir);
    if !dir.join(SETTINGS_FILE).is_file() {
        return HdrStatus::Missing;
    }
    if n > 0 && all_present(&dir, n) {
        HdrStatus::Ready
    } else {
        HdrStatus::Converting
    }
}

/// What a job's `hdr/` folder was made with, read from its `settings.txt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobHdr {
    pub mode: HdrMode,
    pub peak_nits: u32,
    /// Full-rate frames per `out/` frame (2/3 with "every 2nd/3rd frame").
    pub divisor: u32,
}

/// The HDR settings of the job in `job_dir`, or `None` when it has no `hdr/` folder (HDR was
/// off, or the frames were deleted after the export).
pub fn job_hdr(job_dir: &Path) -> Option<JobHdr> {
    let text = std::fs::read_to_string(hdr_dir(job_dir).join(SETTINGS_FILE)).ok()?;
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().to_string())
    };
    let name = value("mode:")?;
    let mode = [HdrMode::Hdr10Plus, HdrMode::Hdr10, HdrMode::Hlg]
        .into_iter()
        .find(|m| m.name() == name)?;
    let peak_nits = value("peak:")?.parse().ok()?;
    // The interpolation stage's marker says how many full-rate frames one AI frame covers.
    let divisor = std::fs::read_to_string(job_dir.join("interp").join(SETTINGS_FILE))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("divisor:"))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(1);
    Some(JobHdr {
        mode,
        peak_nits,
        divisor,
    })
}

/// File name to modification time of the PNGs in `dir`, from one directory read.
fn mtimes_of(dir: &Path) -> std::collections::HashMap<String, Option<std::time::SystemTime>> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Default::default();
    };
    rd.flatten()
        .map(|e| {
            let m = e.metadata().and_then(|m| m.modified()).ok();
            (e.file_name().to_string_lossy().into_owned(), m)
        })
        .collect()
}

/// The state of the `hdr/` frame of every `out/` frame `1..=total`, from one directory read
/// of `hdr/` (and of `out/` when the stale check applies). A frame is `Ready` when its file
/// is there and, without interpolation, not older than its SDR source (it is converted
/// again at the next export); otherwise `Converting`.
pub fn hdr_statuses(
    job_dir: &Path,
    total: u64,
    divisor: u32,
) -> std::collections::HashMap<u64, HdrStatus> {
    let hdr = mtimes_of(&hdr_dir(job_dir));
    let out = if divisor <= 1 {
        mtimes_of(&job_dir.join("out"))
    } else {
        Default::default()
    };
    let name = |n: u64| format!("{n:06}.png");
    (1..=total)
        .map(|k| {
            let h = hdr.get(&name(super::hdr10plus::hdr_frame_for_out(k, divisor)));
            let ready = match h {
                None => false,
                Some(h) => match (h, out.get(&name(k)).copied().flatten()) {
                    (Some(h), Some(o)) if divisor <= 1 => o <= *h,
                    _ => true,
                },
            };
            (
                k,
                if ready {
                    HdrStatus::Ready
                } else {
                    HdrStatus::Converting
                },
            )
        })
        .collect()
}

/// The `hdr/` frame file of `out/` frame `k`.
pub fn hdr_frame_file(job_dir: &Path, k: u64, divisor: u32) -> PathBuf {
    frame_path(
        &hdr_dir(job_dir),
        super::hdr10plus::hdr_frame_for_out(k, divisor),
    )
}

/// Converts frames `1..=total` of `out_dir` into `hdr_dir`. Does nothing when the folder was
/// made with the same mode, peak and version and is complete; otherwise clears and redoes it.
/// `progress` gets the number of frames done; stops with an error when `cancel` is set.
pub fn convert(
    out_dir: &Path,
    hdr_dir: &Path,
    total: u64,
    mode: HdrMode,
    peak_nits: u32,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    let want = settings_text(mode, peak_nits);
    let have = std::fs::read_to_string(hdr_dir.join(SETTINGS_FILE)).ok();
    let todo: Vec<u64> = if have.as_deref() == Some(want.as_str()) {
        // Same settings: only frames that are missing or whose SDR frame was made again
        // (a rejected frame replaced in out/) since they were converted.
        (1..=total)
            .filter(|&n| is_stale(&frame_path(out_dir, n), &frame_path(hdr_dir, n)))
            .collect()
    } else {
        let _ = std::fs::remove_dir_all(hdr_dir);
        std::fs::create_dir_all(hdr_dir)?;
        std::fs::write(hdr_dir.join(SETTINGS_FILE), &want)?;
        (1..=total).collect()
    };
    let already = total - todo.len() as u64;
    if todo.is_empty() {
        progress(total);
        return Ok(());
    }

    let itm = Itm::new(mode, peak_nits);
    let next = AtomicU64::new(0);
    let count = todo.len() as u64;
    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .min(todo.len());
    let (tx, rx) = mpsc::channel::<Result<()>>();
    let mut result = Ok(());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let (itm, next, todo) = (&itm, &next, &todo);
            scope.spawn(move || loop {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= count {
                    return;
                }
                let n = todo[i as usize];
                let r = convert_frame(itm, &frame_path(out_dir, n), &frame_path(hdr_dir, n));
                let failed = r.is_err();
                let _ = tx.send(r);
                if failed {
                    next.store(count, Ordering::Relaxed);
                    return;
                }
            });
        }
        drop(tx);
        let mut done = 0;
        for r in rx {
            match r {
                Ok(()) => {
                    done += 1;
                    progress(already + done);
                }
                Err(e) if result.is_ok() => result = Err(e),
                Err(_) => {}
            }
        }
    });
    result?;
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    Ok(())
}

fn convert_frame(itm: &Itm, src: &Path, dst: &Path) -> Result<()> {
    let img = image::open(src)
        .with_context(|| format!("could not read frame {}", src.display()))?
        .into_rgb16();
    let (w, h) = img.dimensions();
    let mut raw = Vec::with_capacity(img.as_raw().len());
    for px in img.as_raw().chunks_exact(3) {
        raw.extend_from_slice(&itm.pixel([px[0], px[1], px[2]]));
    }
    let out: ImageBuffer<Rgb<u16>, Vec<u16>> =
        ImageBuffer::from_raw(w, h, raw).context("bad frame buffer")?;
    // Written under a temporary name so a half-written frame never counts as done.
    let tmp = dst.with_extension("tmp");
    out.save_with_format(&tmp, image::ImageFormat::Png)
        .with_context(|| format!("could not write {}", tmp.display()))?;
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hdr::measure::{measure_frame, pq_to_nits};
    use image::RgbImage;

    fn write_sdr(dir: &Path, n: u64) {
        std::fs::create_dir_all(dir).unwrap();
        let mut img = RgbImage::new(4, 2);
        for (i, p) in img.pixels_mut().enumerate() {
            // pixel 0 white, pixel 1 mid grey (linear ~0.18), the rest dark
            *p = match i {
                0 => Rgb([255, 255, 255]),
                1 => Rgb([118, 118, 118]),
                _ => Rgb([60, 60, 60]),
            };
        }
        img.save(frame_path(dir, n)).unwrap();
    }

    fn setup(total: u64) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let (o, h) = (t.path().join("out"), t.path().join("hdr"));
        for n in 1..=total {
            write_sdr(&o, n);
        }
        (t, o, h)
    }

    fn run(o: &Path, h: &Path, total: u64, peak: u32) -> Result<()> {
        convert(
            o,
            h,
            total,
            HdrMode::Hdr10,
            peak,
            &AtomicBool::new(false),
            |_| {},
        )
    }

    #[test]
    fn converts_to_16bit_pq() {
        let (_t, o, h) = setup(3);
        let mut last = 0;
        convert(
            &o,
            &h,
            3,
            HdrMode::Hdr10,
            1000,
            &AtomicBool::new(false),
            |d| last = d,
        )
        .unwrap();
        assert_eq!(last, 3);
        for n in 1..=3 {
            let img = image::open(frame_path(&h, n)).unwrap();
            assert!(matches!(img, image::DynamicImage::ImageRgb16(_)), "16-bit");
            let img = img.into_rgb16();
            let light = measure_frame(&img);
            assert!(light.max_scl[1] >= 203.0, "white {:?}", light.max_scl);
            let g = pq_to_nits(img.get_pixel(1, 0).0[1] as f32 / 65535.0);
            let sdr = (118.0f32 / 255.0).powf(2.4) * 203.0;
            assert!((g - sdr).abs() / sdr < 0.25, "{g} vs {sdr}");
        }
    }

    fn mtimes(h: &Path, total: u64) -> Vec<std::time::SystemTime> {
        (1..=total)
            .map(|n| {
                std::fs::metadata(frame_path(h, n))
                    .unwrap()
                    .modified()
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn cached_when_settings_match() {
        let (t, o, h) = setup(3);
        assert_eq!(hdr_status(t.path(), 3), HdrStatus::Missing);
        run(&o, &h, 3, 1000).unwrap();
        assert_eq!(hdr_status(t.path(), 3), HdrStatus::Ready);
        let before = mtimes(&h, 3);
        std::thread::sleep(std::time::Duration::from_millis(50));
        run(&o, &h, 3, 1000).unwrap();
        assert_eq!(before, mtimes(&h, 3));
    }

    #[test]
    fn job_hdr_and_frame_status() {
        let (t, o, h) = setup(3);
        assert_eq!(job_hdr(t.path()), None);
        run(&o, &h, 3, 1000).unwrap();
        let j = job_hdr(t.path()).unwrap();
        assert_eq!((j.mode, j.peak_nits, j.divisor), (HdrMode::Hdr10, 1000, 1));
        assert_eq!(hdr_statuses(t.path(), 3, 1)[&2], HdrStatus::Ready);
        std::fs::remove_file(frame_path(&h, 2)).unwrap();
        assert_eq!(hdr_statuses(t.path(), 3, 1)[&2], HdrStatus::Converting);
        assert_eq!(hdr_statuses(t.path(), 3, 1)[&3], HdrStatus::Ready);
        std::thread::sleep(std::time::Duration::from_millis(50));
        RgbImage::from_pixel(4, 2, Rgb([9, 9, 9]))
            .save(frame_path(&o, 3))
            .unwrap();
        assert_eq!(hdr_statuses(t.path(), 3, 1)[&3], HdrStatus::Converting);
        assert_eq!(hdr_statuses(t.path(), 3, 3)[&3], HdrStatus::Converting);
        // With interpolation out/2 is hdr/4 (divisor 3) and a missing hdr/2 does not matter.
        assert_eq!(hdr_statuses(t.path(), 3, 3)[&2], HdrStatus::Converting);
        std::fs::create_dir_all(t.path().join("interp")).unwrap();
        std::fs::write(
            t.path().join("interp").join(SETTINGS_FILE),
            "fps: 24/1\ndivisor: 3\nframes: 9\nversion: 1\n",
        )
        .unwrap();
        assert_eq!(job_hdr(t.path()).unwrap().divisor, 3);
    }

    #[test]
    fn replaced_sdr_frame_is_reconverted() {
        let (_t, o, h) = setup(3);
        run(&o, &h, 3, 1000).unwrap();
        let before = mtimes(&h, 3);
        let old2 = std::fs::read(frame_path(&h, 2)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // A different picture in out/2 (as when a rejected frame is made again).
        RgbImage::from_pixel(4, 2, Rgb([20, 20, 20]))
            .save(frame_path(&o, 2))
            .unwrap();
        run(&o, &h, 3, 1000).unwrap();
        let after = mtimes(&h, 3);
        assert_eq!(before[0], after[0]);
        assert_eq!(before[2], after[2]);
        assert_ne!(before[1], after[1]);
        assert_ne!(old2, std::fs::read(frame_path(&h, 2)).unwrap());
    }

    #[test]
    fn changed_peak_reconverts() {
        let (_t, o, h) = setup(2);
        run(&o, &h, 2, 1000).unwrap();
        let a = measure_frame(&image::open(frame_path(&h, 1)).unwrap().into_rgb16());
        run(&o, &h, 2, 4000).unwrap();
        let b = measure_frame(&image::open(frame_path(&h, 1)).unwrap().into_rgb16());
        assert!(b.max_scl[1] > a.max_scl[1] * 2.0, "{a:?} {b:?}");
    }

    #[test]
    fn cancel_stops() {
        let (_t, o, h) = setup(3);
        let cancel = AtomicBool::new(true);
        assert!(convert(&o, &h, 3, HdrMode::Hdr10, 1000, &cancel, |_| {}).is_err());
    }
}
