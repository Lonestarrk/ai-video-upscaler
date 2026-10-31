//! End to end with the real ffmpeg (skipped when it is not installed): a short test video with
//! audio is exported with the basic upscaler and with a stand-in for the Codex CLI.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use upscaler_core::ffmpeg::{self, Tools};
use upscaler_core::job::{self, ExportEvent, ExportSpec};
use upscaler_core::settings::{AiRate, OutputSize, ProviderKind, Settings};

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

fn run(tools: &Tools, spec: &ExportSpec) -> Vec<String> {
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
    let first = image::open(spec.job_dir().join("src").join("000001.png"))
        .unwrap()
        .to_rgb8();
    assert!(mean_diff(&first, &frame_by_counting(&tools, &info, 30)) < 1.0);
    // Restored with square pixels, exported anamorphic again: 2x the stored 720x480, same
    // pixel aspect ratio, so it still displays as 16:9.
    let result = ffmpeg::probe(&tools, &out).unwrap();
    assert_eq!((result.coded_width, result.coded_height), (1440, 960));
    assert_eq!(result.sar, (32, 27));
    assert!(result.has_audio);
    let up = image::open(spec.job_dir().join("out").join("000001.png")).unwrap();
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

/// Checks frame accuracy on real files: `UPSCALER_TEST_VIDEOS=a.mp4:b.vob cargo test -- --ignored`.
#[test]
#[ignore]
fn frame_accuracy_on_given_videos() {
    let tools = Tools::locate("").expect("ffmpeg");
    let list = std::env::var("UPSCALER_TEST_VIDEOS").expect("UPSCALER_TEST_VIDEOS");
    for path in list.split(':').filter(|p| !p.is_empty()) {
        let info = ffmpeg::probe(&tools, Path::new(path)).unwrap();
        eprintln!(
            "{path}: {}x{} shown as {}x{}, {} frames, interlaced {}",
            info.coded_width,
            info.coded_height,
            info.width,
            info.height,
            info.frame_count,
            info.interlaced
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
