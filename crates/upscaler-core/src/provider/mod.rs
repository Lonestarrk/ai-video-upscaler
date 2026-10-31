//! The upscaling back ends. Each takes a prepared frame (and optionally the previous
//! result as a reference) and returns an image; [`upscale_frame`] wraps them with the
//! padding, prompt and final resize that every frame needs.

pub mod basic;
pub mod codex;
pub mod openai_api;
pub mod realesrgan;

use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use image::RgbImage;

use crate::frame;
use crate::prompt::{self, PromptParts};
use crate::settings::{ProviderKind, Settings};

/// One request to a back end.
pub struct Request<'a> {
    /// The frame (padded for generative back ends).
    pub image: &'a RgbImage,
    /// The previous restored frame, same canvas layout as `image`.
    pub reference: Option<&'a RgbImage>,
    /// The full instruction (generative back ends only).
    pub prompt: &'a str,
    /// The final output size (local back ends aim for it directly).
    pub target: (u32, u32),
    /// An empty folder the back end may use for its files.
    pub work_dir: &'a Path,
}

pub trait Provider: Send + Sync {
    fn upscale(&self, req: &Request, cancel: &AtomicBool) -> Result<RgbImage>;
}

pub fn create(settings: &Settings) -> Box<dyn Provider> {
    match settings.provider {
        ProviderKind::ChatGptSubscription => Box::new(codex::Codex::from_settings(settings)),
        ProviderKind::OpenAiApi => Box::new(openai_api::OpenAiApi::from_settings(settings)),
        ProviderKind::RealEsrgan => Box::new(realesrgan::RealEsrgan::from_settings(settings)),
        ProviderKind::Basic => Box::new(basic::Basic),
    }
}

/// Upscales one source frame to `target`: pads, builds the prompt, calls the back end and
/// crops/resizes the result. `reference` is the previous *final* frame.
pub fn upscale_frame(
    provider: &dyn Provider,
    settings: &Settings,
    source: &RgbImage,
    reference: Option<&RgbImage>,
    target: (u32, u32),
    work_dir: &Path,
    cancel: &AtomicBool,
) -> Result<RgbImage> {
    std::fs::create_dir_all(work_dir)?;
    if !settings.provider.is_generative() {
        let req = Request {
            image: source,
            reference: None,
            prompt: "",
            target,
            work_dir,
        };
        let out = provider.upscale(&req, cancel)?;
        return Ok(if out.dimensions() == target {
            out
        } else {
            image::imageops::resize(
                &out,
                target.0,
                target.1,
                image::imageops::FilterType::Lanczos3,
            )
        });
    }
    let prepared = frame::prepare(source, settings.pad_to_model_aspect);
    let reference = reference.map(|r| {
        // Same layout as the frame: resize the reference to the source size, then pad alike.
        let small = image::imageops::resize(
            r,
            source.width(),
            source.height(),
            image::imageops::FilterType::Lanczos3,
        );
        frame::limit_size(
            &frame::prepare(&small, settings.pad_to_model_aspect).canvas,
            settings.reference_max_side,
        )
    });
    let text = prompt::build(&PromptParts {
        user_prompt: &settings.prompt,
        padded: prepared.padded(),
        has_reference: reference.is_some(),
    });
    let req = Request {
        image: &prepared.canvas,
        reference: reference.as_ref(),
        prompt: &text,
        target,
        work_dir,
    };
    let result = provider.upscale(&req, cancel)?;
    Ok(frame::finish(&result, &prepared, target))
}

/// Decodes PNG/JPEG/WebP bytes to RGB.
pub fn decode(bytes: &[u8]) -> Result<RgbImage> {
    Ok(image::load_from_memory(bytes)?.to_rgb8())
}
