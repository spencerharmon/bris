//! Star-like peak detection.
//!
//! Stars are point sources: a Gaussian blob a few pixels wide with no
//! gradient direction. The Harris corner detector reports near-zero
//! response for these because the structure tensor is isotropic. Star
//! detection needs a different approach.
//!
//! # Algorithm
//!
//! 1. Estimate a local background by box-filtering the frame at a
//!    coarse scale (~32 px). Subtract the background to flatten gradual
//!    illumination changes (sky glow, vignetting).
//! 2. Find local maxima above a fixed threshold in a small window
//!    (~5×5 px).
//! 3. Refine each peak's position to sub-pixel accuracy via a
//!    Gaussian-fit centroid in a 3×3 neighborhood.
//! 4. Rank by brightness, keep the top N.
//!
//! # When to use this vs `track::detect_corners`
//!
//! - Star fields, Sun/Moon glints, lens flare points → peaks.
//! - Cloud edges, sea texture, daytime structure → corners.
//!
//! The streaming engine will pick the appropriate detector based on
//! frame statistics (mean brightness, saturated-region count). For
//! now both are exposed as separate functions.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_possible_wrap
)]

use crate::frame::Frame;

/// A detected star-like peak.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Peak {
    /// Sub-pixel x coordinate, pixels.
    pub x: f64,
    /// Sub-pixel y coordinate, pixels.
    pub y: f64,
    /// Background-subtracted peak intensity (u16-scale).
    pub intensity: f64,
}

/// Configuration for [`detect_peaks`].
#[derive(Debug, Clone, Copy)]
pub struct PeakConfig {
    /// Half-size of the box filter used for background estimation.
    /// Default 16 (so the box is 33×33 px). Should be larger than
    /// the largest expected star/blob radius.
    pub background_half_size: u32,
    /// Half-size of the local-maximum search window. Default 2 (5×5).
    pub maximum_half_size: u32,
    /// Minimum background-subtracted peak intensity (u16-scale).
    /// Default 2000 — calibrated for a typical 12-bit camera with a
    /// dark sky background. Peaks below this are rejected as noise.
    pub min_intensity: u16,
    /// Number of strongest peaks to return. Default 200.
    pub max_peaks: u32,
}

impl Default for PeakConfig {
    fn default() -> Self {
        Self {
            background_half_size: 16,
            maximum_half_size: 2,
            min_intensity: 2000,
            max_peaks: 200,
        }
    }
}

/// Detect star-like peaks in a frame.
#[must_use]
pub fn detect_peaks(frame: &Frame, cfg: PeakConfig) -> Vec<Peak> {
    let w = frame.width() as usize;
    let h = frame.height() as usize;
    if w < (2 * cfg.background_half_size as usize + 1)
        || h < (2 * cfg.background_half_size as usize + 1)
    {
        return Vec::new();
    }
    let pixels = frame.pixels();

    // Step 1: background estimate via separable box filter.
    let bg = box_filter(pixels, w, h, cfg.background_half_size);

    // Step 2: foreground = max(0, pixel − background).
    let mut foreground = vec![0i32; w * h];
    for i in 0..pixels.len() {
        let f = i32::from(pixels[i]) - bg[i];
        foreground[i] = f.max(0);
    }

    // Step 3: local-maximum search over the foreground.
    let k = cfg.maximum_half_size as i32;
    let threshold = i32::from(cfg.min_intensity);
    let mut raw_peaks: Vec<(u32, u32, i32)> = Vec::new();
    for y in (k as usize)..(h - k as usize) {
        for x in (k as usize)..(w - k as usize) {
            let v = foreground[y * w + x];
            if v < threshold {
                continue;
            }
            let mut is_max = true;
            'nms: for dy in -k..=k {
                for dx in -k..=k {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let nx = (x as i32 + dx) as usize;
                    let ny = (y as i32 + dy) as usize;
                    if foreground[ny * w + nx] >= v {
                        is_max = false;
                        break 'nms;
                    }
                }
            }
            if is_max {
                raw_peaks.push((x as u32, y as u32, v));
            }
        }
    }

    // Step 4: rank and keep top N. We do this *before* sub-pixel
    // refinement because refinement is the most expensive step per
    // peak and we don't want to refine noise peaks we're going to
    // discard.
    raw_peaks.sort_by(|a, b| b.2.cmp(&a.2));
    raw_peaks.truncate(cfg.max_peaks as usize);

    // Step 5: sub-pixel refinement via 3×3 Gaussian centroid (intensity-
    // weighted moments). For a near-Gaussian blob this is unbiased to
    // first order and accurate to ~0.1 px for SNR > 5.
    let mut peaks = Vec::with_capacity(raw_peaks.len());
    for &(px, py, _v) in &raw_peaks {
        if let Some(refined) = refine_subpixel(&foreground, w, h, px, py) {
            peaks.push(refined);
        }
    }
    peaks
}

/// Box filter (separable, two passes) to estimate local mean.
/// Returns an i32 buffer (so subtraction can produce negative values).
fn box_filter(pixels: &[u16], w: usize, h: usize, half: u32) -> Vec<i32> {
    let half = half as usize;
    let mut horizontal = vec![0i32; w * h];
    // Horizontal pass: for each row, prefix-sum + window-average.
    for y in 0..h {
        let mut prefix: Vec<i64> = Vec::with_capacity(w + 1);
        prefix.push(0);
        for x in 0..w {
            prefix.push(prefix[x] + i64::from(pixels[y * w + x]));
        }
        for x in 0..w {
            let lo = x.saturating_sub(half);
            let hi = (x + half + 1).min(w);
            let count = (hi - lo) as i64;
            let sum = prefix[hi] - prefix[lo];
            horizontal[y * w + x] = (sum / count) as i32;
        }
    }
    // Vertical pass.
    let mut vertical = vec![0i32; w * h];
    for x in 0..w {
        let mut prefix: Vec<i64> = Vec::with_capacity(h + 1);
        prefix.push(0);
        for y in 0..h {
            prefix.push(prefix[y] + i64::from(horizontal[y * w + x]));
        }
        for y in 0..h {
            let lo = y.saturating_sub(half);
            let hi = (y + half + 1).min(h);
            let count = (hi - lo) as i64;
            let sum = prefix[hi] - prefix[lo];
            vertical[y * w + x] = (sum / count) as i32;
        }
    }
    vertical
}

/// Refine peak position via intensity-weighted centroid of a 3×3
/// neighborhood. Returns `None` if the peak is on a frame edge (no
/// 3×3 window available) — the local-max search guarantees that's
/// already not the case, so this is defensive.
#[allow(clippy::many_single_char_names)] // dx, dy, x, y, w, h are domain-standard
fn refine_subpixel(foreground: &[i32], w: usize, h: usize, px: u32, py: u32) -> Option<Peak> {
    let x = px as usize;
    let y = py as usize;
    if x == 0 || y == 0 || x + 1 >= w || y + 1 >= h {
        return None;
    }
    let mut sum_w: f64 = 0.0;
    let mut sum_x: f64 = 0.0;
    let mut sum_y: f64 = 0.0;
    let mut peak_intensity = 0.0;
    for dy in -1i32..=1 {
        for dx in -1i32..=1 {
            let nx = (x as i32 + dx) as usize;
            let ny = (y as i32 + dy) as usize;
            let v = foreground[ny * w + nx] as f64;
            if v <= 0.0 {
                continue;
            }
            sum_w += v;
            sum_x += v * (nx as f64);
            sum_y += v * (ny as f64);
            if dx == 0 && dy == 0 {
                peak_intensity = v;
            }
        }
    }
    if sum_w <= 0.0 {
        return None;
    }
    Some(Peak {
        x: sum_x / sum_w,
        y: sum_y / sum_w,
        intensity: peak_intensity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Intrinsics;
    use approx::assert_relative_eq;
    use bris_core::time::{Tt, JD_J2000};

    /// Build a frame with bright Gaussian "stars" placed at given
    /// (cx, cy, `peak_intensity`) locations against a dark background.
    fn synth_star_field(width: u32, height: u32, stars: &[(f64, f64, u16)]) -> Frame {
        let mut pixels = vec![100u16; (width as usize) * (height as usize)];
        let sigma = 1.5_f64;
        let half = 4_i32;
        for &(cx, cy, peak) in stars {
            for dy in -half..=half {
                for dx in -half..=half {
                    let x = (cx + dx as f64).round() as i32;
                    let y = (cy + dy as f64).round() as i32;
                    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                        continue;
                    }
                    let r2 = (cx - x as f64).powi(2) + (cy - y as f64).powi(2);
                    let g = (-r2 / (2.0 * sigma * sigma)).exp();
                    let v = (f64::from(peak) * g) as u16;
                    let idx = (y as usize) * (width as usize) + (x as usize);
                    pixels[idx] = pixels[idx].saturating_add(v);
                }
            }
        }
        Frame::new(
            width,
            height,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    #[test]
    fn detects_isolated_stars() {
        let stars = [
            (50.0_f64, 60.0, 30_000),
            (120.0, 80.0, 20_000),
            (200.0, 150.0, 25_000),
        ];
        let frame = synth_star_field(320, 240, &stars);
        let peaks = detect_peaks(&frame, PeakConfig::default());
        assert!(
            peaks.len() >= stars.len(),
            "expected at least {} peaks, got {}",
            stars.len(),
            peaks.len()
        );
        // Each synthetic star should have a peak within 1 px.
        for &(cx, cy, _) in &stars {
            let nearest = peaks
                .iter()
                .min_by(|a, b| {
                    let da = (a.x - cx).powi(2) + (a.y - cy).powi(2);
                    let db = (b.x - cx).powi(2) + (b.y - cy).powi(2);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap();
            let d = ((nearest.x - cx).powi(2) + (nearest.y - cy).powi(2)).sqrt();
            assert!(
                d < 1.0,
                "no peak within 1 px of ({cx}, {cy}); nearest was ({}, {}) at distance {d:.2}",
                nearest.x,
                nearest.y
            );
        }
    }

    #[test]
    fn ignores_uniform_background() {
        let pixels = vec![5_000u16; 320 * 240];
        let frame = Frame::new(
            320,
            240,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(320, 240),
        )
        .unwrap();
        let peaks = detect_peaks(&frame, PeakConfig::default());
        assert!(peaks.is_empty(), "expected no peaks, got {}", peaks.len());
    }

    #[test]
    fn ignores_gradient_only_background() {
        // A smooth horizontal gradient should not produce peaks — the
        // background subtraction flattens it.
        let mut pixels = vec![0u16; 320 * 240];
        for y in 0..240 {
            for x in 0..320 {
                pixels[(y as usize) * 320 + (x as usize)] = (x * 200) as u16;
            }
        }
        let frame = Frame::new(
            320,
            240,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(320, 240),
        )
        .unwrap();
        let peaks = detect_peaks(&frame, PeakConfig::default());
        // A gradient may produce a few spurious peaks at the brightest
        // edge depending on background filter; tolerate up to a small
        // number rather than asserting zero.
        assert!(
            peaks.len() < 5,
            "gradient produced too many peaks: {}",
            peaks.len()
        );
    }

    #[test]
    fn subpixel_centroid_accurate_for_off_center_peak() {
        // Place a star at (50.3, 60.7) — sub-pixel offsets.
        let stars = [(50.3_f64, 60.7, 30_000)];
        let frame = synth_star_field(320, 240, &stars);
        let peaks = detect_peaks(&frame, PeakConfig::default());
        let nearest = peaks
            .iter()
            .min_by(|a, b| {
                let da = (a.x - 50.3).powi(2) + (a.y - 60.7).powi(2);
                let db = (b.x - 50.3).powi(2) + (b.y - 60.7).powi(2);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();
        // Sub-pixel accuracy: should be within 0.3 px even though we
        // only used a 3×3 centroid.
        assert_relative_eq!(nearest.x, 50.3, epsilon = 0.3);
        assert_relative_eq!(nearest.y, 60.7, epsilon = 0.3);
    }

    #[test]
    fn ranks_by_intensity() {
        let stars = [
            (50.0_f64, 60.0, 10_000),
            (120.0, 80.0, 30_000),
            (200.0, 150.0, 20_000),
        ];
        let frame = synth_star_field(320, 240, &stars);
        let peaks = detect_peaks(&frame, PeakConfig::default());
        // Top peak should be the brightest star (at 120, 80).
        let top = peaks.first().expect("at least one peak");
        let d_to_brightest = ((top.x - 120.0).powi(2) + (top.y - 80.0).powi(2)).sqrt();
        assert!(
            d_to_brightest < 1.0,
            "top peak should be near (120, 80); got ({}, {})",
            top.x,
            top.y
        );
    }

    #[test]
    fn caps_at_max_peaks() {
        // Generate many stars; verify the cap.
        let mut stars: Vec<(f64, f64, u16)> = Vec::new();
        for i in 0..100 {
            stars.push((
                (10 + (i % 30) * 10) as f64,
                (10 + (i / 30) * 20) as f64,
                10_000 + (i as u16 * 100),
            ));
        }
        let frame = synth_star_field(320, 240, &stars);
        let cfg = PeakConfig {
            max_peaks: 25,
            ..PeakConfig::default()
        };
        let peaks = detect_peaks(&frame, cfg);
        assert!(peaks.len() <= 25, "got {} peaks, max was 25", peaks.len());
    }
}
