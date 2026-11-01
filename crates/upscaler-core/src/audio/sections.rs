//! Splitting a long audio job into sections and stitching them back together.
//!
//! Sections are 300 s long and overlap by 2 s so memory stays small and a
//! finished section can be resumed. Zero-phase filters ring at section edges;
//! the overlap is crossfaded so that ringing is hidden.

use anyhow::{bail, Result};

pub const SECTION_SECONDS: u64 = 300;
pub const OVERLAP_SECONDS: u64 = 2;

/// A section of the job. `start` and `len` are in frames (samples per
/// channel) and include the overlap at both inner ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub index: usize,
    pub start: u64,
    pub len: u64,
}

/// Plans the sections covering `0..total` frames. Consecutive sections overlap
/// by exactly `OVERLAP_SECONDS * rate` frames; a short range is one section. The job uses
/// [`plan_with`] with its own sizes; this is the default for the tests.
#[cfg(test)]
pub(crate) fn plan(total: u64, rate: u32) -> Vec<Section> {
    plan_with(
        total,
        SECTION_SECONDS * rate as u64,
        OVERLAP_SECONDS * rate as u64,
    )
}

/// Like [`plan`] with the section length and overlap given in frames (`len_max > overlap`).
/// The audio job's tests use short sections through this.
pub fn plan_with(total: u64, len_max: u64, overlap: u64) -> Vec<Section> {
    assert!(
        len_max > overlap,
        "sections must be longer than their overlap"
    );
    if total == 0 {
        return Vec::new();
    }
    let step = len_max - overlap;
    let mut out = Vec::new();
    let mut start = 0u64;
    loop {
        let len = len_max.min(total - start);
        out.push(Section {
            index: out.len(),
            start,
            len,
        });
        if start + len >= total {
            break;
        }
        start += step;
    }
    out
}

/// Stitches sections into one interleaved stream of exactly
/// `total * channels` samples. Each overlap is blended with a raised-cosine
/// (Hann) crossfade whose two gains sum to exactly 1, so identical content in
/// both sections is reproduced exactly (an equal-power cos/sin fade would
/// bump identical copies by up to +3 dB). At most two sections are held in
/// memory at once.
pub fn stitch(
    sections: &[Section],
    total: u64,
    channels: usize,
    mut read: impl FnMut(&Section) -> Result<Vec<f32>>,
    mut write: impl FnMut(&[f32]) -> Result<()>,
) -> Result<()> {
    if sections.is_empty() {
        if total == 0 {
            return Ok(());
        }
        bail!("no sections for {total} frames");
    }
    let mut load = |s: &Section| -> Result<Vec<f32>> {
        let data = read(s)?;
        if data.len() as u64 != s.len * channels as u64 {
            bail!(
                "section {} has {} samples, expected {}",
                s.index,
                data.len(),
                s.len * channels as u64
            );
        }
        Ok(data)
    };
    let mut cur = load(&sections[0])?;
    // Frames of `cur` already written (the overlap with its predecessor).
    let mut pos = 0usize;
    let mut written = 0u64;
    for pair in sections.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let end_a = a.start + a.len;
        if b.start > end_a || b.start < a.start {
            bail!("sections {} and {} do not overlap", a.index, b.index);
        }
        let ov = (end_a - b.start) as usize;
        let a_len = a.len as usize;
        if ov > a_len - pos || ov as u64 > b.len {
            bail!("overlap of section {} is inconsistent", b.index);
        }
        let next = load(b)?;
        let solo_end = a_len - ov;
        write(&cur[pos * channels..solo_end * channels])?;
        written += (solo_end - pos) as u64;
        // Hann fade: gain_in + gain_out == 1 exactly for every frame.
        let mut blend = Vec::with_capacity(ov * channels);
        for i in 0..ov {
            let t = (i as f64 + 0.5) / ov as f64;
            let g_in = (0.5 * (1.0 - (std::f64::consts::PI * t).cos())) as f32;
            let g_out = 1.0 - g_in;
            for c in 0..channels {
                blend.push(
                    cur[(solo_end + i) * channels + c] * g_out + next[i * channels + c] * g_in,
                );
            }
        }
        write(&blend)?;
        written += ov as u64;
        cur = next;
        pos = ov;
    }
    let last_len = sections[sections.len() - 1].len as usize;
    write(&cur[pos * channels..last_len * channels])?;
    written += (last_len - pos) as u64;
    if written != total {
        bail!("stitched {written} frames, expected {total}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const OV: u64 = OVERLAP_SECONDS * RATE as u64;

    fn check_cover(total: u64) {
        let s = plan(total, RATE);
        assert!(!s.is_empty());
        assert_eq!(s[0].start, 0);
        let last = s.last().unwrap();
        assert_eq!(last.start + last.len, total);
        for (i, w) in s.iter().enumerate() {
            assert_eq!(w.index, i);
            assert!(w.len > 0);
        }
        for p in s.windows(2) {
            assert_eq!(p[0].start + p[0].len - p[1].start, OV);
            assert!(p[1].start + p[1].len > p[0].start + p[0].len);
        }
    }

    #[test]
    fn sections_cover_everything() {
        for total in [1, 300 * 48_000, 92 * 60 * 48_000 + 7] {
            check_cover(total);
        }
        assert_eq!(plan(300 * 48_000, RATE).len(), 1);
        assert!(plan(92 * 60 * 48_000 + 7, RATE).len() > 18);
    }

    fn signal(total: u64, ch: usize) -> Vec<f32> {
        (0..total as usize * ch)
            .map(|i| ((i / ch) as f32 * 0.001 + (i % ch) as f32).sin())
            .collect()
    }

    fn run(total: u64, ch: usize, rate: u32) -> (Vec<f32>, Vec<f32>, usize) {
        let src = signal(total, ch);
        let secs = plan(total, rate);
        let mut out = Vec::new();
        stitch(
            &secs,
            total,
            ch,
            |s| Ok(src[s.start as usize * ch..(s.start + s.len) as usize * ch].to_vec()),
            |b| {
                out.extend_from_slice(b);
                Ok(())
            },
        )
        .unwrap();
        (src, out, secs.len())
    }

    #[test]
    fn stitching_copies_reconstructs_source() {
        // Small rate so several sections exist: 2 s overlap = 2 * rate frames.
        let rate = 100;
        let total = 300 * rate as u64 * 3 + 123;
        let (src, out, n) = run(total, 6, rate);
        assert!(n >= 3);
        assert_eq!(out.len(), src.len());
        for (a, b) in src.iter().zip(&out) {
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn short_range_single_section() {
        let total = 3 * RATE as u64;
        let (src, out, n) = run(total, 6, RATE);
        assert_eq!(n, 1);
        assert_eq!(out, src);
    }

    #[test]
    fn crossfade_blends_differing_overlap() {
        // Section 0 is all 1.0, section 1 all 3.0: the overlap must go from 1 to 3.
        let rate = 100;
        let total = 300 * rate as u64 * 2;
        let secs = plan(total, rate);
        let mut out = Vec::new();
        stitch(
            &secs,
            total,
            1,
            |s| Ok(vec![if s.index == 0 { 1.0 } else { 3.0 }; s.len as usize]),
            |b| {
                out.extend_from_slice(b);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out.len() as u64, total);
        let ov = (OVERLAP_SECONDS * rate as u64) as usize;
        let s1 = secs[1].start as usize;
        assert_eq!(out[s1 - 1], 1.0);
        assert!(out[s1] > 1.0 && out[s1] < 1.05);
        assert!(out[s1 + ov - 1] < 3.0 && out[s1 + ov - 1] > 2.95);
        assert_eq!(out[s1 + ov], 3.0);
    }

    #[test]
    fn wrong_read_length_is_an_error() {
        let secs = plan(1000, RATE);
        let r = stitch(&secs, 1000, 2, |_| Ok(vec![0.0; 3]), |_| Ok(()));
        assert!(r.is_err());
    }
}
