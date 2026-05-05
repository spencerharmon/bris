//! Horizon detection from a captured frame.
//!
//! Classical pipeline:
//! 1. Downsample to a working resolution (~200 px wide). Removes
//!    high-frequency noise; horizon is a low-frequency feature.
//! 2. Compute vertical gradient (Sobel-y).
//! 3. For each column, locate the row of maximum |gradient| above a
//!    threshold. This produces a list of candidate horizon points.
//! 4. RANSAC line fit. Inliers are the consensus horizon; outliers
//!    are clouds, foreground vessels, masts, lens flares, etc.
//! 5. Return the fit line plus residual statistics so callers know
//!    how much to trust the result.
//!
//! # Why not `imageproc`?
//!
//! Each step is simple enough to implement directly, and avoiding the
//! dependency keeps the binary lean and the algorithm fully visible
//! for review. If we ever need richer primitives (Hough transform,
//! Canny, etc.) we can add `imageproc` then.

// Image arithmetic uses casts between u32, usize, and f64 throughout.
// These are pixel coordinates and dimensions; they cannot meaningfully
// overflow in any realistic camera frame. Suppress the corresponding
// clippy lints for this module rather than peppering the code.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]

use crate::frame::Frame;
use bris_core::Sigma;

/// A detected horizon line in *full-resolution* image coordinates.
///
/// The line is parameterized as `y = slope · x + intercept`, with
/// (x, y) in pixels relative to the image origin (top-left, +y down).
/// This formulation is fine because horizons are nearly horizontal
/// in any reasonable mounted-camera frame; we never see a vertical
/// horizon. If we ever need to handle gimbal-mounted cameras with
/// large roll, switch to a normal-form line representation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HorizonLine {
    /// Slope `dy/dx` in pixels per pixel.
    pub slope: f64,
    /// y-intercept in pixels.
    pub intercept: f64,
    /// Number of inlier columns supporting the fit.
    pub inlier_count: u32,
    /// Total candidate columns considered.
    pub candidate_count: u32,
    /// Per-inlier RMS residual, pixels.
    pub residual_rms_px: f64,
    /// 1σ uncertainty in the *altitude* contribution from the horizon
    /// fit, derived from the residual RMS and the camera's instantaneous
    /// vertical FOV. This is the value [`bris_nav`] consumes to add
    /// horizon-fit error to the per-sight altitude covariance.
    pub altitude_sigma: Sigma,
}

/// Errors from the horizon detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HorizonError {
    /// Too few candidate columns survived gradient thresholding to
    /// even attempt a fit. Caller should treat as "horizon not
    /// detected" and retry on the next frame, or report failure.
    #[error("not enough candidate columns ({0}) to fit a horizon")]
    InsufficientCandidates(u32),
    /// RANSAC found a fit but with too few inliers to trust.
    #[error("horizon fit had only {0} inliers (need ≥ {1})")]
    LowConfidence(u32, u32),
}

/// Configuration for [`detect_horizon`]. All fields have sensible
/// defaults exposed via [`HorizonConfig::default`].
#[derive(Debug, Clone, Copy)]
pub struct HorizonConfig {
    /// Working resolution width (pixels). Frames are downsampled to
    /// this width before gradient computation. Default 200.
    pub working_width: u32,
    /// Minimum gradient magnitude (in u16-difference units) for a
    /// column to contribute a candidate point. Default 800 — tuned
    /// for typical 12-bit camera input where the sea-sky transition
    /// is several thousand counts.
    pub gradient_threshold: u16,
    /// Number of RANSAC iterations. Default 200.
    pub ransac_iterations: u32,
    /// RANSAC inlier distance threshold (pixels at working
    /// resolution). Default 2.0.
    pub ransac_inlier_px: f64,
    /// Minimum inlier count to accept a fit, as a fraction of
    /// candidates. Default 0.5.
    pub min_inlier_fraction: f64,
}

impl Default for HorizonConfig {
    fn default() -> Self {
        Self {
            working_width: 200,
            gradient_threshold: 800,
            ransac_iterations: 200,
            ransac_inlier_px: 2.0,
            min_inlier_fraction: 0.5,
        }
    }
}

/// Detect the sea horizon in a frame.
///
/// # Errors
///
/// Returns `Err` if too few columns produced strong vertical
/// gradients to fit a line, or if the RANSAC fit had too few inliers
/// to be trustworthy. Both should be surfaced to the operator as
/// "horizon not detected" rather than fabricating a fit.
#[allow(clippy::similar_names)] // x0/y0/x1/y1 are box-filter coords.
pub fn detect_horizon(frame: &Frame, cfg: HorizonConfig) -> Result<HorizonLine, HorizonError> {
    // Step 1: downsample.
    let scale = f64::from(frame.width()) / f64::from(cfg.working_width);
    let working_height = (f64::from(frame.height()) / scale).round() as u32;
    let work = downsample(frame, cfg.working_width, working_height);

    // Step 2 & 3: per-column row of strongest vertical gradient.
    let candidates = column_gradient_peaks(&work, cfg.gradient_threshold);

    if candidates.len() < 10 {
        #[allow(clippy::cast_possible_truncation)]
        return Err(HorizonError::InsufficientCandidates(candidates.len() as u32));
    }

    // Step 4: RANSAC line fit.
    let fit = ransac_line(&candidates, cfg.ransac_iterations, cfg.ransac_inlier_px);

    let candidate_count = candidates.len();
    let min_inliers = ((candidate_count as f64) * cfg.min_inlier_fraction).ceil() as u32;
    if fit.inlier_count < min_inliers {
        return Err(HorizonError::LowConfidence(fit.inlier_count, min_inliers));
    }

    // Convert working-resolution fit back to full-resolution.
    // y_full = slope_full · x_full + intercept_full
    // x_work = x_full / scale, y_work = y_full / scale
    // y_work = slope_work · x_work + intercept_work
    // → y_full = slope_work · x_full + intercept_work · scale
    let slope_full = fit.slope;
    let intercept_full = fit.intercept * scale;
    let residual_full_px = fit.residual_rms * scale;

    // Convert per-pixel residual to an altitude σ. The full image
    // covers (height / fy) radians of vertical FOV; one pixel ≈
    // 1 / fy radians. So altitude σ ≈ residual_px / fy.
    let altitude_sigma_rad = residual_full_px / frame.intrinsics.fy;
    let altitude_sigma = Sigma::new(altitude_sigma_rad).unwrap_or(Sigma::ZERO);

    Ok(HorizonLine {
        slope: slope_full,
        intercept: intercept_full,
        #[allow(clippy::cast_possible_truncation)]
        inlier_count: fit.inlier_count,
        #[allow(clippy::cast_possible_truncation)]
        candidate_count: candidate_count as u32,
        residual_rms_px: residual_full_px,
        altitude_sigma,
    })
}

/// Box-filter downsample to `(out_w, out_h)`. Each output pixel is
/// the mean of the corresponding source rectangle. Adequate for
/// horizon detection; we are not preserving high-frequency detail.
fn downsample(frame: &Frame, out_w: u32, out_h: u32) -> WorkingImage {
    let in_w = frame.width();
    let in_h = frame.height();
    let mut out = vec![0u16; (out_w as usize) * (out_h as usize)];
    let sx = f64::from(in_w) / f64::from(out_w);
    let sy = f64::from(in_h) / f64::from(out_h);
    for oy in 0..out_h {
        let y0 = (f64::from(oy) * sy).floor() as u32;
        let y1 = ((f64::from(oy + 1) * sy).ceil() as u32).min(in_h);
        for ox in 0..out_w {
            let x0 = (f64::from(ox) * sx).floor() as u32;
            let x1 = ((f64::from(ox + 1) * sx).ceil() as u32).min(in_w);
            let mut sum: u64 = 0;
            let mut count: u64 = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    if let Some(p) = frame.pixel(x, y) {
                        sum += u64::from(p);
                        count += 1;
                    }
                }
            }
            let mean = if count == 0 {
                0
            } else {
                u16::try_from(sum / count).unwrap_or(u16::MAX)
            };
            out[(oy as usize) * (out_w as usize) + (ox as usize)] = mean;
        }
    }
    WorkingImage {
        width: out_w,
        height: out_h,
        pixels: out,
    }
}

struct WorkingImage {
    width: u32,
    height: u32,
    pixels: Vec<u16>,
}

impl WorkingImage {
    fn pixel(&self, x: u32, y: u32) -> u16 {
        self.pixels[(y as usize) * (self.width as usize) + (x as usize)]
    }
}

/// For each column, find the row at which the absolute vertical
/// gradient is maximum, provided it exceeds `threshold`. Uses a
/// 3-tap [-1, 0, +1] Sobel-y kernel for simplicity (full Sobel
/// would smooth horizontally; we don't need that for a near-
/// horizontal feature).
fn column_gradient_peaks(img: &WorkingImage, threshold: u16) -> Vec<(f64, f64)> {
    let mut points = Vec::new();
    let h = img.height;
    let w = img.width;
    if h < 3 {
        return points;
    }
    for x in 0..w {
        let mut best_grad: i32 = 0;
        let mut best_y: u32 = 0;
        for y in 1..(h - 1) {
            let above = i32::from(img.pixel(x, y - 1));
            let below = i32::from(img.pixel(x, y + 1));
            let grad = (below - above).abs();
            if grad > best_grad {
                best_grad = grad;
                best_y = y;
            }
        }
        if u32::try_from(best_grad).unwrap_or(0) >= u32::from(threshold) {
            points.push((f64::from(x), f64::from(best_y)));
        }
    }
    points
}

struct LineFit {
    slope: f64,
    intercept: f64,
    inlier_count: u32,
    residual_rms: f64,
}

/// RANSAC line fitting on a set of (x, y) candidates.
///
/// On each iteration: pick two random points, fit a line through them,
/// count inliers within `inlier_px`. Track the best inlier count.
/// Final step: least-squares refit over the inliers of the best
/// hypothesis, returning the refined line and its RMS residual.
///
/// The PRNG is a tiny self-contained xorshift seeded from the candidate
/// data so RANSAC is deterministic given the same input — important
/// for reproducible tests and for diff-able replays from saved frames.
fn ransac_line(points: &[(f64, f64)], iterations: u32, inlier_px: f64) -> LineFit {
    let n = points.len();
    if n < 2 {
        return LineFit {
            slope: 0.0,
            intercept: 0.0,
            inlier_count: 0,
            residual_rms: f64::INFINITY,
        };
    }

    // Seed PRNG from data to keep results reproducible.
    let mut seed: u64 = 0xa5a5_5a5a_5a5a_a5a5;
    for &(x, y) in points {
        seed ^= x.to_bits().wrapping_mul(0x9E37_79B9_7F4A_7C15);
        seed = seed.rotate_left(13);
        seed ^= y.to_bits().wrapping_mul(0xBF58_476D_1CE4_E5B9);
    }

    let mut best_inliers: Vec<usize> = Vec::new();
    for _ in 0..iterations {
        let i = next_index(&mut seed, n);
        let mut j = next_index(&mut seed, n);
        if j == i {
            j = (j + 1) % n;
        }
        let (x1, y1) = points[i];
        let (x2, y2) = points[j];
        if (x2 - x1).abs() < 1e-9 {
            continue; // vertical line; skip
        }
        let slope = (y2 - y1) / (x2 - x1);
        let intercept = y1 - slope * x1;

        let mut inliers = Vec::new();
        for (k, &(x, y)) in points.iter().enumerate() {
            let predicted = slope * x + intercept;
            if (predicted - y).abs() <= inlier_px {
                inliers.push(k);
            }
        }
        if inliers.len() > best_inliers.len() {
            best_inliers = inliers;
        }
    }

    if best_inliers.is_empty() {
        return LineFit {
            slope: 0.0,
            intercept: 0.0,
            inlier_count: 0,
            residual_rms: f64::INFINITY,
        };
    }

    // Refit by least squares over inliers.
    let inlier_pts: Vec<(f64, f64)> = best_inliers.iter().map(|&k| points[k]).collect();
    let (slope, intercept) = least_squares_line(&inlier_pts);
    let mut sum_sq = 0.0;
    for &(x, y) in &inlier_pts {
        let r = slope * x + intercept - y;
        sum_sq += r * r;
    }
    let residual_rms = (sum_sq / inlier_pts.len() as f64).sqrt();

    LineFit {
        slope,
        intercept,
        #[allow(clippy::cast_possible_truncation)]
        inlier_count: inlier_pts.len() as u32,
        residual_rms,
    }
}

fn next_index(seed: &mut u64, modulus: usize) -> usize {
    // xorshift64*
    let mut x = *seed;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *seed = x;
    let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    (r as usize) % modulus
}

#[allow(clippy::similar_names)] // sum_xx, sum_xy are domain-standard.
fn least_squares_line(points: &[(f64, f64)]) -> (f64, f64) {
    let n = points.len() as f64;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    let mut sum_xx = 0.0;
    let mut sum_xy = 0.0;
    for &(x, y) in points {
        sum_x += x;
        sum_y += y;
        sum_xx += x * x;
        sum_xy += x * y;
    }
    let denom = n * sum_xx - sum_x * sum_x;
    if denom.abs() < 1e-12 {
        return (0.0, sum_y / n);
    }
    let slope = (n * sum_xy - sum_x * sum_y) / denom;
    let intercept = (sum_y - slope * sum_x) / n;
    (slope, intercept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Intrinsics;
    use bris_core::time::{Tt, JD_J2000};

    /// Synthesize a frame with a clear horizontal horizon at row `y_horizon`.
    /// Above the horizon is bright (sky); below is dark (sea).
    fn synth_horizon_frame(width: u32, height: u32, y_horizon: u32) -> Frame {
        let mut pixels = vec![0u16; (width as usize) * (height as usize)];
        for y in 0..height {
            for x in 0..width {
                let v = if y < y_horizon { 50_000 } else { 5_000 };
                pixels[(y as usize) * (width as usize) + (x as usize)] = v;
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

    /// Synthesize a tilted horizon at the given slope and intercept.
    fn synth_tilted_horizon(width: u32, height: u32, slope: f64, intercept: f64) -> Frame {
        let mut pixels = vec![0u16; (width as usize) * (height as usize)];
        for y in 0..height {
            for x in 0..width {
                let horizon_y = slope * f64::from(x) + intercept;
                let v = if f64::from(y) < horizon_y {
                    50_000
                } else {
                    5_000
                };
                pixels[(y as usize) * (width as usize) + (x as usize)] = v;
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
    fn detects_horizontal_horizon_at_known_row() {
        let frame = synth_horizon_frame(800, 600, 300);
        let line = detect_horizon(&frame, HorizonConfig::default()).unwrap();
        // Horizon is at y=300 across the full image.
        // Slope should be ~0; intercept ~300.
        assert!(
            line.slope.abs() < 0.01,
            "slope {} too large for flat horizon",
            line.slope
        );
        assert!(
            (line.intercept - 300.0).abs() < 5.0,
            "intercept {} should be near 300",
            line.intercept
        );
        assert!(line.inlier_count > 100, "too few inliers");
    }

    #[test]
    fn detects_tilted_horizon() {
        // Slope of 0.05 corresponds to ~3° camera roll.
        let frame = synth_tilted_horizon(800, 600, 0.05, 300.0);
        let line = detect_horizon(&frame, HorizonConfig::default()).unwrap();
        assert!(
            (line.slope - 0.05).abs() < 0.005,
            "slope {} should be near 0.05",
            line.slope
        );
        assert!(
            (line.intercept - 300.0).abs() < 5.0,
            "intercept {} should be near 300",
            line.intercept
        );
    }

    #[test]
    fn fails_on_uniform_image() {
        // No gradient anywhere → no candidates → InsufficientCandidates.
        let frame = synth_horizon_frame(200, 150, 1_000_000); // horizon "below" image
        let result = detect_horizon(&frame, HorizonConfig::default());
        assert!(matches!(
            result,
            Err(HorizonError::InsufficientCandidates(_))
        ));
    }

    #[test]
    fn altitude_sigma_finite_and_positive() {
        let frame = synth_horizon_frame(800, 600, 300);
        let line = detect_horizon(&frame, HorizonConfig::default()).unwrap();
        assert!(line.altitude_sigma.value().is_finite());
        // Synthetic noise-free horizon should yield very small sigma.
        // Conservative check: < 1 degree.
        let sigma_deg = line.altitude_sigma.value().to_degrees();
        assert!(
            sigma_deg < 1.0,
            "altitude sigma = {sigma_deg}° unexpectedly large"
        );
    }
}
