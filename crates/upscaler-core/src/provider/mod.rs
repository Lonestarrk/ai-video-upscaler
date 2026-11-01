//! The upscaling back ends. Each takes a prepared frame (and optionally the previous
//! result as a reference) and returns an image; [`upscale_frame`] wraps them with the
//! padding, prompt and final resize that every frame needs.

pub mod basic;
pub mod codex;
pub mod flux;
pub mod gemini;
pub mod grok;
pub mod http;
pub mod openai_api;
pub mod realesrgan;

use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use image::RgbImage;

use crate::frame;
use crate::prompt::{self, PromptParts};
use crate::review;
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

/// One finished frame for quality control.
pub struct ReviewRequest<'a> {
    /// The original frame as decoded from the video.
    pub source: &'a RgbImage,
    /// The restored frame to judge.
    pub result: &'a RgbImage,
    /// When the picture was extended: the part of `result` where the source picture sits,
    /// cut out at the source's size, so framing and faithfulness compare like with like.
    pub result_area: Option<&'a RgbImage>,
    /// The previous restored frame of the same shot, for consistency.
    pub previous: Option<&'a RgbImage>,
    pub instruction: &'a str,
    pub work_dir: &'a Path,
}

pub trait Provider: Send + Sync {
    fn upscale(&self, req: &Request, cancel: &AtomicBool) -> Result<RgbImage>;

    /// Aspect ratios (width / height) the model answers in; frames are padded to the nearest.
    /// Empty: the model keeps any input shape.
    fn canvas_aspects(&self) -> &'static [f64] {
        &frame::MODEL_ASPECTS
    }

    /// Whether [`Provider::review`] works for this service.
    fn can_review(&self) -> bool {
        false
    }

    /// Asks the service's own model to judge a finished frame.
    fn review(&self, _req: &ReviewRequest, _cancel: &AtomicBool) -> Result<review::Verdict> {
        anyhow::bail!("this service cannot review frames")
    }
}

pub fn create(settings: &Settings) -> Box<dyn Provider> {
    match settings.provider {
        ProviderKind::ChatGptSubscription => Box::new(codex::Codex::from_settings(settings)),
        ProviderKind::OpenAiApi => Box::new(openai_api::OpenAiApi::from_settings(settings)),
        ProviderKind::Gemini => Box::new(gemini::Gemini::from_settings(settings)),
        ProviderKind::Flux => Box::new(flux::Flux::from_settings(settings)),
        ProviderKind::Grok => Box::new(grok::Grok::from_settings(settings)),
        ProviderKind::RealEsrgan => Box::new(realesrgan::RealEsrgan::from_settings(settings)),
        ProviderKind::Basic => Box::new(basic::Basic),
    }
}

/// Upscales one source frame to `target`: pads, builds the prompt, calls the back end and
/// crops/resizes the result. `reference` is the previous *final* frame; `feedback` is why
/// quality control rejected the last attempt at this frame.
#[allow(clippy::too_many_arguments)]
pub fn upscale_frame(
    provider: &dyn Provider,
    settings: &Settings,
    source: &RgbImage,
    reference: Option<&RgbImage>,
    feedback: Option<&str>,
    target: (u32, u32),
    work_dir: &Path,
    cancel: &AtomicBool,
) -> Result<RgbImage> {
    upscale_frame_with_prompt(
        provider, settings, source, reference, feedback, target, work_dir, cancel,
    )
    .map(|(img, _)| img)
}

/// Like [`upscale_frame`], and also returns the exact prompt text sent to the service
/// (empty for services that take none).
#[allow(clippy::too_many_arguments)]
pub fn upscale_frame_with_prompt(
    provider: &dyn Provider,
    settings: &Settings,
    source: &RgbImage,
    reference: Option<&RgbImage>,
    feedback: Option<&str>,
    target: (u32, u32),
    work_dir: &Path,
    cancel: &AtomicBool,
) -> Result<(RgbImage, String)> {
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
        let img = if out.dimensions() == target {
            out
        } else {
            image::imageops::resize(
                &out,
                target.0,
                target.1,
                image::imageops::FilterType::Lanczos3,
            )
        };
        return Ok((img, String::new()));
    }
    // A new aspect ratio: the frame sits on a grey canvas of that shape for the AI to fill.
    let aspect = settings.aspect_change();
    let source_aspect = source.width() as f64 / source.height() as f64;
    let extend = aspect.filter(|a| (a.ratio() / source_aspect - 1.0).abs() > 0.01);
    let base = match extend {
        Some(a) => frame::extend_canvas(source, a.ratio()),
        None => source.clone(),
    };
    let aspects: &[f64] = if settings.pad_to_model_aspect {
        provider.canvas_aspects()
    } else {
        &[]
    };
    let prepared = frame::prepare_with(&base, aspects);
    // Services that edit one image at a time get no reference (and no word about one).
    let reference = reference.filter(|_| settings.reference_supported());
    let reference = reference.map(|r| {
        // Same layout as the frame: resize the reference to the canvas size, then pad alike.
        let small = image::imageops::resize(
            r,
            base.width(),
            base.height(),
            image::imageops::FilterType::Lanczos3,
        );
        frame::limit_size(
            &frame::prepare_with(&small, aspects).canvas,
            settings.reference_max_side,
        )
    });
    let text = prompt::build(&PromptParts {
        user_prompt: &settings.prompt,
        padded: prepared.padded(),
        has_reference: reference.is_some(),
        extend: extend.map(|a| (a.name(), a.ratio() > source_aspect)),
        feedback,
    });
    let req = Request {
        image: &prepared.canvas,
        reference: reference.as_ref(),
        prompt: &text,
        target,
        work_dir,
    };
    let result = provider.upscale(&req, cancel)?;
    let finished = frame::finish(&result, &prepared, target);
    if extend.is_some() && frame::canvas_left_grey(&finished, source.dimensions()) {
        return Err(GreyCanvasLeft.into());
    }
    Ok((finished, text))
}

/// The AI returned the extended picture with the grey canvas still in it.
#[derive(Debug)]
pub struct GreyCanvasLeft;

impl std::fmt::Display for GreyCanvasLeft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the AI left the grey canvas unfilled")
    }
}

impl std::error::Error for GreyCanvasLeft {}

/// Whether `e` is (or was caused by) [`GreyCanvasLeft`].
pub fn is_grey_canvas_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<GreyCanvasLeft>())
}

/// Decodes PNG/JPEG/WebP bytes to RGB.
pub fn decode(bytes: &[u8]) -> Result<RgbImage> {
    Ok(image::load_from_memory(bytes)?.to_rgb8())
}
