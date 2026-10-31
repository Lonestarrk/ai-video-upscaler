//! Real-ESRGAN through `realesrgan-ncnn-vulkan` (free, local, any Vulkan GPU).
//! https://github.com/xinntao/Real-ESRGAN/releases

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use image::RgbImage;

use super::{Provider, Request};
use crate::process::{self, command};
use crate::settings::Settings;

pub struct RealEsrgan {
    exe: String,
    model: String,
}

impl RealEsrgan {
    pub fn from_settings(s: &Settings) -> Self {
        RealEsrgan {
            exe: s.realesrgan_path.trim().to_string(),
            model: s.realesrgan_model.trim().to_string(),
        }
    }

    pub fn locate(path: &str) -> Option<PathBuf> {
        let p = PathBuf::from(path.trim());
        if !path.trim().is_empty() && p.is_file() {
            return Some(p);
        }
        which::which("realesrgan-ncnn-vulkan").ok()
    }
}

impl Provider for RealEsrgan {
    fn upscale(&self, req: &Request, cancel: &AtomicBool) -> Result<RgbImage> {
        let Some(exe) = Self::locate(&self.exe) else {
            bail!("realesrgan-ncnn-vulkan was not found: set its path in the settings");
        };
        let input = req.work_dir.join("input.png");
        let output = req.work_dir.join("output.png");
        req.image.save(&input)?;
        let model = if self.model.is_empty() {
            "realesrgan-x4plus"
        } else {
            &self.model
        };
        // The x4plus models only run at 4x; the final resize brings it to the target.
        let scale = if model.contains("x4plus") {
            4
        } else {
            ((req.target.0 as f64 / req.image.width() as f64).ceil() as u32).clamp(2, 4)
        };
        let mut cmd = command(&exe);
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        cmd.arg("-i").arg(&input).arg("-o").arg(&output).args([
            "-n",
            model,
            "-s",
            &scale.to_string(),
            "-f",
            "png",
        ]);
        let out = process::run(cmd, None, cancel, Some(Duration::from_secs(600)))?;
        if !out.success || !output.is_file() {
            bail!("Real-ESRGAN failed: {}", out.tail(6));
        }
        Ok(image::open(&output)
            .context("Real-ESRGAN output")?
            .to_rgb8())
    }
}
