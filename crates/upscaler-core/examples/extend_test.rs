//! Runs the app's frame pipeline outside the GUI to judge aspect-ratio extension.
//!
//! cargo run -p upscaler-core --release --example extend_test -- <video> <out dir> [--grab] [--aspect 2.35] <seconds>...
//!
//! `--grab` only saves the source frames (no AI call). Otherwise each frame goes through
//! `upscale_frame` with the saved settings, forced to ChatGPT (Codex) and the chosen aspect,
//! and `<n>_marked.png` shows the result with red lines where the original picture ends.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{bail, Context, Result};
use image::Rgb;
use upscaler_core::ffmpeg::{self, Tools};
use upscaler_core::settings::{AspectChoice, ProviderKind, Settings};
use upscaler_core::{frame, job, provider};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let video = PathBuf::from(args.next().context("video path")?);
    let out = PathBuf::from(args.next().context("output folder")?);
    let (mut grab_only, mut aspect, mut times) = (false, AspectChoice::Scope235, Vec::new());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--grab" => grab_only = true,
            "--aspect" => {
                let want: f64 = args.next().context("aspect")?.parse()?;
                aspect = *AspectChoice::ALL
                    .iter()
                    .find(|c| (c.ratio() - want).abs() < 0.005)
                    .context("unknown aspect")?;
            }
            t => times.push(t.parse::<f64>().with_context(|| format!("seconds: {t}"))?),
        }
    }
    if times.is_empty() {
        bail!("give at least one time in seconds");
    }
    std::fs::create_dir_all(&out)?;

    let mut settings = Settings::load();
    settings.provider = ProviderKind::ChatGptSubscription;
    settings.change_aspect = true;
    settings.target_aspect = aspect;
    let tools = Tools::locate(&settings.ffmpeg_dir).context("ffmpeg not found")?;
    let info = ffmpeg::probe(&tools, &video)?;
    let target = job::geometry(&settings, &info).work;
    let cancel = AtomicBool::new(false);

    for (n, t) in times.iter().enumerate() {
        let index = info.frame_at(*t);
        let source = ffmpeg::grab_frame(&tools, &info, index, None, false)?;
        let name = format!("{n:02}_{t:.0}s");
        source.save(out.join(format!("{name}_source.png")))?;
        if grab_only {
            println!("{name}: frame {index}");
            continue;
        }
        let tmp = out.join(format!("{name}_tmp"));
        let started = std::time::Instant::now();
        let result = provider::upscale_frame(
            provider::create(&settings).as_ref(),
            &settings,
            &source,
            None,
            None,
            target,
            &tmp,
            &cancel,
        );
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                println!("{name}: FAILED {e:#}");
                continue;
            }
        };
        let _ = std::fs::remove_dir_all(&tmp);
        result.save(out.join(format!("{name}_result.png")))?;
        let (x, _, w, _) = frame::inner_fraction(
            source.width() as f64 / source.height() as f64,
            aspect.ratio(),
        );
        let mut marked = result.clone();
        let (rw, rh) = marked.dimensions();
        for edge in [x, x + w] {
            let ex = ((edge * rw as f64) as u32).min(rw - 2);
            for y in 0..rh {
                for dx in 0..2 {
                    marked.put_pixel(ex + dx, y, Rgb([255, 0, 0]));
                }
            }
        }
        marked.save(out.join(format!("{name}_marked.png")))?;
        println!(
            "{name}: {:?} in {:.0} s",
            result.dimensions(),
            started.elapsed().as_secs_f64()
        );
    }
    Ok(())
}
