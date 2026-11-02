//! Runs a folder of finished SDR frames (from any upscaler) through the export's HDR stages:
//! `hdr::convert::measure_pass` (pass 1, light levels into `hdr/stats.txt`),
//! `hdr::hdr10plus::write_json` and `ffmpeg::encode` fed by `hdr::convert::stream_frames`
//! (pass 2, x265 with the HDR10/HDR10+ parameters), then `ffmpeg::mux` with the source's
//! audio of the same range (the "Original" track). Uses default settings; the saved settings
//! are never read or written.
//!
//! cargo run -p upscaler-core --release --example hdr_test -- \
//!     <sdr frames dir> <source video> <start s> <out.mkv> <work dir> \
//!     [--mode hdr10plus|hdr10|hlg] [--peak 1000] [--white 203]
//!     [--previews 1,120,200 --share <dir> --name <prefix>]
//!
//! The frames are taken in name order and must be the range's frames at the source's rate,
//! starting with the first frame at or after `<start s>` (as `ffmpeg -ss <start> -i` gives).
//! Prints the range, the light levels and the time per pass; with `--previews` also writes
//! side-by-side JPGs (SDR frame | HDR frame tone mapped back to SDR, as the frame browser
//! shows it) into the share folder.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use upscaler_core::ffmpeg::{self, EncodeOptions, Tools, VideoInput};
use upscaler_core::frame;
use upscaler_core::hdr::convert::{HdrParams, JobHdr};
use upscaler_core::hdr::measure::Measurement;
use upscaler_core::hdr::{self, HdrEncode};
use upscaler_core::settings::{HdrMode, Settings, VideoCodec};

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut opt = |name: &str| -> Option<String> {
        let i = args.iter().position(|a| a == name)?;
        let v = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
        v
    };
    let mode = match opt("--mode").as_deref().unwrap_or("hdr10plus") {
        "hdr10plus" => HdrMode::Hdr10Plus,
        "hdr10" => HdrMode::Hdr10,
        "hlg" => HdrMode::Hlg,
        m => bail!("unknown mode {m}"),
    };
    let peak: u32 = opt("--peak").as_deref().unwrap_or("1000").parse()?;
    let white: u32 = opt("--white").as_deref().unwrap_or("203").parse()?;
    let previews: Vec<u64> = opt("--previews")
        .map(|v| v.split(',').map(|n| n.trim().parse()).collect())
        .transpose()?
        .unwrap_or_default();
    let share = opt("--share").map(PathBuf::from);
    let name = opt("--name").unwrap_or_else(|| "hdr".into());
    if args.len() != 5 {
        bail!("usage: <sdr frames dir> <source video> <start s> <out.mkv> <work dir> [options]");
    }
    let sdr_in = PathBuf::from(&args[0]);
    let video = PathBuf::from(&args[1]);
    let start_s: f64 = args[2].parse()?;
    let out = PathBuf::from(&args[3]);
    let work = PathBuf::from(&args[4]);

    let s = Settings::default();
    let tools = Tools::locate("").context("ffmpeg not found")?;
    let info = ffmpeg::probe(&tools, &video)?;

    // The job layout: SDR frames in out/000001.png.., the HDR stats in hdr/.
    let out_dir = work.join("out");
    let hdr_dir = hdr::convert::hdr_dir(&work);
    std::fs::create_dir_all(&out_dir)?;
    let mut names: Vec<PathBuf> = std::fs::read_dir(&sdr_in)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
        .collect();
    names.sort();
    if names.is_empty() {
        bail!("no PNG frames in {}", sdr_in.display());
    }
    let total = names.len() as u64;
    for (i, src) in names.iter().enumerate() {
        let dst = ffmpeg::frame_file(&out_dir, i as u64 + 1);
        if !dst.is_file() && std::fs::hard_link(src, &dst).is_err() {
            std::fs::copy(src, &dst)?;
        }
    }
    let first = image::open(ffmpeg::frame_file(&out_dir, 1))?;
    // The first frame at or after `start_s`, as `ffmpeg -ss <start_s> -i` extracts it (the
    // way the SDR frames are made outside the app); `frame_at` would round down.
    let start = ((start_s * info.fps()).ceil() as u64).min(info.frame_count.saturating_sub(1));
    let end = start + total;
    println!(
        "source {}x{} fps {}/{}; frames {start}..{end} ({total}), {:.4} s from {:.4} s; SDR frames {}x{}",
        info.width,
        info.height,
        info.fps_num,
        info.fps_den,
        total as f64 / info.fps(),
        info.frame_time(start),
        first.width(),
        first.height()
    );

    let cancel = AtomicBool::new(false);

    // Scene cuts as the export finds them (fingerprint difference of consecutive frames),
    // here on the SDR frames.
    let mut cut = vec![true; total as usize + 1];
    let mut prev: Option<image::GrayImage> = None;
    for n in 1..=total {
        let fp = frame::fingerprint(&image::open(ffmpeg::frame_file(&out_dir, n))?.to_rgb8());
        if let Some(p) = &prev {
            cut[n as usize] = frame::difference(p, &fp) > s.scene_cut_threshold;
        }
        prev = Some(fp);
    }
    let cut_at: Vec<u64> = (2..=total).filter(|&n| cut[n as usize]).collect();
    println!("scene cuts at out/ frames {cut_at:?}");

    let params = HdrParams {
        mode,
        peak_nits: if mode == HdrMode::Hlg { 1000 } else { peak },
        sdr_white_nits: white,
    };
    let t = Instant::now();
    let m = Measurement::from_frames(hdr::convert::measure_pass(
        &out_dir,
        &hdr_dir,
        total,
        &params,
        &cancel,
        |_| {},
    )?);
    let measure_s = t.elapsed().as_secs_f64();
    let mut hdr10plus_json = None;
    let (max_cll, max_fall) = if mode.is_pq() {
        if mode == HdrMode::Hdr10Plus {
            let cuts = hdr::hdr10plus::encoded_cuts(&cut, true, 1, m.frames.len());
            let path = work.join("hdr10plus.json");
            hdr::hdr10plus::write_json(&m, &cuts, params.peak_nits, &path)?;
            hdr10plus_json = Some(path);
        }
        let f0 = &m.frames[0];
        println!(
            "frame 0: MaxSCL {:?} nits, avg maxRGB {:.2} nits, percentiles {:?}",
            f0.max_scl, f0.avg_max_rgb, f0.percentiles
        );
        (m.max_cll, m.max_fall)
    } else {
        (0, 0)
    };
    println!("MaxCLL {max_cll} nits, MaxFALL {max_fall} nits");

    let hdr_enc = HdrEncode {
        mode,
        peak_nits: params.peak_nits,
        max_cll,
        max_fall,
        hdr10plus_json,
    };
    println!("x265-params {}", hdr::x265_params(&hdr_enc, s.crf));
    let opts = EncodeOptions {
        in_fps_num: info.fps_num,
        in_fps_den: info.fps_den as u64,
        interpolate_to: None,
        anamorphic: None,
        codec: VideoCodec::H265,
        crf: s.crf,
        // The video alone, then the source audio of the range muxed in as the export does with
        // new audio (`video.mkv` + `ffmpeg::mux`).
        audio_from: None,
        hdr: Some(hdr_enc),
    };
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d)?;
    }
    let video_mkv = work.join("video.mkv");
    let t = Instant::now();
    let (width, height) = hdr::convert::frame_size(&out_dir)?;
    let feed = |pipe: &mut dyn std::io::Write| {
        hdr::convert::stream_frames(&out_dir, total, &params, &cancel, pipe)
    };
    let input = VideoInput::Rgb48 {
        width,
        height,
        feed: &feed,
    };
    ffmpeg::encode(&tools, &input, &opts, &video_mkv, &cancel, |_| {})?;
    let encode_s = t.elapsed().as_secs_f64();
    let original = info.has_audio.then(|| {
        (
            info.path.clone(),
            info.frame_time(start),
            total as f64 / info.fps(),
        )
    });
    ffmpeg::mux(&tools, &video_mkv, &[], original, &out, &cancel)?;

    let per = |s: f64| s / total as f64 * 1000.0;
    println!(
        "time: pass 1 (convert + measure) {measure_s:.1} s ({:.0} ms/frame), \
         pass 2 (convert + encode) {encode_s:.1} s ({:.0} ms/frame)",
        per(measure_s),
        per(encode_s)
    );
    println!("wrote {}", out.display());

    if !previews.is_empty() {
        let share = share.context("--previews needs --share")?;
        std::fs::create_dir_all(&share)?;
        let tmp = work.join("preview");
        std::fs::create_dir_all(&tmp)?;
        let font = "C\\:/Windows/Fonts/arialbd.ttf";
        let job = JobHdr { params, divisor: 1 };
        for n in previews {
            let p = tmp.join(format!("{n:06}_hdr.png"));
            hdr::convert::tone_mapped_frame(&work, n, &job)
                .context("could not convert a preview frame")?
                .save(&p)?;
            let label_l = format!("SDR SeedVR2 7B  frame {n}");
            let label_r = format!(
                "{} {} nits white {white}, tone mapped to SDR",
                mode.name(),
                params.peak_nits
            );
            let jpg = share.join(format!("{name}_{n:06}.jpg"));
            let draw = |l: &str| {
                format!(
                    "drawtext=fontfile='{font}':text='{l}':x=24:y=20:fontsize=36:fontcolor=white:borderw=3:bordercolor=black"
                )
            };
            let mut cmd = std::process::Command::new(&tools.ffmpeg);
            cmd.args(["-v", "error", "-y", "-i"])
                .arg(ffmpeg::frame_file(&out_dir, n))
                .arg("-i")
                .arg(&p)
                .arg("-filter_complex")
                .arg(format!(
                    "[0:v]{}[a];[1:v]{}[b];[a][b]hstack=inputs=2,scale=iw/2:ih/2:flags=lanczos",
                    draw(&label_l),
                    draw(&label_r)
                ))
                .args(["-frames:v", "1", "-q:v", "3"])
                .arg(&jpg);
            if !cmd.status()?.success() {
                bail!("preview {n} failed");
            }
            println!("preview {}", jpg.display());
        }
    }
    Ok(())
}
