//! Local AI for the audio job: separation into speech / music / effects (BandIt Plus) and
//! lossy-codec repair (Apollo), run by a Python worker from the downloaded AI audio pack.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use super::pack;
use crate::process::{self, Cancelled};

/// The three stems of a separation, 48 kHz float WAVs with the input's channel count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stems {
    pub speech: PathBuf,
    pub music: PathBuf,
    pub effects: PathBuf,
}

pub trait AudioAi: Send + Sync {
    /// Splits `wav` into `speech.wav`, `music.wav` and `effects.wav` in `out_dir`.
    fn separate(
        &self,
        wav: &Path,
        out_dir: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f32),
    ) -> Result<Stems>;

    /// Repairs lossy-codec artefacts in `wav`, writing `out` (same length and channels).
    fn enhance(
        &self,
        wav: &Path,
        out: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f32),
    ) -> Result<()>;
}

/// Where the worker runs its models.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Device {
    /// A GPU, with the name the driver reports.
    Gpu(String),
    Cpu,
}

/// `PROGRESS 0.42` from the worker's stdout, clamped to 0..=1.
pub fn parse_progress(line: &str) -> Option<f32> {
    let value: f32 = line.trim().strip_prefix("PROGRESS ")?.trim().parse().ok()?;
    if value.is_nan() {
        return None;
    }
    Some(value.clamp(0.0, 1.0))
}

/// The worker's `device` answer: `{"device":"gpu","name":"..."}` or `{"device":"cpu"}`.
/// Anything printed before it (library warnings) is ignored; the last JSON line counts.
pub fn parse_device(text: &str) -> Result<Device> {
    let line = text
        .lines()
        .map(str::trim)
        .rfind(|l| l.starts_with('{'))
        .ok_or_else(|| anyhow!("the AI audio pack gave no device answer: {}", text.trim()))?;
    let v: serde_json::Value = serde_json::from_str(line)
        .with_context(|| format!("the AI audio pack gave an unreadable device answer: {line}"))?;
    device_from_json(&v)
}

pub(crate) fn device_from_json(v: &serde_json::Value) -> Result<Device> {
    match v.get("device").and_then(|d| d.as_str()) {
        Some("gpu") => Ok(Device::Gpu(
            v.get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("GPU")
                .to_string(),
        )),
        Some("cpu") => Ok(Device::Cpu),
        _ => bail!("unknown device in the AI audio pack's answer: {v}"),
    }
}

pub(crate) fn device_to_json(d: &Device) -> serde_json::Value {
    match d {
        Device::Gpu(name) => serde_json::json!({"device": "gpu", "name": name}),
        Device::Cpu => serde_json::json!({"device": "cpu"}),
    }
}

/// The error text when the pack is missing and nothing says what it was wanted for.
pub const PACK_MISSING: &str = "The local AI audio pack is not installed (or is incomplete). \
     Install it with the AI audio pack button in Settings.";
/// The pack is missing and the AI separation (5.1 with the AI engine) needs it.
pub const PACK_MISSING_SEPARATION: &str =
    "The local AI audio pack is not installed (or is incomplete). \
     Install it with the AI audio pack button in Settings, or choose the classic 5.1 engine.";
/// The pack is missing and only the enhancement needs it.
pub const PACK_MISSING_ENHANCE: &str =
    "The local AI audio pack is not installed (or is incomplete). \
     Install it with the AI audio pack button in Settings, or turn off \"Improve the audio quality\".";
/// The pack is missing and both the AI separation and the enhancement need it.
pub const PACK_MISSING_BOTH: &str = "The local AI audio pack is not installed (or is incomplete). \
     Install it with the AI audio pack button in Settings, or choose the classic 5.1 engine \
     and turn off \"Improve the audio quality\".";

/// The "pack missing" text for a job that uses the AI to separate and/or to enhance.
pub fn pack_missing_message(separation: bool, enhance: bool) -> &'static str {
    match (separation, enhance) {
        (true, true) => PACK_MISSING_BOTH,
        (true, false) => PACK_MISSING_SEPARATION,
        (false, true) => PACK_MISSING_ENHANCE,
        (false, false) => PACK_MISSING,
    }
}

/// The AI audio pack's Python worker.
#[derive(Clone, Debug)]
pub struct LocalAi {
    pub python: PathBuf,
    pub script: PathBuf,
    pub device: Device,
}

impl LocalAi {
    /// Opens the installed pack in `%LOCALAPPDATA%\AI Video Upscaler\tools\audio-ai`.
    pub fn open() -> Result<LocalAi> {
        Self::open_in(&pack::pack_dir())
    }

    /// Opens the pack in `dir`; a clear error when it is missing or incomplete. Refreshes the
    /// worker script when this build of the app carries a different one.
    pub fn open_in(dir: &Path) -> Result<LocalAi> {
        let Some(device) = pack::installed_device(dir) else {
            bail!(PACK_MISSING);
        };
        let script = pack::write_script(dir)?;
        Ok(LocalAi {
            python: pack::python_exe(dir),
            script,
            device,
        })
    }

    /// Asks the worker which device it would use now (starts Python and imports torch).
    pub fn probe_device(&self, cancel: &AtomicBool) -> Result<Device> {
        let out = self.run(&["device"], cancel, &mut |_| {})?;
        parse_device(&out)
    }

    /// Runs the worker with `args`, forwarding its progress; returns its stdout.
    fn run<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f32),
    ) -> Result<String> {
        run_worker(&self.python, &self.script, args, cancel, progress)
    }
}

impl AudioAi for LocalAi {
    fn separate(
        &self,
        wav: &Path,
        out_dir: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f32),
    ) -> Result<Stems> {
        std::fs::create_dir_all(out_dir)?;
        self.run(
            &[OsStr::new("separate"), wav.as_os_str(), out_dir.as_os_str()],
            cancel,
            progress,
        )?;
        let stems = Stems {
            speech: out_dir.join("speech.wav"),
            music: out_dir.join("music.wav"),
            effects: out_dir.join("effects.wav"),
        };
        for p in [&stems.speech, &stems.music, &stems.effects] {
            if !p.is_file() {
                bail!("the AI separation did not write {}", p.display());
            }
        }
        Ok(stems)
    }

    fn enhance(
        &self,
        wav: &Path,
        out: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f32),
    ) -> Result<()> {
        self.run(
            &[OsStr::new("enhance"), wav.as_os_str(), out.as_os_str()],
            cancel,
            progress,
        )?;
        if !out.is_file() {
            bail!("the AI enhancement did not write {}", out.display());
        }
        Ok(())
    }
}

/// Runs `python script args...`: progress lines from stdout go to `progress`, the child is
/// killed on cancel, and a failure carries the end of stderr.
pub(crate) fn run_worker<S: AsRef<OsStr>>(
    python: &Path,
    script: &Path,
    args: &[S],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<String> {
    let mut cmd = process::command(python);
    cmd.arg("-u")
        .arg(script)
        .args(args)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUNBUFFERED", "1")
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONHOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process::spawn(&mut cmd)
        .with_context(|| format!("could not start {}", python.display()))?;

    let (tx, rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take();
    let out_thread = std::thread::spawn(move || {
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        }
    });
    let stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        let mut tail: Vec<String> = Vec::new();
        if let Some(err) = stderr {
            for line in BufReader::new(err).lines() {
                let Ok(line) = line else { break };
                tail.push(line);
                if tail.len() > 200 {
                    tail.remove(0);
                }
            }
        }
        tail
    });

    let mut stdout_text = String::new();
    let mut handle = |line: String, text: &mut String| {
        if let Some(p) = parse_progress(&line) {
            progress(p);
        } else {
            text.push_str(&line);
            text.push('\n');
        }
    };
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!(Cancelled);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => handle(line, &mut stdout_text),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // stdout closed: the worker is exiting.
                break process::wait(&mut child, cancel, None)?;
            }
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
    };
    let _ = out_thread.join();
    for line in rx.try_iter() {
        handle(line, &mut stdout_text);
    }
    let tail = err_thread.join().unwrap_or_default();
    if !status.success() {
        bail!("the AI audio worker failed: {}", error_tail(&tail));
    }
    Ok(stdout_text)
}

/// The worker's `ERROR:` line if it printed one, else the last lines of stderr.
fn error_tail(lines: &[String]) -> String {
    let lines: Vec<&str> = lines
        .iter()
        .map(|l| l.trim_end())
        .filter(|l| !l.trim().is_empty())
        .collect();
    let message = lines.iter().rev().find_map(|l| l.strip_prefix("ERROR: "));
    // The ERROR: line leads the text, so the tail leaves it out.
    let rest: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| message.is_none() || !l.starts_with("ERROR: "))
        .collect();
    let tail = rest[rest.len().saturating_sub(8)..].join("\n");
    match message {
        Some(msg) if tail.is_empty() => msg.to_string(),
        Some(msg) => format!("{msg}\n{tail}"),
        None if tail.is_empty() => "no error output".into(),
        None => tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_tail_names_the_error_once() {
        let lines: Vec<String> = [
            "Traceback (most recent call last):",
            "  File x",
            "ERROR: boom",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let text = error_tail(&lines);
        assert_eq!(text.matches("boom").count(), 1, "{text}");
        assert!(text.starts_with("boom"), "{text}");
        assert!(text.contains("Traceback"), "{text}");
    }

    #[test]
    fn parses_progress_lines() {
        assert_eq!(parse_progress("PROGRESS 0.42"), Some(0.42));
        assert_eq!(parse_progress("PROGRESS 0"), Some(0.0));
        assert_eq!(parse_progress("PROGRESS 1\r"), Some(1.0));
        assert_eq!(parse_progress("  PROGRESS 0.5  "), Some(0.5));
        assert_eq!(parse_progress("PROGRESS 1.7"), Some(1.0));
        assert_eq!(parse_progress("PROGRESS -0.2"), Some(0.0));
        assert_eq!(parse_progress("PROGRESS abc"), None);
        assert_eq!(parse_progress("PROGRESS NaN"), None);
        assert_eq!(parse_progress("Using device: cuda:0"), None);
        assert_eq!(parse_progress(""), None);
    }

    #[test]
    fn parses_device_json() {
        assert_eq!(
            parse_device(r#"{"device":"gpu","name":"AMD Radeon RX 7900 XTX"}"#).unwrap(),
            Device::Gpu("AMD Radeon RX 7900 XTX".into())
        );
        assert_eq!(parse_device(r#"{"device":"cpu"}"#).unwrap(), Device::Cpu);
        // Torch may print warnings before the answer: the last JSON line counts.
        assert_eq!(
            parse_device("UserWarning: something\n{\"device\":\"cpu\"}\n").unwrap(),
            Device::Cpu
        );
        assert!(parse_device("not json").is_err());
        assert!(parse_device(r#"{"device":"tpu"}"#).is_err());
    }

    #[test]
    fn missing_pack_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = LocalAi::open_in(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not installed"), "{err}");
        assert!(err.contains("Settings"), "{err}");
        assert!(err.contains("button"), "{err}");
    }
}
