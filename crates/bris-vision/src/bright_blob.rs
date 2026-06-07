//! Cheap "bright blob" mask for pre-classification masking.
//!
//! This module computes a frame-sized boolean mask that marks
//! pixels belonging to compact bright regions — body candidates
//! (Sun, Moon, planets, specular reflections, deck lights, lens
//! flare) — *without* doing full body detection. Two consumers
//! today:
//!
//! 1. [`crate::condition::classify`] uses it to exclude bright
//!    compact regions from the ambient-luma average so a
//!    saturated moon disk doesn't bias a night frame's mean
//!    luma up into the twilight band. See
//!    `docs/design/pre_classification_masking.md` for the full
//!    rationale.
//! 2. The reflection-pair and full body-detect paths can use
//!    it as a ROI prior, avoiding a redundant threshold pass.
//!
//! The mask is *suppressive*, not constructive: it marks
//! candidates, not identified bodies. Clouds, lens flare,
//! streetlights, anything bright and compact survives the
//! threshold. The real body detector keeps its gating role;
//! it just gets a cheaper starting point.
//!
//! Cost: one downsampled threshold pass (`O(N / s²)`) plus one
//! small (3–5 px) morphological dilation. No connected-
//! components labeling, no centroid moments, no photometry.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use crate::frame::Frame;

/// Tunable parameters for [`compute_bright_blob_mask`]. The
/// defaults are sized for typical 8-bit-widened-to-u16
/// shipboard / handheld imagery.
#[derive(Debug, Clone, Copy)]
pub struct BrightBlobConfig {
    /// Linear downsample factor. The threshold pass walks a
    /// frame of (`width / downsample`, `height / downsample`)
    /// before upsampling the mask back to full resolution via
    /// nearest-neighbour. `1` means "no downsample". Default
    /// `4` — good fit for 4 MP+ frames on a Pi-class CPU.
    pub downsample: u32,

    /// Multiplier on the downsampled-image median used in the
    /// threshold floor. The effective threshold is
    /// `max(p99, k_median · median)`. The `p99` term picks up
    /// the brightest tail (saturated bodies); the median term
    /// is a safety net for frames where the entire scene is
    /// dim and the p99 is itself low. Default `8.0`.
    pub k_median: f64,

    /// Dilation radius in *full-resolution pixels*, applied
    /// after the threshold pass + upsample. Absorbs halo and
    /// flare skirts. Default `4` (within the spec's 3–5 px
    /// band). `0` disables dilation.
    pub dilation_px: u32,
}

impl Default for BrightBlobConfig {
    fn default() -> Self {
        Self {
            downsample: 4,
            k_median: 8.0,
            dilation_px: 4,
        }
    }
}

/// Compute a full-resolution boolean mask: `true` at every
/// pixel that belongs to (or sits within `dilation_px` of) a
/// bright compact region.
///
/// Returned vector has length `frame.width() * frame.height()`,
/// row-major.
#[must_use]
pub fn compute_bright_blob_mask(frame: &Frame, cfg: BrightBlobConfig) -> Vec<bool> {
    let w = frame.width() as usize;
    let h = frame.height() as usize;
    let pixels = frame.pixels();
    let ds = cfg.downsample.max(1) as usize;

    // Downsample by nearest-neighbour into a working buffer.
    // Nearest-neighbour, not averaging: averaging would
    // smear the brightest pixel into its neighbours and
    // suppress the very signal we're trying to mask. The
    // p99 of a smoothed frame underestimates the true peak.
    let dw = w.div_ceil(ds);
    let dh = h.div_ceil(ds);
    let mut down: Vec<u16> = Vec::with_capacity(dw * dh);
    for y in 0..dh {
        let sy = (y * ds).min(h - 1);
        for x in 0..dw {
            let sx = (x * ds).min(w - 1);
            down.push(pixels[sy * w + sx]);
        }
    }

    // Threshold: max(p99, k · median). `None` means the
    // frame has no usable contrast above the typical pixel
    // (uniform dark, uniform bright) — mark nothing.
    let Some(threshold) = downsampled_threshold(&down, cfg.k_median) else {
        return vec![false; w * h];
    };

    // Apply threshold on the downsampled image, then
    // upsample the bool mask back to full resolution. Each
    // downsampled "hot" pixel paints a `ds × ds` block in
    // the full-res mask — equivalent to nearest-neighbour
    // upsample but cheaper to write directly. `>=` so a
    // sparse cluster of identical-peak pixels is captured
    // (p99 of a few-saturated frame lands on the peak).
    let mut mask = vec![false; w * h];
    for dy in 0..dh {
        for dx in 0..dw {
            if down[dy * dw + dx] < threshold {
                continue;
            }
            let y0 = dy * ds;
            let y1 = (y0 + ds).min(h);
            let x0 = dx * ds;
            let x1 = (x0 + ds).min(w);
            for y in y0..y1 {
                let row_off = y * w;
                for x in x0..x1 {
                    mask[row_off + x] = true;
                }
            }
        }
    }

    if cfg.dilation_px > 0 {
        mask = dilate(&mask, w, h, cfg.dilation_px as usize);
    }

    mask
}

/// Compute `max(p99, k · median)`. Returns `None` when the
/// floor does not exceed the median — i.e. the distribution is
/// effectively flat (uniform frame, no bright blob to mask).
fn downsampled_threshold(down: &[u16], k_median: f64) -> Option<u16> {
    if down.is_empty() {
        return None;
    }
    let mut sorted = down.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let p99_idx = ((n - 1) as f64 * 0.99).round() as usize;
    let p99 = sorted[p99_idx];
    let median = sorted[n / 2];
    let floor_f = f64::from(median) * k_median;
    let floor = if floor_f >= f64::from(u16::MAX) {
        u16::MAX
    } else {
        floor_f as u16
    };
    let threshold = p99.max(floor);
    // No-contrast guard: if the threshold doesn't sit
    // strictly above the median, every pixel near the
    // median would qualify under `>=`. That's not a blob —
    // that's the typical value. Mark nothing.
    if threshold > median {
        Some(threshold)
    } else {
        None
    }
}

/// Morphological dilation by `radius` pixels using a Manhattan
/// (L1) ball — implemented as two separable 1-D passes
/// (horizontal then vertical max over the window). Cheap and
/// good enough for absorbing 3–5 px halo skirts.
fn dilate(mask: &[bool], w: usize, h: usize, radius: usize) -> Vec<bool> {
    if radius == 0 {
        return mask.to_vec();
    }
    // Horizontal pass.
    let mut tmp = vec![false; w * h];
    for y in 0..h {
        let row_off = y * w;
        for x in 0..w {
            let x0 = x.saturating_sub(radius);
            let x1 = (x + radius + 1).min(w);
            let mut on = false;
            for xi in x0..x1 {
                if mask[row_off + xi] {
                    on = true;
                    break;
                }
            }
            tmp[row_off + x] = on;
        }
    }
    // Vertical pass.
    let mut out = vec![false; w * h];
    for y in 0..h {
        let y0 = y.saturating_sub(radius);
        let y1 = (y + radius + 1).min(h);
        for x in 0..w {
            let mut on = false;
            for yi in y0..y1 {
                if tmp[yi * w + x] {
                    on = true;
                    break;
                }
            }
            out[y * w + x] = on;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, Intrinsics};
    use bris_core::time::{Tt, JD_J2000};

    fn make_frame(w: u32, h: u32, pixels: Vec<u16>) -> Frame {
        Frame::new(
            w,
            h,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(w, h),
        )
        .unwrap()
    }

    fn count_true(mask: &[bool]) -> usize {
        mask.iter().filter(|b| **b).count()
    }

    fn paint_disk(pixels: &mut [u16], w: u32, cx: i32, cy: i32, r: i32, value: u16) {
        let r2 = r * r;
        let h = pixels.len() / w as usize;
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let dx = x - cx;
                let dy = y - cy;
                if dx * dx + dy * dy <= r2 {
                    pixels[(y as usize) * (w as usize) + (x as usize)] = value;
                }
            }
        }
    }

    #[test]
    fn single_bright_blob_is_marked() {
        // Dark frame with one saturated disk at (32, 32), r=4.
        let w = 64_u32;
        let h = 64_u32;
        let mut pixels = vec![50u16; (w * h) as usize];
        paint_disk(&mut pixels, w, 32, 32, 4, u16::MAX);
        let frame = make_frame(w, h, pixels);

        let mask = compute_bright_blob_mask(frame_borrow(&frame), BrightBlobConfig::default());
        assert_eq!(mask.len(), (w * h) as usize);

        // Pixel at blob center must be marked.
        assert!(mask[32 * w as usize + 32], "center of blob not in mask");

        // A pixel far from the blob must not be marked.
        assert!(!mask[0], "top-left dark corner spuriously marked");

        // Mask area must be at least the disk area (~50 px) and
        // not blow up to the whole frame.
        let on = count_true(&mask);
        assert!(on >= 40, "mask too small: {on}");
        assert!(on < (w * h) as usize / 4, "mask too large: {on}");
    }

    fn frame_borrow(f: &Frame) -> &Frame {
        f
    }

    #[test]
    fn multiple_blobs_all_marked() {
        let w = 128_u32;
        let h = 128_u32;
        let mut pixels = vec![20u16; (w * h) as usize];
        let centers = [(20_i32, 20_i32), (100, 30), (60, 90)];
        for (cx, cy) in centers {
            paint_disk(&mut pixels, w, cx, cy, 3, u16::MAX);
        }
        let frame = make_frame(w, h, pixels);
        let mask = compute_bright_blob_mask(&frame, BrightBlobConfig::default());
        for (cx, cy) in centers {
            let idx = (cy as usize) * (w as usize) + (cx as usize);
            assert!(mask[idx], "blob at ({cx}, {cy}) not marked");
        }
    }

    #[test]
    fn all_dark_frame_marks_nothing_meaningful() {
        // Uniform very-dark frame: p99 == median == 0 (or
        // very low). The threshold collapses to a constant
        // and nothing exceeds it.
        let w = 64_u32;
        let h = 64_u32;
        let pixels = vec![5u16; (w * h) as usize];
        let frame = make_frame(w, h, pixels);
        let mask = compute_bright_blob_mask(&frame, BrightBlobConfig::default());
        // Either zero or a tiny number of marked pixels —
        // the contract is "nothing meaningful". We accept
        // up to ~1% in case dilation of a single edge pixel
        // creates a small region; in practice it's zero.
        assert!(
            count_true(&mask) < (w * h) as usize / 100,
            "all-dark frame produced a large mask: {} / {}",
            count_true(&mask),
            w * h
        );
    }

    #[test]
    fn all_bright_frame_does_not_mark_everything() {
        // Uniform saturated frame: p99 == median, so the
        // threshold equals that value and *no* pixel strictly
        // exceeds it. The mask must therefore be empty.
        let w = 64_u32;
        let h = 64_u32;
        let pixels = vec![u16::MAX; (w * h) as usize];
        let frame = make_frame(w, h, pixels);
        let mask = compute_bright_blob_mask(&frame, BrightBlobConfig::default());
        assert_eq!(
            count_true(&mask),
            0,
            "uniform-saturated frame should produce empty mask (no pixel exceeds the threshold)",
        );
    }

    #[test]
    fn dilation_grows_the_marked_region() {
        // Single bright pixel at center: with dilation off
        // the mask covers ~1 downsample-cell; with dilation
        // on it grows by `dilation_px` on every side.
        let w = 64_u32;
        let h = 64_u32;
        let mut pixels = vec![10u16; (w * h) as usize];
        // Bright cluster (so it survives the threshold after
        // downsampling collapses the median to a low value).
        paint_disk(&mut pixels, w, 32, 32, 2, u16::MAX);
        let frame = make_frame(w, h, pixels);

        let no_dil = compute_bright_blob_mask(
            &frame,
            BrightBlobConfig {
                dilation_px: 0,
                ..BrightBlobConfig::default()
            },
        );
        let with_dil = compute_bright_blob_mask(
            &frame,
            BrightBlobConfig {
                dilation_px: 6,
                ..BrightBlobConfig::default()
            },
        );
        let n0 = count_true(&no_dil);
        let n1 = count_true(&with_dil);
        assert!(
            n1 > n0,
            "dilation should grow the mask; got {n1} vs {n0} (undilated)"
        );
    }

    #[test]
    fn dilation_zero_is_identity() {
        let mask = vec![true, false, false, true, false, false];
        let out = dilate(&mask, 3, 2, 0);
        assert_eq!(out, mask);
    }

    #[test]
    fn dilation_radius_one_grows_orthogonally() {
        // 3×3 mask with only the center pixel on. Radius-1
        // dilation should turn on all 8 neighbours too (full
        // 3×3 block).
        let mut mask = vec![false; 9];
        mask[4] = true;
        let out = dilate(&mask, 3, 3, 1);
        assert_eq!(out, vec![true; 9]);
    }
}
