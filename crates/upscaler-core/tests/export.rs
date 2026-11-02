//! End to end with the real ffmpeg (skipped when it is not installed): a short test video with
//! audio is exported with the basic upscaler and with a stand-in for the Codex CLI.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

use upscaler_core::ffmpeg::{self, Tools};
use upscaler_core::job::{self, ExportEvent, ExportSpec};
use upscaler_core::settings::{
    AiRate, AudioMode, LfeLevel, OutputSize, ProviderKind, Settings, SurroundEngine,
};

fn make_video(tools: &Tools, path: &Path) {
    let status = std::process::Command::new(&tools.ffmpeg)
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=24:duration=1",
        ])
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// Points the AI audio pack at an empty folder for the whole test binary, once, so no test
/// ever sees the pack installed on this machine. Every export in this file goes through it.
fn no_audio_pack() -> &'static Path {
    static EMPTY: OnceLock<tempfile::TempDir> = OnceLock::new();
    EMPTY
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var(upscaler_core::audio::pack::DIR_ENV, dir.path());
            dir
        })
        .path()
}

fn run(tools: &Tools, spec: &ExportSpec) -> Vec<String> {
    no_audio_pack();
    let log = Mutex::new(Vec::new());
    job::run_export(tools, spec, &AtomicBool::new(false), &|e| {
        let line = match &e {
            ExportEvent::Finished(_) => "finished".to_string(),
            ExportEvent::Failed(m) => format!("failed: {m}"),
            ExportEvent::FrameDone { seconds, .. } => {
                format!("frame {}", if seconds.is_some() { "ai" } else { "reused" })
            }
            _ => return,
        };
        log.lock().unwrap().push(line);
    });
    log.into_inner().unwrap()
}

#[test]
fn export_basic_with_audio_and_resume() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("in.mp4");
    make_video(&tools, &src);
    let info = ffmpeg::probe(&tools, &src).unwrap();
    assert_eq!((info.width, info.height), (320, 180));
    assert!(info.has_audio);

    let frame = ffmpeg::grab_frame(&tools, &info, 10, None, false).unwrap();
    assert_eq!(frame.dimensions(), (320, 180));

    let mut settings = Settings {
        provider: ProviderKind::Basic,
        output_size: OutputSize::Scale2,
        ..Settings::default()
    };
    settings.work_dir = dir.path().join("work").display().to_string();
    let out = dir.path().join("out.mp4");
    let spec = ExportSpec {
        info: info.clone(),
        start: 4,
        end: 16,
        output: out.clone(),
        settings: settings.clone(),
    };
    let log = run(&tools, &spec);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");
    assert_eq!(log.iter().filter(|l| l.starts_with("frame")).count(), 12);
    let result = ffmpeg::probe(&tools, &out).unwrap();
    assert_eq!((result.width, result.height), (640, 360));
    assert!(result.has_audio, "audio must be carried over");
    assert!(
        (result.frame_count as i64 - 12).abs() <= 1,
        "frames: {}",
        result.frame_count
    );

    // Same job again: every frame is already there, nothing is upscaled anew.
    let again = run(&tools, &spec);
    assert_eq!(again, vec!["finished".to_string()]);

    // Every other frame with interpolation back to 24 fps.
    let spec2 = ExportSpec {
        settings: Settings {
            ai_rate: AiRate::Half,
            ..settings
        },
        output: dir.path().join("half.mp4"),
        ..spec
    };
    let log = run(&tools, &spec2);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");
    assert_eq!(log.iter().filter(|l| l.starts_with("frame")).count(), 6);
    let half = ffmpeg::probe(&tools, &dir.path().join("half.mp4")).unwrap();
    assert!((half.fps() - 24.0).abs() < 0.1);
}

/// A shell script that behaves like `codex exec`: reads the prompt on stdin, checks it was
/// asked for output.png and "edits" input.png (it just copies it), like the real agent would.
#[cfg(unix)]
#[test]
fn chatgpt_provider_through_codex_stand_in() {
    use std::os::unix::fs::PermissionsExt;
    let Some(tools) = Tools::locate("") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("codex");
    std::fs::write(
        &fake,
        "#!/bin/sh\n\
         prompt=$(cat)\n\
         case \"$prompt\" in *output.png*) ;; *) echo 'no output.png in prompt' >&2; exit 3;; esac\n\
         case \"$*\" in *--skip-git-repo-check*) ;; *) exit 4;; esac\n\
         if [ -n \"$OPENAI_API_KEY\" ]; then echo 'api key leaked' >&2; exit 5; fi\n\
         echo \"$*\" >> ../../calls.txt\n\
         cp input.png output.png\n\
         echo DONE\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let src = dir.path().join("in.mp4");
    make_video(&tools, &src);
    let info = ffmpeg::probe(&tools, &src).unwrap();
    let mut settings = Settings {
        provider: ProviderKind::ChatGptSubscription,
        ..Settings::default()
    };
    settings.codex_path = fake.display().to_string();
    settings.work_dir = dir.path().join("work").display().to_string();
    let spec = ExportSpec {
        info,
        start: 0,
        end: 5,
        output: dir.path().join("gpt.mp4"),
        settings,
    };
    std::env::set_var("OPENAI_API_KEY", "sk-test");
    let log = run(&tools, &spec);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");
    let calls = std::fs::read_to_string(dir.path().join("work/calls.txt")).unwrap();
    assert_eq!(calls.lines().count(), 5);
    // Frame 1 has no reference; later frames of the same shot carry the previous result.
    let first = calls.lines().next().unwrap();
    assert!(!first.contains("reference.png"), "{first}");
    assert!(calls.lines().nth(1).unwrap().contains("reference.png"));
    let out = ffmpeg::probe(&tools, &dir.path().join("gpt.mp4")).unwrap();
    assert_eq!((out.width, out.height), (640, 360));
}

/// Reference decode of frame `n` by counting frames from the start (no seeking).
fn frame_by_counting(tools: &Tools, info: &ffmpeg::VideoInfo, n: u64) -> image::RgbImage {
    let filter = format!(
        "{},select=eq(n\\,{n})",
        ffmpeg::frame_filter(info, info.width, info.height)
    );
    let out = std::process::Command::new(&tools.ffmpeg)
        .args(["-loglevel", "error", "-i"])
        .arg(&info.path)
        .args([
            "-vf",
            &filter,
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ])
        .output()
        .unwrap();
    image::RgbImage::from_raw(info.width, info.height, out.stdout).expect("reference frame")
}

fn mean_diff(a: &image::RgbImage, b: &image::RgbImage) -> f64 {
    assert_eq!(a.dimensions(), b.dimensions());
    let sum: u64 = a
        .as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64)
        .sum();
    sum as f64 / a.as_raw().len() as f64
}

/// An NTSC DVD (MPEG-2 program stream, 720x480 anamorphic 16:9, interlaced, AC-3): the app
/// must show it at 854x480, land on exactly the frame asked for, and export 16:9.
#[test]
fn dvd_source_is_frame_accurate_and_widescreen() {
    let Some(tools) = Tools::locate("") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("dvd.mpg");
    let status = std::process::Command::new(&tools.ffmpeg)
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=854x480:rate=30000/1001:duration=3",
        ])
        .args(["-f", "lavfi", "-i", "sine=frequency=500:duration=3"])
        .args([
            "-target",
            "ntsc-dvd",
            "-aspect",
            "16:9",
            "-flags",
            "+ilme+ildct",
            "-top",
            "1",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    let info = ffmpeg::probe(&tools, &src).unwrap();
    assert_eq!((info.coded_width, info.coded_height), (720, 480));
    assert_eq!((info.width, info.height), (854, 480));
    assert!(info.interlaced);
    assert!(info.has_audio);

    for n in [0u64, 1, 13, 44, 71] {
        let got = ffmpeg::grab_frame(&tools, &info, n, None, false).unwrap();
        let want = frame_by_counting(&tools, &info, n);
        let d = mean_diff(&got, &want);
        assert!(d < 1.0, "frame {n} differs from the reference by {d}");
        if n > 0 {
            // ...and is clearly not its neighbour.
            let prev = frame_by_counting(&tools, &info, n - 1);
            assert!(
                mean_diff(&got, &prev) > d,
                "frame {n} looks like frame {}",
                n - 1
            );
        }
    }

    let mut settings = Settings {
        provider: ProviderKind::Basic,
        ..Settings::default()
    };
    settings.work_dir = dir.path().join("work").display().to_string();
    let out = dir.path().join("dvd_up.mp4");
    let spec = ExportSpec {
        info: info.clone(),
        start: 30,
        end: 45,
        output: out.clone(),
        settings,
    };
    let log = run(&tools, &spec);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");
    // The extracted frames are the same frames the preview shows.
    let first = image::open(spec.job_dir().join(job::INPUT_DIR).join("000001.png"))
        .unwrap()
        .to_rgb8();
    assert!(mean_diff(&first, &frame_by_counting(&tools, &info, 30)) < 1.0);
    // Restored with square pixels, exported anamorphic again: 2x the stored 720x480, same
    // pixel aspect ratio, so it still displays as 16:9.
    let result = ffmpeg::probe(&tools, &out).unwrap();
    assert_eq!((result.coded_width, result.coded_height), (1440, 960));
    assert_eq!(result.sar, (32, 27));
    assert!(result.has_audio);
    let up = image::open(spec.job_dir().join(job::OUTPUT_DIR).join("000001.png")).unwrap();
    assert_eq!(
        (up.width(), up.height()),
        (1708, 960),
        "frames are restored with square pixels"
    );

    // With "keep anamorphic" off the export has square pixels.
    let mut square = spec.clone();
    square.settings.keep_anamorphic = false;
    square.output = dir.path().join("dvd_square.mp4");
    assert_eq!(
        run(&tools, &square).last().map(String::as_str),
        Some("finished")
    );
    let result = ffmpeg::probe(&tools, &square.output).unwrap();
    assert_eq!(
        (result.coded_width, result.coded_height, result.sar),
        (1708, 960, (1, 1))
    );
}

// ---------------------------------------------------------------- surround audio export

/// A 4 s 30000/1001 clip (160x90) with Swedish-tagged stereo AC-3 audio. The audio is a
/// different rising tone in each channel: never periodic, so tracks can be lined up by
/// cross-correlation.
fn make_ntsc_clip(tools: &Tools, path: &Path) {
    let status = std::process::Command::new(&tools.ffmpeg)
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=160x90:rate=30000/1001:duration=4")
        .args(["-f", "lavfi", "-i"])
        .arg(
            "aevalsrc=0.4*sin(2*PI*(150*t+400*t*t))|0.4*sin(2*PI*(230*t+300*t*t))\
             :s=48000:d=4,aformat=channel_layouts=stereo",
        )
        .args([
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "ac3",
            "-b:a",
            "192k",
            "-metadata:s:a:0",
            "language=swe",
            "-shortest",
        ])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// One stream entry (`stream=...`) of `path` as `key=value` lines.
fn stream_entries(tools: &Tools, path: &Path, select: &str, entries: &str) -> String {
    let out = std::process::Command::new(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", select, "-show_entries"])
        .arg(entries)
        .args(["-of", "default=nw=1"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).replace('\r', "")
}

fn entry(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .map(str::to_string)
}

/// Samples of audio stream `index` when decoded (at 48 kHz).
fn decoded_samples(tools: &Tools, path: &Path, index: usize) -> u64 {
    let out = std::process::Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", &format!("0:a:{index}"), "-ac", "1", "-ar", "48000"])
        .args(["-f", "s16le", "-"])
        .output()
        .unwrap();
    assert!(out.status.success());
    out.stdout.len() as u64 / 2
}

/// The front-left channel of audio stream `index`, decoded at 48 kHz.
fn decoded_front_left(tools: &Tools, path: &Path, index: usize) -> Vec<f32> {
    let out = std::process::Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", &format!("0:a:{index}"), "-af", "pan=mono|c0=FL"])
        .args(["-ar", "48000", "-f", "f32le", "-"])
        .output()
        .unwrap();
    assert!(out.status.success());
    out.stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// The lag `l` (within Â±`max`) at which `b[n + l]` matches `a[n]` best, over 0.5 s of `a`
/// from 0.1 s on.
fn best_lag(a: &[f32], b: &[f32], max: i64) -> i64 {
    let (from, len) = (4800usize, 24000usize);
    assert!(a.len() >= from + len, "too short: {}", a.len());
    let score = |l: i64| {
        let (mut dot, mut eb) = (0.0f64, 0.0f64);
        for (n, x) in a.iter().enumerate().skip(from).take(len) {
            let j = n as i64 + l;
            if j >= 0 && (j as usize) < b.len() {
                let y = b[j as usize] as f64;
                dot += *x as f64 * y;
                eb += y * y;
            }
        }
        dot / eb.sqrt().max(1e-9)
    };
    (-max..=max)
        .max_by(|x, y| score(*x).total_cmp(&score(*y)))
        .unwrap()
}

/// `start_time` of stream `select` in seconds.
fn start_time(tools: &Tools, path: &Path, select: &str) -> f64 {
    let s = stream_entries(tools, path, select, "stream=start_time");
    entry(&s, "start_time").unwrap().parse().unwrap()
}

/// Video frames in `path`, counted.
fn video_frames(tools: &Tools, path: &Path) -> u64 {
    let out = std::process::Command::new(&tools.ffprobe)
        .args(["-v", "error", "-count_packets", "-select_streams", "v:0"])
        .args([
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "default=nw=1",
        ])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).replace('\r', "");
    entry(&text, "nb_read_packets").unwrap().parse().unwrap()
}

fn surround_settings(work: &Path) -> Settings {
    let mut s = Settings {
        provider: ProviderKind::Basic,
        output_size: OutputSize::Scale2,
        audio_mode: AudioMode::Surround,
        surround_engine: SurroundEngine::Classic,
        ..Settings::default()
    };
    s.work_dir = work.display().to_string();
    s
}

/// Runs an export and returns every event in a readable form.
fn run_events(tools: &Tools, spec: &ExportSpec) -> Vec<String> {
    no_audio_pack();
    let log = Mutex::new(Vec::new());
    job::run_export(tools, spec, &AtomicBool::new(false), &|e| {
        let line = match &e {
            ExportEvent::Finished(p) => format!("finished {}", p.display()),
            ExportEvent::Failed(m) => format!("failed: {m}"),
            ExportEvent::FrameDone { .. } => "frame".to_string(),
            ExportEvent::Progress { stage, .. } => format!("progress {stage:?}"),
            other => format!("{other:?}"),
        };
        log.lock().unwrap().push(line);
    });
    log.into_inner().unwrap()
}

#[test]
fn keeps_exact_frame_rate() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("ntsc.mkv");
    make_ntsc_clip(&tools, &src);
    let info = ffmpeg::probe(&tools, &src).unwrap();
    assert_eq!((info.fps_num, info.fps_den), (30000, 1001));

    // Original audio: the MP4 path.
    let mut original = surround_settings(&dir.path().join("work"));
    original.audio_mode = AudioMode::Original;
    let spec = ExportSpec {
        info: info.clone(),
        start: 10,
        end: 40,
        output: dir.path().join("orig.mp4"),
        settings: original,
    };
    let log = run_events(&tools, &spec);
    assert!(log.last().unwrap().starts_with("finished"), "{log:?}");
    let v = stream_entries(&tools, &spec.output, "v:0", "stream=r_frame_rate");
    assert_eq!(entry(&v, "r_frame_rate").as_deref(), Some("30000/1001"));

    // 5.1: the MKV path.
    let spec = ExportSpec {
        output: dir.path().join("surround.mp4"),
        settings: surround_settings(&dir.path().join("work")),
        ..spec
    };
    let log = run_events(&tools, &spec);
    let mkv = dir.path().join("surround.mkv");
    assert_eq!(
        log.last().unwrap(),
        &format!("finished {}", mkv.display()),
        "{log:?}"
    );
    let v = stream_entries(&tools, &mkv, "v:0", "stream=r_frame_rate");
    assert_eq!(entry(&v, "r_frame_rate").as_deref(), Some("30000/1001"));
}

#[test]
fn audio_matches_video_length() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("ntsc.mkv");
    make_ntsc_clip(&tools, &src);
    let info = ffmpeg::probe(&tools, &src).unwrap();
    // 2.5 s: 75 frames from 0.5 s on.
    let spec = ExportSpec {
        info,
        start: 15,
        end: 90,
        output: dir.path().join("out.mkv"),
        settings: surround_settings(&dir.path().join("work")),
    };
    let log = run_events(&tools, &spec);
    assert!(log.last().unwrap().starts_with("finished"), "{log:?}");
    assert_eq!(video_frames(&tools, &spec.output), 75);
    let video = (75.0 * 1001.0 / 30000.0 * 48000.0_f64).round() as i64;
    let frame = (1001.0 / 30000.0 * 48000.0_f64).round() as i64;
    for (i, name) in ["flac 5.1", "ac3 5.1"].iter().enumerate() {
        let n = decoded_samples(&tools, &spec.output, i) as i64;
        assert!(
            (n - video).abs() <= frame,
            "{name}: {n} samples, video {video}"
        );
    }
    // The original is stream-copied, so it can only be cut on its own (AC-3, 1536-sample)
    // frame boundaries: one audio frame more is allowed for it.
    let n = decoded_samples(&tools, &spec.output, 2) as i64;
    assert!(
        (n - video).abs() <= frame + 1536,
        "original: {n} samples, video {video}"
    );
    // The original sits where the FLAC (cut sample-exact from the same range) sits, within
    // one of its own frames: its content offset plus its start time against the FLAC's.
    let flac = decoded_front_left(&tools, &spec.output, 0);
    let orig = decoded_front_left(&tools, &spec.output, 2);
    let lag = best_lag(&flac, &orig, 4000);
    let starts = start_time(&tools, &spec.output, "a:2") - start_time(&tools, &spec.output, "a:0");
    // FLAC sample n plays at its start + n; the matching original sample n + lag at the
    // original's start + n + lag.
    let offset = starts * 48000.0 + lag as f64;
    assert!(
        offset.abs() <= 1536.0,
        "original placed {offset} samples off (lag {lag}, start difference {starts} s)"
    );
    assert!(!dir.path().join("out.part.mkv").exists());
}

/// Original audio copied from a long-GOP source into a range that starts mid-GOP: the copy
/// must start where the picture starts, not at the keyframe before it (input seeking with
/// stream copy keeps that pre-roll).
#[test]
fn original_audio_has_no_preroll_on_long_gop() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("gop.avi");
    let status = std::process::Command::new(&tools.ffmpeg)
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=160x90:rate=25:duration=10")
        .args(["-f", "lavfi", "-i"])
        .arg(
            "aevalsrc=0.4*sin(2*PI*(150*t+40*t*t))|0.4*sin(2*PI*(230*t+30*t*t))\
             :s=48000:d=10,aformat=channel_layouts=stereo",
        )
        .args(["-c:v", "libx264", "-g", "250", "-keyint_min", "250"])
        .args(["-sc_threshold", "0", "-pix_fmt", "yuv420p"])
        .args(["-c:a", "ac3", "-b:a", "192k", "-shortest"])
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    let info = ffmpeg::probe(&tools, &src).unwrap();
    let mut settings = surround_settings(&dir.path().join("work"));
    settings.audio_mode = AudioMode::Original;
    // The source's audio decoded from 6 s on, to check the copy's content against.
    let reference = dir.path().join("reference.wav");
    let st = std::process::Command::new(&tools.ffmpeg)
        .args(["-y", "-v", "error", "-i"])
        .arg(&src)
        .args(["-ss", "6", "-t", "1", "-vn", "-ar", "48000"])
        .arg(&reference)
        .status()
        .unwrap();
    assert!(st.success());
    let want = decoded_front_left(&tools, &reference, 0);
    for name in ["out.mkv", "out.mp4"] {
        // 1 s from 6 s on: the only keyframe before it is at 0 s.
        let spec = ExportSpec {
            info: info.clone(),
            start: 150,
            end: 175,
            output: dir.path().join(name),
            settings: settings.clone(),
        };
        check_no_preroll(&tools, &spec, &want);
    }
}

fn check_no_preroll(tools: &Tools, spec: &ExportSpec, want: &[f32]) {
    let log = run_events(tools, spec);
    assert!(log.last().unwrap().starts_with("finished"), "{log:?}");
    let (v0, a0) = (
        start_time(&tools, &spec.output, "v:0"),
        start_time(&tools, &spec.output, "a:0"),
    );
    assert!(
        (a0 - v0).abs() <= 0.032,
        "{}: audio starts at {a0}, video at {v0}",
        spec.output.display()
    );
    let out = std::process::Command::new(&tools.ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration"])
        .args(["-of", "default=nw=1"])
        .arg(&spec.output)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).replace('\r', "");
    let duration: f64 = entry(&text, "duration").unwrap().parse().unwrap();
    assert!((duration - 1.0).abs() <= 0.05, "duration {duration}");
    // The content is the range's: the source decoded from 6 s lines up with the copy.
    let got = decoded_front_left(tools, &spec.output, 0);
    let lag = best_lag(want, &got, 4000);
    let offset = (a0 - v0) * 48000.0 + lag as f64;
    assert!(
        offset.abs() <= 1536.0,
        "audio placed {offset} samples off (lag {lag}, start difference {} s)",
        a0 - v0
    );
}

#[test]
fn surround_export_has_three_tracks() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("ntsc.mkv");
    make_ntsc_clip(&tools, &src);
    let info = ffmpeg::probe(&tools, &src).unwrap();
    let spec = ExportSpec {
        info,
        start: 0,
        end: 30,
        output: dir.path().join("three.mkv"),
        settings: surround_settings(&dir.path().join("work")),
    };
    let log = run_events(&tools, &spec);
    assert!(log.last().unwrap().starts_with("finished"), "{log:?}");
    let all = "stream=index,codec_type,codec_name,channels:stream_disposition=default:stream_tags=title,language";
    let s = |i: usize| stream_entries(&tools, &spec.output, &i.to_string(), all);
    let v = s(0);
    assert_eq!(entry(&v, "codec_type").as_deref(), Some("video"));
    let flac = s(1);
    assert_eq!(
        entry(&flac, "codec_name").as_deref(),
        Some("flac"),
        "{flac}"
    );
    assert_eq!(entry(&flac, "channels").as_deref(), Some("6"));
    assert_eq!(entry(&flac, "DISPOSITION:default").as_deref(), Some("1"));
    assert_eq!(entry(&flac, "TAG:title").as_deref(), Some("5.1 (FLAC)"));
    assert_eq!(entry(&flac, "TAG:language").as_deref(), Some("swe"));
    let ac3 = s(2);
    assert_eq!(entry(&ac3, "codec_name").as_deref(), Some("ac3"), "{ac3}");
    assert_eq!(entry(&ac3, "channels").as_deref(), Some("6"));
    assert_eq!(entry(&ac3, "DISPOSITION:default").as_deref(), Some("0"));
    assert_eq!(entry(&ac3, "TAG:title").as_deref(), Some("5.1 (AC-3)"));
    let orig = s(3);
    assert_eq!(entry(&orig, "codec_name").as_deref(), Some("ac3"), "{orig}");
    assert_eq!(entry(&orig, "channels").as_deref(), Some("2"));
    assert_eq!(entry(&orig, "DISPOSITION:default").as_deref(), Some("0"));
    assert_eq!(entry(&orig, "TAG:title").as_deref(), Some("Original"));
    assert_eq!(entry(&orig, "TAG:language").as_deref(), Some("swe"));
    assert!(stream_entries(&tools, &spec.output, "4", "stream=index").is_empty());
    // The AC-3 lines up with the FLAC sample for sample (the encoder's priming is removed).
    let flac = decoded_front_left(&tools, &spec.output, 0);
    let ac3 = decoded_front_left(&tools, &spec.output, 1);
    let lag = best_lag(&flac, &ac3, 600);
    assert!(lag.abs() <= 2, "AC-3 is {lag} samples off the FLAC");
}

/// `film.mkv` exported as `film.mp4` with new audio would be written over the source.
#[test]
fn new_audio_never_overwrites_the_source() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("film.mkv");
    make_ntsc_clip(&tools, &src);
    let before = std::fs::read(&src).unwrap();
    let info = ffmpeg::probe(&tools, &src).unwrap();
    let spec = ExportSpec {
        info,
        start: 0,
        end: 10,
        output: dir.path().join("film.mp4"),
        settings: surround_settings(&dir.path().join("work")),
    };
    let log = run_events(&tools, &spec);
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("source video"), "{log:?}");
    assert!(!spec.job_dir().join(job::INPUT_DIR).exists());
    assert_eq!(std::fs::read(&src).unwrap(), before);
}

#[test]
fn audio_only_change_skips_frames() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("ntsc.mkv");
    make_ntsc_clip(&tools, &src);
    let info = ffmpeg::probe(&tools, &src).unwrap();
    let spec = ExportSpec {
        info,
        start: 0,
        end: 20,
        output: dir.path().join("a.mkv"),
        settings: surround_settings(&dir.path().join("work")),
    };
    let log = run_events(&tools, &spec);
    assert!(log.last().unwrap().starts_with("finished"), "{log:?}");
    assert_eq!(log.iter().filter(|l| *l == "frame").count(), 20);
    let video = spec.job_dir().join("video.mkv");
    let before = std::fs::metadata(&video).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let mut changed = spec.clone();
    changed.settings.lfe_level = LfeLevel::Strong;
    changed.output = dir.path().join("b.mkv");
    assert_eq!(changed.job_dir(), spec.job_dir());
    let log = run_events(&tools, &changed);
    assert!(log.last().unwrap().starts_with("finished"), "{log:?}");
    assert!(!log.iter().any(|l| l == "frame"), "{log:?}");
    let after = std::fs::metadata(&video).unwrap().modified().unwrap();
    assert_eq!(before, after, "video.mkv must not be encoded again");
    assert!(changed.output.is_file());
}

/// Quality improvement without 5.1 yields one stereo FLAC: it leads (default), then the
/// original. A PCM original is never put in the container as PCM: it becomes FLAC.
#[test]
fn mux_stereo_flac_first_and_no_pcm() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ff = |args: &[&str], out: &Path| {
        let ok = std::process::Command::new(&tools.ffmpeg)
            .args(["-y", "-loglevel", "error"])
            .args(args)
            .arg(out)
            .status()
            .unwrap();
        assert!(ok.success());
    };
    let video = dir.path().join("video.mkv");
    ff(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=25:duration=2",
            "-c:v",
            "libx264",
        ],
        &video,
    );
    let flac = dir.path().join("stereo.flac");
    ff(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=500:sample_rate=48000:duration=2",
            "-ac",
            "2",
            "-c:a",
            "flac",
        ],
        &flac,
    );
    let src = dir.path().join("pcm.mkv");
    ff(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=300:sample_rate=48000:duration=4",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=25:duration=4",
            "-map",
            "1:v",
            "-map",
            "0:a",
            "-c:v",
            "libx264",
            "-c:a",
            "pcm_s16le",
        ],
        &src,
    );
    let out = dir.path().join("out.mkv");
    ffmpeg::mux(
        &tools,
        &video,
        &[upscaler_core::audio::job::AudioTrack::FlacStereo(flac)],
        Some((src, 1.0, 2.0)),
        &out,
        &AtomicBool::new(false),
    )
    .unwrap();
    let all = "stream=codec_name,channels:stream_disposition=default:stream_tags=title";
    let a = stream_entries(&tools, &out, "a:0", all);
    assert_eq!(entry(&a, "codec_name").as_deref(), Some("flac"), "{a}");
    assert_eq!(entry(&a, "channels").as_deref(), Some("2"));
    assert_eq!(entry(&a, "DISPOSITION:default").as_deref(), Some("1"));
    assert_eq!(entry(&a, "TAG:title").as_deref(), Some("Stereo (FLAC)"));
    let o = stream_entries(&tools, &out, "a:1", all);
    assert_eq!(entry(&o, "codec_name").as_deref(), Some("flac"), "{o}");
    assert_eq!(entry(&o, "DISPOSITION:default").as_deref(), Some("0"));
    assert_eq!(entry(&o, "TAG:title").as_deref(), Some("Original"));
    // PCM is cut sample exactly.
    assert_eq!(decoded_samples(&tools, &out, 1), 96000);
    assert!(stream_entries(&tools, &out, "a:2", "stream=index").is_empty());
    assert!(!out.with_extension("original.part.mka").exists());
    assert!(!out.with_extension("part.mkv").exists());
}

/// A mux that fails after ffmpeg has created its output leaves neither a `.part.mkv` nor a
/// changed file at `out`. The failure: a track Matroska cannot hold (ADPCM IMA QT has no
/// codec tag there), which ffmpeg only notices when it writes the header.
#[test]
fn failed_mux_leaves_out_untouched() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ff = |args: &[&str], out: &Path| {
        let ok = std::process::Command::new(&tools.ffmpeg)
            .args(["-y", "-loglevel", "error"])
            .args(args)
            .arg(out)
            .status()
            .unwrap();
        assert!(ok.success());
    };
    let video = dir.path().join("video.mkv");
    ff(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=25:duration=1",
            "-c:v",
            "libx264",
        ],
        &video,
    );
    let bad = dir.path().join("bad.mov");
    ff(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=sample_rate=48000:duration=1",
            "-c:a",
            "adpcm_ima_qt",
        ],
        &bad,
    );
    let out = dir.path().join("out.mkv");
    let before = b"the user's own file".to_vec();
    std::fs::write(&out, &before).unwrap();
    let result = ffmpeg::mux(
        &tools,
        &video,
        &[upscaler_core::audio::job::AudioTrack::FlacStereo(bad)],
        None,
        &out,
        &AtomicBool::new(false),
    );
    assert!(result.is_err(), "the mux must fail");
    assert!(!out.with_extension("part.mkv").exists());
    assert_eq!(std::fs::read(&out).unwrap(), before);
}

/// A 5.1 source with 5.1 chosen and no enhancement has nothing to change: the export takes
/// the MP4 path with the source audio, like "Unchanged", with no audio job (and no pack, even
/// with the AI engine chosen).
#[test]
fn five_one_source_keeps_its_audio() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("surround.mkv");
    let status = std::process::Command::new(&tools.ffmpeg)
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=160x90:rate=24:duration=1")
        .args(["-f", "lavfi", "-i"])
        .arg("sine=frequency=440:duration=1,aformat=channel_layouts=5.1(side)")
        .args([
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "ac3",
            "-shortest",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    let info = ffmpeg::probe(&tools, &src).unwrap();
    let work = dir.path().join("work");
    let mut settings = surround_settings(&work);
    settings.surround_engine = SurroundEngine::LocalAi;
    let spec = ExportSpec {
        info,
        start: 0,
        end: 12,
        output: dir.path().join("out.mp4"),
        settings,
    };
    let log = run_events(&tools, &spec);
    assert_eq!(
        log.last().unwrap(),
        &format!("finished {}", spec.output.display()),
        "{log:?}"
    );
    assert!(!log.iter().any(|l| l.contains("Audio")), "{log:?}");
    assert!(!dir.path().join("out.mkv").exists());
    assert!(!work.join("audio").exists(), "no audio job may run");
    let a = stream_entries(&tools, &spec.output, "a:0", "stream=channels");
    assert_eq!(entry(&a, "channels").as_deref(), Some("6"), "{a}");
}

#[test]
fn missing_pack_fails_before_frames() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    // run_events points the pack at an empty folder: never the real pack of this machine.
    let src = dir.path().join("ntsc.mkv");
    make_ntsc_clip(&tools, &src);
    let info = ffmpeg::probe(&tools, &src).unwrap();
    let mut settings = surround_settings(&dir.path().join("work"));
    settings.surround_engine = SurroundEngine::LocalAi;
    let spec = ExportSpec {
        info,
        start: 0,
        end: 10,
        output: dir.path().join("x.mkv"),
        settings,
    };
    let log = run_events(&tools, &spec);
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].starts_with("failed: "), "{log:?}");
    assert!(log[0].contains("Settings"), "{log:?}");
    assert!(!spec.job_dir().join(job::INPUT_DIR).exists());
}

/// Checks frame accuracy on real files: `UPSCALER_TEST_VIDEOS=a.mp4:b.vob cargo test -- --ignored`
/// (`;` between files on Windows).
#[test]
#[ignore]
fn frame_accuracy_on_given_videos() {
    let tools = Tools::locate("").expect("ffmpeg");
    let list = std::env::var("UPSCALER_TEST_VIDEOS").expect("UPSCALER_TEST_VIDEOS");
    for path in std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()) {
        let path = path.display().to_string();
        let info = ffmpeg::probe(&tools, Path::new(&path)).unwrap();
        eprintln!(
            "{path}: {}x{} shown as {}x{}, {} frames, interlaced {}",
            info.coded_width,
            info.coded_height,
            info.width,
            info.height,
            info.frame_count,
            info.interlaced
        );
        let mut checked = info.clone();
        checked.detected_crop = ffmpeg::detect_black_bars(&tools, &info);
        eprintln!("  black bars: {:?}", checked.detected_crop);
        eprintln!(
            "  aspect check: {:?}",
            upscaler_core::aspect::check(&checked)
        );
        let last = info.frame_count - 1;
        for n in [0, 1, 2, 100, 333, 548, 777, last / 2, last - 30] {
            let got = ffmpeg::grab_frame(&tools, &info, n, None, false).unwrap();
            let d = mean_diff(&got, &frame_by_counting(&tools, &info, n));
            eprintln!("  frame {n}: diff {d:.3}");
            assert!(d < 1.0, "{path} frame {n}: {d}");
        }
    }
}

/// "Change aspect ratio": a 16:9 video exported as 4:3 through the Codex stand-in. The frame
/// arrives on a 4:3 canvas with grey above and below, the prompt asks the AI to fill it, and
/// the exported video is 4:3.
#[cfg(unix)]
#[test]
fn change_aspect_ratio_extends_the_canvas() {
    use std::os::unix::fs::PermissionsExt;
    let Some(tools) = Tools::locate("") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("codex");
    std::fs::write(
        &fake,
        "#!/bin/sh\ncat > ../../last-prompt.txt\ncp input.png ../../last-input.png\ncp input.png output.png\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let src = dir.path().join("in.mp4");
    make_video(&tools, &src); // 320x180, 16:9
    let info = ffmpeg::probe(&tools, &src).unwrap();
    let mut settings = Settings {
        provider: ProviderKind::ChatGptSubscription,
        change_aspect: true,
        target_aspect: upscaler_core::settings::AspectChoice::Tv4x3,
        pad_to_model_aspect: false,
        ..Settings::default()
    };
    settings.codex_path = fake.display().to_string();
    settings.work_dir = dir.path().join("work").display().to_string();
    assert_eq!(job::geometry(&settings, &info).work, (640, 480));
    let spec = ExportSpec {
        info,
        start: 0,
        end: 3,
        output: dir.path().join("43.mp4"),
        settings,
    };
    let log = run(&tools, &spec);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");

    let prompt = std::fs::read_to_string(dir.path().join("work/last-prompt.txt")).unwrap();
    assert!(
        prompt.contains("Change the aspect ratio to 4:3"),
        "{prompt}"
    );
    assert!(prompt.contains("above and below"));
    let canvas = image::open(dir.path().join("work/last-input.png"))
        .unwrap()
        .to_rgb8();
    assert_eq!(canvas.dimensions(), (320, 240));
    assert_eq!(canvas.get_pixel(160, 5), &upscaler_core::frame::EXTEND_FILL);
    let out = ffmpeg::probe(&tools, &spec.output).unwrap();
    assert_eq!((out.width, out.height), (640, 480));
}

/// Black bars are found and cut away: a 2.35:1 picture letterboxed into a 4:3 PAL DVD frame.
#[test]
fn letterbox_is_detected_and_removed() {
    let Some(tools) = Tools::locate("") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("letterbox.mpg");
    let status = std::process::Command::new(&tools.ffmpeg)
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=768x326:rate=25:duration=12",
        ])
        .args([
            "-vf",
            "scale=720:326,pad=720:576:0:125:black",
            "-target",
            "pal-dvd",
            "-aspect",
            "4:3",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    let info = ffmpeg::probe(&tools, &src).unwrap();
    let crop = ffmpeg::detect_black_bars(&tools, &info).expect("bars found");
    assert_eq!((crop.w, crop.x), (720, 0));
    assert!(
        (crop.h as i64 - 326).abs() <= 4 && (crop.y as i64 - 125).abs() <= 4,
        "{crop:?}"
    );
    let cut = info.with_crop(Some(crop));
    assert_eq!(cut.width, 768);
    let frame = ffmpeg::grab_frame(&tools, &cut, 100, None, false).unwrap();
    assert_eq!(frame.dimensions(), (cut.width, cut.height));
    // No black rows left at the top or bottom.
    let row_mean = |y: u32| {
        (0..frame.width())
            .map(|x| {
                frame
                    .get_pixel(x, y)
                    .0
                    .iter()
                    .map(|v| *v as u32)
                    .sum::<u32>()
            })
            .sum::<u32>()
            / frame.width()
    };
    assert!(row_mean(1) > 60 && row_mean(frame.height() - 2) > 60);

    // The export has no bars either: 2x the cut area, still anamorphic like the DVD.
    let mut settings = Settings {
        provider: ProviderKind::Basic,
        ..Settings::default()
    };
    settings.work_dir = dir.path().join("work").display().to_string();
    let spec = ExportSpec {
        info: cut.clone(),
        start: 50,
        end: 56,
        output: dir.path().join("cut.mp4"),
        settings,
    };
    assert_eq!(
        run(&tools, &spec).last().map(String::as_str),
        Some("finished")
    );
    let out = ffmpeg::probe(&tools, &spec.output).unwrap();
    assert_eq!(
        (out.coded_width, out.coded_height, out.sar),
        (1440, crop.h * 2, (16, 15))
    );

    // A full-frame video has no bars.
    let full = dir.path().join("full.mp4");
    make_video(&tools, &full);
    let info = ffmpeg::probe(&tools, &full).unwrap();
    assert_eq!(ffmpeg::detect_black_bars(&tools, &info), None);
}

/// A 2.35:1 film letterboxed in a 4:3 PAL frame but flagged 16:9: the check notices that the
/// picture would be 3.14:1, suggests 4:3, and with that the export is 2.35:1 and flagged right.
#[test]
fn wrongly_flagged_dvd_is_detected_and_fixed() {
    let Some(tools) = Tools::locate("") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("wrong.mpg");
    let status = std::process::Command::new(&tools.ffmpeg)
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=768x326:rate=25:duration=12",
        ])
        .args([
            "-vf",
            "scale=720:326,pad=720:576:0:125:black",
            "-target",
            "pal-dvd",
            "-aspect",
            "16:9",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    let mut info = ffmpeg::probe(&tools, &src).unwrap();
    info.detected_crop = ffmpeg::detect_black_bars(&tools, &info);
    let check = upscaler_core::aspect::check(&info);
    assert!(check.standard.is_none(), "{check:?}");
    assert_eq!(check.suggestions.len(), 1);
    let s = &check.suggestions[0];
    assert_eq!((s.frame, s.picture_name), ((4, 3), "2.35:1"));

    let fixed = info
        .with_frame_aspect(Some(s.frame))
        .with_crop(info.detected_crop);
    assert_eq!(fixed.sar, (16, 15));
    assert!((fixed.width as f64 / fixed.height as f64 - 2.35).abs() < 0.03);
    let mut settings = Settings {
        provider: ProviderKind::Basic,
        ..Settings::default()
    };
    settings.work_dir = dir.path().join("work").display().to_string();
    let spec = ExportSpec {
        info: fixed,
        start: 20,
        end: 24,
        output: dir.path().join("fixed.mp4"),
        settings,
    };
    assert_eq!(
        run(&tools, &spec).last().map(String::as_str),
        Some("finished")
    );
    let out = ffmpeg::probe(&tools, &spec.output).unwrap();
    assert_eq!(out.sar, (16, 15));
    assert!(
        (out.width as f64 / out.height as f64 - 2.35).abs() < 0.03,
        "{}x{}",
        out.width,
        out.height
    );
}

// ---- HDR output -------------------------------------------------------------------------

use upscaler_core::settings::HdrMode;

fn hdr_settings(work: &Path, mode: HdrMode) -> Settings {
    let mut s = Settings {
        provider: ProviderKind::Basic,
        output_size: OutputSize::Scale2,
        hdr_mode: mode,
        hdr_peak_nits: 1000,
        hdr_delete_frames_after: false,
        ..Settings::default()
    };
    s.work_dir = work.display().to_string();
    s
}

fn hdr_spec(tools: &Tools, dir: &Path, mode: HdrMode, out: &str) -> ExportSpec {
    let src = dir.join("in.mp4");
    if !src.is_file() {
        make_video(tools, &src);
    }
    let info = ffmpeg::probe(tools, &src).unwrap();
    ExportSpec {
        info,
        start: 2,
        end: 14,
        output: dir.join(out),
        settings: hdr_settings(&dir.join("work"), mode),
    }
}

/// ffprobe JSON of the first video frame (with side data) and the stream.
fn first_frame_json(tools: &Tools, path: &Path) -> serde_json::Value {
    let out = std::process::Command::new(&tools.ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-read_intervals",
            "%+#1",
        ])
        .args(["-show_frames", "-show_streams", "-of", "json"])
        .arg(path)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

fn ratio(v: &serde_json::Value) -> f64 {
    let s = v.as_str().unwrap();
    match s.split_once('/') {
        Some((n, d)) => n.parse::<f64>().unwrap() / d.parse::<f64>().unwrap(),
        None => s.parse().unwrap(),
    }
}

#[test]
fn hdr10_export_is_pq_bt2020_10bit() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let spec = hdr_spec(&tools, dir.path(), HdrMode::Hdr10, "hdr.mp4");
    let log = run(&tools, &spec);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");

    let j = first_frame_json(&tools, &spec.output);
    let s = &j["streams"][0];
    assert_eq!(s["codec_name"], "hevc");
    assert_eq!(s["pix_fmt"], "yuv420p10le");
    assert_eq!(s["color_transfer"], "smpte2084");
    assert_eq!(s["color_primaries"], "bt2020");
    assert_eq!(s["color_space"], "bt2020nc");
    let side = j["frames"][0]["side_data_list"].as_array().unwrap();
    let find = |name: &str| {
        side.iter()
            .find(|d| d["side_data_type"].as_str().unwrap().starts_with(name))
            .unwrap_or_else(|| panic!("no {name} in {side:?}"))
    };
    let mastering = find("Mastering display");
    assert!((ratio(&mastering["max_luminance"]) - 1000.0).abs() < 0.5);
    let cll = find("Content light level");
    let max_cll = cll["max_content"].as_u64().unwrap();
    let max_fall = cll["max_average"].as_u64().unwrap();
    assert!(max_cll > 0 && max_fall > 0 && max_fall <= max_cll, "{cll}");
    // The values are measured from the HDR frames, not fixed ones.
    let m = upscaler_core::hdr::measure::measure_dir(
        &upscaler_core::hdr::convert::hdr_dir(&spec.job_dir()),
        spec.job_frames(),
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    assert_eq!((max_cll, max_fall), (m.max_cll as u64, m.max_fall as u64));
}

#[test]
fn hlg_export_has_hlg_transfer() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let spec = hdr_spec(&tools, dir.path(), HdrMode::Hlg, "hlg.mp4");
    let log = run(&tools, &spec);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");
    let j = first_frame_json(&tools, &spec.output);
    let s = &j["streams"][0];
    assert_eq!(s["codec_name"], "hevc");
    assert_eq!(s["pix_fmt"], "yuv420p10le");
    assert_eq!(s["color_transfer"], "arib-std-b67");
    assert_eq!(s["color_primaries"], "bt2020");
    let side = j["frames"][0]["side_data_list"].as_array();
    assert!(
        side.is_none_or(|l| l.iter().all(|d| {
            let t = d["side_data_type"].as_str().unwrap();
            !t.starts_with("Mastering") && !t.starts_with("Content light")
        })),
        "{side:?}"
    );
}

/// Distinct luma codes (10-bit) along the middle row of frame `n` of `path`, and the share of
/// samples on the 8-bit grid (code divisible by 4: 25 % by chance, ~100 % after an 8-bit stage).
fn luma_levels(tools: &Tools, path: &Path, n: u32) -> (usize, f64) {
    let info = ffmpeg::probe(tools, path).unwrap();
    let (w, h) = (info.width as usize, info.height as usize);
    let raw = std::process::Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-vf", &format!(r"select=eq(n\,{n})"), "-frames:v", "1"])
        .args(["-pix_fmt", "yuv444p10le", "-f", "rawvideo", "-"])
        .output()
        .unwrap()
        .stdout;
    assert!(raw.len() >= w * h * 2, "no frame {n}");
    let row = (h / 2) * w;
    let mut seen = std::collections::BTreeSet::new();
    let mut on_grid = 0;
    for x in 0..w {
        let i = (row + x) * 2;
        let y = u16::from_le_bytes([raw[i], raw[i + 1]]);
        seen.insert(y);
        on_grid += (y % 4 == 0) as usize;
    }
    (seen.len(), on_grid as f64 / w as f64)
}

#[test]
fn hdr_with_interpolation_and_anamorphic() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("dvdlike.mkv");
    // A smooth horizontal grey ramp (220 legal levels, lossless): in PQ the steps between
    // neighbouring SDR levels are 1-4 ten-bit codes, so an 8-bit PQ stage would merge many.
    let status = std::process::Command::new(&tools.ffmpeg)
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(
            "nullsrc=size=1024x64:rate=30000/1001:duration=3,             geq=lum='16+219*X/W':cb=128:cr=128,setsar=32/27",
        )
        .args(["-c:v", "libx264", "-qp", "0", "-pix_fmt", "yuv420p"])
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    let info = ffmpeg::probe(&tools, &src).unwrap();
    assert_eq!((info.fps_num, info.fps_den), (30000, 1001));
    let mut settings = hdr_settings(&dir.path().join("work"), HdrMode::Hdr10);
    settings.ai_rate = AiRate::Half;
    settings.crf = 6;
    let spec = ExportSpec {
        info,
        start: 4,
        end: 28,
        output: dir.path().join("dvd.mkv"),
        settings,
    };
    let log = run(&tools, &spec);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");
    let v = stream_entries(
        &tools,
        &spec.output,
        "v:0",
        "stream=r_frame_rate,sample_aspect_ratio,color_transfer,color_primaries,pix_fmt",
    );
    assert_eq!(
        entry(&v, "r_frame_rate").as_deref(),
        Some("30000/1001"),
        "{v}"
    );
    assert_eq!(
        entry(&v, "sample_aspect_ratio").as_deref(),
        Some("32:27"),
        "{v}"
    );
    assert_eq!(entry(&v, "color_transfer").as_deref(), Some("smpte2084"));
    assert_eq!(entry(&v, "color_primaries").as_deref(), Some("bt2020"));
    assert_eq!(entry(&v, "pix_fmt").as_deref(), Some("yuv420p10le"));
    // An interpolated (odd) frame still has the fine PQ steps: an 8-bit PQ stage would put
    // (nearly) every sample on the 8-bit grid (measured 67 % after encoding, vs 26 % here).
    let (levels, grid) = luma_levels(&tools, &spec.output, 5);
    assert!(levels > 170, "only {levels} luma levels");
    assert!(
        grid < 0.4,
        "{:.0} % of the samples are on the 8-bit grid",
        grid * 100.0
    );
}

#[test]
fn hdr_frames_deleted_after_export_when_set() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut spec = hdr_spec(&tools, dir.path(), HdrMode::Hdr10, "del.mp4");
    spec.settings.hdr_delete_frames_after = true;
    // Every 2nd frame: the interpolated SDR frames go too.
    spec.settings.ai_rate = AiRate::Half;
    let log = run(&tools, &spec);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");
    assert!(!upscaler_core::hdr::convert::hdr_dir(&spec.job_dir()).exists());
    assert!(!spec.job_dir().join("interp").exists());
    assert!(spec.job_dir().join("out").is_dir(), "out/ is never deleted");

    let mut keep = hdr_spec(&tools, dir.path(), HdrMode::Hdr10, "keep.mp4");
    keep.settings.hdr_delete_frames_after = false;
    let log = run(&tools, &keep);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");
    assert!(upscaler_core::hdr::convert::hdr_dir(&keep.job_dir()).is_dir());
    assert!(
        !keep.job_dir().join("interp").exists(),
        "no interp without skipped frames"
    );
}

/// The encoder must apply nothing but RGB to YUV with the BT.2020 matrix, full to limited
/// range: grey 16-bit PQ (100 nits) comes back as limited-range grey with the same code
/// value, and a colour follows the BT.2020 non-constant-luminance matrix.
#[test]
fn hdr_encode_keeps_pq_code_values() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let frames = dir.path().join("hdr");
    std::fs::create_dir_all(&frames).unwrap();
    let v = upscaler_core::hdr::measure::nits_to_pq(100.0);
    let code = (v * 65535.0).round() as u16;
    let img = image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::from_fn(64, 64, |x, _| {
        if x < 32 {
            image::Rgb([code, code, code])
        } else {
            image::Rgb([code, code / 4, code / 4])
        }
    });
    for n in 1..=3 {
        img.save(ffmpeg::frame_file(&frames, n)).unwrap();
    }
    // Plain, and squeezed to an anamorphic stored size (a later lanczos scale must not touch
    // the matrix or the range).
    for (anamorphic, width, grey_x, colour_x) in [
        (None, 64usize, 8usize, 48usize),
        (Some(((48u32, 64u32), (4u32, 3u32))), 48, 6, 36),
    ] {
        let opts = ffmpeg::EncodeOptions {
            in_fps_num: 24,
            in_fps_den: 1,
            interpolate_to: None,
            anamorphic,
            codec: upscaler_core::settings::VideoCodec::H265,
            crf: 4,
            audio_from: None,
            hdr: Some(upscaler_core::hdr::HdrEncode {
                mode: HdrMode::Hdr10,
                peak_nits: 1000,
                max_cll: 100,
                max_fall: 100,
                hdr10plus_json: None,
            }),
        };
        let out = dir.path().join(format!("grey{width}.mkv"));
        ffmpeg::encode(
            &tools,
            &frames,
            3,
            &opts,
            &out,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let raw = std::process::Command::new(&tools.ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args([
                "-frames:v",
                "1",
                "-pix_fmt",
                "yuv444p10le",
                "-f",
                "rawvideo",
                "-",
            ])
            .output()
            .unwrap()
            .stdout;
        let plane = width * 64;
        assert_eq!(raw.len(), plane * 3 * 2, "width {width}");
        let at = |p: usize, x: usize, y: usize| {
            let i = (p * plane + y * width + x) * 2;
            u16::from_le_bytes([raw[i], raw[i + 1]]) as f64
        };
        let expect_y = 64.0 + 876.0 * v as f64;
        let (y, cb, cr) = (at(0, grey_x, 32), at(1, grey_x, 32), at(2, grey_x, 32));
        assert!(
            (y - expect_y).abs() <= 4.0,
            "width {width}: Y {y} expected {expect_y}"
        );
        assert!(
            (cb - 512.0).abs() <= 4.0 && (cr - 512.0).abs() <= 4.0,
            "width {width}: {cb} {cr}"
        );
        let (r, g) = (code as f64 / 65535.0, code as f64 / 4.0 / 65535.0);
        let b = g;
        let ey = 0.2627 * r + 0.6780 * g + 0.0593 * b;
        let (wy, wcb, wcr) = (
            64.0 + 876.0 * ey,
            512.0 + 896.0 * (b - ey) / 1.8814,
            512.0 + 896.0 * (r - ey) / 1.4746,
        );
        let (y, cb, cr) = (
            at(0, colour_x, 32),
            at(1, colour_x, 32),
            at(2, colour_x, 32),
        );
        assert!((y - wy).abs() <= 6.0, "width {width}: Y {y} vs {wy}");
        assert!(
            (cb - wcb).abs() <= 6.0 && (cr - wcr).abs() <= 6.0,
            "width {width}: chroma {cb} {cr} vs {wcb} {wcr}"
        );
    }
}

#[test]
fn hdr10plus_export_carries_dynamic_metadata() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    // The temp dir (and so the job folder with hdr10plus.json) has a drive letter and
    // backslashes on Windows: the path must survive ffmpeg's -x265-params parsing.
    let dir = tempfile::tempdir().unwrap();
    let spec = hdr_spec(&tools, dir.path(), HdrMode::Hdr10Plus, "hdr10plus.mp4");
    let log = run(&tools, &spec);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");
    let json = spec.job_dir().join("hdr10plus.json");
    assert!(json.is_file(), "{} missing", json.display());
    eprintln!("json path: {}", json.display());

    let j = first_frame_json(&tools, &spec.output);
    let side = j["frames"][0]["side_data_list"].as_array().unwrap();
    let types: Vec<&str> = side
        .iter()
        .map(|d| d["side_data_type"].as_str().unwrap())
        .collect();
    assert!(
        types.iter().any(|t| t.contains("SMPTE2094-40")),
        "no HDR10+ side data: {types:?}"
    );
    assert!(types.iter().any(|t| t.starts_with("Mastering display")));
}

/// Round trip through `hdr10plus_tool extract`: frame count and MaxScl of frame 0 match the
/// measurement. Needs the tool on PATH or in UPSCALER_HDR10PLUS_TOOL; skipped otherwise.
#[test]
fn hdr10plus_round_trip() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let tool = std::env::var("UPSCALER_HDR10PLUS_TOOL")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "hdr10plus_tool".into());
    if std::process::Command::new(&tool)
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("hdr10plus_tool not found (PATH or UPSCALER_HDR10PLUS_TOOL), skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let spec = hdr_spec(&tools, dir.path(), HdrMode::Hdr10Plus, "hdr10plus.mp4");
    let log = run(&tools, &spec);
    assert_eq!(log.last().map(String::as_str), Some("finished"), "{log:?}");
    let hevc = dir.path().join("raw.hevc");
    let st = std::process::Command::new(&tools.ffmpeg)
        .args(["-y", "-v", "error", "-i"])
        .arg(&spec.output)
        .args(["-c", "copy", "-bsf:v", "hevc_mp4toannexb", "-f", "hevc"])
        .arg(&hevc)
        .status()
        .unwrap();
    assert!(st.success());
    let extracted = dir.path().join("extracted.json");
    let st = std::process::Command::new(&tool)
        .arg("extract")
        .arg(&hevc)
        .arg("-o")
        .arg(&extracted)
        .status()
        .unwrap();
    assert!(st.success());
    let j: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&extracted).unwrap()).unwrap();
    let m = upscaler_core::hdr::measure::measure_dir(
        &upscaler_core::hdr::convert::hdr_dir(&spec.job_dir()),
        spec.job_frames(),
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    let frames = j["SceneInfo"].as_array().unwrap();
    assert_eq!(frames.len(), m.frames.len());
    let got: Vec<f64> = frames[0]["LuminanceParameters"]["MaxScl"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    for (g, want) in got.iter().zip(m.frames[0].max_scl) {
        assert!(
            (g - want as f64 * 10.0).abs() <= 2.0,
            "{got:?} vs {:?}",
            m.frames[0]
        );
    }
}
