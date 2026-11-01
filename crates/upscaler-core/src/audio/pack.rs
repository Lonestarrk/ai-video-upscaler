//! The AI audio pack: embeddable CPython + PyTorch (AMD ROCm for Windows, else CPU) + MSST +
//! the BandIt Plus and Apollo checkpoints, downloaded by the app into
//! `%LOCALAPPDATA%\AI Video Upscaler\tools\audio-ai`.
//!
//! Layout: `python\` (CPython), `msst\` (MSST source), `models\`, `audio_ai.py` (the worker),
//! `downloads\` (only while installing), `.steps-v<N>\<step>` (finished install steps) and
//! `installed.json` (`{"version":N,"device":{...}}`, written last).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};

use super::ai::{self, Device};
use crate::paths;
use crate::process::{self, Cancelled};

/// Bumped when the pack's contents change; an older pack counts as not installed.
pub const PACK_VERSION: u32 = 1;
pub const SCRIPT_NAME: &str = "audio_ai.py";
/// The worker script, written into the pack.
pub const SCRIPT: &str = include_str!("../../assets/audio_ai.py");
const INSTALLED_FILE: &str = "installed.json";
const PYTHON_EXE: &str = "python.exe";

// Verified 2026-11-01 (see "Spike findings" in docs/superpowers/plans/2026-11-01-surround-audio.md).
/// Embeddable CPython 3.12 (AMD's ROCm torch wheels are cp312 only; 3.12.10 is the last
/// 3.12 release with Windows binaries).
const PYTHON_URL: &str = "https://www.python.org/ftp/python/3.12.10/python-3.12.10-embed-amd64.zip";
const PYTHON_PTH: &str = "python312._pth";
const GET_PIP_URL: &str = "https://bootstrap.pypa.io/get-pip.py";
/// AMD "PyTorch on Windows" ROCm 7.2.1 (RN-AMDGPU-WINDOWS-PYTORCH, driver 26.2.2 or newer;
/// RX 7000 / gfx1100 and RX 9000). Installed in this order: SDK first, then torch.
const ROCM_BASE: &str = "https://repo.radeon.com/rocm/windows/rocm-rel-7.2.1/";
/// AMD's guide also installs `rocm_sdk_devel` (headers and tools, 1.2 GB unpacked); running
/// torch does not need it (verified: GPU separation works without it).
const ROCM_SDK: [&str; 3] = [
    "rocm_sdk_core-7.2.1-py3-none-win_amd64.whl",
    "rocm_sdk_libraries_custom-7.2.1-py3-none-win_amd64.whl",
    "rocm-7.2.1.tar.gz",
];
const ROCM_TORCH: [&str; 2] = [
    "torch-2.9.1%2Brocm7.2.1-cp312-cp312-win_amd64.whl",
    "torchaudio-2.9.1%2Brocm7.2.1-cp312-cp312-win_amd64.whl",
];
/// The fallback when ROCm does not install or does not see a GPU.
const CPU_TORCH_INDEX: &str = "https://download.pytorch.org/whl/cpu";
const TORCH_PINS: [&str; 2] = ["torch==2.9.1", "torchaudio==2.9.1"];
/// ZFTurbo/Music-Source-Separation-Training, pinned to a commit of 2026-10-30.
const MSST_URL: &str = "https://github.com/ZFTurbo/Music-Source-Separation-Training/archive/9cc76c03ccd4da5ddf3477e93d386a1648a77732.zip";
/// What MSST's BandIt and Apollo inference imports (its requirements.txt is mostly training
/// tools, several of which do not build on Windows).
const PACKAGES: &[&str] = &[
    "numpy==2.5.4",
    "scipy==1.18.1",
    "soundfile==0.14.0",
    "soxr==1.1.0",
    "librosa==1.0.0",
    "PyYAML==6.0.3",
    "omegaconf==2.2.3",
    "ml_collections==1.1.0",
    "tqdm==4.70.1",
    "einops==0.8.1",
    "beartype==0.14.1",
    "rotary_embedding_torch==0.3.5",
    "pytorch_lightning==2.6.6",
    "torchmetrics==0.11.4",
    "spafe==0.3.2",
    "pyloudnorm==0.2.0",
    "pedalboard==0.9.26",
    "torch_audiomentations==0.12.0",
    "pandas==3.1.0",
    "fire==0.7.1",
    "asteroid-filterbanks==0.4.0",
    "huggingface_hub==2.4.0",
    "torch-optimizer==0.3.0",
    "torch-stoi==0.2.3",
    // torchmetrics 0.11 imports pkg_resources, which setuptools 81 removed.
    "setuptools<81",
];
/// Installed with `--no-deps`: its pb-bss-eval dependency needs a C compiler (pesq), and
/// BandIt only uses its loss functions at import time.
const NO_DEPS_PACKAGES: &[&str] = &["asteroid==0.7.0"];
/// (url, file name in `models\`), from MSST docs/pretrained_models.md.
const MODELS: [(&str, &str); 3] = [
    (
        "https://github.com/ZFTurbo/Music-Source-Separation-Training/releases/download/v.1.0.3/config_dnr_bandit_bsrnn_multi_mus64.yaml",
        "config_dnr_bandit_bsrnn_multi_mus64.yaml",
    ),
    (
        "https://github.com/ZFTurbo/Music-Source-Separation-Training/releases/download/v.1.0.3/model_bandit_plus_dnr_sdr_11.47.chpt",
        "model_bandit_plus_dnr_sdr_11.47.chpt",
    ),
    (
        "https://huggingface.co/JusperLee/Apollo/resolve/main/pytorch_model.bin",
        "apollo_pytorch_model.bin",
    ),
];

/// Stage names passed to the install progress callback (the GUI translates them).
pub mod stage {
    pub const PYTHON: &str = "python";
    pub const PIP: &str = "pip";
    pub const TORCH_ROCM: &str = "torch-rocm";
    pub const TORCH_CPU: &str = "torch-cpu";
    pub const MSST: &str = "msst";
    pub const PACKAGES: &str = "packages";
    pub const MODELS: &str = "models";
    pub const CHECK: &str = "check";
}

pub fn pack_dir() -> PathBuf {
    paths::tools_dir().join("audio-ai")
}

pub fn python_exe(dir: &Path) -> PathBuf {
    dir.join("python").join(PYTHON_EXE)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackStatus {
    pub installed: bool,
    pub device: Option<Device>,
    pub size_bytes: u64,
}

pub fn status() -> PackStatus {
    status_in(&pack_dir())
}

pub fn status_in(dir: &Path) -> PackStatus {
    let device = installed_device(dir);
    PackStatus {
        installed: device.is_some(),
        device,
        size_bytes: dir_size(dir),
    }
}

/// The device recorded at install time, when the pack in `dir` is complete and current.
pub(crate) fn installed_device(dir: &Path) -> Option<Device> {
    if !python_exe(dir).is_file() {
        return None;
    }
    let text = std::fs::read_to_string(dir.join(INSTALLED_FILE)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    if v.get("version")?.as_u64()? != PACK_VERSION as u64 {
        return None;
    }
    // A pack with deleted parts counts as missing, so an export stops before any frame work.
    let parts_present = dir.join("msst").join("utils").join("settings.py").is_file()
        && MODELS
            .iter()
            .all(|(_, name)| dir.join("models").join(name).is_file());
    if !parts_present {
        return None;
    }
    ai::device_from_json(v.get("device")?).ok()
}

/// Writes the embedded worker script into the pack when it differs; returns its path.
pub(crate) fn write_script(dir: &Path) -> Result<PathBuf> {
    let path = dir.join(SCRIPT_NAME);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(SCRIPT) {
        write_atomic(&path, SCRIPT.as_bytes())?;
    }
    Ok(path)
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

pub fn remove() -> Result<()> {
    remove_in(&pack_dir())
}

pub fn remove_in(dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)
            .with_context(|| format!("could not remove {}", dir.display()))?;
    }
    Ok(())
}

/// Downloads and sets up the pack; `progress(stage, overall 0..=1)`. Steps that finished in
/// an earlier attempt are skipped, so a cancelled or failed install continues where it was.
pub fn install(cancel: &AtomicBool, progress: impl FnMut(&str, f32)) -> Result<()> {
    install_in(&pack_dir(), cancel, progress)
}

pub fn install_in(
    dir: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(&str, f32),
) -> Result<()> {
    if !cfg!(windows) {
        bail!("the AI audio pack is only available on Windows");
    }
    std::fs::create_dir_all(dir)?;
    let downloads = dir.join("downloads");
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ai-video-upscaler/", env!("CARGO_PKG_VERSION")))
        .timeout(None)
        .build()?;
    let py = python_exe(dir);
    let script = write_script(dir)?;
    // Overall progress: each stage owns a slice [lo, hi].
    let mut report = |name: &str, lo: f32, hi: f32, f: f32| progress(name, lo + (hi - lo) * f);

    step(dir, "python", || {
        report(stage::PYTHON, 0.0, 0.02, 0.0);
        let zip = downloads.join("python.zip");
        download(&client, PYTHON_URL, &zip, cancel, &mut |f| {
            report(stage::PYTHON, 0.0, 0.02, f)
        })?;
        let target = dir.join("python");
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        zip::ZipArchive::new(std::fs::File::open(&zip)?)
            .context("the Python download is not a zip file")?
            .extract(&target)?;
        let pth = target.join(PYTHON_PTH);
        let text = std::fs::read_to_string(&pth)
            .with_context(|| format!("{} is missing from the Python download", PYTHON_PTH))?;
        std::fs::write(&pth, enable_site(&text))?;
        let _ = std::fs::remove_file(&zip);
        Ok(())
    })?;

    step(dir, "pip", || {
        report(stage::PIP, 0.02, 0.04, 0.0);
        let get_pip = downloads.join("get-pip.py");
        download(&client, GET_PIP_URL, &get_pip, cancel, &mut |_| {})?;
        python(
            &py,
            &[get_pip.as_os_str(), "--no-warn-script-location".as_ref()],
            cancel,
        )
        .context("could not install pip")?;
        // setuptools builds the ROCm meta package (an sdist).
        pip(&py, &["install", "setuptools", "wheel"], cancel)?;
        let _ = std::fs::remove_file(&get_pip);
        Ok(())
    })?;

    step(dir, "torch", || {
        let files = download_rocm_torch(&client, &downloads, cancel, &mut |f| {
            report(stage::TORCH_ROCM, 0.04, 0.45, f)
        })?;
        let rocm = install_rocm_torch(&py, &script, &files, cancel, &mut |f| {
            report(stage::TORCH_ROCM, 0.45, 0.60, f)
        });
        match rocm {
            Ok(Device::Gpu(name)) => {
                log::info!("AI audio pack: ROCm PyTorch sees {name}");
                Ok(())
            }
            Err(e) if process::is_cancelled(&e) => Err(e),
            other => {
                match other {
                    Ok(_) => log::warn!("AI audio pack: ROCm PyTorch sees no GPU; using CPU torch"),
                    Err(e) => {
                        log::warn!("AI audio pack: ROCm PyTorch failed ({e:#}); using CPU torch")
                    }
                }
                report(stage::TORCH_CPU, 0.04, 0.60, 0.5);
                install_cpu_torch(&py, cancel)
            }
        }
    })?;
    let _ = std::fs::remove_dir_all(&downloads);

    step(dir, "msst", || {
        let zip = downloads.join("msst.zip");
        download(&client, MSST_URL, &zip, cancel, &mut |f| {
            report(stage::MSST, 0.60, 0.62, f)
        })?;
        let target = dir.join("msst");
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        unpack_stripped(&zip, &target)?;
        let _ = std::fs::remove_file(&zip);
        Ok(())
    })?;

    step(dir, "packages", || {
        report(stage::PACKAGES, 0.62, 0.80, 0.0);
        let constraints = downloads.join("constraints.txt");
        write_atomic(&constraints, TORCH_PINS.join("\n").as_bytes())?;
        let mut args: Vec<&std::ffi::OsStr> =
            vec!["install".as_ref(), "-c".as_ref(), constraints.as_os_str()];
        args.extend(PACKAGES.iter().map(|p| std::ffi::OsStr::new(*p)));
        pip(&py, &args, cancel).context("could not install the Python packages")?;
        let mut args = vec!["install", "--no-deps"];
        args.extend(NO_DEPS_PACKAGES);
        pip(&py, &args, cancel).context("could not install the Python packages")?;
        Ok(())
    })?;

    step(dir, "models", || {
        let models = dir.join("models");
        let n = MODELS.len() as f32;
        for (i, (url, name)) in MODELS.iter().enumerate() {
            download(&client, url, &models.join(name), cancel, &mut |f| {
                report(stage::MODELS, 0.80, 0.95, (i as f32 + f) / n)
            })?;
        }
        Ok(())
    })?;
    let _ = std::fs::remove_dir_all(&downloads);

    report(stage::CHECK, 0.95, 1.0, 0.0);
    let out = ai::run_worker(&py, &script, &["device"], cancel, &mut |_| {})
        .context("the AI audio pack does not start")?;
    let device = ai::parse_device(&out)?;
    let marker =
        serde_json::json!({"version": PACK_VERSION, "device": ai::device_to_json(&device)});
    write_atomic(&dir.join(INSTALLED_FILE), marker.to_string().as_bytes())?;
    report(stage::CHECK, 0.95, 1.0, 1.0);
    Ok(())
}

/// Downloads AMD's ROCm SDK + torch wheels (a network failure is an error, not a reason to
/// fall back to the CPU).
fn download_rocm_torch(
    client: &reqwest::blocking::Client,
    downloads: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<Vec<PathBuf>> {
    let all: Vec<&str> = ROCM_SDK.iter().chain(ROCM_TORCH.iter()).copied().collect();
    let n = all.len() as f32;
    let mut files = Vec::new();
    for (i, name) in all.iter().enumerate() {
        let local = downloads.join(name.replace("%2B", "+"));
        download(
            client,
            &format!("{ROCM_BASE}{name}"),
            &local,
            cancel,
            &mut |f| progress((i as f32 + f) / n),
        )?;
        files.push(local);
    }
    Ok(files)
}

/// Installs the downloaded ROCm wheels; returns the device torch then reports.
fn install_rocm_torch(
    py: &Path,
    script: &Path,
    files: &[PathBuf],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<Device> {
    let (sdk, torch) = files.split_at(ROCM_SDK.len());
    let mut args: Vec<&std::ffi::OsStr> = vec!["install".as_ref(), "--no-cache-dir".as_ref()];
    args.extend(sdk.iter().map(|p| p.as_os_str()));
    pip(py, &args, cancel).context("could not install the ROCm SDK")?;
    progress(0.5);
    let mut args: Vec<&std::ffi::OsStr> = vec!["install".as_ref(), "--no-cache-dir".as_ref()];
    args.extend(torch.iter().map(|p| p.as_os_str()));
    pip(py, &args, cancel).context("could not install ROCm PyTorch")?;
    progress(0.9);
    let out = ai::run_worker(py, script, &["device"], cancel, &mut |_| {})?;
    let device = ai::parse_device(&out)?;
    progress(1.0);
    Ok(device)
}

fn install_cpu_torch(py: &Path, cancel: &AtomicBool) -> Result<()> {
    // Whatever the ROCm attempt left behind goes first, so pip does not keep it.
    let _ = pip(
        py,
        &[
            "uninstall",
            "-y",
            "torch",
            "torchaudio",
            "rocm",
            "rocm-sdk-core",
            "rocm-sdk-devel",
            "rocm-sdk-libraries-custom",
        ],
        cancel,
    );
    if cancel.load(Ordering::Relaxed) {
        bail!(Cancelled);
    }
    let mut args = vec![
        "install",
        "--index-url",
        CPU_TORCH_INDEX,
        "--extra-index-url",
        "https://pypi.org/simple",
    ];
    args.extend(TORCH_PINS);
    pip(py, &args, cancel).context("could not install PyTorch for the CPU")?;
    Ok(())
}

fn pip<S: AsRef<std::ffi::OsStr>>(py: &Path, args: &[S], cancel: &AtomicBool) -> Result<()> {
    let mut full: Vec<&std::ffi::OsStr> = vec![
        "-m".as_ref(),
        "pip".as_ref(),
        "--disable-pip-version-check".as_ref(),
        "--no-input".as_ref(),
    ];
    full.extend(args.iter().map(|a| a.as_ref()));
    full.push("--no-warn-script-location".as_ref());
    if args.first().map(|a| a.as_ref()) == Some("uninstall".as_ref()) {
        full.pop();
    }
    python(py, &full, cancel)
}

fn python<S: AsRef<std::ffi::OsStr>>(py: &Path, args: &[S], cancel: &AtomicBool) -> Result<()> {
    let mut cmd = process::command(py);
    cmd.args(args)
        .env("PYTHONIOENCODING", "utf-8")
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONHOME");
    let fin = process::run(cmd, None, cancel, None)?;
    if !fin.success {
        bail!("{}", fin.tail(8));
    }
    Ok(())
}

/// Downloads `url` to `dest` through `dest.part`; an existing `dest` counts as done.
/// `progress` gets 0..=1 when the size is known.
fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<()> {
    if dest.is_file() {
        progress(1.0);
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part = part_path(dest);
    let result = (|| -> Result<()> {
        let mut resp = client
            .get(url)
            .send()
            .with_context(|| format!("could not download {url}"))?
            .error_for_status()
            .with_context(|| format!("could not download {url}"))?;
        let total = resp.content_length();
        let mut file = std::fs::File::create(&part)?;
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!(Cancelled);
            }
            let n = resp
                .read(&mut buf)
                .with_context(|| format!("the download of {url} broke off"))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            done += n as u64;
            if let Some(t) = total.filter(|&t| t > 0) {
                progress((done as f32 / t as f32).min(1.0));
            }
        }
        if let Some(t) = total {
            if done != t {
                bail!("the download of {url} is incomplete ({done} of {t} bytes)");
            }
        }
        file.flush()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, dest).with_context(|| format!("could not finish {}", dest.display()))?;
    Ok(())
}

fn part_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part = part_path(path);
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, path)?;
    Ok(())
}

/// `python3XX._pth` with `import site` enabled, so pip's site-packages is used.
fn enable_site(pth: &str) -> String {
    pth.lines()
        .map(|l| {
            if l.trim() == "#import site" {
                "import site"
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
}

/// Unpacks every file of `zip_path` into `dest`, dropping the archive's single top folder.
fn unpack_stripped(zip_path: &Path, dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip_path)?)
        .context("the download is not a zip file")?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let rel: PathBuf = name.components().skip(1).collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

/// Runs `f` unless step `name` is already marked done in `dir`; marks it done on success.
fn step(dir: &Path, name: &str, f: impl FnOnce() -> Result<()>) -> Result<()> {
    // Per pack version, so a newer pack redoes every step.
    let marker = dir.join(format!(".steps-v{PACK_VERSION}")).join(name);
    if marker.is_file() {
        return Ok(());
    }
    f()?;
    write_atomic(&marker, b"done")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fake_pack(dir: &Path, device_json: &str) {
        std::fs::create_dir_all(dir.join("python")).unwrap();
        std::fs::write(dir.join("python").join(PYTHON_EXE), b"exe").unwrap();
        std::fs::write(dir.join(SCRIPT_NAME), b"print()").unwrap();
        std::fs::create_dir_all(dir.join("msst").join("utils")).unwrap();
        std::fs::write(dir.join("msst").join("utils").join("settings.py"), b"").unwrap();
        std::fs::create_dir_all(dir.join("models")).unwrap();
        for (_, name) in MODELS {
            std::fs::write(dir.join("models").join(name), b"weights").unwrap();
        }
        std::fs::write(
            dir.join(INSTALLED_FILE),
            format!(r#"{{"version":{PACK_VERSION},"device":{device_json}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn empty_dir_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let st = status_in(dir.path());
        assert_eq!(
            st,
            PackStatus {
                installed: false,
                device: None,
                size_bytes: 0
            }
        );
        let st = status_in(&dir.path().join("does-not-exist"));
        assert!(!st.installed);
    }

    #[test]
    fn installed_pack_reports_device_and_size() {
        let dir = tempfile::tempdir().unwrap();
        fake_pack(
            dir.path(),
            r#"{"device":"gpu","name":"AMD Radeon RX 7900 XTX"}"#,
        );
        let st = status_in(dir.path());
        assert!(st.installed);
        assert_eq!(
            st.device,
            Some(Device::Gpu("AMD Radeon RX 7900 XTX".into()))
        );
        assert!(st.size_bytes > 0);
    }

    #[test]
    fn old_pack_version_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        fake_pack(dir.path(), r#"{"device":"cpu"}"#);
        std::fs::write(
            dir.path().join(INSTALLED_FILE),
            r#"{"version":0,"device":{"device":"cpu"}}"#,
        )
        .unwrap();
        assert!(!status_in(dir.path()).installed);
    }

    #[test]
    fn pack_with_a_missing_model_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        fake_pack(dir.path(), r#"{"device":"cpu"}"#);
        assert!(status_in(dir.path()).installed);
        std::fs::remove_file(dir.path().join("models").join(MODELS[1].1)).unwrap();
        assert!(!status_in(dir.path()).installed);
        assert!(super::super::ai::LocalAi::open_in(dir.path()).is_err());
    }

    #[test]
    fn remove_deletes_the_pack() {
        let dir = tempfile::tempdir().unwrap();
        let pack = dir.path().join("audio-ai");
        fake_pack(&pack, r#"{"device":"cpu"}"#);
        remove_in(&pack).unwrap();
        assert!(!pack.exists());
        // Removing a missing pack is fine.
        remove_in(&pack).unwrap();
    }

    #[test]
    fn enables_site_in_pth() {
        let pth = "python312.zip\r\n.\r\n\r\n# Uncomment to run site.main() automatically\r\n#import site\r\n";
        let out = enable_site(pth);
        assert!(out.lines().any(|l| l.trim() == "import site"), "{out}");
        assert!(!out.contains("#import site"), "{out}");
        assert!(out.lines().any(|l| l.trim() == "python312.zip"));
        // Already enabled stays as it is.
        assert_eq!(enable_site(&out), out);
    }

    #[test]
    fn unpacks_zip_stripping_top_folder() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("src.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            w.add_directory("repo-abc/", opts).unwrap();
            w.start_file("repo-abc/inference.py", opts).unwrap();
            w.write_all(b"main").unwrap();
            w.start_file("repo-abc/utils/settings.py", opts).unwrap();
            w.write_all(b"settings").unwrap();
            w.finish().unwrap();
        }
        let dest = dir.path().join("msst");
        unpack_stripped(&zip_path, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("inference.py")).unwrap(), b"main");
        assert_eq!(
            std::fs::read(dest.join("utils").join("settings.py")).unwrap(),
            b"settings"
        );
    }

    #[test]
    fn done_steps_are_skipped_and_failed_steps_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let mut calls = 0;
        step(dir.path(), "models", || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        step(dir.path(), "models", || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 1);

        assert!(step(dir.path(), "torch", || anyhow::bail!("network")).is_err());
        step(dir.path(), "torch", || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 2);
    }

    #[test]
    fn script_is_embedded() {
        assert!(SCRIPT.contains("PROGRESS"));
        assert!(SCRIPT.contains("model_bandit_plus_dnr_sdr_11.47.chpt"));
    }
}
