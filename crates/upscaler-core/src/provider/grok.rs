//! xAI Grok Imagine through api.x.ai (`/v1/images/edits`, JSON). Fast and cheap; edits one
//! image at a time and keeps its shape. Billed per image.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use image::RgbImage;
use serde_json::{json, Value};

use super::http;
use super::{Provider, Request};
use crate::process::Cancelled;
use crate::settings::Settings;

pub const ENV_KEYS: [&str; 1] = ["XAI_API_KEY"];

pub struct Grok {
    /// Service address (tests point it at a local server).
    pub base: String,
    key: String,
    model: String,
}

impl Grok {
    pub fn from_settings(s: &Settings) -> Self {
        Grok {
            base: "https://api.x.ai".into(),
            key: http::key(&s.xai_api_key, &ENV_KEYS),
            model: if s.xai_model.trim().is_empty() {
                "grok-imagine-image-2.0".into()
            } else {
                s.xai_model.trim().into()
            },
        }
    }
}

pub fn request_body(model: &str, prompt: &str, data_uri: String) -> Value {
    json!({
        "model": model,
        "prompt": prompt,
        "image": { "url": data_uri, "type": "image_url" },
        "response_format": "b64_json",
    })
}

impl Provider for Grok {
    fn canvas_aspects(&self) -> &'static [f64] {
        &[] // answers in the input's shape
    }

    fn upscale(&self, req: &Request, cancel: &AtomicBool) -> Result<RgbImage> {
        if self.key.is_empty() {
            bail!("no xAI API key: add one in the settings");
        }
        let client = http::client()?;
        let resp = client
            .post(format!("{}/v1/images/edits", self.base))
            .bearer_auth(&self.key)
            .json(&request_body(
                &self.model,
                req.prompt,
                http::png_data_uri(req.image)?,
            ))
            .send()
            .context("could not reach api.x.ai")?;
        if cancel.load(Ordering::Relaxed) {
            bail!(Cancelled);
        }
        let status = resp.status();
        let body: Value = resp.json().context("unexpected answer from api.x.ai")?;
        if !status.is_success() {
            bail!("xAI API {status}: {}", http::error_text(&body));
        }
        let item = &body["data"][0];
        if let Some(b64) = item["b64_json"].as_str() {
            return http::decode_base64_image(b64);
        }
        match item["url"].as_str() {
            Some(url) => http::download(&client, url),
            None => bail!("xAI returned no image: {}", http::error_text(&body)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn talks_to_the_api() {
        use crate::provider::http::mock;
        let out = RgbImage::from_pixel(5, 3, image::Rgb([7, 7, 7]));
        let (base, rx) = mock::serve(vec![mock::json(
            200,
            json!({"data":[{"b64_json": http::png_base64(&out).unwrap()}]}),
        )]);
        let mut g = Grok::from_settings(&Settings {
            xai_api_key: "k3".into(),
            ..Settings::default()
        });
        g.base = base;
        let dir = tempfile::tempdir().unwrap();
        let img = RgbImage::new(5, 3);
        let req = Request {
            image: &img,
            reference: None,
            prompt: "Restore.",
            target: (5, 3),
            work_dir: dir.path(),
        };
        assert_eq!(
            g.upscale(&req, &AtomicBool::new(false))
                .unwrap()
                .dimensions(),
            (5, 3)
        );
        let r = rx.recv().unwrap();
        assert!(r.head.starts_with("POST /v1/images/edits"));
        assert!(r.head.to_lowercase().contains("authorization: bearer k3"));
        let sent: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(sent["image"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
    }

    #[test]
    fn body() {
        let b = request_body(
            "grok-imagine-image-2.0",
            "Restore.",
            "data:image/png;base64,AAA".into(),
        );
        assert_eq!(b["image"]["type"], "image_url");
        assert_eq!(b["image"]["url"], "data:image/png;base64,AAA");
    }
}
