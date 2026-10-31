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
    let (w, h) = img.dimensions();
    let aspect = w as f64 / h as f64;
    let target = nearest_model_aspect(aspect);
    if !pad || (aspect / target - 1.0).abs() < 0.02 {
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

/// Cuts the picture back out of a model result (which may have any size) and resizes it to
/// exactly `target`.
pub fn finish(result: &RgbImage, prepared: &Prepared, target: (u32, u32)) -> RgbImage {
    let (cw, ch) = prepared.canvas.dimensions();
    let (rw, rh) = result.dimensions();
    let (sx, sy) = (rw as f64 / cw as f64, rh as f64 / ch as f64);
    let (x, y, w, h) = prepared.content;
    let crop_x = ((x as f64 * sx).round() as u32).min(rw.saturating_sub(1));
    let crop_y = ((y as f64 * sy).round() as u32).min(rh.saturating_sub(1));
    let crop_w = ((w as f64 * sx).round() as u32).clamp(1, rw - crop_x);
    let crop_h = ((h as f64 * sy).round() as u32).clamp(1, rh - crop_y);
    let cropped = imageops::crop_imm(result, crop_x, crop_y, crop_w, crop_h).to_image();
    if cropped.dimensions() == target {
        cropped
    } else {
        imageops::resize(&cropped, target.0, target.1, FilterType::Lanczos3)
    }
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
