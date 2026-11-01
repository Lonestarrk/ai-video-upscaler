//! Exports a range with new audio (5.1 or enhanced stereo) through `job::run_export`, with
//! the local Basic image provider so no frame goes to an AI service. Loads the saved
//! settings and overrides only this run's copy; the settings file is never written.
//!
//! cargo run -p upscaler-core --release --example export_audio_test -- \
//!     <video> <start s> <end s> <ai|classic|enhance> <subtle|normal|strong|cinema> <out.mkv> <work dir>
//!
//! Prints the range, the AI device, every audio warning and the time per stage.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use upscaler_core::audio::ai::LocalAi;
use upscaler_core::audio::job::{self as audio_job, AudioEvent};
use upscaler_core::ffmpeg::{self, Tools};
use upscaler_core::job::{self, ExportEvent, ExportSpec, Stage};
use upscaler_core::settings::{
    AiRate, AudioMode, LfeLevel, OutputSize, ProviderKind, Settings, SurroundEngine,
};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 7 {
        bail!("usage: <video> <start s> <end s> <ai|classic|enhance> <lfe> <out.mkv> <work dir>");
    }
    let video = PathBuf::from(&args[0]);
    let start_s: f64 = args[1].parse()?;
    let end_s: f64 = args[2].parse()?;
    let out = PathBuf::from(&args[5]);
    let work = PathBuf::from(&args[6]);

    let mut s = Settings::load();
    s.project_dir = Some(work.clone());
    s.provider = ProviderKind::Basic;
    s.change_aspect = false;
    s.review_frames = false;
    s.ai_rate = AiRate::All;
    s.output_size = OutputSize::Scale2;
    match args[3].as_str() {
        "ai" => {
            s.audio_mode = AudioMode::Surround;
            s.surround_engine = SurroundEngine::LocalAi;
            s.audio_enhance = false;
        }
        "classic" => {
            s.audio_mode = AudioMode::Surround;
            s.surround_engine = SurroundEngine::Classic;
            s.audio_enhance = false;
        }
        "enhance" => {
            s.audio_mode = AudioMode::Original;
            s.audio_enhance = true;
        }
        m => bail!("unknown mode {m}"),
    }
    s.lfe_level = match args[4].as_str() {
        "subtle" => LfeLevel::Subtle,
        "normal" => LfeLevel::Normal,
        "strong" => LfeLevel::Strong,
        "cinema" => LfeLevel::Cinema,
        l => bail!("unknown LFE level {l}"),
    };

    let tools = Tools::locate(&s.ffmpeg_dir).context("ffmpeg not found")?;
    let info = ffmpeg::probe(&tools, &video)?;
    let crop = if s.remove_black_bars {
        info.detected_crop
    } else {
        None
    };
    let info = info.with_crop(crop);
    let start = info.frame_at(start_s);
    let end = info.frame_at(end_s);
    let (t0, dur, samples) = audio_job::range(&info, start, end);
    println!(
        "fps {}/{} frames {start}..{end} ({}) audio t0 {t0:.6} s dur {dur:.6} s samples {samples}",
        info.fps_num,
        info.fps_den,
        end - start
    );
    if audio_job::may_use_ai(&s) {
        let ai = LocalAi::open()?;
        println!("pack device (installed) {:?}", ai.device);
        println!(
            "pack device (worker now) {:?}",
            ai.probe_device(&AtomicBool::new(false))?
        );
    } else {
        println!("pack device: not used");
    }
    let spec = ExportSpec {
        info,
        start,
        end,
        output: out,
        settings: s,
    };
    println!("job {}", spec.job_dir().display());

    let started = Instant::now();
    let audio_started = Mutex::new(None::<Instant>);
    let result = Mutex::new(None::<Result<PathBuf, String>>);
    job::run_export(&tools, &spec, &AtomicBool::new(false), &|e| {
        let t = started.elapsed().as_secs_f64();
        match e {
            ExportEvent::Progress {
                stage: Stage::Audio,
                done,
                total,
            } => {
                let mut a = audio_started.lock().unwrap();
                if a.is_none() {
                    *a = Some(Instant::now());
                    println!("[{t:>7.1}s] audio job starts");
                }
                if done > 0 {
                    println!("[{t:>7.1}s] audio section {done}/{total}");
                }
            }
            ExportEvent::Audio(AudioEvent::Warning(w)) => println!("[{t:>7.1}s] WARNING {w}"),
            ExportEvent::Audio(AudioEvent::Stage(st)) => println!("[{t:>7.1}s] audio {st:?}"),
            ExportEvent::Finished(p) => {
                println!("[{t:>7.1}s] FINISHED {}", p.display());
                *result.lock().unwrap() = Some(Ok(p));
            }
            ExportEvent::Failed(err) => {
                println!("[{t:>7.1}s] FAILED {err}");
                *result.lock().unwrap() = Some(Err(err));
            }
            ExportEvent::Cancelled => println!("[{t:>7.1}s] cancelled"),
            _ => {}
        }
    });
    let total = started.elapsed().as_secs_f64();
    let audio = audio_started
        .into_inner()
        .unwrap()
        .map(|a| a.elapsed().as_secs_f64());
    println!(
        "total {total:.1} s, audio job + mux {}",
        audio.map_or("-".into(), |a| format!("{a:.1} s"))
    );
    match result.into_inner().unwrap() {
        Some(Ok(_)) => Ok(()),
        Some(Err(e)) => bail!(e),
        None => bail!("the export ended without a result"),
    }
}
