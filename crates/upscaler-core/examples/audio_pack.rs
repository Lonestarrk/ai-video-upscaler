//! Manual check of the AI audio pack:
//! `cargo run -p upscaler-core --release --example audio_pack -- <command>`
//!   status | install | remove | device | separate <wav> <dir> | enhance <wav> <out.wav>

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{bail, Result};
use upscaler_core::audio::ai::{AudioAi, LocalAi};
use upscaler_core::audio::pack;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cancel = AtomicBool::new(false);
    let start = Instant::now();
    let mut last = String::new();
    let mut show = |p: f32| {
        let line = format!("{:5.1} %", p * 100.0);
        if line != last {
            eprint!("\r{line}  ");
            last = line;
        }
    };
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["status"] => println!("{:?} in {}", pack::status(), pack::pack_dir().display()),
        ["install"] => {
            let mut last_stage = String::new();
            pack::install(&cancel, |stage, p| {
                if stage != last_stage {
                    eprintln!("\n[{:7.1} s] {stage}", start.elapsed().as_secs_f32());
                    last_stage = stage.to_string();
                }
                show(p);
            })?;
            eprintln!();
            println!("{:?}", pack::status());
        }
        ["remove"] => pack::remove()?,
        ["device"] => {
            let ai = LocalAi::open()?;
            println!("recorded at install: {:?}", ai.device);
            println!("now: {:?}", ai.probe_device(&cancel)?);
        }
        ["separate", wav, dir] => {
            let ai = LocalAi::open()?;
            let stems = ai.separate(Path::new(wav), Path::new(dir), &cancel, &mut show)?;
            eprintln!();
            println!("{stems:?}");
        }
        ["enhance", wav, out] => {
            let ai = LocalAi::open()?;
            ai.enhance(Path::new(wav), Path::new(out), &cancel, &mut show)?;
            eprintln!();
        }
        _ => bail!("usage: status | install | remove | device | separate <wav> <dir> | enhance <wav> <out.wav>"),
    }
    eprintln!("done in {:.1} s", start.elapsed().as_secs_f32());
    Ok(())
}
