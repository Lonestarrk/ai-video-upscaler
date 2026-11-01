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

mod job_tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    use upscaler_core::audio::io::{encode_stream_filtered, read_wav, AudioCodec, Pcm, RATE};
    use upscaler_core::audio::job::{
        job_dir, run, run_with, AudioEvent, AudioSpec, AudioStage, AudioTrack, JobOptions,
    };
    use upscaler_core::ffmpeg::{self, Tools};
    use upscaler_core::settings::{AudioMode, Settings, SurroundEngine};

    fn tools() -> Option<Tools> {
        let t = Tools::locate("");
        if t.is_none() {
            eprintln!("ffmpeg not installed, skipping");
        }
        t
    }

    /// A 24 fps video of `secs` seconds with the given lavfi audio, AAC in MKV.
    fn video(tools: &Tools, path: &Path, secs: u32, audio: &str) {
        let ok = std::process::Command::new(&tools.ffmpeg)
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size=160x90:rate=24:duration={secs}"))
            .args(["-f", "lavfi", "-i", audio])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .args(["-b:a", "192k", "-t"])
            .arg(secs.to_string())
            .arg(path)
            .status()
            .unwrap();
        assert!(ok.success());
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

    /// Decodes the whole file (no padding or trimming) to 48 kHz float.
    fn decode(tools: &Tools, path: &Path, out: &Path) -> Pcm {
        let ok = std::process::Command::new(&tools.ffmpeg)
            .args(["-y", "-loglevel", "error", "-i"])
            .arg(path)
            .args(["-ar", "48000", "-c:a", "pcm_f32le", "-f", "wav"])
            .arg(out)
            .status()
            .unwrap();
        assert!(ok.success());
        read_wav(out).unwrap()
    }

    fn surround_settings(work: &Path, engine: SurroundEngine) -> Settings {
        Settings {
            audio_mode: AudioMode::Surround,
            surround_engine: engine,
            project_dir: Some(work.to_path_buf()),
            ..Settings::default()
        }
    }

    const STEREO: &str = "aevalsrc=0.25*sin(2*PI*300*t)*(0.6+0.4*sin(2*PI*4*t))+0.05*random(0)\
        |0.18*sin(2*PI*300*t)+0.1*sin(2*PI*1500*t)+0.05*random(1):s=48000:d=20";

    /// The export's 5.1 tracks (the third is the untouched original, muxed by the export).
    #[test]
    fn classic_job_writes_three_tracks() {
        let Some(tools) = tools() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("stereo.mkv");
        video(&tools, &src, 20, STEREO);
        let info = ffmpeg::probe(&tools, &src).unwrap();
        // Frames 24..456 at 24 fps: 1 s to 19 s, 18 s = 864 000 samples.
        let spec = AudioSpec {
            info,
            start: 24,
            end: 456,
            settings: surround_settings(dir.path(), SurroundEngine::Classic),
        };
        let n = 864_000usize;
        let events = Mutex::new(Vec::new());
        let emit = |e: AudioEvent| events.lock().unwrap().push(e);
        // 8 s sections with 2 s overlap: three sections, two crossfades.
        let opts = JobOptions {
            section_frames: 8 * RATE as u64,
            overlap_frames: 2 * RATE as u64,
        };
        let cancel = AtomicBool::new(false);
        let result = run_with(&tools, &spec, None, &cancel, &emit, &opts).unwrap();
        eprintln!("warnings: {:?}", result.warnings);
        let events = events.into_inner().unwrap();
        assert!(events.contains(&AudioEvent::Section { done: 3, total: 3 }));
        for stage in [
            AudioStage::Extracting,
            AudioStage::Placing,
            AudioStage::Joining,
            AudioStage::Checking,
        ] {
            assert!(events.contains(&AudioEvent::Stage(stage)), "{stage:?}");
        }
        assert!(
            !result.warnings.iter().any(|w| w.contains("sync")),
            "{:?}",
            result.warnings
        );

        assert_eq!(result.tracks.len(), 2, "{:?}", result.tracks);
        let flac = match &result.tracks[0] {
            AudioTrack::Flac51(p) => p.clone(),
            t => panic!("expected FLAC 5.1 first, got {t:?}"),
        };
        let ac3 = match &result.tracks[1] {
            AudioTrack::Ac351(p) => p.clone(),
            t => panic!("expected AC-3 5.1 second, got {t:?}"),
        };
        for (p, codec) in [(&flac, "flac"), (&ac3, "ac3")] {
            assert!(p.is_file(), "{}", p.display());
            assert_eq!(
                probe_field(&tools, p, "stream=codec_name,channels,channel_layout"),
                format!("{codec},6,5.1(side)")
            );
        }
        let dec = decode(&tools, &flac, &dir.path().join("flac.wav"));
        assert_eq!((dec.channels, dec.frames()), (6, n), "FLAC length");
        assert!(dec.samples.iter().all(|v| v.is_finite()));
        // AC-3 works in 1536-sample frames: the length is right to within one frame.
        let dec = decode(&tools, &ac3, &dir.path().join("ac3.wav"));
        assert_eq!(dec.channels, 6);
        let diff = dec.frames() as i64 - n as i64;
        assert!(
            (0..=1536 + 256).contains(&diff),
            "AC-3 length off by {diff}"
        );

        // The big intermediates are gone; the finished result is cached.
        let job = job_dir(&spec);
        assert!(!job.join("source.f32").exists());
        assert!(!job.join("sections").exists());
        let again = run(&tools, &spec, None, &cancel, &|_| {}).unwrap();
        assert_eq!(again, result);
    }

    #[test]
    fn mono_surround_rejected_before_work() {
        let Some(tools) = tools() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("mono.mkv");
        video(
            &tools,
            &src,
            2,
            "sine=frequency=440:duration=2,aformat=channel_layouts=mono",
        );
        let spec = AudioSpec {
            info: ffmpeg::probe(&tools, &src).unwrap(),
            start: 0,
            end: 48,
            settings: surround_settings(dir.path(), SurroundEngine::Classic),
        };
        let err = run(&tools, &spec, None, &AtomicBool::new(false), &|_| {}).unwrap_err();
        assert!(
            err.to_string()
                .contains("mono source cannot be turned into 5.1"),
            "{err}"
        );
        assert!(!job_dir(&spec).exists());

        // Local AI without the pack: also refused before anything is written.
        let stereo = dir.path().join("stereo.mkv");
        video(&tools, &stereo, 2, STEREO);
        let spec = AudioSpec {
            info: ffmpeg::probe(&tools, &stereo).unwrap(),
            start: 0,
            end: 48,
            settings: surround_settings(dir.path(), SurroundEngine::LocalAi),
        };
        let err = run(&tools, &spec, None, &AtomicBool::new(false), &|_| {}).unwrap_err();
        assert!(err.to_string().contains("not installed"), "{err}");
        assert!(!job_dir(&spec).exists());
    }

    /// Fake separation: everything is "music" at 4x, so the surrounds (from the music's
    /// side signal) come out far louder than the source and the loudness check must step in.
    struct LoudAi;

    impl upscaler_core::audio::ai::AudioAi for LoudAi {
        fn separate(
            &self,
            wav: &Path,
            out_dir: &Path,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(f32),
        ) -> anyhow::Result<upscaler_core::audio::ai::Stems> {
            use upscaler_core::audio::io::write_wav;
            let x = read_wav(wav)?;
            std::fs::create_dir_all(out_dir)?;
            let scaled = |g: f32| Pcm {
                rate: x.rate,
                channels: x.channels,
                samples: x.samples.iter().map(|v| v * g).collect(),
            };
            let stems = upscaler_core::audio::ai::Stems {
                speech: out_dir.join("speech.wav"),
                music: out_dir.join("music.wav"),
                effects: out_dir.join("effects.wav"),
            };
            write_wav(&stems.speech, &scaled(0.0))?;
            write_wav(&stems.music, &scaled(4.0))?;
            write_wav(&stems.effects, &scaled(0.0))?;
            Ok(stems)
        }

        fn enhance(
            &self,
            _wav: &Path,
            _out: &Path,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(f32),
        ) -> anyhow::Result<()> {
            anyhow::bail!("not used")
        }
    }

    #[test]
    fn loudness_is_corrected_to_the_source() {
        let Some(tools) = tools() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("wide.mkv");
        // Mostly side signal: L and R in opposite phase.
        video(
            &tools,
            &src,
            6,
            "aevalsrc=0.1*sin(2*PI*500*t)+0.05*random(0)|-0.1*sin(2*PI*500*t)+0.05*random(1):s=48000:d=6",
        );
        let spec = AudioSpec {
            info: ffmpeg::probe(&tools, &src).unwrap(),
            start: 0,
            end: 144,
            settings: surround_settings(dir.path(), SurroundEngine::LocalAi),
        };
        let events = Mutex::new(Vec::new());
        let emit = |e: AudioEvent| events.lock().unwrap().push(e);
        let result = run(&tools, &spec, Some(&LoudAi), &AtomicBool::new(false), &emit).unwrap();
        eprintln!("warnings: {:?}", result.warnings);
        let encodes = events
            .into_inner()
            .unwrap()
            .iter()
            .filter(|e| **e == AudioEvent::Stage(AudioStage::Encoding))
            .count();
        assert!(encodes >= 2, "no corrective re-encode ({encodes})");
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.contains("loudness") || w.contains("dBTP")),
            "{:?}",
            result.warnings
        );
        let dec = decode(&tools, result.tracks[0].path(), &dir.path().join("out.wav"));
        assert_eq!((dec.channels, dec.frames()), (6, 6 * RATE as usize));
    }

    /// Pass-through AI: enhancement returns its input unchanged.
    struct PassAi;

    impl upscaler_core::audio::ai::AudioAi for PassAi {
        fn separate(
            &self,
            _wav: &Path,
            _out_dir: &Path,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(f32),
        ) -> anyhow::Result<upscaler_core::audio::ai::Stems> {
            anyhow::bail!("not used")
        }

        fn enhance(
            &self,
            wav: &Path,
            out: &Path,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(f32),
        ) -> anyhow::Result<()> {
            std::fs::copy(wav, out)?;
            Ok(())
        }
    }

    fn rms_db(x: &[f32]) -> f64 {
        let p = x.iter().map(|v| *v as f64 * *v as f64).sum::<f64>() / x.len() as f64;
        10.0 * p.log10()
    }

    /// Mono becomes L = R stereo: each channel keeps the mono level, so no loudness gain
    /// may be applied (BS.1770 counts a mono source as dual mono).
    #[test]
    fn mono_source_gets_no_gain() {
        let Some(tools) = tools() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("mono.mkv");
        video(
            &tools,
            &src,
            6,
            "aevalsrc=0.2*sin(2*PI*300*t)*(0.6+0.4*sin(2*PI*3*t))+0.05*random(0):s=48000:d=6,\
             aformat=channel_layouts=mono",
        );
        let spec = AudioSpec {
            info: ffmpeg::probe(&tools, &src).unwrap(),
            start: 0,
            end: 144,
            settings: Settings {
                audio_mode: AudioMode::Original,
                audio_enhance: true,
                project_dir: Some(dir.path().to_path_buf()),
                ..Settings::default()
            },
        };
        let result = run(
            &tools,
            &spec,
            Some(&PassAi),
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        eprintln!("warnings: {:?}", result.warnings);
        let flac = match &result.tracks[..] {
            [AudioTrack::FlacStereo(p)] => p.clone(),
            t => panic!("expected one FLAC stereo track, got {t:?}"),
        };
        let out = decode(&tools, &flac, &dir.path().join("out.wav"));
        assert_eq!((out.channels, out.frames()), (2, 6 * RATE as usize));
        let orig = decode(&tools, &src, &dir.path().join("src.wav"));
        let n = out.frames().min(orig.frames());
        let d = rms_db(&out.channel(0)[..n]) - rms_db(&orig.channel(0)[..n]);
        assert!(d.abs() <= 0.2, "mono level changed by {d:.2} dB");
        assert_eq!(out.channel(0), out.channel(1));
    }

    fn lag(a: &[f32], b: &[f32], max: i64) -> i64 {
        let (lo, hi) = (a.len() / 4, a.len() * 3 / 4);
        (-max..=max)
            .max_by(|x, y| {
                let c = |lag: i64| -> f64 {
                    (lo..hi)
                        .map(|i| a[i] as f64 * b[(i as i64 + lag) as usize] as f64)
                        .sum()
                };
                c(*x).total_cmp(&c(*y))
            })
            .unwrap()
    }

    /// The limiter used for true-peak control keeps the length exact and adds no delay.
    #[test]
    fn limiter_keeps_length_and_sync() {
        let Some(tools) = tools() else { return };
        let dir = tempfile::tempdir().unwrap();
        let n = 3 * RATE as usize + 77;
        let mut s = 99u32;
        let x: Vec<f32> = (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                ((s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * 1.4
            })
            .collect();
        let pcm = Pcm::from_channels(RATE, &[x.clone(), x.clone()]);
        let out: PathBuf = dir.path().join("lim.flac");
        let cancel = AtomicBool::new(false);
        let mut enc = encode_stream_filtered(
            &tools,
            2,
            "stereo",
            AudioCodec::Flac24,
            Some(upscaler_core::audio::job::LIMITER),
            &out,
            &cancel,
        )
        .unwrap();
        enc.write(&pcm.samples).unwrap();
        enc.finish().unwrap();
        let dec = decode(&tools, &out, &dir.path().join("lim.wav"));
        assert_eq!(dec.frames(), n);
        let peak = dec.samples.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak <= 0.9, "peak {peak}");
        assert_eq!(lag(&x, &dec.channel(0), 50), 0);
    }
}

mod classic_tests {
    use std::sync::atomic::AtomicBool;

    use upscaler_core::audio::classic::upmix_classic;
    use upscaler_core::audio::io::Pcm;
    use upscaler_core::settings::LfeLevel;

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
        let cancel = AtomicBool::new(false);
        for n in [48000usize, 100_003, 1500, 0] {
            let out = upmix_classic(&stereo(n), LfeLevel::Normal, &cancel).unwrap();
            assert_eq!((out.channels, out.frames()), (6, n), "n={n}");
            assert!(out.samples.iter().all(|v| v.is_finite()));
        }
    }

    #[test]
    fn classic_is_in_sync() {
        let cancel = AtomicBool::new(false);
        let src = stereo(5 * 48000);
        let out = upmix_classic(&src, LfeLevel::Normal, &cancel).unwrap();
        let (l, r) = (src.channel(0), src.channel(1));
        let (fl, fr, fc) = (out.channel(0), out.channel(1), out.channel(2));
        let a: Vec<f32> = (0..l.len()).map(|i| l[i] + r[i]).collect();
        let b: Vec<f32> = (0..l.len()).map(|i| fl[i] + fr[i] + fc[i]).collect();
        let (lo, hi) = (96_000usize, 96_000 + 24_000);
        let mut best = (0i32, f64::MIN);
        for lag in -3000i32..=3000 {
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
