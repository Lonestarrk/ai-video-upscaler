//! OpenAI Images API (`/v1/images/edits`) with an API key: pay per image, but no agent turn
//! in between, so it is faster and can run several frames in parallel.

use std::io::Cursor;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use image::RgbImage;
use reqwest::blocking::multipart::{Form, Part};

use super::{Provider, Request};
use crate::process::Cancelled;
use crate::settings::Settings;

pub struct OpenAiApi {
    key: String,
    model: String,
    size: String,
    quality: String,
    input_fidelity: bool,
}

impl OpenAiApi {
    pub fn from_settings(s: &Settings) -> Self {
        let key = if s.openai_api_key.trim().is_empty() {
            std::env::var("OPENAI_API_KEY").unwrap_or_default()
        } else {
            s.openai_api_key.trim().to_string()
        };
        OpenAiApi {
            key,
            model: s.openai_model.trim().to_string(),
            size: s.openai_size.trim().to_string(),
            quality: s.openai_quality.trim().to_string(),
            input_fidelity: s.openai_input_fidelity,
        }
    }
}

fn png_part(img: &RgbImage, name: &str) -> Result<Part> {
    let mut buf = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)?;
    Ok(Part::bytes(buf)
        .file_name(name.to_string())
        .mime_str("image/png")?)
}

impl Provider for OpenAiApi {
    fn upscale(&self, req: &Request, cancel: &AtomicBool) -> Result<RgbImage> {
        if self.key.is_empty() {
            bail!("no OpenAI API key: add one in the settings");
        }
        let mut form = Form::new()
            .text(
                "model",
                if self.model.is_empty() {
                    "gpt-image-2".to_string()
                } else {
                    self.model.clone()
                },
            )
            .text("prompt", req.prompt.to_string())
            .text("n", "1")
            .part("image[]", png_part(req.image, "frame.png")?);
        if let Some(r) = req.reference {
            form = form.part("image[]", png_part(r, "reference.png")?);
        }
        if !self.size.is_empty() {
            form = form.text("size", self.size.clone());
        }
        if !self.quality.is_empty() {
            form = form.text("quality", self.quality.clone());
        }
        if self.input_fidelity {
            form = form.text("input_fidelity", "high");
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        let resp = client
            .post("https://api.openai.com/v1/images/edits")
            .bearer_auth(&self.key)
            .multipart(form)
            .send()
            .context("could not reach api.openai.com")?;
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            bail!(Cancelled);
        }
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .context("unexpected answer from the OpenAI API")?;
        if !status.is_success() {
            let msg = body["error"]["message"].as_str().unwrap_or("unknown error");
            bail!("OpenAI API {status}: {msg}");
        }
        let b64 = body["data"][0]["b64_json"]
            .as_str()
            .ok_or_else(|| anyhow!("the OpenAI API returned no image"))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
        super::decode(&bytes)
    }
}
