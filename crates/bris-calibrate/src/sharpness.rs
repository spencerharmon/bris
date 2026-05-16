//! Sharpness / blur estimator.
//!
//! Calibration frames that look fine to the operator but
//! are subtly motion-blurred (camera shake) or defocused
//! produce corners with biased sub-pixel positions; the
//! solver still converges, but on systematically-wrong
//! correspondences. The resulting RMS often looks good
//! while the recovered intrinsics drift.
//!
//! The Laplacian-variance estimator below gives the
//! operator a quick "how sharp is the chessboard region in
//! this frame" number. It's the OpenCV-folklore blur
//! detector: convolve with a 3×3 Laplacian, take the
//! variance of the response. Higher = more high-frequency
//! content = sharper.
//!
//! There is no single threshold that works across every
//! sensor and ISO. As a starting heuristic on 8-bit luma:
//! values below ~50 routinely correlate with visible blur,
//! values above ~200 with a sharp board. The Android UI
//! uses these as suggestions in the operator-facing
//! feedback, not as hard accept/reject gates.

use image::GrayImage;

/// Compute the variance of the Laplacian over the
/// rectangle `[x0, x1] × [y0, y1]` (inclusive) of the
/// supplied grayscale image.
///
/// Returns `f64::NAN` if the rectangle has fewer than 3×3
/// pixels (the 3×3 Laplacian kernel needs a one-pixel
/// border).
///
/// # Panics
///
/// Does not panic; out-of-bounds rectangles are clamped to
/// the image and may yield `NaN` for degenerate inputs.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    // c/l/r/u/d are the canonical center/left/right/up/down
    // labels for the 3×3 Laplacian neighbourhood.
    clippy::many_single_char_names,
)]
#[must_use]
pub fn laplacian_variance(image: &GrayImage, x0: u32, y0: u32, x1: u32, y1: u32) -> f64 {
    let w = image.width();
    let h = image.height();
    if w < 3 || h < 3 {
        return f64::NAN;
    }
    // Clamp to a 1-px-inset interior so the 3×3 Laplacian
    // never reads outside the image.
    let xi0 = x0.max(1);
    let yi0 = y0.max(1);
    let xi1 = x1.min(w - 2);
    let yi1 = y1.min(h - 2);
    if xi1 <= xi0 || yi1 <= yi0 {
        return f64::NAN;
    }
    // Two-pass mean / variance over Laplacian responses.
    // Standard 3×3 4-neighbour kernel:
    //   0 -1  0
    //  -1  4 -1
    //   0 -1  0
    let mut sum = 0.0_f64;
    let mut sum_sq = 0.0_f64;
    let mut n = 0_u64;
    for y in yi0..=yi1 {
        for x in xi0..=xi1 {
            let c = i32::from(image.get_pixel(x, y).0[0]);
            let l = i32::from(image.get_pixel(x - 1, y).0[0]);
            let r = i32::from(image.get_pixel(x + 1, y).0[0]);
            let u = i32::from(image.get_pixel(x, y - 1).0[0]);
            let d = i32::from(image.get_pixel(x, y + 1).0[0]);
            let lap = f64::from(4 * c - l - r - u - d);
            sum += lap;
            sum_sq += lap * lap;
            n += 1;
        }
    }
    if n == 0 {
        return f64::NAN;
    }
    let nf = n as f64;
    let mean = sum / nf;
    (sum_sq / nf) - mean * mean
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    #[test]
    fn flat_image_has_zero_variance() {
        let img = GrayImage::from_pixel(64, 64, Luma([100]));
        let v = laplacian_variance(&img, 0, 0, 63, 63);
        assert!(v.abs() < 1e-9, "flat image variance should be ~0, got {v}");
    }

    #[test]
    fn checkerboard_pattern_has_high_variance() {
        let mut img = GrayImage::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                let on = ((x / 4) + (y / 4)) % 2 == 0;
                img.put_pixel(x, y, Luma([if on { 255 } else { 0 }]));
            }
        }
        let v = laplacian_variance(&img, 0, 0, 63, 63);
        assert!(
            v > 1000.0,
            "high-contrast checkerboard should have large laplacian variance, got {v}"
        );
    }

    #[test]
    fn tiny_region_returns_nan() {
        let img = GrayImage::from_pixel(64, 64, Luma([100]));
        assert!(laplacian_variance(&img, 0, 0, 1, 1).is_nan());
    }

    #[test]
    fn tiny_image_returns_nan() {
        let img = GrayImage::from_pixel(2, 2, Luma([100]));
        assert!(laplacian_variance(&img, 0, 0, 1, 1).is_nan());
    }
}
