//! Exports a short clip exactly as the app would, in the video's project folder, and prints
//! every frame result and review verdict.
//!
//! cargo run -p upscaler-core --release --example export_clip -- <video> <start s> <seconds> <aspect|none> <out.mp4>

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use upscaler_core::ffmpeg::{self, Tools};
use upscaler_core::job::{self, ExportEvent, ExportSpec, ReviewOutcome};
use upscaler_core::project::{self, Project};
use upscaler_core::settings::{AspectChoice, ProviderKind, Settings};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let video = PathBuf::from(args.first().context("video")?);
    let start_s: f64 = args.get(1).context("start")?.parse()?;
    let secs: f64 = args.get(2).context("seconds")?.parse()?;
    let aspect = args.get(3).context("aspect")?;
    let out = PathBuf::from(args.get(4).context("output")?);

    let mut s = Settings::load();
    let dir = project::find_for_video(&video, &s.recent_projects, &s)
        .unwrap_or_else(|| project::new_dir(&video, &s));
    if let Ok(p) = Project::load(&dir) {
        p.settings.apply(&mut s);
    }
    s.project_dir = Some(dir.clone());
    s.provider = ProviderKind::ChatGptSubscription;
    s.review_frames = true;
    s.use_previous_frame = true;
    match aspect.as_str() {
        "none" => s.change_aspect = false,
        a => {
            let want: f64 = match a {
                "16:9" => 16.0 / 9.0,
                x => x.parse()?,
            };
            s.change_aspect = true;
            s.target_aspect = *AspectChoice::ALL
                .iter()
                .find(|c| (c.ratio() - want).abs() < 0.005)
                .context("unknown aspect")?;
        }
    }
    let tools = Tools::locate(&s.ffmpeg_dir).context("ffmpeg not found")?;
    let info = ffmpeg::probe(&tools, &video)?;
    let crop = if s.remove_black_bars { info.detected_crop } else { None };
    let info = info.with_crop(crop);
    let start = info.frame_at(start_s);
    let end = start + (secs * info.fps()).round() as u64;
    let spec = ExportSpec {
        info,
        start,
        end,
        output: out.clone(),
        settings: s,
    };
    println!("project {}", dir.display());
    println!("job {}", spec.job_dir().display());
    println!("frames {start}..{end} ({} frames)", end - start);
    let started = std::time::Instant::now();
    let counts = std::sync::Mutex::new([0u32; 4]);
    job::run_export(&tools, &spec, &AtomicBool::new(false), &|e| {
        let t = started.elapsed().as_secs();
        match e {
            ExportEvent::FrameDone {
                n,
                seconds: Some(sec),
                ..
            } => println!("[{t:>5}s] made   {n:>4} in {sec:.0} s"),
            ExportEvent::Reviewed {
                n,
                outcome,
                problems,
                attempt,
            } => {
                let i = match outcome {
                    ReviewOutcome::Approved => 0,
                    ReviewOutcome::Rejected => 1,
                    ReviewOutcome::Kept => 2,
                    ReviewOutcome::Unchecked => 3,
                };
                counts.lock().unwrap()[i] += 1;
                println!("[{t:>5}s] review {n:>4} try {attempt}: {outcome:?} {problems}");
            }
            ExportEvent::Retry { n, error } => println!("[{t:>5}s] retry  {n:>4}: {error}"),
            ExportEvent::Finished(p) => println!("[{t:>5}s] FINISHED {}", p.display()),
            ExportEvent::Failed(err) => println!("[{t:>5}s] FAILED {err}"),
            ExportEvent::Cancelled => println!("[{t:>5}s] cancelled"),
            _ => {}
        }
    });
    let c = counts.into_inner().unwrap();
    println!(
        "reviews: {} approved, {} rejected, {} kept, {} unchecked; {:.0} min",
        c[0],
        c[1],
        c[2],
        c[3],
        started.elapsed().as_secs_f64() / 60.0
    );
    Ok(())
}
