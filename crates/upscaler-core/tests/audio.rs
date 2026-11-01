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

mod io_tests {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    use upscaler_core::audio::io::{encode_stream, extract, read_wav, write_wav, AudioCodec, Pcm};
    use upscaler_core::ffmpeg::Tools;

    fn tools() -> Option<Tools> {
        let t = Tools::locate("");
        if t.is_none() {
            eprintln!("ffmpeg not installed, skipping");
        }
        t
    }

    fn goertzel(x: &[f32], rate: f32, freq: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * freq / rate;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &v in x {
            let s = v + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        s1 * s1 + s2 * s2 - coeff * s1 * s2
    }

    fn probe_field(tools: &Tools, path: &Path, entries: &str) -> String {
        let out = std::process::Command::new(&tools.ffprobe)
            .args(["-v", "error", "-select_streams", "a:0", "-show_entries"])
            .arg(entries)
            .args(["-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn extract_is_sample_exact() {
        let Some(tools) = tools() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("sine.mp3");
        let ok = std::process::Command::new(&tools.ffmpeg)
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("sine=frequency=440:sample_rate=44100:duration=3")
            .arg(&src)
            .status()
            .unwrap();
        assert!(ok.success());
        let out = dir.path().join("out.wav");
        let cancel = AtomicBool::new(false);
        extract(&tools, &src, 0.5, 1.2345, &out, &cancel).unwrap();
        let pcm = read_wav(&out).unwrap();
        assert_eq!(pcm.rate, 48000);
        assert_eq!(pcm.channels, 1);
        assert_eq!(pcm.frames(), 59256);
        // Past the end of the source: padded with silence to the exact length.
        extract(&tools, &src, 2.5, 1.0, &out, &cancel).unwrap();
        let pcm = read_wav(&out).unwrap();
        assert_eq!(pcm.frames(), 48000);
        assert!(pcm.channel(0)[47000..].iter().all(|v| v.abs() < 1e-4));
    }

    #[test]
    fn wav_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let a: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        let b: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.02).cos() * 0.25).collect();
        let pcm = Pcm::from_channels(48000, &[a.clone(), b.clone()]);
        assert_eq!((pcm.channels, pcm.frames()), (2, 1000));
        let path = dir.path().join("x.wav");
        write_wav(&path, &pcm).unwrap();
        assert!(!dir.path().join("x.wav.part").exists());
        let back = read_wav(&path).unwrap();
        assert_eq!(back.rate, 48000);
        assert_eq!(back.channel(0), a);
        assert_eq!(back.channel(1), b);
    }

    #[test]
    fn flac_51_keeps_layout() {
        let Some(tools) = tools() else { return };
        let dir = tempfile::tempdir().unwrap();
        let freqs = [300.0f32, 500.0, 700.0, 900.0, 1100.0, 1300.0];
        let n = 48000usize;
        let chans: Vec<Vec<f32>> = freqs
            .iter()
            .map(|f| {
                (0..n)
                    .map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / 48000.0).sin() * 0.3)
                    .collect()
            })
            .collect();
        let pcm = Pcm::from_channels(48000, &chans);
        let cancel = AtomicBool::new(false);
        for (codec, name) in [
            (AudioCodec::Flac24, "a.flac"),
            (AudioCodec::Ac3At640 { dialnorm: -27 }, "a.ac3"),
        ] {
            let out = dir.path().join(name);
            let mut enc = encode_stream(&tools, 6, "5.1(side)", codec, &out, &cancel).unwrap();
            for chunk in pcm.samples.chunks(6 * 4800) {
                enc.write(chunk).unwrap();
            }
            enc.finish().unwrap();
            assert_eq!(
                probe_field(&tools, &out, "stream=channels,channel_layout"),
                "6,5.1(side)",
                "{name}"
            );
            let wav = dir.path().join("dec.wav");
            extract(&tools, &out, 0.0, 1.0, &wav, &cancel).unwrap();
            let dec = read_wav(&wav).unwrap();
            assert_eq!(dec.channels, 6);
            for (c, f) in freqs.iter().enumerate() {
                let x = dec.channel(c);
                let best = freqs
                    .iter()
                    .copied()
                    .max_by(|a, b| {
                        goertzel(&x[4800..40000], 48000.0, *a).total_cmp(&goertzel(
                            &x[4800..40000],
                            48000.0,
                            *b,
                        ))
                    })
                    .unwrap();
                assert_eq!(best, *f, "{name} channel {c}");
            }
        }
    }
}

mod classic_tests {
    use std::sync::atomic::AtomicBool;

    use upscaler_core::audio::classic::upmix_classic;
    use upscaler_core::audio::io::{write_wav, Pcm};
    use upscaler_core::ffmpeg::Tools;
    use upscaler_core::settings::LfeLevel;

    fn tools() -> Option<Tools> {
        let t = Tools::locate("");
        if t.is_none() {
            eprintln!("ffmpeg not installed, skipping");
        }
        t
    }

    fn stereo(n: usize) -> Pcm {
        let mut s = 777u32;
        let mut noise = move || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        let tau = std::f32::consts::TAU;
        let (mut l, mut r) = (Vec::new(), Vec::new());
        for i in 0..n {
            let t = i as f32 / 48000.0;
            let v = 0.2 * (tau * 300.0 * t).sin() * (0.6 + 0.4 * (tau * 4.0 * t).sin());
            l.push(v + 0.08 * noise());
            r.push(0.7 * v + 0.08 * noise());
        }
        Pcm::from_channels(48000, &[l, r])
    }

    #[test]
    fn classic_length_matches() {
        let Some(tools) = tools() else { return };
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        for n in [48000usize, 100_003, 1500] {
            let wav = dir.path().join(format!("in{n}.wav"));
            write_wav(&wav, &stereo(n)).unwrap();
            let out = upmix_classic(&tools, &wav, LfeLevel::Normal, &cancel).unwrap();
            assert_eq!((out.channels, out.frames()), (6, n), "n={n}");
            assert!(out.samples.iter().all(|v| v.is_finite()));
            let leftovers: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|f| !f.starts_with("in"))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }
    }

    #[test]
    fn classic_is_in_sync() {
        let Some(tools) = tools() else { return };
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let src = stereo(5 * 48000);
        let wav = dir.path().join("in.wav");
        write_wav(&wav, &src).unwrap();
        let out = upmix_classic(&tools, &wav, LfeLevel::Normal, &cancel).unwrap();
        let (l, r) = (src.channel(0), src.channel(1));
        let (fl, fr, fc) = (out.channel(0), out.channel(1), out.channel(2));
        let a: Vec<f32> = (0..l.len()).map(|i| l[i] + r[i]).collect();
        let b: Vec<f32> = (0..l.len()).map(|i| fl[i] + fr[i] + fc[i]).collect();
        let (lo, hi) = (48000usize, 48000 * 3);
        let mut best = (0i32, f64::MIN);
        for lag in -100i32..=100 {
            let c: f64 = (lo..hi)
                .map(|i| a[i] as f64 * b[(i as i32 + lag) as usize] as f64)
                .sum();
            if c > best.1 {
                best = (lag, c);
            }
        }
        assert!(best.0.abs() <= 1, "peak at lag {}", best.0);
    }
}
