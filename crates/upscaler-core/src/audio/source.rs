//! What the source's audio is: channel count, codec and, for AC-3, the flags that say whether
//! a stereo track is really a Dolby Surround downmix and how loud its dialogue is.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::ffmpeg::{Tools, VideoInfo};
use crate::process::{self, command};

/// The parts of an AC-3 syncframe header we care about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ac3Header {
    /// Audio coding mode (2 = stereo, 7 = 3/2).
    pub acmod: u8,
    /// Stereo flagged as Dolby Surround encoded (`dsurmod == 2`).
    pub dolby_surround: bool,
    /// Dialogue level in dB (-31..=-1).
    pub dialnorm: i8,
}

/// Parses the start of an AC-3 syncframe (sync word 0x0B77). `None` when it is not AC-3.
pub fn parse_ac3(frame: &[u8]) -> Option<Ac3Header> {
    if frame.len() < 8 || frame[0] != 0x0B || frame[1] != 0x77 {
        return None;
    }
    // After sync word (16) and crc1 (16): fscod (2), frmsizecod (6), bsid (5), bsmod (3).
    let mut b = Bits {
        data: frame,
        pos: 32,
    };
    let fscod = b.take(2)?;
    b.take(6)?;
    let bsid = b.take(5)?;
    b.take(3)?;
    // bsid 8 is plain AC-3 (6 for the alternate syntax); E-AC-3 (16) has a different header.
    if fscod == 3 || bsid > 8 {
        return None;
    }
    let acmod = b.take(3)? as u8;
    if acmod & 1 == 1 && acmod != 1 {
        b.take(2)?; // cmixlev
    }
    if acmod & 4 != 0 {
        b.take(2)?; // surmixlev
    }
    let mut dolby_surround = false;
    if acmod == 2 {
        dolby_surround = b.take(2)? == 2;
    }
    b.take(1)?; // lfeon
    let dialnorm = match b.take(5)? {
        0 => -31, // reserved value, treated as -31 dB
        v => -(v as i8),
    };
    Some(Ac3Header {
        acmod,
        dolby_surround,
        dialnorm,
    })
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn take(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = (v << 1) | ((byte >> (7 - self.pos % 8)) & 1) as u32;
            self.pos += 1;
        }
        Some(v)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceAudio {
    pub channels: u32,
    pub codec: String,
    pub dolby_surround: bool,
    pub dialnorm: Option<i8>,
}

impl SourceAudio {
    pub fn is_surround(&self) -> bool {
        self.channels >= 6
    }
}

/// Looks at the first audio stream of `info`: `None` when there is none.
pub fn analyse(tools: &Tools, info: &VideoInfo) -> Result<Option<SourceAudio>> {
    if !info.has_audio {
        return Ok(None);
    }
    let cancel = AtomicBool::new(false);
    let mut cmd = command(&tools.ffprobe);
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "a:0",
        "-show_entries",
        "stream=codec_name,channels",
        "-print_format",
        "json",
    ])
    .arg(&info.path);
    let out = process::run(cmd, None, &cancel, Some(Duration::from_secs(60)))?;
    if !out.success {
        bail!("ffprobe: {}", out.tail(5));
    }
    let v: Value = serde_json::from_str(&out.stdout_text())?;
    let Some(stream) = v["streams"].get(0) else {
        return Ok(None);
    };
    let codec = stream["codec_name"]
        .as_str()
        .or(info.audio_codec.as_deref())
        .unwrap_or("?")
        .to_string();
    let mut audio = SourceAudio {
        channels: stream["channels"].as_u64().unwrap_or(0) as u32,
        codec,
        dolby_surround: false,
        dialnorm: None,
    };
    if audio.codec == "ac3" {
        let mut cmd = tools.ffmpeg_cmd();
        cmd.arg("-i")
            .arg(&info.path)
            .args(["-map", "0:a:0", "-c", "copy", "-frames:a", "1", "-f", "ac3"])
            .arg("pipe:1");
        let out = process::run(cmd, None, &cancel, Some(Duration::from_secs(60)))?;
        if let Some(h) = parse_ac3(&out.stdout) {
            audio.dolby_surround = h.dolby_surround;
            audio.dialnorm = Some(h.dialnorm);
        }
    }
    Ok(Some(audio))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A syncframe header: acmod 2 (2/0), dsurmod `dsur`, no LFE, dialnorm `dn`.
    fn header(dsur: u32, dn: u32) -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        let mut put = |v: u32, n: usize| {
            for i in (0..n).rev() {
                bits.push(((v >> i) & 1) as u8);
            }
        };
        put(0x0B77, 16);
        put(0, 16); // crc1
        put(0, 2); // fscod 48 kHz
        put(20, 6); // frmsizecod
        put(8, 5); // bsid
        put(0, 3); // bsmod
        put(2, 3); // acmod
        put(dsur, 2);
        put(0, 1); // lfeon
        put(dn, 5);
        put(0, 16);
        bits.chunks(8)
            .map(|c| {
                let mut byte = 0u8;
                for (i, b) in c.iter().enumerate() {
                    byte |= b << (7 - i);
                }
                byte
            })
            .collect()
    }

    #[test]
    fn parses_dolby_surround_header() {
        let h = parse_ac3(&header(2, 27)).unwrap();
        assert_eq!(h.acmod, 2);
        assert!(h.dolby_surround);
        assert_eq!(h.dialnorm, -27);
        let plain = parse_ac3(&header(1, 31)).unwrap();
        assert!(!plain.dolby_surround);
        assert_eq!(plain.dialnorm, -31);
        // The reserved value 0 counts as -31 dB.
        assert_eq!(parse_ac3(&header(0, 0)).unwrap().dialnorm, -31);
    }

    #[test]
    fn rejects_non_ac3() {
        let junk: Vec<u8> = (0..64u32).map(|i| (i * 37 + 11) as u8).collect();
        assert_eq!(parse_ac3(&junk), None);
        assert_eq!(parse_ac3(&[0x0B, 0x77, 0, 0]), None);
        assert_eq!(parse_ac3(&[]), None);
    }
}
