//! Child processes: no console window on Windows, cancellable, with a timeout, and with
//! stdout/stderr drained on their own threads so a chatty child never blocks.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

/// A `Command` that opens no console window on Windows.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Starts `cmd` and ties the child to this app: on Windows it goes into a job object that
/// kills everything in it (the child and whatever it starts, such as pip or the Python
/// worker's own helpers) when the app ends, so nothing keeps running and holding the GPU
/// after the window is closed or the app crashes. Every long-running child (FFmpeg, the
/// audio worker, the pack installer, Codex) is started through this.
pub fn spawn(cmd: &mut Command) -> std::io::Result<Child> {
    let child = cmd.spawn()?;
    #[cfg(windows)]
    kill_on_exit::adopt(&child);
    Ok(child)
}

#[cfg(windows)]
mod kill_on_exit {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::OnceLock;

    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// The app's job object (its handle as an integer, so it can live in a static), or 0
    /// when it could not be made. The handle is never closed: the system closes it when the
    /// app's process ends, which is what kills the children.
    fn job() -> usize {
        static JOB: OnceLock<usize> = OnceLock::new();
        *JOB.get_or_init(|| {
            // SAFETY: plain Win32 calls with valid arguments; the info struct is a
            // zero-initialised POD the call only reads.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    log::warn!("could not create a job object for child processes");
                    return 0;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    log::warn!("could not set up the job object for child processes");
                }
                job as usize
            }
        })
    }

    /// Puts `child` into the app's job object (best effort: a failure only means the child
    /// may outlive the app, as before).
    pub fn adopt(child: &Child) {
        let job = job();
        if job == 0 {
            return;
        }
        // SAFETY: both handles are valid for the duration of the call.
        let ok = unsafe { AssignProcessToJobObject(job as _, child.as_raw_handle() as _) };
        if ok == 0 {
            log::warn!(
                "could not tie child process {} to the app: {}",
                child.id(),
                std::io::Error::last_os_error()
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::JobObjects::IsProcessInJob;

        #[test]
        fn children_join_the_job() {
            let mut cmd = crate::process::command("cmd");
            // Runs for about a second, so it is still alive when it is checked.
            cmd.args(["/C", "ping", "-n", "2", "127.0.0.1"])
                .stdout(std::process::Stdio::null());
            let mut child = crate::process::spawn(&mut cmd).unwrap();
            let mut inside = 0;
            // SAFETY: valid handles; `inside` is a valid out pointer.
            let ok = unsafe { IsProcessInJob(child.as_raw_handle() as _, job() as _, &mut inside) };
            let _ = child.wait();
            assert!(ok != 0 && inside != 0, "the child is not in the app's job");
        }
    }
}

pub struct Finished {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Finished {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// The last `n` lines of stderr and stdout, for error messages.
    pub fn tail(&self, n: usize) -> String {
        let text = format!("{}\n{}", self.stderr_text(), self.stdout_text());
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }
}

/// Runs `cmd` to completion, feeding it `stdin`; kills it when `cancel` is set or `timeout` passes.
pub fn run(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    cancel: &AtomicBool,
    timeout: Option<Duration>,
) -> Result<Finished> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = spawn(&mut cmd).with_context(|| format!("could not start {program}"))?;
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        let input = input.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let status = wait(&mut child, cancel, timeout)?;
    Ok(Finished {
        success: status.success(),
        code: status.code(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

/// Waits for `child`, killing it on cancel or timeout.
pub fn wait(
    child: &mut Child,
    cancel: &AtomicBool,
    timeout: Option<Duration>,
) -> Result<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!(Cancelled);
        }
        if timeout.is_some_and(|t| start.elapsed() > t) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("timed out after {} s", start.elapsed().as_secs()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The error a cancelled operation returns; check with [`is_cancelled`].
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Cancelled>().is_some()
}
