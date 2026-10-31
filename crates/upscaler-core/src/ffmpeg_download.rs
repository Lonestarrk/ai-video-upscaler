//! Downloads FFmpeg into the app's tools folder, so nobody has to install it by hand.
//! Windows: the BtbN GPL build from GitHub. macOS: the static builds from evermeet.cx.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};

use crate::paths;
use crate::process::Cancelled;

pub fn supported() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

/// (url, file names to take from the zip)
fn sources() -> Vec<(&'static str, Vec<&'static str>)> {
    if cfg!(windows) {
        vec![(
            "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip",
            vec!["ffmpeg.exe", "ffprobe.exe"],
        )]
    } else {
        vec![
            ("https://evermeet.cx/ffmpeg/getrelease/zip", vec!["ffmpeg"]),
            (
                "https://evermeet.cx/ffmpeg/getrelease/ffprobe/zip",
                vec!["ffprobe"],
            ),
        ]
    }
}

/// Downloads and unpacks; `progress(downloaded, total)` in bytes per file.
pub fn download(cancel: &AtomicBool, mut progress: impl FnMut(u64, Option<u64>)) -> Result<()> {
    if !supported() {
        bail!("install ffmpeg with your package manager");
    }
    let bin = paths::tools_dir().join("bin");
    std::fs::create_dir_all(&bin)?;
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ai-video-upscaler/", env!("CARGO_PKG_VERSION")))
        .timeout(None)
        .build()?;
    for (url, names) in sources() {
        let mut resp = client.get(url).send()?.error_for_status()?;
        let total = resp.content_length();
        let tmp = paths::tools_dir().join("download.zip");
        let mut file = std::fs::File::create(&tmp)?;
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                bail!(Cancelled);
            }
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            done += n as u64;
            progress(done, total);
        }
        drop(file);
        unpack(&tmp, &names, &bin)?;
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

fn unpack(zip_path: &Path, names: &[&str], dest: &Path) -> Result<()> {
    let mut archive =
        zip::ZipArchive::new(std::fs::File::open(zip_path)?).context("not a zip file")?;
    let mut found = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(name) = entry
            .enclosed_name()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        else {
            continue;
        };
        if !entry.is_file() || !names.contains(&name.as_str()) {
            continue;
        }
        let target = dest.join(&name);
        let mut out = std::fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
        }
        found += 1;
    }
    if found < names.len() {
        bail!("the download did not contain {}", names.join(", "));
    }
    Ok(())
}
