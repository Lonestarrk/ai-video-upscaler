//! Google Gemini image models ("Nano Banana", e.g. Gemini 3 Pro Image) through the Gemini
//! API (`generateContent`) with a key from Google AI Studio. Rated among the best at keeping
//! identity and composition when editing real photos. Billed per image.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use serde_json::{json, Value};

use super::http;
use super::{Provider, Request};
use crate::process::Cancelled;
use crate::settings::Settings;

/// Aspect ratios the Gemini image models answer in.
pub const ASPECTS: [(f64, &str); 10] = [
    (1.0, "1:1"),
    (2.0 / 3.0, "2:3"),
    (1.5, "3:2"),
    (0.75, "3:4"),
    (4.0 / 3.0, "4:3"),
    (0.8, "4:5"),
    (1.25, "5:4"),
    (9.0 / 16.0, "9:16"),
    (16.0 / 9.0, "16:9"),
    (21.0 / 9.0, "21:9"),
];
const RATIOS: [f64; 10] = [
    1.0,
    2.0 / 3.0,
    1.5,
    0.75,
    4.0 / 3.0,
    0.8,
    1.25,
    9.0 / 16.0,
    16.0 / 9.0,
    21.0 / 9.0,
];

pub const ENV_KEYS: [&str; 2] = ["GEMINI_API_KEY", "GOOGLE_API_KEY"];

pub struct Gemini {
    /// Service address (tests point it at a local server).
    pub base: String,
    key: String,
    model: String,
    size: String,
}

impl Gemini {
    pub fn from_settings(s: &Settings) -> Self {
        Gemini {
            base: "https://generativelanguage.googleapis.com".into(),
            key: http::key(&s.gemini_api_key, &ENV_KEYS),
            model: if s.gemini_model.trim().is_empty() {
                "gemini-3-pro-image".into()
            } else {
                s.gemini_model.trim().into()
            },
            size: s.gemini_image_size.trim().to_string(),
        }
    }
}

/// The supported aspect ratio name nearest to `w`x`h`.
pub fn aspect_name(w: u32, h: u32) -> &'static str {
    let a = (w as f64 / h as f64).ln();
    ASPECTS
        .iter()
        .min_by(|x, y| (x.0.ln() - a).abs().total_cmp(&(y.0.ln() - a).abs()))
        .unwrap()
        .1
}

/// The body of a `generateContent` request.
pub fn request_body(
    prompt: &str,
    images: &[String],
    aspect: &str,
    size: &str,
    model: &str,
) -> Value {
    let mut parts = vec![json!({ "text": prompt })];
    for b64 in images {
        parts.push(json!({ "inlineData": { "mimeType": "image/png", "data": b64 } }));
    }
    let mut image_config = json!({ "aspectRatio": aspect });
    // The first Nano Banana (2.5 Flash Image) has no size setting.
    if !size.is_empty() && !model.contains("2.5") {
        image_config["imageSize"] = json!(size);
    }
    json!({
        "contents": [{ "role": "user", "parts": parts }],
        "generationConfig": { "responseModalities": ["IMAGE"], "imageConfig": image_config }
    })
}

/// The first image in a `generateContent` answer.
pub fn image_from_response(body: &Value) -> Result<RgbImage> {
    let candidates = body["candidates"].as_array().cloned().unwrap_or_default();
    for c in &candidates {
        for p in c["content"]["parts"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let inline = if p["inlineData"].is_object() {
                &p["inlineData"]
            } else {
                &p["inline_data"]
            };
            if let Some(data) = inline["data"].as_str() {
                return http::decode_base64_image(data);
            }
        }
    }
    if let Some(reason) = body["promptFeedback"]["blockReason"].as_str() {
        bail!("Gemini refused the frame ({reason})");
    }
    let finish = candidates
        .first()
        .and_then(|c| c["finishReason"].as_str())
        .unwrap_or("no image");
    let text: String = candidates
        .first()
        .and_then(|c| c["content"]["parts"].as_array())
        .map(|ps| {
            ps.iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Err(anyhow!(
        "Gemini returned no image ({finish}). {}",
        text.chars().take(300).collect::<String>()
    ))
}

impl Provider for Gemini {
    fn canvas_aspects(&self) -> &'static [f64] {
        &RATIOS
    }

    fn upscale(&self, req: &Request, cancel: &AtomicBool) -> Result<RgbImage> {
        if self.key.is_empty() {
            bail!("no Gemini API key: add one in the settings (Google AI Studio)");
        }
        let mut images = vec![http::png_base64(req.image)?];
        if let Some(r) = req.reference {
            images.push(http::png_base64(r)?);
        }
        let aspect = aspect_name(req.image.width(), req.image.height());
        let body = request_body(req.prompt, &images, aspect, &self.size, &self.model);
        let url = format!("{}/v1beta/models/{}:generateContent", self.base, self.model);
        let resp = http::client()?
            .post(url)
            .header("x-goog-api-key", &self.key)
            .json(&body)
            .send()
            .context("could not reach the Gemini API")?;
        if cancel.load(Ordering::Relaxed) {
            bail!(Cancelled);
        }
        let status = resp.status();
        let body: Value = resp
            .json()
            .context("unexpected answer from the Gemini API")?;
        if !status.is_success() {
            bail!("Gemini API {status}: {}", http::error_text(&body));
        }
        image_from_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn talks_to_the_api() {
        use crate::provider::http::mock;
        let out = RgbImage::from_pixel(8, 4, image::Rgb([9, 9, 9]));
        let b64 = http::png_base64(&out).unwrap();
        let (base, rx) = mock::serve(vec![
            mock::json(
                200,
                json!({"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":b64}}]}}]}),
            ),
            mock::json(
                429,
                json!({"error":{"code":429,"message":"Resource has been exhausted"}}),
            ),
        ]);
        let mut g = Gemini::from_settings(&Settings {
            gemini_api_key: "k1".into(),
            ..Settings::default()
        });
        g.base = base;
        let dir = tempfile::tempdir().unwrap();
        let img = RgbImage::new(4, 2);
        let req = Request {
            image: &img,
            reference: Some(&img),
            prompt: "Restore.",
            target: (8, 4),
            work_dir: dir.path(),
        };
        assert_eq!(
            g.upscale(&req, &AtomicBool::new(false))
                .unwrap()
                .dimensions(),
            (8, 4)
        );
        let r = rx.recv().unwrap();
        assert!(
            r.head
                .starts_with("POST /v1beta/models/gemini-3-pro-image:generateContent"),
            "{}",
            r.head
        );
        assert!(r.head.contains("x-goog-api-key: k1"));
        let sent: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(
            sent["contents"][0]["parts"].as_array().unwrap().len(),
            3,
            "prompt, frame and reference"
        );
        assert_eq!(
            sent["generationConfig"]["imageConfig"]["aspectRatio"],
            "16:9"
        );
        let err = g
            .upscale(&req, &AtomicBool::new(false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("429") && err.contains("exhausted"), "{err}");
    }

    #[test]
    fn body_and_response() {
        let body = request_body(
            "Restore.",
            &["AAA".into(), "BBB".into()],
            "16:9",
            "2K",
            "gemini-3-pro-image",
        );
        assert_eq!(body["contents"][0]["parts"].as_array().unwrap().len(), 3);
        assert_eq!(
            body["generationConfig"]["imageConfig"]["aspectRatio"],
            "16:9"
        );
        assert_eq!(body["generationConfig"]["imageConfig"]["imageSize"], "2K");
        let old = request_body("x", &[], "1:1", "2K", "gemini-2.5-flash-image");
        assert!(old["generationConfig"]["imageConfig"]["imageSize"].is_null());
        assert_eq!(aspect_name(1920, 1080), "16:9");
        assert_eq!(aspect_name(768, 576), "4:3");
        assert_eq!(aspect_name(854, 364), "21:9");

        let img = RgbImage::from_pixel(4, 2, image::Rgb([1, 2, 3]));
        let b64 = http::png_base64(&img).unwrap();
        let resp = json!({"candidates":[{"content":{"parts":[{"text":"here"},{"inlineData":{"mimeType":"image/png","data":b64}}]}}]});
        assert_eq!(image_from_response(&resp).unwrap().dimensions(), (4, 2));
        let blocked = json!({"promptFeedback":{"blockReason":"SAFETY"}});
        assert!(image_from_response(&blocked)
            .unwrap_err()
            .to_string()
            .contains("SAFETY"));
    }
}
