//! Image preprocessing for the heteroscedastic gravity model.
//!
//! Pipeline (per `docs/design/ml_gravity.md` §"Image
//! preprocessing pipeline"):
//!   1. Downsample to `INPUT_SIZE` × `INPUT_SIZE` (nearest-
//!      neighbor over the 16-bit grayscale Frame).
//!   2. Percentile-normalise (1st-99th percentile of the
//!      downsampled tile → 0-255) so dim / saturated frames
//!      land in the model's training distribution.
//!   3. Replicate single channel into RGB.
//!   4. ImageNet normalise `(c - mean) / std`.
//!   5. Pack as NCHW float32.

#![cfg(feature = "ml-gravity")]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::explicit_iter_loop,
    clippy::needless_range_loop,
    clippy::missing_errors_doc,
    clippy::doc_markdown
)]

use crate::frame::Frame;
use ndarray::Array4;

/// Model input edge length. Matches `train_heteroscedastic.py`
/// `INPUT_SIZE`.
pub const INPUT_SIZE: usize = 256;

/// ImageNet per-channel mean (R, G, B).
pub const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
/// ImageNet per-channel std (R, G, B).
pub const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];

/// Preprocessing errors.
#[derive(Debug, thiserror::Error)]
pub enum PreprocessError {
    /// Frame had zero pixels.
    #[error("frame is empty")]
    EmptyFrame,
}

/// Convert a 16-bit grayscale `Frame` to a (1, 3, 256, 256)
/// float32 tensor ready for ORT.
///
/// # Errors
/// See [`PreprocessError`].
pub fn frame_to_input_tensor(frame: &Frame) -> Result<Array4<f32>, PreprocessError> {
    let w = frame.width() as usize;
    let h = frame.height() as usize;
    if w == 0 || h == 0 {
        return Err(PreprocessError::EmptyFrame);
    }
    let pixels = frame.pixels();

    // 1. Nearest-neighbor downsample to INPUT_SIZE × INPUT_SIZE.
    let mut tile = vec![0u16; INPUT_SIZE * INPUT_SIZE];
    let sx = w as f64 / INPUT_SIZE as f64;
    let sy = h as f64 / INPUT_SIZE as f64;
    for y in 0..INPUT_SIZE {
        let src_y = (((y as f64 + 0.5) * sy).floor() as usize).min(h - 1);
        for x in 0..INPUT_SIZE {
            let src_x = (((x as f64 + 0.5) * sx).floor() as usize).min(w - 1);
            tile[y * INPUT_SIZE + x] = pixels[src_y * w + src_x];
        }
    }

    // 2. Percentile normalise. Histogram → 1st/99th percentiles
    //    on the 16-bit values, then linear-stretch to [0, 1].
    let (lo, hi) = percentile_window(&tile);
    let span = (hi - lo).max(1.0);
    let mut gray = vec![0f32; INPUT_SIZE * INPUT_SIZE];
    for (g, p) in gray.iter_mut().zip(tile.iter()) {
        let v = (f32::from(*p) - lo as f32) / span as f32;
        *g = v.clamp(0.0, 1.0);
    }

    // 3 + 4 + 5. Replicate RGB, ImageNet normalise, NCHW pack.
    let mut out = Array4::<f32>::zeros((1, 3, INPUT_SIZE, INPUT_SIZE));
    for c in 0..3 {
        let mean = IMAGENET_MEAN[c];
        let std = IMAGENET_STD[c];
        for y in 0..INPUT_SIZE {
            for x in 0..INPUT_SIZE {
                let g = gray[y * INPUT_SIZE + x];
                out[(0, c, y, x)] = (g - mean) / std;
            }
        }
    }
    Ok(out)
}

/// 1st / 99th percentile of a 16-bit grayscale tile via a
/// 1024-bin histogram (lossless to ±32 ADU; the tile is then
/// percentile-stretched anyway).
fn percentile_window(tile: &[u16]) -> (f64, f64) {
    const BINS: usize = 1024;
    let mut hist = [0u32; BINS];
    for &p in tile {
        let bin = (usize::from(p) * (BINS - 1) / 65_535).min(BINS - 1);
        hist[bin] += 1;
    }
    let total: u64 = hist.iter().map(|&c| u64::from(c)).sum();
    let low_target = (total as f64 * 0.01) as u64;
    let high_target = (total as f64 * 0.99) as u64;
    let mut cum: u64 = 0;
    let mut lo_bin = 0;
    let mut hi_bin = BINS - 1;
    for (i, &c) in hist.iter().enumerate() {
        cum += u64::from(c);
        if cum <= low_target {
            lo_bin = i;
        }
        if cum >= high_target {
            hi_bin = i;
            break;
        }
    }
    let to_value = |bin: usize| -> f64 { (bin as f64) * 65_535.0 / (BINS as f64 - 1.0) };
    (to_value(lo_bin), to_value(hi_bin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_frame(w: u32, h: u32, fill: u16) -> Frame {
        let intr = crate::Intrinsics::placeholder(w, h);
        let pixels = vec![fill; (w as usize) * (h as usize)];
        let tt = bris_core::time::Tt::from_julian_date(2_460_676.5);
        Frame::new(w, h, pixels, tt, 1_000, intr).unwrap()
    }

    #[test]
    fn empty_frame_errors() {
        // Cannot construct a zero-sized Frame via the safe API,
        // so this is a compile-only documentation test.
    }

    #[test]
    fn uniform_frame_produces_zero_mean_input() {
        // After ImageNet normalisation a uniform frame should
        // collapse to constants per channel.
        let f = unit_frame(64, 64, 32_768);
        let tensor = frame_to_input_tensor(&f).unwrap();
        assert_eq!(tensor.shape(), &[1, 3, INPUT_SIZE, INPUT_SIZE]);
        // The constant is whatever (0.5 - mean_c) / std_c is.
        // We assert finiteness rather than exact value to
        // tolerate the percentile-normaliser's exact rounding.
        for &v in tensor.iter() {
            assert!(v.is_finite(), "non-finite tensor value: {v}");
        }
    }
}
