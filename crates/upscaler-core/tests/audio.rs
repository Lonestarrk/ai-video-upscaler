//! Source audio analysis with the real ffmpeg (skipped when it is not installed).

use std::path::Path;

use upscaler_core::audio::source::analyse;
use upscaler_core::ffmpeg::{self, Tools};

fn make(tools: &Tools, path: &Path, audio_args: &[&str]) {
    let status = std::process::Command::new(&tools.ffmpeg)
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=160x90:rate=24:duration=2")
        .args(["-f", "lavfi", "-i"])
        .arg(audio_args[0])
        .args(&audio_args[1..])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-shortest"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn detects_flag_and_layouts() {
    let Some(tools) = Tools::locate("") else {
        eprintln!("ffmpeg not installed, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let probe = |name: &str, args: &[&str]| {
        let p = dir.path().join(name);
        make(&tools, &p, args);
        let info = ffmpeg::probe(&tools, &p).unwrap();
        analyse(&tools, &info).unwrap().expect("has audio")
    };

    let stereo = probe(
        "stereo.mkv",
        &[
            "sine=frequency=440:duration=2,aformat=channel_layouts=stereo",
            "-c:a",
            "ac3",
            "-dsur_mode",
            "on",
        ],
    );
    assert_eq!((stereo.channels, stereo.codec.as_str()), (2, "ac3"));
    assert!(stereo.dolby_surround);
    assert!(stereo.dialnorm.is_some());
    assert!(stereo.can_upmix() && !stereo.is_surround());

    let plain = probe(
        "plain.mkv",
        &[
            "sine=frequency=440:duration=2,aformat=channel_layouts=stereo",
            "-c:a",
            "ac3",
        ],
    );
    assert!(!plain.dolby_surround);

    let mono = probe(
        "mono.mp4",
        &[
            "sine=frequency=440:duration=2,aformat=channel_layouts=mono",
            "-c:a",
            "aac",
        ],
    );
    assert_eq!((mono.channels, mono.codec.as_str()), (1, "aac"));
    assert!(!mono.can_upmix() && !mono.is_surround() && !mono.dolby_surround);

    let surround = probe(
        "surround.mkv",
        &[
            "sine=frequency=440:duration=2,aformat=channel_layouts=5.1",
            "-c:a",
            "ac3",
        ],
    );
    assert_eq!(surround.channels, 6);
    assert!(!surround.can_upmix() && surround.is_surround());

    // No audio at all.
    let silent = dir.path().join("silent.mp4");
    let ok = std::process::Command::new(&tools.ffmpeg)
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=160x90:rate=24:duration=1")
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(&silent)
        .status()
        .unwrap();
    assert!(ok.success());
    let info = ffmpeg::probe(&tools, &silent).unwrap();
    assert_eq!(analyse(&tools, &info).unwrap(), None);
}
