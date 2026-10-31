//! FFmpeg and FFprobe: finding them, probing a video, grabbing single frames and
//! thumbnails, extracting a range of frames and encoding the result with the original audio.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicBool;

use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use serde_json::Value;

use crate::process::{self, command};

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

impl Tools {
    /// Looks in `dir` (from the settings) if given, next to the app, in the downloaded tools and on PATH.
    pub fn locate(dir: &str) -> Option<Tools> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if !dir.trim().is_empty() {
            dirs.push(PathBuf::from(dir.trim()));
        }
        if let Some(app_dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
        {
            dirs.push(app_dir.join("ffmpeg"));
            dirs.push(app_dir.clone());
        }
        dirs.push(crate::paths::tools_dir().join("bin"));
        if cfg!(target_os = "macos") {
            dirs.push("/opt/homebrew/bin".into());
            dirs.push("/usr/local/bin".into());
        }
        for d in dirs {
            let (f, p) = (d.join(exe("ffmpeg")), d.join(exe("ffprobe")));
            if f.is_file() && p.is_file() {
                return Some(Tools {
                    ffmpeg: f,
                    ffprobe: p,
                });
            }
        }
        match (which::which("ffmpeg"), which::which("ffprobe")) {
            (Ok(f), Ok(p)) => Some(Tools {
                ffmpeg: f,
                ffprobe: p,
            }),
            _ => None,
        }
    }

    fn ffmpeg_cmd(&self) -> std::process::Command {
        let mut c = command(&self.ffmpeg);
        c.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
        c
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoInfo {
    pub path: PathBuf,
    /// Display size in square pixels (what everything downstream works with). DVDs store
    /// 720x480/576 with non-square pixels; this is the size they are shown at, e.g. 854x480.
    pub width: u32,
    pub height: u32,
    /// Size as stored in the file.
    pub coded_width: u32,
    pub coded_height: u32,
    /// Interlaced (typical for DVD and TV): frames are deinterlaced when decoded.
    pub interlaced: bool,
    /// Timestamp (s) of the first video frame; frame `n` is at `start_time + n / fps`.
    pub start_time: f64,
    /// Frame rate as a fraction.
    pub fps_num: u32,
    pub fps_den: u32,
    pub duration: f64,
    pub frame_count: u64,
    pub video_codec: String,
    pub has_audio: bool,
    pub audio_codec: Option<String>,
    pub file_size: u64,
    pub modified: u64,
}

impl VideoInfo {
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den.max(1) as f64
    }

    /// Start time in seconds of frame `index`.
    pub fn frame_time(&self, index: u64) -> f64 {
        index as f64 / self.fps()
    }

    pub fn frame_at(&self, seconds: f64) -> u64 {
        ((seconds * self.fps()).floor().max(0.0) as u64).min(self.frame_count.saturating_sub(1))
    }

    /// Identifies this exact file (path, size and modification time) for caches.
    pub fn identity(&self) -> String {
        format!(
            "{}|{}|{}",
            self.path.display(),
            self.file_size,
            self.modified
        )
    }
}

fn parse_ratio(s: &str) -> Option<(u32, u32)> {
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let (n, d) = (n.trim().parse::<f64>().ok()?, d.trim().parse::<f64>().ok()?);
    if n <= 0.0 || d <= 0.0 {
        return None;
    }
    if n.fract() == 0.0 && d.fract() == 0.0 {
        Some((n as u32, d as u32))
    } else {
        Some(((n / d * 1000.0).round() as u32, 1000))
    }
}

/// Parses `ffprobe -show_streams -show_format -print_format json` output.
pub fn parse_probe(path: &Path, json: &str) -> Result<VideoInfo> {
    let v: Value = serde_json::from_str(json).context("ffprobe output is not JSON")?;
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let video = streams
        .iter()
        .find(|s| {
            s["codec_type"] == "video"
                && s["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 0
        })
        .ok_or_else(|| anyhow!("the file has no video stream"))?;
    let audio = streams.iter().find(|s| s["codec_type"] == "audio");
    let mut width = video["width"].as_u64().unwrap_or(0) as u32;
    let mut height = video["height"].as_u64().unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        bail!("the video has no size");
    }
    let (coded_width, coded_height) = (width, height);
    // Non-square pixels (DVD, DV, some TV recordings): widen to the display aspect ratio.
    if let Some((n, d)) = video["sample_aspect_ratio"]
        .as_str()
        .and_then(|s| s.split_once(':'))
        .and_then(|(n, d)| Some((n.parse::<f64>().ok()?, d.parse::<f64>().ok()?)))
        .filter(|(n, d)| *n > 0.0 && *d > 0.0 && (n / d - 1.0).abs() > 0.01)
    {
        width = ((width as f64 * n / d / 2.0).round() as u32).max(1) * 2;
    }
    let interlaced = matches!(
        video["field_order"].as_str(),
        Some("tt" | "bb" | "tb" | "bt")
    );
    // Phone videos are often stored sideways with a rotation tag; ffmpeg rotates them on decode.
    let rotation = video["side_data_list"]
        .as_array()
        .and_then(|l| l.iter().find_map(|d| d["rotation"].as_f64()))
        .or_else(|| {
            video["tags"]["rotate"]
                .as_str()
                .and_then(|r| r.parse().ok())
        })
        .unwrap_or(0.0);
    if (rotation.abs() - 90.0).abs() < 1.0 || (rotation.abs() - 270.0).abs() < 1.0 {
        std::mem::swap(&mut width, &mut height);
    }
    let (fps_num, fps_den) = video["avg_frame_rate"]
        .as_str()
        .and_then(parse_ratio)
        .or_else(|| video["r_frame_rate"].as_str().and_then(parse_ratio))
        .unwrap_or((25, 1));
    let num = |x: &Value| {
        x.as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| x.as_f64())
    };
    let duration = num(&video["duration"])
        .or_else(|| num(&v["format"]["duration"]))
        .unwrap_or(0.0);
    let fps = fps_num as f64 / fps_den as f64;
    let frame_count = video["nb_frames"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| (duration * fps).round() as u64)
        .max(1);
    let start_time = num(&video["start_time"])
        .or_else(|| num(&v["format"]["start_time"]))
        .unwrap_or(0.0);
    let meta = std::fs::metadata(path).ok();
    Ok(VideoInfo {
        path: path.to_path_buf(),
        width,
        height,
        coded_width,
        coded_height,
        interlaced,
        start_time,
        fps_num,
        fps_den,
        duration,
        frame_count,
        video_codec: video["codec_name"].as_str().unwrap_or("?").to_string(),
        has_audio: audio.is_some(),
        audio_codec: audio
            .and_then(|a| a["codec_name"].as_str())
            .map(str::to_string),
        file_size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
        modified: meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0),
    })
}

pub fn probe(tools: &Tools, path: &Path) -> Result<VideoInfo> {
    let mut cmd = command(&tools.ffprobe);
    cmd.args([
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_streams",
        "-show_format",
    ])
    .arg(path);
    let out = process::run(
        cmd,
        None,
        &AtomicBool::new(false),
        Some(std::time::Duration::from_secs(60)),
    )?;
    if !out.success {
        bail!("ffprobe: {}", out.tail(5));
    }
    parse_probe(path, &out.stdout_text())
}

/// Seek time that lands exactly on frame `index` (a hair before its timestamp).
/// The filter chain every decoded frame goes through: deinterlace when the video is
/// interlaced, then scale to `w`x`h` square pixels.
pub fn frame_filter(info: &VideoInfo, w: u32, h: u32) -> String {
    filter_chain(info, None, w, h)
}

/// Deinterlace (with all decoded frames as context), then `select`, then scale.
fn filter_chain(info: &VideoInfo, select: Option<&str>, w: u32, h: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if info.interlaced {
        // bwdif only touches frames flagged as interlaced; progressive ones pass through.
        parts.push("bwdif=mode=send_frame:parity=auto:deint=interlaced".into());
    }
    if let Some(s) = select {
        parts.push(s.to_string());
    }
    parts.push(format!("scale={w}:{h}:flags=bicubic,setsar=1"));
    parts.join(",")
}

/// How far before a frame decoding starts, so the deinterlacer has neighbouring fields and
/// the exact frame can be picked by its timestamp.
const PRE_ROLL: f64 = 1.0;

/// Input options that seek to just before frame `index` and keep the file's own
/// timestamps (`-copyts`), plus the `select` expression that lets exactly frames from
/// `index` on through. Accurate seeking alone is off by one on some MPEG-2/DVD files, and it
/// starts the deinterlacer without the previous field.
fn seek_args(info: &VideoInfo, index: u64) -> (Vec<String>, String) {
    let t = info.frame_time(index);
    let pre = (t - PRE_ROLL).max(0.0);
    let args = vec![
        "-copyts".to_string(),
        "-noaccurate_seek".to_string(),
        "-ss".to_string(),
        format!("{pre:.4}"),
    ];
    // Timestamps keep the file's start offset; take the frame whose time is closest to `t`.
    let from = info.start_time + t - 0.5 / info.fps();
    (args, format!("select=gte(t\\,{from:.5})"))
}

/// Decodes frame `index` at full size. `fast` seeks to the nearest keyframe instead (thumbnails).
pub fn grab_frame(
    tools: &Tools,
    info: &VideoInfo,
    index: u64,
    scale_height: Option<u32>,
    fast: bool,
) -> Result<RgbImage> {
    let (w, h) = match scale_height {
        Some(sh) => (
            ((info.width as f64 * sh as f64 / info.height as f64 / 2.0).round() as u32).max(1) * 2,
            sh,
        ),
        None => (info.width, info.height),
    };
    let mut cmd = tools.ffmpeg_cmd();
    let filter = if fast {
        // Thumbnails: the nearest keyframe is close enough and much faster.
        cmd.args(["-noaccurate_seek", "-ss"])
            .arg(format!("{:.4}", info.frame_time(index)));
        frame_filter(info, w, h)
    } else {
        let (args, select) = seek_args(info, index);
        cmd.args(args);
        filter_chain(info, Some(&select), w, h)
    };
    cmd.arg("-i")
        .arg(&info.path)
        .args(["-frames:v", "1", "-an", "-sn", "-vf"])
        .arg(filter)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"]);
    let out = process::run(
        cmd,
        None,
        &AtomicBool::new(false),
        Some(std::time::Duration::from_secs(60)),
    )?;
    let need = (w * h * 3) as usize;
    if out.stdout.len() < need {
        // Past the last decodable frame (frame counts are estimates): try one earlier.
        if index > 0 && out.success {
            return grab_frame(tools, info, index - 1, scale_height, fast);
        }
        bail!("could not decode frame {index}: {}", out.tail(4));
    }
    RgbImage::from_raw(w, h, out.stdout[..need].to_vec()).ok_or_else(|| anyhow!("bad frame buffer"))
}

/// What [`extract_range`] produces.
#[derive(Clone, Debug)]
pub struct Range {
    /// First source frame.
    pub start: u64,
    /// Number of source frames.
    pub count: u64,
}

/// Writes frames of `range` as `dir/000001.png`, … at `fps / divisor`. Calls `progress(done)`.
/// Returns the number of frames written.
pub fn extract_range(
    tools: &Tools,
    info: &VideoInfo,
    range: &Range,
    divisor: u32,
    dir: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<u64> {
    std::fs::create_dir_all(dir)?;
    let out_frames = range.count.div_ceil(divisor as u64);
    let mut cmd = tools.ffmpeg_cmd();
    let (args, select) = seek_args(info, range.start);
    cmd.args(args)
        .arg("-i")
        .arg(&info.path)
        .args(["-an", "-sn", "-vf"])
        .arg(format!(
            "{},fps={}/{}",
            filter_chain(info, Some(&select), info.width, info.height),
            info.fps_num,
            info.fps_den as u64 * divisor as u64
        ))
        // Without this, ffmpeg would duplicate or drop frames to match the copied timestamps.
        .args(["-fps_mode", "passthrough"])
        .args([
            "-frames:v",
            &out_frames.to_string(),
            "-start_number",
            "1",
            "-progress",
            "pipe:1",
            "-nostats",
        ])
        .arg(dir.join("%06d.png"));
    run_with_progress(cmd, cancel, |frame| progress(frame))?;
    let written = count_frames(dir);
    if written == 0 {
        bail!("ffmpeg wrote no frames");
    }
    Ok(written)
}

/// Number of `NNNNNN.png` files in `dir`.
pub fn count_frames(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.len() == 10
                        && n.ends_with(".png")
                        && n[..6].bytes().all(|b| b.is_ascii_digit())
                })
                .count() as u64
        })
        .unwrap_or(0)
}

pub fn frame_file(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{n:06}.png"))
}

#[derive(Clone, Debug)]
pub struct EncodeOptions {
    /// Frame rate of the PNG sequence (fps / divisor).
    pub in_fps_num: u32,
    pub in_fps_den: u64,
    /// Rebuild the original frame rate with motion interpolation (when frames were skipped).
    pub interpolate_to: Option<(u32, u32)>,
    pub codec: crate::settings::VideoCodec,
    pub crf: u8,
    /// Audio: the source file and the range to cut from it.
    pub audio_from: Option<(PathBuf, f64, f64)>,
}

/// Encodes `frames_dir/%06d.png` to `out`, with audio copied from the source when it fits the
/// container and re-encoded to AAC when it does not.
pub fn encode(
    tools: &Tools,
    frames_dir: &Path,
    frames: u64,
    opts: &EncodeOptions,
    out: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    let build = |audio_codec: &str| {
        let mut cmd = tools.ffmpeg_cmd();
        cmd.arg("-y")
            .arg("-framerate")
            .arg(format!("{}/{}", opts.in_fps_num, opts.in_fps_den))
            .args(["-start_number", "1", "-i"])
            .arg(frames_dir.join("%06d.png"));
        if let Some((src, start, dur)) = &opts.audio_from {
            cmd.arg("-ss")
                .arg(format!("{start:.4}"))
                .arg("-t")
                .arg(format!("{dur:.4}"))
                .arg("-i")
                .arg(src);
            cmd.args(["-map", "0:v:0", "-map", "1:a:0?"]);
        } else {
            cmd.args(["-map", "0:v:0"]);
        }
        if let Some((n, d)) = opts.interpolate_to {
            cmd.arg("-vf").arg(format!(
                "minterpolate=fps={n}/{d}:mi_mode=mci:mc_mode=aobmc:me_mode=bidir:vsbmc=1"
            ));
        }
        match opts.codec {
            crate::settings::VideoCodec::H264 => {
                cmd.args(["-c:v", "libx264", "-preset", "slow", "-profile:v", "high"]);
            }
            crate::settings::VideoCodec::H265 => {
                cmd.args(["-c:v", "libx265", "-preset", "medium", "-tag:v", "hvc1"]);
            }
        }
        cmd.args(["-crf", &opts.crf.to_string(), "-pix_fmt", "yuv420p"]);
        if opts.audio_from.is_some() {
            cmd.args(["-c:a", audio_codec]);
            if audio_codec == "aac" {
                cmd.args(["-b:a", "256k"]);
            }
            cmd.arg("-shortest");
        }
        cmd.args(["-movflags", "+faststart", "-progress", "pipe:1", "-nostats"])
            .arg(out);
        cmd
    };
    let _ = frames;
    match run_with_progress(build("copy"), cancel, &mut progress) {
        Ok(()) => Ok(()),
        Err(e) if process::is_cancelled(&e) || opts.audio_from.is_none() => Err(e),
        Err(e) => {
            log::info!("audio copy failed ({e}), re-encoding audio to AAC");
            run_with_progress(build("aac"), cancel, &mut progress)
        }
    }
}

/// Runs an ffmpeg command that has `-progress pipe:1`, reporting `frame=` values.
fn run_with_progress(
    mut cmd: std::process::Command,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("could not start ffmpeg")?;
    let stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let (tx, rx) = std::sync::mpsc::channel::<u64>();
    let out_thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(v) = line
                .strip_prefix("frame=")
                .and_then(|v| v.trim().parse::<u64>().ok())
            {
                let _ = tx.send(v);
            }
        }
    });
    let status = loop {
        while let Ok(f) = rx.try_recv() {
            progress(f);
        }
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!(process::Cancelled);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let _ = out_thread.join();
    while let Ok(f) = rx.try_recv() {
        progress(f);
    }
    let err = err_thread.join().unwrap_or_default();
    if !status.success() {
        let tail: Vec<&str> = err.lines().rev().take(6).collect();
        bail!(
            "ffmpeg failed: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratios() {
        assert_eq!(parse_ratio("30000/1001"), Some((30000, 1001)));
        assert_eq!(parse_ratio("25"), Some((25, 1)));
        assert_eq!(parse_ratio("0/0"), None);
    }

    #[test]
    fn probe_json() {
        let json = r#"{"streams":[
            {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,"avg_frame_rate":"24000/1001","nb_frames":"240","duration":"10.01"},
            {"codec_type":"audio","codec_name":"aac"}],"format":{"duration":"10.01"}}"#;
        let info = parse_probe(Path::new("x.mp4"), json).unwrap();
        assert_eq!(
            (info.width, info.height, info.frame_count),
            (1920, 1080, 240)
        );
        assert!(info.has_audio);
        assert!((info.fps() - 23.976).abs() < 0.01);
    }

    #[test]
    fn dvd_probe_uses_display_aspect() {
        let json = r#"{"streams":[{"codec_type":"video","codec_name":"mpeg2video","width":720,"height":480,
            "sample_aspect_ratio":"32:27","field_order":"tt","avg_frame_rate":"30000/1001"}],"format":{"duration":"10"}}"#;
        let info = parse_probe(Path::new("x.vob"), json).unwrap();
        assert_eq!((info.width, info.height), (854, 480));
        assert_eq!((info.coded_width, info.coded_height), (720, 480));
        assert!(info.interlaced);
        assert_eq!(info.frame_count, 300);
        let pal = r#"{"streams":[{"codec_type":"video","width":720,"height":576,"sample_aspect_ratio":"16:15",
            "field_order":"progressive","avg_frame_rate":"25/1"}],"format":{"duration":"1"}}"#;
        let info = parse_probe(Path::new("x.vob"), pal).unwrap();
        assert_eq!((info.width, info.height), (768, 576));
        assert!(!info.interlaced);
    }

    #[test]
    fn rotated_probe_swaps_size() {
        let json = r#"{"streams":[{"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,
            "avg_frame_rate":"30/1","duration":"2","side_data_list":[{"rotation":-90}]}],"format":{}}"#;
        let info = parse_probe(Path::new("x.mov"), json).unwrap();
        assert_eq!(
            (info.width, info.height, info.frame_count),
            (1080, 1920, 60)
        );
        assert!(!info.has_audio);
    }
}
