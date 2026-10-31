//! Black Forest Labs FLUX through api.bfl.ai: FLUX.1 Kontext (one image) or FLUX.2 (takes the
//! previous frame as a second image). Known for keeping identity and composition while
//! editing, keeps any input shape. Billed per image; requests are queued and polled.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use image::RgbImage;
use serde_json::{json, Value};

use super::http;
use super::{Provider, Request};
use crate::process::Cancelled;
use crate::settings::Settings;

pub const ENV_KEYS: [&str; 1] = ["BFL_API_KEY"];
pub const MODELS: [&str; 4] = [
    "flux-kontext-pro",
    "flux-kontext-max",
    "flux-2-pro",
    "flux-2-max",
];

pub struct Flux {
    /// Service address (tests point it at a local server).
    pub base: String,
    key: String,
    model: String,
}

impl Flux {
    pub fn from_settings(s: &Settings) -> Self {
        Flux {
            base: "https://api.bfl.ai".into(),
            key: http::key(&s.bfl_api_key, &ENV_KEYS),
            model: if s.bfl_model.trim().is_empty() {
                "flux-kontext-pro".into()
            } else {
                s.bfl_model.trim().into()
            },
        }
    }
}

pub fn request_body(prompt: &str, image: String, reference: Option<String>) -> Value {
    let mut body = json!({
        "prompt": prompt,
        "input_image": image,
        "output_format": "png",
        "safety_tolerance": 2,
    });
    if let Some(r) = reference {
        body["input_image_2"] = json!(r);
    }
    body
}

impl Provider for Flux {
    fn canvas_aspects(&self) -> &'static [f64] {
        &[] // keeps the input's shape (3:7 to 7:3)
    }

    fn upscale(&self, req: &Request, cancel: &AtomicBool) -> Result<RgbImage> {
        if self.key.is_empty() {
            bail!("no Black Forest Labs API key: add one in the settings");
        }
        let client = http::client()?;
        let reference = match req.reference {
            Some(r) if !self.model.contains("kontext") => Some(http::png_base64(r)?),
            _ => None,
        };
        let body = request_body(req.prompt, http::png_base64(req.image)?, reference);
        let resp = client
            .post(format!("{}/v1/{}", self.base, self.model))
            .header("x-key", &self.key)
            .header("accept", "application/json")
            .json(&body)
            .send()
            .context("could not reach api.bfl.ai")?;
        let status = resp.status();
        let created: Value = resp.json().context("unexpected answer from api.bfl.ai")?;
        if !status.is_success() {
            bail!("FLUX API {status}: {}", http::error_text(&created));
        }
        let poll = created["polling_url"]
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                created["id"]
                    .as_str()
                    .map(|id| format!("{}/v1/get_result?id={id}", self.base))
            })
            .context("api.bfl.ai returned no job id")?;
        let started = Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!(Cancelled);
            }
            if started.elapsed() > Duration::from_secs(600) {
                bail!("FLUX did not finish within 10 minutes");
            }
            std::thread::sleep(Duration::from_millis(800));
            let r: Value = client
                .get(&poll)
                .header("x-key", &self.key)
                .header("accept", "application/json")
                .send()?
                .json()?;
            match r["status"].as_str().unwrap_or("") {
                "Ready" => {
                    let url = r["result"]["sample"]
                        .as_str()
                        .context("FLUX result without image")?;
                    return http::download(&client, url);
                }
                "Pending" | "Queued" | "Processing" | "Task not found" | "" => continue,
                other => bail!("FLUX: {other} {}", http::error_text(&r)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submits_polls_and_downloads() {
        use crate::provider::http::mock;
        let out = RgbImage::from_pixel(6, 6, image::Rgb([1, 2, 3]));
        let (dl_base, dl_rx) =
            mock::serve(vec![(200, http::png_bytes(&out).unwrap(), "image/png")]);
        let (poll_base, poll_rx) = mock::serve(vec![
            mock::json(200, json!({"status":"Pending"})),
            mock::json(
                200,
                json!({"status":"Ready","result":{"sample":format!("{dl_base}/sample.png")}}),
            ),
        ]);
        let (create_base, create_rx) = mock::serve(vec![mock::json(
            200,
            json!({"id":"job1","polling_url":format!("{poll_base}/v1/get_result?id=job1")}),
        )]);
        let mut f = Flux::from_settings(&Settings {
            bfl_api_key: "k2".into(),
            bfl_model: "flux-2-pro".into(),
            ..Settings::default()
        });
        f.base = create_base;
        let dir = tempfile::tempdir().unwrap();
        let img = RgbImage::new(4, 4);
        let req = Request {
            image: &img,
            reference: Some(&img),
            prompt: "Restore.",
            target: (6, 6),
            work_dir: dir.path(),
        };
        assert_eq!(
            f.upscale(&req, &AtomicBool::new(false))
                .unwrap()
                .dimensions(),
            (6, 6)
        );
        let c = create_rx.recv().unwrap();
        assert!(
            c.head.starts_with("POST /v1/flux-2-pro") && c.head.contains("x-key: k2"),
            "{}",
            c.head
        );
        let sent: Value = serde_json::from_slice(&c.body).unwrap();
        assert!(sent["input_image"].is_string() && sent["input_image_2"].is_string());
        assert!(poll_rx
            .recv()
            .unwrap()
            .head
            .starts_with("GET /v1/get_result?id=job1"));
        assert!(dl_rx.recv().unwrap().head.starts_with("GET /sample.png"));

        // Kontext edits one image: no second one is sent.
        let (create_base, create_rx) = mock::serve(vec![mock::json(400, json!({"detail":"bad"}))]);
        let mut k = Flux::from_settings(&Settings {
            bfl_api_key: "k2".into(),
            ..Settings::default()
        });
        k.base = create_base;
        assert!(k
            .upscale(&req, &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("bad"));
        let sent: Value = serde_json::from_slice(&create_rx.recv().unwrap().body).unwrap();
        assert!(sent["input_image_2"].is_null());
    }

    #[test]
    fn body() {
        let b = request_body("Restore.", "AAA".into(), Some("BBB".into()));
        assert_eq!(
            (b["input_image"].as_str(), b["input_image_2"].as_str()),
            (Some("AAA"), Some("BBB"))
        );
        assert!(request_body("x", "A".into(), None)["input_image_2"].is_null());
    }
}
