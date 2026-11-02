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

    pub(crate) fn ffmpeg_cmd(&self) -> std::process::Command {
        let mut c = command(&self.ffmpeg);
        c.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
        c
    }
}

/// A rectangle cut out of the decoded picture (in stored pixels), e.g. to drop letterbox bars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub w: u32,
    pub h: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoInfo {
    pub path: PathBuf,
    /// Display size in square pixels (what everything downstream works with). DVDs store
    /// 720x480/576 with non-square pixels; this is the size they are shown at, e.g. 854x480.
    pub width: u32,
    pub height: u32,
    /// Size as stored in the file (of the cropped area when `crop` is set).
    pub coded_width: u32,
    pub coded_height: u32,
    /// Full stored size, before any crop.
    pub source_width: u32,
    pub source_height: u32,
    /// Black bars (letterbox/pillarbox) found in the picture, if any.
    pub detected_crop: Option<Crop>,
    /// The crop in use: every decoded frame is cut to it.
    pub crop: Option<Crop>,
    /// Stored sideways with a rotation tag (phones); such videos are never cropped.
    pub rotated: bool,
    /// Interlaced (typical for DVD and TV): frames are deinterlaced when decoded.
    pub interlaced: bool,
    /// Pixel aspect ratio as stored (32:27 for NTSC 16:9 DVD); (1, 1) for square pixels.
    pub sar: (u32, u32),
    /// The pixel aspect ratio the file declares (`sar` differs when the user overrides it).
    pub declared_sar: (u32, u32),
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
    /// Stored with non-square pixels (anamorphic DVD and the like).
    pub fn anamorphic(&self) -> bool {
        self.sar != (1, 1)
    }

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

    /// Identifies this exact file (path, size and modification time) and crop for caches.
    pub fn identity(&self) -> String {
        let crop = self
            .crop
            .map(|c| format!("|crop {}x{}+{}+{}", c.w, c.h, c.x, c.y))
            .unwrap_or_default();
        let sar = if self.sar != self.declared_sar {
            format!("|sar {}:{}", self.sar.0, self.sar.1)
        } else {
            String::new()
        };
        format!(
            "{}|{}|{}{crop}{sar}",
            self.path.display(),
            self.file_size,
            self.modified
        )
    }

    /// This video with the whole stored frame shown at `dar` (width:height) instead of what
    /// the file declares (`None`: as declared). Used when a file is flagged wrongly.
    pub fn with_frame_aspect(&self, dar: Option<(u32, u32)>) -> VideoInfo {
        if self.rotated {
            return self.clone();
        }
        let sar = match dar {
            None => self.declared_sar,
            Some((n, d)) => {
                let (a, b) = (
                    n as u64 * self.source_height as u64,
                    d as u64 * self.source_width as u64,
                );
                if (a as f64 / b as f64 - 1.0).abs() < 0.01 {
                    (1, 1)
                } else {
                    let g = gcd(a, b);
                    ((a / g) as u32, (b / g) as u32)
                }
            }
        };
        let mut v = self.clone();
        v.sar = sar;
        v.with_crop(self.crop)
    }

    /// Display aspect ratio of the whole stored frame as declared by the file.
    pub fn declared_frame_aspect(&self) -> f64 {
        self.source_width as f64 * self.declared_sar.0 as f64
            / self.declared_sar.1 as f64
            / self.source_height as f64
    }

    /// This video cut to `crop` (or uncut with `None`): sizes are recomputed from the
    /// stored size and the pixel aspect ratio.
    pub fn with_crop(&self, crop: Option<Crop>) -> VideoInfo {
        let mut v = self.clone();
        // A rotated video keeps its full frame: crop rectangles are in unrotated pixels.
        let rotated = self.rotated;
        let crop = crop.filter(|_| !rotated);
        let (cw, ch) = crop
            .map(|c| (c.w, c.h))
            .unwrap_or((self.source_width, self.source_height));
        v.crop = crop;
        v.coded_width = cw;
        v.coded_height = ch;
        if !rotated {
            let (n, d) = self.sar;
            v.width = ((cw as f64 * n as f64 / d as f64 / 2.0).round() as u32).max(1) * 2;
            v.height = ch;
        }
        v
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a.max(1)
    } else {
        gcd(b, a % b)
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
    let mut sar = (1u32, 1u32);
    if let Some((n, d)) = video["sample_aspect_ratio"]
        .as_str()
        .and_then(|s| s.split_once(':'))
        .and_then(|(n, d)| Some((n.parse::<u32>().ok()?, d.parse::<u32>().ok()?)))
        .filter(|(n, d)| *n > 0 && *d > 0 && (*n as f64 / *d as f64 - 1.0).abs() > 0.01)
    {
        sar = (n, d);
        width = ((width as f64 * n as f64 / d as f64 / 2.0).round() as u32).max(1) * 2;
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
    let rotated = (rotation.abs() - 90.0).abs() < 1.0 || (rotation.abs() - 270.0).abs() < 1.0;
    if rotated {
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
        source_width: coded_width,
        source_height: coded_height,
        detected_crop: None,
        crop: None,
        rotated,
        interlaced,
        sar,
        declared_sar: sar,
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
    if let Some(c) = info.crop {
        parts.push(format!("crop={}:{}:{}:{}", c.w, c.h, c.x, c.y));
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

/// Finds black bars (letterbox above/below, pillarbox at the sides) by running `cropdetect`
/// on a few frames at ten points in the video. The least aggressive result wins, so a dark
/// scene never crops into the picture; bars thinner than 2 % are ignored.
pub fn detect_black_bars(tools: &Tools, info: &VideoInfo) -> Option<Crop> {
    if info.rotated {
        return None;
    }
    let (fw, fh) = (info.source_width, info.source_height);
    let mut best: Option<Crop> = None;
    let mut samples = 0;
    for i in 1..=10 {
        let t = info.duration * i as f64 / 11.0;
        let mut cmd = command(&tools.ffmpeg);
        cmd.args([
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "info",
            "-noaccurate_seek",
            "-ss",
        ])
        .arg(format!("{t:.3}"))
        .arg("-i")
        .arg(&info.path)
        .args([
            "-frames:v",
            "8",
            "-an",
            "-sn",
            "-vf",
            "cropdetect=limit=24:round=2:reset=0",
            "-f",
            "null",
            "-",
        ]);
        let Ok(out) = process::run(
            cmd,
            None,
            &AtomicBool::new(false),
            Some(std::time::Duration::from_secs(30)),
        ) else {
            continue;
        };
        let Some(c) = out.stderr_text().lines().rev().find_map(parse_cropdetect) else {
            continue;
        };
        // A nearly black frame reports a tiny or empty area: no information.
        if c.w < fw / 3 || c.h < fh / 3 {
            continue;
        }
        samples += 1;
        best = Some(match best {
            None => c,
            Some(b) => {
                // Union of the two: keep everything either sample saw as picture.
                let x0 = b.x.min(c.x);
                let y0 = b.y.min(c.y);
                let x1 = (b.x + b.w).max(c.x + c.w);
                let y1 = (b.y + b.h).max(c.y + c.h);
                Crop {
                    w: x1 - x0,
                    h: y1 - y0,
                    x: x0,
                    y: y0,
                }
            }
        });
    }
    let c = best.filter(|_| samples >= 3)?;
    let even = |v: u32| v & !1;
    let c = Crop {
        w: even(c.w),
        h: even(c.h),
        x: even(c.x),
        y: even(c.y),
    };
    let meaningful = (c.w as f64) < fw as f64 * 0.98 || (c.h as f64) < fh as f64 * 0.98;
    meaningful.then_some(c)
}

/// `crop=720:432:0:72` from a cropdetect log line.
fn parse_cropdetect(line: &str) -> Option<Crop> {
    let spec = line
        .rsplit("crop=")
        .next()
        .filter(|_| line.contains("crop="))?;
    let mut it = spec
        .split_whitespace()
        .next()?
        .split(':')
        .map(|v| v.parse::<i64>().ok());
    let (w, h, x, y) = (it.next()??, it.next()??, it.next()??, it.next()??);
    if w <= 0 || h <= 0 || x < 0 || y < 0 {
        return None;
    }
    Some(Crop {
        w: w as u32,
        h: h as u32,
        x: x as u32,
        y: y as u32,
    })
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
    progress: impl FnMut(u64),
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
    run_with_progress(cmd, cancel, progress)?;
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
    /// Squeeze the square-pixel frames back to this stored size and pixel aspect ratio
    /// (anamorphic output).
    pub anamorphic: Option<((u32, u32), (u32, u32))>,
    pub codec: crate::settings::VideoCodec,
    pub crf: u8,
    /// Audio: the source file and the range to cut from it.
    pub audio_from: Option<(PathBuf, f64, f64)>,
    /// HDR output: the input then holds 16-bit RGB already in BT.2020 PQ/HLG code values.
    pub hdr: Option<crate::hdr::HdrEncode>,
}

/// Writes raw frames to the encoder's stdin; may be called again when the encode is retried.
pub type FrameFeed<'a> = &'a (dyn Fn(&mut dyn std::io::Write) -> Result<()> + Sync);

/// The pictures `encode` reads.
pub enum VideoInput<'a> {
    /// `dir/000001.png`, `dir/000002.png`, …
    Frames(&'a Path),
    /// Raw `rgb48le` frames of this size, written by `feed` (HDR, converted on the fly).
    Rgb48 {
        width: u32,
        height: u32,
        feed: FrameFeed<'a>,
    },
}

/// The video filters of HDR output. The input already holds BT.2020 PQ/HLG code values: the
/// only conversion is RGB to YUV 4:2:0 10-bit with the BT.2020 non-constant-luminance matrix,
/// full to limited range (plus the squeeze back to the stored size for anamorphic output).
/// zscale does it exactly, with error-diffusion dither to 10 bits; swscale (when the FFmpeg
/// build has no zimg) has a +0.39 % gain error on Y and blockier highlights. Motion
/// interpolation is never done here (it is 8-bit only): the job interpolates the SDR frames.
pub fn hdr_filters(
    mode: crate::settings::HdrMode,
    anamorphic: Option<((u32, u32), (u32, u32))>,
    zscale: bool,
) -> String {
    let mut f = Vec::new();
    if zscale {
        let t = crate::hdr::transfer_name(mode);
        let size = anamorphic.map_or(String::new(), |((w, h), _)| {
            format!("w={w}:h={h}:f=lanczos:")
        });
        f.push(format!(
            "zscale={size}rin=full:pin=2020:tin={t}:min=gbr\
             :r=limited:p=2020:t={t}:m=2020_ncl:dither=error_diffusion"
        ));
        f.push("format=yuv420p10le".to_string());
        if let Some((_, (n, d))) = anamorphic {
            f.push(format!("setsar={n}/{d}"));
        }
    } else {
        // Done first, explicitly, and pinned to 16-bit 4:4:4, so no later scale or format
        // negotiation decides the matrix or drops precision.
        f.push(
            "scale=out_color_matrix=bt2020:out_range=tv:flags=accurate_rnd+full_chroma_int,\
             format=yuv444p16le"
                .to_string(),
        );
        if let Some(((w, h), (n, d))) = anamorphic {
            f.push(format!("scale={w}:{h}:flags=lanczos,setsar={n}/{d}"));
        }
        f.push("format=yuv420p10le".to_string());
    }
    f.join(",")
}

impl Tools {
    /// Whether this FFmpeg has the zscale filter (zimg); asked once per FFmpeg binary.
    pub fn has_zscale(&self) -> bool {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        static SEEN: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
        let seen = SEEN.get_or_init(Default::default);
        if let Some(v) = seen.lock().unwrap().get(&self.ffmpeg) {
            return *v;
        }
        let mut cmd = self.ffmpeg_cmd();
        cmd.arg("-filters");
        let found = process::run(
            cmd,
            None,
            &AtomicBool::new(false),
            Some(std::time::Duration::from_secs(30)),
        )
        .map(|o| {
            o.stdout_text()
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some("zscale"))
        })
        .unwrap_or(false);
        if !found {
            log::warn!(
                "{} has no zscale filter (zimg): HDR is converted to YUV with swscale, \
                 which is slightly less exact",
                self.ffmpeg.display()
            );
        }
        seen.lock().unwrap().insert(self.ffmpeg.clone(), found);
        found
    }
}

/// Motion-interpolates `src_dir/%06d.png` (at `in_fps`) to `to_fps` and writes the full-rate
/// 8-bit frames to `out_dir/%06d.png`. Returns the number of frames written.
pub fn interpolate_frames(
    tools: &Tools,
    src_dir: &Path,
    in_fps: (u32, u64),
    to_fps: (u32, u32),
    out_dir: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(u64),
) -> Result<u64> {
    std::fs::create_dir_all(out_dir)?;
    let mut cmd = tools.ffmpeg_cmd();
    cmd.arg("-y")
        .arg("-framerate")
        .arg(format!("{}/{}", in_fps.0, in_fps.1))
        .args(["-start_number", "1", "-i"])
        .arg(src_dir.join("%06d.png"))
        .arg("-vf")
        .arg(format!(
            "minterpolate=fps={}/{}:mi_mode=mci:mc_mode=aobmc:me_mode=bidir:vsbmc=1",
            to_fps.0, to_fps.1
        ))
        .args(["-fps_mode", "passthrough", "-pix_fmt", "rgb24"])
        .args(["-start_number", "1", "-progress", "pipe:1", "-nostats"])
        .arg(out_dir.join("%06d.png"));
    run_with_progress(cmd, cancel, progress)?;
    Ok(count_frames(out_dir))
}

/// Encodes `input` to `out`, with audio copied from the source when it fits the container and
/// re-encoded to AAC when it does not.
pub fn encode(
    tools: &Tools,
    input: &VideoInput,
    opts: &EncodeOptions,
    out: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    // The audio: first cut by stream copy into a file of its own with `cut_original`'s
    // two-stage seek. A plain `-ss` before the source's `-i` with `-c:a copy` keeps every
    // packet back to the demuxer's seek point (the video keyframe before the range: seconds
    // of audio before the picture on long-GOP sources). Encoding to AAC straight from the
    // source is the fallback; when decoding, the input seek is exact.
    let audio_cut = out.with_extension("audio.part.mka");
    let mut copied = None;
    if let Some((src, start, dur)) = &opts.audio_from {
        match cut_original(tools, src, *start, *dur, true, &audio_cut, cancel) {
            Ok(()) => copied = Some(audio_cut.clone()),
            Err(e) if process::is_cancelled(&e) => {
                let _ = std::fs::remove_file(&audio_cut);
                return Err(e);
            }
            Err(e) => log::info!("cutting the audio by stream copy failed ({e})"),
        }
    }
    let build = |audio: Option<&Path>| {
        let mut cmd = tools.ffmpeg_cmd();
        cmd.arg("-y");
        if audio.is_some() {
            // Keeps the cut's sub-frame start (its first whole packet may begin a few ms
            // into the range), as `mux` does.
            cmd.arg("-copyts");
        }
        let rate = format!("{}/{}", opts.in_fps_num, opts.in_fps_den);
        match input {
            VideoInput::Frames(dir) => {
                cmd.args(["-framerate", &rate, "-start_number", "1", "-i"])
                    .arg(dir.join("%06d.png"));
            }
            VideoInput::Rgb48 { width, height, .. } => {
                cmd.args(["-f", "rawvideo", "-pix_fmt", "rgb48le", "-s"])
                    .arg(format!("{width}x{height}"))
                    .args(["-framerate", &rate, "-i", "pipe:0"]);
            }
        }
        match (audio, &opts.audio_from) {
            (Some(cut), _) => {
                cmd.arg("-i").arg(cut);
                cmd.args(["-map", "0:v:0", "-map", "1:a:0?"]);
            }
            (None, Some((src, start, dur))) => {
                cmd.arg("-ss")
                    .arg(format!("{start:.4}"))
                    .arg("-t")
                    .arg(format!("{dur:.4}"))
                    .arg("-i")
                    .arg(src);
                cmd.args(["-map", "0:v:0", "-map", "1:a:0?"]);
            }
            (None, None) => {
                cmd.args(["-map", "0:v:0"]);
            }
        }
        if let Some(h) = &opts.hdr {
            let mut h = h.clone();
            // x265 opens the HDR10+ JSON with a narrow fopen: a non-ASCII path (a job folder
            // under "Användare", "å", …) fails, and x265 then aborts. ffmpeg runs in the JSON's
            // folder instead and x265 gets the bare (ASCII) file name; every other path given
            // to ffmpeg here is absolute.
            if let Some(json) = &h.hdr10plus_json {
                if let (false, Some(dir), Some(name)) = (
                    json.to_string_lossy().is_ascii(),
                    json.parent(),
                    json.file_name(),
                ) {
                    cmd.current_dir(dir);
                    h.hdr10plus_json = Some(PathBuf::from(name));
                }
            }
            cmd.arg("-vf")
                .arg(hdr_filters(h.mode, opts.anamorphic, tools.has_zscale()));
            // HDR is always H.265 (H.264 has no usable HDR path here).
            cmd.args(["-c:v", "libx265", "-preset", "medium", "-tag:v", "hvc1"]);
            cmd.args(["-pix_fmt", "yuv420p10le"]);
            cmd.args([
                "-color_primaries",
                "bt2020",
                "-color_trc",
                crate::hdr::transfer_name(h.mode),
                "-colorspace",
                "bt2020nc",
                "-color_range",
                "tv",
            ]);
            cmd.arg("-x265-params")
                .arg(crate::hdr::x265_params(&h, opts.crf));
        } else {
            let mut filters: Vec<String> = Vec::new();
            if let Some((n, d)) = opts.interpolate_to {
                filters.push(format!(
                    "minterpolate=fps={n}/{d}:mi_mode=mci:mc_mode=aobmc:me_mode=bidir:vsbmc=1"
                ));
            }
            if let Some(((w, h), (n, d))) = opts.anamorphic {
                filters.push(format!("scale={w}:{h}:flags=lanczos,setsar={n}/{d}"));
            }
            if !filters.is_empty() {
                cmd.arg("-vf").arg(filters.join(","));
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
        }
        if opts.audio_from.is_some() {
            if audio.is_some() {
                cmd.args(["-c:a", "copy"]);
            } else {
                cmd.args(["-c:a", "aac", "-b:a", "256k"]);
            }
            cmd.arg("-shortest");
        }
        cmd.args(["-movflags", "+faststart", "-progress", "pipe:1", "-nostats"])
            .arg(out);
        cmd
    };
    let feed = match input {
        VideoInput::Rgb48 { feed, .. } => Some(*feed),
        VideoInput::Frames(_) => None,
    };
    let result = match &copied {
        None => run_ffmpeg(build(None), cancel, &mut progress, feed),
        Some(cut) => match run_ffmpeg(build(Some(cut)), cancel, &mut progress, feed) {
            // Only ffmpeg's own failure (the copied audio does not fit) is worth a retry.
            Err(e) if !process::is_cancelled(&e) && e.downcast_ref::<FeedFailed>().is_none() => {
                log::info!("audio copy failed ({e}), re-encoding audio to AAC");
                run_ffmpeg(build(None), cancel, &mut progress, feed)
            }
            r => r,
        },
    };
    let _ = std::fs::remove_file(&audio_cut);
    result
}

/// Codecs the "Original" track keeps by stream copy; anything else (PCM above all, which the
/// output container must never hold) is encoded to FLAC.
const COPY_AUDIO: &[&str] = &[
    "ac3", "eac3", "aac", "mp3", "mp2", "dts", "truehd", "flac", "opus", "vorbis",
];

/// Codec and language tag of the first audio stream of `path`.
fn first_audio_stream(tools: &Tools, path: &Path) -> Result<(String, Option<String>)> {
    let mut cmd = command(&tools.ffprobe);
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "a:0",
        "-show_entries",
        "stream=codec_name:stream_tags=language",
        "-print_format",
        "json",
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
    let v: Value =
        serde_json::from_str(&out.stdout_text()).context("ffprobe output is not JSON")?;
    let s = &v["streams"][0];
    let codec = s["codec_name"].as_str().unwrap_or("?").to_string();
    let lang = s["tags"]["language"]
        .as_str()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "und")
        .map(str::to_string);
    Ok((codec, lang))
}

/// Cuts `dur` seconds from `start` of `src`'s first audio stream into the Matroska audio
/// file `out`: stream-copied when `copy` (cut on the codec's own frame boundaries, so the
/// track can be up to one audio frame off at each end), else encoded to FLAC (sample exact).
fn cut_original(
    tools: &Tools,
    src: &Path,
    start: f64,
    dur: f64,
    copy: bool,
    out: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut cmd = tools.ffmpeg_cmd();
    // Two-stage seek: a fast input seek to 30 s before the range, then output-side seeking
    // for the rest, which with stream copy drops whole packets and keeps the sub-frame
    // position of the first kept one in its timestamp (and when encoding cuts sample-exact).
    let coarse = (start - 30.0).max(0.0);
    cmd.arg("-y")
        .arg("-ss")
        .arg(format!("{coarse:.6}"))
        .arg("-i")
        .arg(src)
        .arg("-ss")
        .arg(format!("{:.6}", start - coarse))
        .arg("-t")
        .arg(format!("{dur:.6}"))
        .args(["-map", "0:a:0", "-vn", "-sn", "-dn"])
        .args(["-map_metadata", "-1", "-map_chapters", "-1"])
        .args(["-c:a", if copy { "copy" } else { "flac" }])
        .args(["-f", "matroska", "-progress", "pipe:1", "-nostats"])
        .arg(out);
    run_with_progress(cmd, cancel, |_| {})
}

/// Muxes the video (copied) with the audio job's tracks and the source's own audio for the
/// same range into the MKV `out`.
///
/// Track order: FLAC (5.1, or stereo when only the quality was improved; default track),
/// then AC-3 5.1, then the original. Titles "5.1 (FLAC)", "Stereo (FLAC)", "5.1 (AC-3)",
/// "Original"; every audio track gets the source's language tag when it has one.
///
/// `original` is `(source file, start s, duration s)`. Every stream is cut to the video's
/// length (`duration`, or the probed length of `video` without an original): stream copy
/// cuts on packets, so a copied track may differ from the video's length by up to one of
/// its own frames (AC-3: 1536 samples, 32 ms; the original by one at each end). The AC-3
/// track can therefore end up to 32 ms after the video, which is less than a frame at 30 fps
/// but up to two frames at 50/60 fps.
///
/// The MKV is written as `<out minus extension>.part.mkv` and renamed to `out` only when
/// complete, so a cancel or failure never leaves a partial file at `out` nor replaces a file
/// that was there.
pub fn mux(
    tools: &Tools,
    video: &Path,
    tracks: &[crate::audio::job::AudioTrack],
    original: Option<(PathBuf, f64, f64)>,
    out: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    use crate::audio::job::AudioTrack;
    let duration = match &original {
        Some((_, _, d)) => *d,
        None => probe(tools, video)?.duration,
    };
    let mut ordered: Vec<&AudioTrack> = tracks.iter().collect();
    ordered.sort_by_key(|t| match t {
        AudioTrack::Flac51(_) | AudioTrack::FlacStereo(_) => 0,
        AudioTrack::Ac351(_) => 1,
    });
    let mut inputs: Vec<(PathBuf, &str)> = ordered
        .iter()
        .map(|t| {
            let title = match t {
                AudioTrack::Flac51(_) => "5.1 (FLAC)",
                AudioTrack::Ac351(_) => "5.1 (AC-3)",
                AudioTrack::FlacStereo(_) => "Stereo (FLAC)",
            };
            (t.path().to_path_buf(), title)
        })
        .collect();

    let part = out.with_extension("part.mkv");
    let orig_file = out.with_extension("original.part.mka");
    let mut language = None;
    if let Some((src, start, dur)) = &original {
        let (codec, lang) = first_audio_stream(tools, src)?;
        language = lang;
        let copy = COPY_AUDIO.contains(&codec.as_str());
        let cut = cut_original(tools, src, *start, *dur, copy, &orig_file, cancel);
        match cut {
            Ok(()) => {}
            Err(e) if process::is_cancelled(&e) || !copy => {
                let _ = std::fs::remove_file(&orig_file);
                return Err(e.context("could not cut the original audio"));
            }
            Err(e) => {
                log::info!("copying the original audio failed ({e}), encoding it to FLAC");
                if let Err(e) = cut_original(tools, src, *start, *dur, false, &orig_file, cancel) {
                    let _ = std::fs::remove_file(&orig_file);
                    return Err(e.context("could not cut the original audio"));
                }
            }
        }
        inputs.push((orig_file.clone(), "Original"));
    }

    let mut cmd = tools.ffmpeg_cmd();
    // -copyts keeps the original's sub-frame start (its first whole packet may begin a few
    // ms into the range); the video and the job's tracks all start at 0.
    cmd.args(["-y", "-copyts", "-i"]).arg(video);
    for (path, _) in &inputs {
        cmd.arg("-i").arg(path);
    }
    cmd.args(["-map", "0:v:0"]);
    for i in 0..inputs.len() {
        cmd.arg("-map").arg(format!("{}:a:0", i + 1));
    }
    cmd.args(["-c", "copy", "-t"]).arg(format!("{duration:.6}"));
    for (i, (_, title)) in inputs.iter().enumerate() {
        cmd.arg(format!("-disposition:a:{i}"))
            .arg(if i == 0 { "default" } else { "0" })
            .arg(format!("-metadata:s:a:{i}"))
            .arg(format!("title={title}"));
        if let Some(lang) = &language {
            cmd.arg(format!("-metadata:s:a:{i}"))
                .arg(format!("language={lang}"));
        }
    }
    cmd.args(["-f", "matroska", "-progress", "pipe:1", "-nostats"])
        .arg(&part);
    let result = run_with_progress(cmd, cancel, |_| {}).and_then(|()| {
        std::fs::rename(&part, out).with_context(|| format!("could not write {}", out.display()))
    });
    let _ = std::fs::remove_file(&orig_file);
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// Runs an ffmpeg command that has `-progress pipe:1`, reporting `frame=` values.
fn run_with_progress(
    cmd: std::process::Command,
    cancel: &AtomicBool,
    progress: impl FnMut(u64),
) -> Result<()> {
    run_ffmpeg(cmd, cancel, progress, None)
}

/// `run_with_progress` with `feed` (when given) writing ffmpeg's stdin on a thread of its own.
/// A failing feed stops ffmpeg and is the error; when ffmpeg stops first (it failed, or
/// `-shortest` ended the output), the feed's write fails and ffmpeg's own result counts.
/// Cancelling kills ffmpeg, which closes the pipe.
fn run_ffmpeg(
    mut cmd: std::process::Command,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
    feed: Option<FrameFeed>,
) -> Result<()> {
    cmd.stdin(if feed.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = crate::process::spawn(&mut cmd).context("could not start ffmpeg")?;
    let stdin = child.stdin.take();
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
    std::thread::scope(|scope| {
        let mut writer = match (feed, stdin) {
            (Some(feed), Some(mut pipe)) => Some(scope.spawn(move || {
                let r = feed(&mut pipe);
                // Closing the pipe ends ffmpeg's input.
                drop(pipe);
                r
            })),
            _ => None,
        };
        let join = |h: std::thread::ScopedJoinHandle<'_, Result<()>>| {
            h.join()
                .unwrap_or_else(|_| Err(anyhow!("the frame feed panicked")))
        };
        // A feed that fails on its own (a frame could not be made) stops ffmpeg; a closed pipe
        // only means ffmpeg stopped reading, and ffmpeg's own exit says why.
        let mut feed_error = None;
        let mut pipe_closed = None;
        let mut take_writer =
            |writer: &mut Option<std::thread::ScopedJoinHandle<'_, Result<()>>>,
             child: &mut std::process::Child| {
                if let Some(Err(e)) = writer.take().map(join) {
                    if is_broken_pipe(&e) {
                        pipe_closed = Some(e);
                    } else {
                        let _ = child.kill();
                        feed_error = Some(e);
                    }
                }
            };
        let status = loop {
            while let Ok(f) = rx.try_recv() {
                progress(f);
            }
            if writer.as_ref().is_some_and(|h| h.is_finished()) {
                take_writer(&mut writer, &mut child);
            }
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                bail!(process::Cancelled);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        // ffmpeg is gone, so a feed still writing ends now (its pipe is closed).
        take_writer(&mut writer, &mut child);
        let _ = out_thread.join();
        while let Ok(f) = rx.try_recv() {
            progress(f);
        }
        let err = err_thread.join().unwrap_or_default();
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            bail!(process::Cancelled);
        }
        // A frame that could not be made fails the encode even when ffmpeg ended cleanly on
        // the closed pipe: the video would be short.
        if let Some(e) = feed_error {
            return Err(e.context(FeedFailed));
        }
        if !status.success() {
            let t: Vec<&str> = err.lines().rev().take(6).collect();
            let tail = t.into_iter().rev().collect::<Vec<_>>().join("\n");
            bail!("ffmpeg failed ({status}): {tail}");
        }
        if let Some(e) = pipe_closed {
            // ffmpeg finished on its own before taking every frame (-shortest).
            log::info!("ffmpeg stopped reading frames early (-shortest): {e:#}");
        }
        Ok(())
    })
}

/// The frame feed failed on its own (not because ffmpeg closed the pipe): the encode fails,
/// and retrying it (with other audio) would fail the same way.
#[derive(Debug)]
pub struct FeedFailed;

impl std::fmt::Display for FeedFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not make the frames for the encoder")
    }
}

/// Whether `e` comes from writing to a pipe whose reader has gone.
fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr_filter_chains() {
        use crate::settings::HdrMode;
        let z = hdr_filters(HdrMode::Hdr10, Some(((720, 480), (32, 27))), true);
        assert_eq!(
            z,
            "zscale=w=720:h=480:f=lanczos:rin=full:pin=2020:tin=smpte2084:min=gbr\
             :r=limited:p=2020:t=smpte2084:m=2020_ncl:dither=error_diffusion,\
             format=yuv420p10le,setsar=32/27"
        );
        let hlg = hdr_filters(HdrMode::Hlg, None, true);
        assert!(hlg.contains("tin=arib-std-b67") && hlg.contains(":t=arib-std-b67"));
        assert!(!hlg.contains("setsar") && !hlg.contains("w="));
        let s = hdr_filters(HdrMode::Hdr10, Some(((720, 480), (32, 27))), false);
        assert_eq!(
            s,
            "scale=out_color_matrix=bt2020:out_range=tv:flags=accurate_rnd+full_chroma_int,\
             format=yuv444p16le,scale=720:480:flags=lanczos,setsar=32/27,format=yuv420p10le"
        );
        assert!(!z.contains(' ') && !s.contains(' '));
    }

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
    fn cropdetect_lines_and_crop_sizes() {
        let line = "[Parsed_cropdetect_0 @ 0x5] x1:0 x2:719 y1:72 y2:503 w:720 h:432 x:0 y:72 pts:1 t:0.04 limit:0.09 crop=720:432:0:72";
        assert_eq!(
            parse_cropdetect(line),
            Some(Crop {
                w: 720,
                h: 432,
                x: 0,
                y: 72
            })
        );
        assert_eq!(parse_cropdetect("crop=-720:-576:719:575"), None);
        let json = r#"{"streams":[{"codec_type":"video","width":720,"height":576,"sample_aspect_ratio":"16:15",
            "avg_frame_rate":"25/1"}],"format":{"duration":"1"}}"#;
        let info = parse_probe(Path::new("x.vob"), json).unwrap();
        let cropped = info.with_crop(Some(Crop {
            w: 720,
            h: 432,
            x: 0,
            y: 72,
        }));
        assert_eq!((cropped.width, cropped.height), (768, 432));
        assert_eq!((cropped.coded_width, cropped.coded_height), (720, 432));
        assert_ne!(cropped.identity(), info.identity());
        let back = cropped.with_crop(None);
        assert_eq!(
            (back.width, back.height, back.coded_height),
            (768, 576, 576)
        );
    }

    #[test]
    fn dvd_probe_uses_display_aspect() {
        let json = r#"{"streams":[{"codec_type":"video","codec_name":"mpeg2video","width":720,"height":480,
            "sample_aspect_ratio":"32:27","field_order":"tt","avg_frame_rate":"30000/1001"}],"format":{"duration":"10"}}"#;
        let info = parse_probe(Path::new("x.vob"), json).unwrap();
        assert_eq!((info.width, info.height), (854, 480));
        assert_eq!((info.coded_width, info.coded_height), (720, 480));
        assert_eq!(info.sar, (32, 27));
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
