//! ChatGPT on a ChatGPT plan (Plus/Pro/Business), without API costs.
//!
//! OpenAI's official Codex CLI signs in with a ChatGPT account (`codex login`) and its
//! built-in image generation (gpt-image) counts against the plan's limits instead of API
//! billing. Each frame is one non-interactive `codex exec` turn: the frame (and the previous
//! restored frame) are attached as images and copied into a private folder, Codex is told to
//! edit the frame with its image tool and save the result there as `output.png`.
//! When it does not copy the file, the newest image in `$CODEX_HOME/generated_images` that
//! appeared during the turn is used.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Result};
use image::RgbImage;

use super::{Provider, Request, ReviewRequest};
use crate::process::{self, command};
use crate::review::Verdict;
use crate::settings::Settings;

pub struct Codex {
    path: String,
    model: String,
    reasoning: String,
    sandbox: String,
    timeout: Duration,
}

/// Fallback results already handed to a frame, so parallel turns never take the same file.
static CLAIMED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// Finds the Codex CLI: the configured path, PATH, then the usual npm/Homebrew folders.
pub fn locate(configured: &str) -> Option<PathBuf> {
    let c = configured.trim();
    if !c.is_empty() {
        let p = PathBuf::from(c);
        return p.is_file().then_some(p);
    }
    if let Ok(p) = which::which("codex") {
        return Some(p);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        if let Some(appdata) = dirs::data_dir() {
            candidates.push(appdata.join("npm").join("codex.cmd"));
        }
        if let Some(local) = dirs::data_local_dir() {
            candidates.push(local.join("Programs").join("codex").join("codex.exe"));
        }
    } else {
        candidates.push("/opt/homebrew/bin/codex".into());
        candidates.push("/usr/local/bin/codex".into());
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(".npm-global/bin/codex"));
            candidates.push(home.join(".local/bin/codex"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"))
}

fn base_command(exe: &Path) -> std::process::Command {
    let mut cmd = command(exe);
    // With an API key in the environment Codex may bill the API instead of the plan.
    cmd.env_remove("OPENAI_API_KEY").env_remove("CODEX_API_KEY");
    cmd
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoginState {
    NotInstalled,
    LoggedInChatGpt(String),
    LoggedInApiKey(String),
    LoggedOut(String),
}

/// Runs `codex login status`.
pub fn login_status(configured: &str) -> LoginState {
    let Some(exe) = locate(configured) else {
        return LoginState::NotInstalled;
    };
    let mut cmd = base_command(&exe);
    cmd.args(["login", "status"]);
    match process::run(
        cmd,
        None,
        &AtomicBool::new(false),
        Some(Duration::from_secs(30)),
    ) {
        Ok(out) => {
            let text = format!("{}\n{}", out.stdout_text(), out.stderr_text())
                .trim()
                .to_string();
            let lower = text.to_lowercase();
            if out.success && lower.contains("chatgpt") {
                LoginState::LoggedInChatGpt(text)
            } else if out.success && lower.contains("api key") {
                LoginState::LoggedInApiKey(text)
            } else {
                LoginState::LoggedOut(text)
            }
        }
        Err(e) => LoginState::LoggedOut(e.to_string()),
    }
}

/// Runs `codex login`, which opens the browser for the ChatGPT sign-in and returns once it is done.
pub fn login(configured: &str, cancel: &AtomicBool) -> Result<String> {
    let exe = locate(configured).ok_or_else(|| anyhow!("Codex CLI was not found"))?;
    let mut cmd = base_command(&exe);
    cmd.arg("login");
    let out = process::run(cmd, None, cancel, Some(Duration::from_secs(900)))?;
    if !out.success {
        bail!("{}", out.tail(6));
    }
    Ok(out.tail(3))
}

pub fn logout(configured: &str) -> Result<()> {
    let exe = locate(configured).ok_or_else(|| anyhow!("Codex CLI was not found"))?;
    let mut cmd = base_command(&exe);
    cmd.arg("logout");
    process::run(
        cmd,
        None,
        &AtomicBool::new(false),
        Some(Duration::from_secs(30)),
    )?;
    Ok(())
}

/// Installs the Codex CLI with npm (`npm install -g @openai/codex`).
pub fn install_with_npm(cancel: &AtomicBool) -> Result<String> {
    let npm = which::which("npm").map_err(|_| anyhow!("npm (Node.js) was not found"))?;
    let mut cmd = command(npm);
    cmd.args(["install", "-g", "@openai/codex"]);
    let out = process::run(cmd, None, cancel, Some(Duration::from_secs(600)))?;
    if !out.success {
        bail!("{}", out.tail(8));
    }
    Ok(out.tail(3))
}

pub fn npm_available() -> bool {
    which::which("npm").is_ok()
}

impl Codex {
    pub fn from_settings(s: &Settings) -> Self {
        Codex {
            path: s.codex_path.clone(),
            model: s.codex_model.trim().to_string(),
            reasoning: s.codex_reasoning.trim().to_string(),
            sandbox: if s.codex_sandbox.trim().is_empty() {
                "workspace-write".into()
            } else {
                s.codex_sandbox.trim().into()
            },
            timeout: Duration::from_secs(s.codex_timeout_secs.max(60)),
        }
    }
}

/// The agent instruction around the image prompt.
pub fn agent_prompt(image_prompt: &str, has_reference: bool, size: (u32, u32)) -> String {
    let (w, h) = size;
    let ratio = w as f64 / h.max(1) as f64;
    let inputs = if has_reference {
        "the attached image `input.png` (the frame to edit) together with the attached reference image `reference.png`"
    } else {
        "the attached image `input.png`"
    };
    let grey = if crate::prompt::has_extension_task(image_prompt) {
        "The output must contain no flat grey areas: every grey area of input.png has to be painted over with new scenery. "
    } else {
        ""
    };
    format!(
        "You are the image back end of a video upscaling app. Do exactly this, without asking questions:\n\
         1. Use your built-in image generation tool exactly once to edit {inputs} (both are also in the current directory), following the IMAGE INSTRUCTIONS below. input.png is {w}x{h} pixels (aspect ratio {ratio:.3}:1). Ask for the largest output size the tool supports with exactly that aspect ratio; the result must have the same shape as input.png, without added borders or bars.\n\
         2. Save the generated image in the current working directory as `output.png`. If the tool stored it elsewhere, copy that file here as `output.png`.\n\
         Do not write scripts or use Python, ImageMagick or any other program to change the picture, and do not create other files. \
         {grey}When output.png exists, reply only with DONE.\n\n\
         === IMAGE INSTRUCTIONS ===\n{image_prompt}\n=== END OF IMAGE INSTRUCTIONS ===\n"
    )
}

fn is_image(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "webp")
    )
}

/// The newest image below `dir` modified at or after `since`.
fn newest_image_since(dir: &Path, since: SystemTime, skip: &HashSet<PathBuf>) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(p);
            } else if is_image(&p) && !skip.contains(&p) {
                let t = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                if t >= since && best.as_ref().is_none_or(|(bt, _)| t > *bt) {
                    best = Some((t, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Short, readable reason from Codex's output.
fn explain_failure(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.contains("usage limit") || lower.contains("rate limit") || lower.contains("quota") {
        return format!(
            "The ChatGPT plan's limit is reached for now (try again later).\n{}",
            last_lines(text, 4)
        );
    }
    if lower.contains("not logged in")
        || lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("login") && lower.contains("required")
    {
        return format!(
            "Codex is not signed in: use \"Sign in with ChatGPT\" in the settings.\n{}",
            last_lines(text, 4)
        );
    }
    last_lines(text, 8)
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

impl Provider for Codex {
    fn canvas_aspects(&self) -> &'static [f64] {
        &[] // gpt-image-2 takes free sizes; black bars made it return a reframed picture
    }

    fn upscale(&self, req: &Request, cancel: &AtomicBool) -> Result<RgbImage> {
        let exe = locate(&self.path)
            .ok_or_else(|| anyhow!("Codex CLI was not found: install it in the settings"))?;
        let dir = req.work_dir;
        let input = dir.join("input.png");
        let output = dir.join("output.png");
        let _ = std::fs::remove_file(&output);
        req.image.save(&input)?;
        let reference = match req.reference {
            Some(r) => {
                let p = dir.join("reference.png");
                r.save(&p)?;
                Some(p)
            }
            None => None,
        };
        let started = SystemTime::now() - Duration::from_secs(2);

        let mut cmd = base_command(&exe);
        cmd.current_dir(dir)
            .arg("exec")
            .arg("--skip-git-repo-check")
            .args(["--color", "never"])
            .args(["--sandbox", &self.sandbox])
            .arg("--cd")
            .arg(dir)
            .arg(format!("--image={}", input.display()));
        if let Some(r) = &reference {
            cmd.arg(format!("--image={}", r.display()));
        }
        if !self.model.is_empty() {
            cmd.args(["-m", &self.model]);
        }
        if !self.reasoning.is_empty() {
            cmd.arg("-c")
                .arg(format!("model_reasoning_effort={}", self.reasoning));
        }
        cmd.arg("-");
        let instruction = agent_prompt(req.prompt, reference.is_some(), req.image.dimensions());
        log::info!("codex exec in {}", dir.display());
        let out = process::run(
            cmd,
            Some(instruction.as_bytes()),
            cancel,
            Some(self.timeout),
        )?;
        let transcript = format!("{}\n{}", out.stderr_text(), out.stdout_text());
        let _ = std::fs::write(dir.join("codex-output.txt"), &transcript);

        let mut claimed = CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
        let claimed = claimed.get_or_insert_with(HashSet::new);
        let mut skip: HashSet<PathBuf> = claimed.clone();
        skip.insert(input.clone());
        if let Some(r) = &reference {
            skip.insert(r.clone());
        }
        let found = if output.is_file() {
            Some(output.clone())
        } else {
            newest_image_since(dir, started, &skip).or_else(|| {
                newest_image_since(&codex_home().join("generated_images"), started, &skip)
            })
        };
        let Some(path) = found else {
            if !out.success {
                bail!("Codex failed: {}", explain_failure(&transcript));
            }
            bail!(
                "Codex finished without producing an image. Last output:\n{}",
                explain_failure(&transcript)
            );
        };
        claimed.insert(path.clone());
        let img = image::open(&path)
            .map_err(|e| anyhow!("could not read {}: {e}", path.display()))?
            .to_rgb8();
        log::info!(
            "codex result {} ({}x{})",
            path.display(),
            img.width(),
            img.height()
        );
        Ok(img)
    }

    fn can_review(&self) -> bool {
        true
    }

    fn review(&self, req: &ReviewRequest, cancel: &AtomicBool) -> Result<Verdict> {
        let exe = locate(&self.path)
            .ok_or_else(|| anyhow!("Codex CLI was not found: install it in the settings"))?;
        let dir = req.work_dir;
        std::fs::create_dir_all(dir)?;
        // Small copies are enough to judge and keep the turn quick.
        let mut images = vec![("original.png", req.source), ("result.png", req.result)];
        if let Some(a) = req.result_area {
            images.push(("result-source-area.png", a));
        }
        if let Some(p) = req.previous {
            images.push(("previous.png", p));
        }
        let mut cmd = base_command(&exe);
        cmd.current_dir(dir)
            .arg("exec")
            .arg("--skip-git-repo-check")
            .arg("--ephemeral")
            .args(["--color", "never"])
            .args(["--sandbox", "read-only"])
            .arg("--cd")
            .arg(dir);
        for (name, img) in &images {
            let p = dir.join(name);
            crate::frame::limit_size(img, 1280).save(&p)?;
            cmd.arg(format!("--image={}", p.display()));
        }
        let schema = dir.join("verdict-schema.json");
        std::fs::write(&schema, crate::review::SCHEMA)?;
        let answer = dir.join("verdict.json");
        cmd.arg("--output-schema")
            .arg(&schema)
            .arg("-o")
            .arg(&answer);
        if !self.model.is_empty() {
            cmd.args(["-m", &self.model]);
        }
        // Judging framing and detail needs more care than driving the image tool.
        cmd.arg("-c").arg("model_reasoning_effort=medium");
        cmd.arg("-");
        log::info!("codex review in {}", dir.display());
        let out = process::run(
            cmd,
            Some(req.instruction.as_bytes()),
            cancel,
            Some(self.timeout),
        )?;
        let transcript = format!("{}\n{}", out.stderr_text(), out.stdout_text());
        let _ = std::fs::write(dir.join("codex-output.txt"), &transcript);
        let text = std::fs::read_to_string(&answer).unwrap_or_default();
        if text.trim().is_empty() {
            bail!("Codex review failed: {}", explain_failure(&transcript));
        }
        crate::review::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_names_files() {
        let p = agent_prompt("Restore it.", true, (1032, 432));
        assert!(
            p.contains("reference.png") && p.contains("output.png") && p.contains("Restore it.")
        );
        assert!(p.contains("1032x432") && p.contains("2.389:1"));
        assert!(!agent_prompt("x", false, (640, 480)).contains("reference.png"));
        assert!(!p.contains("no flat grey"));
        let task = crate::prompt::build(&crate::prompt::PromptParts {
            user_prompt: "x",
            padded: false,
            has_reference: false,
            extend: Some(("16:9", true)),
            feedback: None,
        });
        assert!(agent_prompt(&task, false, (640, 480)).contains("no flat grey"));
    }

    #[test]
    fn finds_newest_new_image() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("a.png");
        std::fs::write(&old, b"x").unwrap();
        let since = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        let sub = dir.path().join("sess");
        std::fs::create_dir(&sub).unwrap();
        let new = sub.join("ig_1.png");
        std::fs::write(&new, b"x").unwrap();
        std::fs::write(sub.join("notes.txt"), b"x").unwrap();
        assert_eq!(
            newest_image_since(dir.path(), since, &HashSet::new()),
            Some(new.clone())
        );
        let skip: HashSet<PathBuf> = [new].into_iter().collect();
        assert_eq!(newest_image_since(dir.path(), since, &skip), None);
    }

    #[test]
    fn limit_errors_are_explained() {
        assert!(explain_failure("ERROR: You've hit your usage limit").contains("limit"));
    }
}
