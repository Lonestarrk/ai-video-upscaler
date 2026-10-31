//! Lanczos resize plus a little sharpening. Instant and free; useful for trying the whole
//! workflow (preview, export, audio) before spending AI quota.

use std::sync::atomic::AtomicBool;

use anyhow::Result;
use image::RgbImage;

use super::{Provider, Request};

pub struct Basic;

impl Provider for Basic {
    fn upscale(&self, req: &Request, _cancel: &AtomicBool) -> Result<RgbImage> {
        Ok(crate::frame::basic_upscale(req.image, req.target))
    }
}
