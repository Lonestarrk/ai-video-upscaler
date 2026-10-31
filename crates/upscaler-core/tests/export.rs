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
