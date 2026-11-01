//! Asks the chosen service's reviewer about one frame, exactly as an export would.
//!
//! cargo run -p upscaler-core --release --example review_test -- <original.png> <result.png> [previous.png] [--aspect 2.35]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Context as _, Result};
use upscaler_core::provider::{self, ReviewRequest};
use upscaler_core::review::{self, Context};
use upscaler_core::settings::{AspectChoice, ProviderKind, Settings};

fn main() -> Result<()> {
    let mut paths = Vec::new();
    let mut aspect = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--aspect" {
            let want: f64 = args.next().context("aspect")?.parse()?;
            aspect = AspectChoice::ALL
                .iter()
                .copied()
                .find(|c| (c.ratio() - want).abs() < 0.005);
        } else {
            paths.push(PathBuf::from(a));
        }
    }
    let open = |p: &PathBuf| -> Result<image::RgbImage> {
        Ok(image::open(p)
            .with_context(|| p.display().to_string())?
            .to_rgb8())
    };
    let source = open(paths.first().context("original.png")?)?;
    let result = open(paths.get(1).context("result.png")?)?;
    let area = aspect.map(|_| upscaler_core::frame::source_area(&result, source.dimensions()));
    let previous = paths.get(2).map(open).transpose()?;

    let mut s = Settings::load();
    s.provider = ProviderKind::ChatGptSubscription;
    let sides = aspect.map(|a| (a.name(), true));
    let instruction = review::instruction(&Context {
        has_previous: previous.is_some(),
        extend: sides,
        user_prompt: &s.prompt,
    });
    let tmp = std::env::temp_dir().join(format!("review-test-{}", std::process::id()));
    let started = std::time::Instant::now();
    let p = provider::create(&s);
    let ask = || {
        p.review(
            &ReviewRequest {
                source: &source,
                result: &result,
                result_area: area.as_ref(),
                previous: previous.as_ref(),
                instruction: &instruction,
                work_dir: &tmp,
            },
            &AtomicBool::new(false),
        )
    };
    // As in an export: a rejection needs a second opinion.
    let first = ask()?;
    let verdict = if first.approved {
        first
    } else {
        let second = ask()?;
        if second.approved {
            println!("(first said no: {:?})", first.problems);
        }
        review::confirm(first, second)
    };
    println!(
        "{} in {:.0} s: {:?}",
        if verdict.approved {
            "APPROVED"
        } else {
            "REJECTED"
        },
        started.elapsed().as_secs_f64(),
        verdict.problems
    );
    Ok(())
}
