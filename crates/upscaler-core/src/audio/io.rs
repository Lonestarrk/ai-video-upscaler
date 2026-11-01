//! Audio in and out of the audio job: extraction from the source with FFmpeg, WAV files,
//! and a streaming encoder that pipes raw float samples into FFmpeg.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::ffmpeg::Tools;
use crate::process::{self, Cancelled};

/// Sample rate inside the audio job.
pub const RATE: u32 = 48_000;

/// Interleaved 32-bit float audio.
#[derive(Clone, Debug, PartialEq)]
pub struct Pcm {
    pub rate: u32,
    pub channels: usize,
    pub samples: Vec<f32>,
}

impl Pcm {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels
        }
    }

    pub fn channel(&self, i: usize) -> Vec<f32> {
        self.samples
            .iter()
            .skip(i)
            .step_by(self.channels)
            .copied()
            .collect()
    }

    /// Interleaves `chans`; shorter channels are padded with silence to the longest.
    pub fn from_channels(rate: u32, chans: &[Vec<f32>]) -> Pcm {
        let n = chans.iter().map(Vec::len).max().unwrap_or(0);
        let mut samples = Vec::with_capacity(n * chans.len());
        for i in 0..n {
            for c in chans {
                samples.push(c.get(i).copied().unwrap_or(0.0));
            }
        }
        Pcm {
            rate,
            channels: chans.len(),
            samples,
        }
    }
}

/// Number of frames at 48 kHz for `dur` seconds: `round(dur * 48000)`.
pub fn frames_for(dur: f64) -> usize {
    (dur * RATE as f64).round().max(0.0) as usize
}

fn part_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Extracts the first audio stream of `src` from `start` for `dur` seconds as a 48 kHz float
/// WAV with the source's channel count, exactly `round(dur * 48000)` frames long (padded with
/// silence or trimmed). Written to `*.part` and renamed.
pub fn extract(
    tools: &Tools,
    src: &Path,
    start: f64,
    dur: f64,
    out_wav: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    run_extract(
        tools,
        src,
        start,
        dur,
        out_wav,
        &["-c:a", "pcm_f32le", "-f", "wav"],
        cancel,
    )
}

/// Like [`extract`], but writes headerless interleaved f32le with exactly `channels`
/// channels: a WAV cannot exceed 4 GB, and a two-hour 5.1 track at 48 kHz float is about
/// 8 GB. Read it back with [`read_raw_frames`].
pub fn extract_raw(
    tools: &Tools,
    src: &Path,
    start: f64,
    dur: f64,
    channels: usize,
    out: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    let ac = channels.to_string();
    run_extract(
        tools,
        src,
        start,
        dur,
        out,
        &["-ac", &ac, "-c:a", "pcm_f32le", "-f", "f32le"],
        cancel,
    )?;
    let want = frames_for(dur) as u64 * channels as u64 * 4;
    let got = std::fs::metadata(out)?.len();
    if got != want {
        let _ = std::fs::remove_file(out);
        bail!("the extracted audio has {got} bytes, expected {want}");
    }
    Ok(())
}

fn run_extract(
    tools: &Tools,
    src: &Path,
    start: f64,
    dur: f64,
    out: &Path,
    format: &[&str],
    cancel: &AtomicBool,
) -> Result<()> {
    let n = frames_for(dur);
    let part = part_path(out);
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut cmd = tools.ffmpeg_cmd();
    cmd.arg("-y")
        .arg("-ss")
        .arg(format!("{:.6}", start.max(0.0)))
        .arg("-i")
        .arg(src)
        .args(["-map", "0:a:0", "-vn", "-af"])
        .arg(format!(
            "aresample={RATE},apad=whole_len={n},atrim=end_sample={n}"
        ))
        .args(format)
        .arg(&part);
    let res = process::run(cmd, None, cancel, None);
    let fin = match res {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    if !fin.success {
        let _ = std::fs::remove_file(&part);
        bail!("ffmpeg could not extract the audio: {}", fin.tail(6));
    }
    std::fs::rename(&part, out).context("could not finish the audio file")?;
    Ok(())
}

/// Reads `len` frames from frame `start` of a headerless f32le file with `channels`
/// channels (as [`extract_raw`] writes); past the end of the file is silence.
pub fn read_raw_frames(path: &Path, channels: usize, start: u64, len: u64) -> Result<Pcm> {
    use std::io::{Read, Seek, SeekFrom};
    if channels == 0 {
        bail!("raw audio needs at least one channel");
    }
    let mut f =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let frame_bytes = channels as u64 * 4;
    let total = f.metadata()?.len() / frame_bytes;
    let begin = start.min(total);
    let avail = (total - begin).min(len);
    f.seek(SeekFrom::Start(begin * frame_bytes))?;
    let mut bytes = vec![0u8; (avail * frame_bytes) as usize];
    f.read_exact(&mut bytes)?;
    let mut samples: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    samples.resize(len as usize * channels, 0.0);
    Ok(Pcm {
        rate: RATE,
        channels,
        samples,
    })
}

/// Writes headerless interleaved f32le atomically (`*.part`, then rename).
pub fn write_raw(path: &Path, pcm: &Pcm) -> Result<()> {
    let part = part_path(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut bytes = Vec::with_capacity(pcm.samples.len() * 4);
    for s in &pcm.samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, path).context("could not finish the raw audio file")?;
    Ok(())
}

pub fn read_wav(path: &Path) -> Result<Pcm> {
    let mut r = hound::WavReader::open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    let spec = r.spec();
    let samples: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => r.samples::<f32>().collect::<Result<_, _>>()?,
        (hound::SampleFormat::Int, bits @ 8..=32) => {
            let scale = 1.0 / (1u64 << (bits - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()?
        }
        (f, b) => bail!("unsupported WAV format {f:?} {b} bit"),
    };
    Ok(Pcm {
        rate: spec.sample_rate,
        channels: spec.channels as usize,
        samples,
    })
}

/// Writes a 32-bit float WAV atomically (`*.part`, then rename).
pub fn write_wav(path: &Path, pcm: &Pcm) -> Result<()> {
    let part = part_path(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let spec = hound::WavSpec {
        channels: pcm.channels as u16,
        sample_rate: pcm.rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let result = (|| -> Result<()> {
        let mut w = hound::WavWriter::create(&part, spec)?;
        for &s in &pcm.samples {
            w.write_sample(s)?;
        }
        w.finalize()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, path).context("could not finish the WAV file")?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    /// FLAC, 24 bit.
    Flac24,
    /// AC-3 at 640 kbit/s with the given dialogue normalisation (-31..=-1 dB).
    Ac3At640 { dialnorm: i8 },
}

/// A running FFmpeg that encodes raw float samples written to it.
pub struct StreamEncoder<'a> {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<Vec<u8>>>,
    drain: Option<std::thread::JoinHandle<()>>,
    channels: usize,
    out: PathBuf,
    cancel: &'a AtomicBool,
}

/// Starts an FFmpeg encoding `channels` channels of 48 kHz f32le (arriving through
/// [`StreamEncoder::write`]) with channel layout `layout` into `out`.
pub fn encode_stream<'a>(
    tools: &Tools,
    channels: usize,
    layout: &str,
    codec: AudioCodec,
    out: &Path,
    cancel: &'a AtomicBool,
) -> Result<StreamEncoder<'a>> {
    encode_stream_filtered(tools, channels, layout, codec, None, out, cancel)
}

/// Like [`encode_stream`], with an FFmpeg audio filter (e.g. a limiter) run before encoding.
pub fn encode_stream_filtered<'a>(
    tools: &Tools,
    channels: usize,
    layout: &str,
    codec: AudioCodec,
    filter: Option<&str>,
    out: &Path,
    cancel: &'a AtomicBool,
) -> Result<StreamEncoder<'a>> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let format = format!("aformat=channel_layouts={layout}");
    let chain = match filter {
        Some(f) => format!("{f},{format}"),
        None => format,
    };
    let mut cmd = tools.ffmpeg_cmd();
    cmd.arg("-y")
        .args(["-f", "f32le", "-ar"])
        .arg(RATE.to_string())
        .arg("-ac")
        .arg(channels.to_string())
        .arg("-channel_layout")
        .arg(layout)
        .args(["-i", "pipe:0", "-af"])
        .arg(chain);
    match codec {
        AudioCodec::Flac24 => {
            cmd.args([
                "-c:a",
                "flac",
                "-sample_fmt",
                "s32",
                "-bits_per_raw_sample",
                "24",
            ]);
        }
        AudioCodec::Ac3At640 { dialnorm } => {
            cmd.args(["-c:a", "ac3", "-b:a", "640k", "-dialnorm"])
                .arg(dialnorm.to_string());
        }
    }
    cmd.arg(out);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("could not start ffmpeg")?;
    let stdin = child.stdin.take();
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let pipe = child.stderr.take();
    let buf = stderr.clone();
    let drain = std::thread::spawn(move || {
        use std::io::Read;
        if let Some(mut p) = pipe {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = p.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut b = buf.lock().unwrap();
                if b.len() < 64 * 1024 {
                    b.extend_from_slice(&chunk[..n]);
                }
            }
        }
    });
    Ok(StreamEncoder {
        child,
        stdin,
        stderr,
        drain: Some(drain),
        channels,
        out: out.to_path_buf(),
        cancel,
    })
}

impl StreamEncoder<'_> {
    fn stderr_tail(&mut self) -> String {
        if let Some(d) = self.drain.take() {
            let _ = d.join();
        }
        let text = String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(6)..].join("\n")
    }

    fn kill(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.out);
    }

    /// Appends interleaved samples (a whole number of frames).
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            self.kill();
            bail!(Cancelled);
        }
        if samples.len() % self.channels != 0 {
            bail!("audio chunk is not a whole number of frames");
        }
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let pipe = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("the audio encoder is closed"))?;
        if pipe.write_all(&bytes).is_err() {
            self.stdin = None;
            let _ = self.child.wait();
            let tail = self.stderr_tail();
            bail!("ffmpeg stopped while encoding audio: {tail}");
        }
        Ok(())
    }

    /// Closes the input and waits for FFmpeg to finish the file.
    pub fn finish(mut self) -> Result<()> {
        self.stdin = None;
        let status =
            match process::wait(&mut self.child, self.cancel, Some(Duration::from_secs(600))) {
                Ok(s) => s,
                Err(e) => {
                    let _ = std::fs::remove_file(&self.out);
                    return Err(e);
                }
            };
        let tail = self.stderr_tail();
        if !status.success() {
            let _ = std::fs::remove_file(&self.out);
            bail!("ffmpeg could not encode the audio: {tail}");
        }
        Ok(())
    }
}

impl Drop for StreamEncoder<'_> {
    fn drop(&mut self) {
        // Dropped without finish(): do not leave a running child behind.
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill();
        }
    }
}
