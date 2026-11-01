//! Frame geometry and comparisons: padding a frame to an aspect ratio image models handle
//! well and cropping it back, fitting results to the exact output size, and the cheap
//! difference measure behind duplicate and scene-cut detection.

use image::imageops::{self, FilterType};
use image::{GrayImage, Rgb, RgbImage};

/// Aspect ratios image models produce without reframing (1:1, 3:2, 2:3).
pub const MODEL_ASPECTS: [f64; 3] = [1.0, 1.5, 2.0 / 3.0];

/// A frame ready for a model: the canvas sent, and where the real picture sits in it.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub canvas: RgbImage,
    /// x, y, width, height of the picture inside the canvas.
    pub content: (u32, u32, u32, u32),
}

impl Prepared {
    pub fn padded(&self) -> bool {
        self.content.2 != self.canvas.width() || self.content.3 != self.canvas.height()
    }
}

/// The model aspect closest to `aspect` (compared on a log scale).
pub fn nearest_model_aspect(aspect: f64) -> f64 {
    *MODEL_ASPECTS
        .iter()
        .min_by(|a, b| {
            (a.ln() - aspect.ln())
                .abs()
                .total_cmp(&(b.ln() - aspect.ln()).abs())
        })
        .unwrap()
}

/// Shrinks `img` so its longest side is at most `max_side`; returns it unchanged when smaller.
pub fn limit_size(img: &RgbImage, max_side: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    if w.max(h) <= max_side {
        return img.clone();
    }
    let s = max_side as f64 / w.max(h) as f64;
    imageops::resize(
        img,
        ((w as f64 * s).round() as u32).max(1),
        ((h as f64 * s).round() as u32).max(1),
        FilterType::Lanczos3,
    )
}

/// Puts `img` centred on a black canvas of the nearest model aspect when `pad` is set and the
/// aspect differs by more than 2 %.
pub fn prepare(img: &RgbImage, pad: bool) -> Prepared {
    prepare_with(img, if pad { &MODEL_ASPECTS } else { &[] })
}

/// Like [`prepare`], with the aspect ratios this model produces (`[]`: any, no padding).
pub fn prepare_with(img: &RgbImage, aspects: &[f64]) -> Prepared {
    let (w, h) = img.dimensions();
    let aspect = w as f64 / h as f64;
    let Some(&target) = aspects.iter().min_by(|a, b| {
        (a.ln() - aspect.ln())
            .abs()
            .total_cmp(&(b.ln() - aspect.ln()).abs())
    }) else {
        return Prepared {
            canvas: img.clone(),
            content: (0, 0, w, h),
        };
    };
    if (aspect / target - 1.0).abs() < 0.02 {
        return Prepared {
            canvas: img.clone(),
            content: (0, 0, w, h),
        };
    }
    let (cw, ch) = if target > aspect {
        (((h as f64 * target).round() as u32).max(w), h)
    } else {
        (w, ((w as f64 / target).round() as u32).max(h))
    };
    let mut canvas = RgbImage::from_pixel(cw, ch, Rgb([0, 0, 0]));
    let (x, y) = ((cw - w) / 2, (ch - h) / 2);
    imageops::replace(&mut canvas, img, x as i64, y as i64);
    Prepared {
        canvas,
        content: (x, y, w, h),
    }
}

/// Cuts the picture back out of a model result (which may have any size) and sizes it to
/// exactly `target` without changing its proportions.
pub fn finish(result: &RgbImage, prepared: &Prepared, target: (u32, u32)) -> RgbImage {
    let (cw, ch) = prepared.canvas.dimensions();
    let (rw, rh) = result.dimensions();
    let same_shape = ((rw as f64 / rh as f64) / (cw as f64 / ch as f64) - 1.0).abs() < 0.02;
    let picture = if same_shape {
        let (sx, sy) = (rw as f64 / cw as f64, rh as f64 / ch as f64);
        let (x, y, w, h) = prepared.content;
        let crop_x = ((x as f64 * sx).round() as u32).min(rw.saturating_sub(1));
        let crop_y = ((y as f64 * sy).round() as u32).min(rh.saturating_sub(1));
        let crop_w = ((w as f64 * sx).round() as u32).clamp(1, rw - crop_x);
        let crop_h = ((h as f64 * sy).round() as u32).clamp(1, rh - crop_y);
        imageops::crop_imm(result, crop_x, crop_y, crop_w, crop_h).to_image()
    } else if prepared.padded() {
        // The model reframed the canvas: our bars are gone or resized, so cut away whatever
        // black edges are left instead of trusting their old position.
        trim_black_edges(result)
    } else {
        result.clone()
    };
    fit_cover(&picture, target)
}

/// Removes solid black rows and columns at the edges (at most a quarter of each side).
fn trim_black_edges(img: &RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    let dark = |p: &Rgb<u8>| p.0.iter().all(|&c| c < 20);
    let row_black =
        |y: u32| (0..w).filter(|&x| dark(img.get_pixel(x, y))).count() * 100 >= w as usize * 98;
    let col_black =
        |x: u32| (0..h).filter(|&y| dark(img.get_pixel(x, y))).count() * 100 >= h as usize * 98;
    let top = (0..h / 4).take_while(|&y| row_black(y)).count() as u32;
    let bottom = (0..h / 4).take_while(|&i| row_black(h - 1 - i)).count() as u32;
    let left = (0..w / 4).take_while(|&x| col_black(x)).count() as u32;
    let right = (0..w / 4).take_while(|&i| col_black(w - 1 - i)).count() as u32;
    imageops::crop_imm(img, left, top, w - left - right, h - top - bottom).to_image()
}

/// Scales `img` by the same factor in both directions until it covers `target`, then crops
/// the overflow evenly from both sides. Never stretches.
pub fn fit_cover(img: &RgbImage, target: (u32, u32)) -> RgbImage {
    let (w, h) = img.dimensions();
    if (w, h) == target {
        return img.clone();
    }
    let s = (target.0 as f64 / w as f64).max(target.1 as f64 / h as f64);
    let sw = ((w as f64 * s).round() as u32).max(target.0);
    let sh = ((h as f64 * s).round() as u32).max(target.1);
    let scaled = imageops::resize(img, sw, sh, FilterType::Lanczos3);
    if (sw, sh) == target {
        return scaled;
    }
    imageops::crop_imm(
        &scaled,
        (sw - target.0) / 2,
        (sh - target.1) / 2,
        target.0,
        target.1,
    )
    .to_image()
}

/// Grey used for the empty canvas the AI fills in when it extends the picture.
pub const EXTEND_FILL: Rgb<u8> = Rgb([128, 128, 128]);

/// Where a picture of aspect `inner` sits, centred, in a canvas of aspect `outer`, as
/// fractions of the canvas: (x, y, width, height).
pub fn inner_fraction(inner: f64, outer: f64) -> (f64, f64, f64, f64) {
    if outer > inner {
        let w = inner / outer;
        ((1.0 - w) / 2.0, 0.0, w, 1.0)
    } else {
        let h = outer / inner;
        (0.0, (1.0 - h) / 2.0, 1.0, h)
    }
}

/// `img` centred on a grey canvas of aspect `aspect` (wider: grey at the sides; taller:
/// above and below). The picture keeps its size; the canvas grows.
pub fn extend_canvas(img: &RgbImage, aspect: f64) -> RgbImage {
    let (w, h) = img.dimensions();
    let (cw, ch) = if aspect > w as f64 / h as f64 {
        (((h as f64 * aspect).round() as u32).max(w), h)
    } else {
        (w, ((w as f64 / aspect).round() as u32).max(h))
    };
    let mut canvas = RgbImage::from_pixel(cw, ch, EXTEND_FILL);
    imageops::replace(
        &mut canvas,
        img,
        ((cw - w) / 2) as i64,
        ((ch - h) / 2) as i64,
    );
    canvas
}

/// The part of an extended `result` where the source picture sits, scaled to `size` (the
/// source's size).
pub fn source_area(result: &RgbImage, size: (u32, u32)) -> RgbImage {
    let (rw, rh) = result.dimensions();
    let (x, y, w, h) = inner_fraction(size.0 as f64 / size.1 as f64, rw as f64 / rh as f64);
    let cx = (x * rw as f64).round() as u32;
    let cy = (y * rh as f64).round() as u32;
    let cw = ((w * rw as f64).round() as u32).clamp(1, rw - cx.min(rw - 1));
    let ch = ((h * rh as f64).round() as u32).clamp(1, rh - cy.min(rh - 1));
    let area = imageops::crop_imm(result, cx, cy, cw, ch).to_image();
    imageops::resize(&area, size.0, size.1, FilterType::Lanczos3)
}

/// A plain resize to `target` followed by a gentle unsharp mask.
pub fn basic_upscale(img: &RgbImage, target: (u32, u32)) -> RgbImage {
    let up = imageops::resize(img, target.0, target.1, FilterType::Lanczos3);
    imageops::unsharpen(&up, 1.2, 4)
}

/// A tiny grayscale fingerprint of a frame for [`difference`].
pub fn fingerprint(img: &RgbImage) -> GrayImage {
    let small = imageops::resize(img, 64, 36, FilterType::Triangle);
    imageops::grayscale(&small)
}

/// Mean absolute difference (0-255) between two fingerprints.
pub fn difference(a: &GrayImage, b: &GrayImage) -> f32 {
    let sum: u64 = a
        .pixels()
        .zip(b.pixels())
        .map(|(p, q)| (p.0[0] as i32 - q.0[0] as i32).unsigned_abs() as u64)
        .sum();
    sum as f32 / (a.width() * a.height()).max(1) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widescreen_is_padded_to_3_2_and_cropped_back() {
        let img = RgbImage::from_pixel(1920, 1080, Rgb([200, 10, 10]));
        let p = prepare(&img, true);
        assert_eq!(p.canvas.dimensions(), (1920, 1280));
        assert_eq!(p.content, (0, 100, 1920, 1080));
        assert_eq!(p.canvas.get_pixel(5, 5), &Rgb([0, 0, 0]));
        // The model answers at 1536x1024: the picture is cut out and sized to the target.
        let result = imageops::resize(&p.canvas, 1536, 1024, FilterType::Nearest);
        let out = finish(&result, &p, (3840, 2160));
        assert_eq!(out.dimensions(), (3840, 2160));
        let c = out.get_pixel(1920, 3);
        assert!(c.0[0] > 150, "padding leaked into the picture: {c:?}");
    }

    #[test]
    fn reframed_result_is_not_stretched() {
        // 4:3 extended to 2.39:1 and padded to 3:2; the model answers 2.34:1 with thin bars.
        let p = prepare_with(&RgbImage::new(1032, 432), &MODEL_ASPECTS);
        assert!(p.padded());
        let mut result = RgbImage::from_pixel(1919, 820, Rgb([0, 0, 0]));
        for y in 62..758 {
            for x in 0..1919 {
                result.put_pixel(x, y, Rgb([200, 200, 200]));
            }
        }
        // A 4x4 white square in the middle must stay square.
        for y in 408..412 {
            for x in 958..962 {
                result.put_pixel(x, y, Rgb([255, 255, 255]));
            }
        }
        let out = finish(&result, &p, (2064, 864));
        assert_eq!(out.dimensions(), (2064, 864));
        assert!(out.get_pixel(1032, 2).0[0] > 150, "a black bar was kept");
        let white = |x: u32, y: u32| out.get_pixel(x, y).0[0] > 240;
        let wide = (0..2064).filter(|&x| white(x, 432)).count();
        let tall = (0..864).filter(|&y| white(1032, y)).count();
        assert!(wide.abs_diff(tall) <= 1, "stretched: {wide}x{tall}");
    }

    #[test]
    fn fit_cover_keeps_proportions() {
        let out = fit_cover(&RgbImage::new(1919, 820), (2064, 864));
        assert_eq!(out.dimensions(), (2064, 864));
        let out = fit_cover(&RgbImage::new(1536, 1024), (1536, 1024));
        assert_eq!(out.dimensions(), (1536, 1024));
    }

    #[test]
    fn matching_aspect_is_untouched() {
        let img = RgbImage::new(900, 600);
        let p = prepare(&img, true);
        assert!(!p.padded());
        let q = prepare(&RgbImage::new(1920, 1080), false);
        assert!(!q.padded());
    }

    #[test]
    fn portrait_pads_sideways() {
        let p = prepare(&RgbImage::new(1080, 1920), true);
        assert_eq!(p.canvas.dimensions(), (1280, 1920));
    }

    #[test]
    fn extend_to_wider_and_taller() {
        let img = RgbImage::from_pixel(400, 300, Rgb([255, 0, 0]));
        let wide = extend_canvas(&img, 16.0 / 9.0);
        assert_eq!(wide.dimensions(), (533, 300));
        assert_eq!(wide.get_pixel(2, 150), &EXTEND_FILL);
        assert_eq!(wide.get_pixel(266, 150), &Rgb([255, 0, 0]));
        let tall = extend_canvas(&img, 9.0 / 16.0);
        assert_eq!(tall.dimensions(), (400, 711));
        let (x, y, w, h) = inner_fraction(4.0 / 3.0, 16.0 / 9.0);
        assert!((w - 0.75).abs() < 1e-9 && h == 1.0 && (x - 0.125).abs() < 1e-9 && y == 0.0);
    }

    #[test]
    fn difference_detects_change() {
        let a = fingerprint(&RgbImage::from_pixel(320, 180, Rgb([10, 10, 10])));
        let b = fingerprint(&RgbImage::from_pixel(320, 180, Rgb([200, 200, 200])));
        assert_eq!(difference(&a, &a), 0.0);
        assert!(difference(&a, &b) > 150.0);
    }
}
