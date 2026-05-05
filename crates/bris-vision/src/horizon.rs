//! Horizon detection from a captured frame.
//!
//! Two strategies are available:
//!
//! 1. [`detect_horizon`] — gradient + RANSAC. Fast and robust on
//!    open-ocean scenes where the sea-sky boundary is the dominant
//!    horizontal edge in the frame. Tends to be fooled in cluttered
//!    scenes (deck-mounted cameras with sail / rigging / boat
//!    structure occupying the lower half).
//!
//! 2. [`detect_horizon_via_sky_region`] — find the bright sky region
//!    first (largest connected component touching the top of the
//!    frame above a brightness threshold), then take its lower
//!    boundary as the horizon. Robust against deck/sail/rigging
//!    occlusion because those don't extend up to and connect with
//!    the sky region.
//!
//! Both detectors share the downsample → column-candidates → RANSAC
//! line-fit → uncertainty machinery; only the candidate-extraction
//! step differs.
//!
//! Neither approach handles night frames where the entire scene is
//! dark; that case needs IMU-assisted dead reckoning of the horizon
//! direction or a "horizon not visible — supply manually" mode.

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
    /// Brightness percentile used by [`detect_horizon_via_sky_region`]
    /// to threshold sky pixels. The sky is typically the brightest
    /// large region in a daytime marine scene; the default 0.6 (60th
    /// percentile) means "pixels brighter than 60% of the frame's
    /// pixels are sky candidates." Lower values include more sky,
    /// risking inclusion of bright sails or sun glint; higher values
    /// risk missing dim sky.
    pub sky_brightness_percentile: f64,
}

impl Default for HorizonConfig {
    fn default() -> Self {
        Self {
            working_width: 200,
            gradient_threshold: 800,
            ransac_iterations: 200,
            ransac_inlier_px: 2.0,
            min_inlier_fraction: 0.5,
            sky_brightness_percentile: 0.6,
        }
    }
}

/// Detect the sea horizon in a frame via per-column gradient peaks.
///
/// Best for open-ocean scenes where the sea-sky boundary is the
/// dominant horizontal edge in the frame. Tends to be fooled by
/// stronger competing horizontal edges (deck rails, sail edges,
/// boom shadows) in cluttered shipboard scenes; for those, see
/// [`detect_horizon_via_sky_region`].
///
/// # Errors
///
/// Returns `Err` if too few columns produced strong vertical
/// gradients to fit a line, or if the RANSAC fit had too few inliers
/// to be trustworthy. Both should be surfaced to the operator as
/// "horizon not detected" rather than fabricating a fit.
#[allow(clippy::similar_names)] // x0/y0/x1/y1 are box-filter coords.
pub fn detect_horizon(frame: &Frame, cfg: HorizonConfig) -> Result<HorizonLine, HorizonError> {
    let scale = f64::from(frame.width()) / f64::from(cfg.working_width);
    let working_height = (f64::from(frame.height()) / scale).round() as u32;
    let work = downsample(frame, cfg.working_width, working_height);
    let candidates = column_gradient_peaks(&work, cfg.gradient_threshold);
    finalize_horizon(frame, &candidates, scale, &cfg)
}

/// Detect the sea horizon by finding the bright sky region first,
/// then taking its lower boundary.
///
/// Best for cluttered shipboard scenes where the deck, sail, and
/// rigging dominate the frame's horizontal-gradient features. The
/// sky region is identified as the largest connected component of
/// bright pixels touching the top of the frame; its bottom boundary
/// is by construction either a sky-sea or a sky-other edge, with the
/// sky-other edges naturally rejected as RANSAC outliers when the
/// sky-sea boundary spans more columns.
///
/// # Errors
///
/// Returns `Err` if no sky region is found or the RANSAC fit fails
/// the same confidence checks as [`detect_horizon`].
pub fn detect_horizon_via_sky_region(
    frame: &Frame,
    cfg: HorizonConfig,
) -> Result<HorizonLine, HorizonError> {
    let scale = f64::from(frame.width()) / f64::from(cfg.working_width);
    let working_height = (f64::from(frame.height()) / scale).round() as u32;
    let work = downsample(frame, cfg.working_width, working_height);
    let candidates = sky_region_lower_boundary(&work, cfg.sky_brightness_percentile);
    finalize_horizon(frame, &candidates, scale, &cfg)
}

/// Shared post-extraction pipeline: RANSAC, refit, and uncertainty
/// computation. Both detectors call this with a slice of working-
/// resolution candidate points (in (x, y) pixel coordinates).
fn finalize_horizon(
    frame: &Frame,
    candidates: &[(f64, f64)],
    scale: f64,
    cfg: &HorizonConfig,
) -> Result<HorizonLine, HorizonError> {
    if candidates.len() < 10 {
        return Err(HorizonError::InsufficientCandidates(candidates.len() as u32));
    }

    let fit = ransac_line(candidates, cfg.ransac_iterations, cfg.ransac_inlier_px);

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

    let altitude_sigma_rad = residual_full_px / frame.intrinsics.fy;
    let altitude_sigma = Sigma::new(altitude_sigma_rad).unwrap_or(Sigma::ZERO);

    Ok(HorizonLine {
        slope: slope_full,
        intercept: intercept_full,
        inlier_count: fit.inlier_count,
        candidate_count: candidate_count as u32,
        residual_rms_px: residual_full_px,
        altitude_sigma,
    })
}

/// Find the lower boundary of the sky region, column by column.
///
/// 1. Compute a brightness threshold at the configured percentile.
///    The sky is typically the brightest large connected region in
///    a daytime marine scene.
/// 2. Two-pass connected-components labeling on the thresholded image.
/// 3. Pick the largest component that touches the top row of the frame
///    (touching the top is what distinguishes "sky" from "bright sail
///    edge" or "sun glint cluster").
/// 4. For each column, find the lowest row that's still part of that
///    component. That's the column's sky-bottom y.
/// 5. Skip columns where the sky doesn't reach (no boundary point).
fn sky_region_lower_boundary(img: &WorkingImage, brightness_percentile: f64) -> Vec<(f64, f64)> {
    let w = img.width;
    let h = img.height;
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let pixels: &[u16] = &img.pixels;

    // Step 1: percentile-based threshold. Sort a copy of the pixels;
    // for working-resolution images (typically 200×height ~ 200×113)
    // this is < 25k elements, fast.
    let mut sorted: Vec<u16> = pixels.to_vec();
    sorted.sort_unstable();
    let idx = ((sorted.len() as f64 - 1.0) * brightness_percentile.clamp(0.0, 1.0)) as usize;
    let threshold = sorted[idx];

    // Step 2: connected components on the thresholded image.
    let labels = connected_components_above(pixels, w, h, threshold);

    // Step 3: pick the largest component touching the top row.
    let mut top_labels: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for x in 0..w {
        let lbl = labels[x as usize];
        if lbl > 0 {
            top_labels.insert(lbl);
        }
    }
    if top_labels.is_empty() {
        return Vec::new();
    }
    let mut areas: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for &lbl in &labels {
        if top_labels.contains(&lbl) {
            *areas.entry(lbl).or_insert(0) += 1;
        }
    }
    let sky_label = areas
        .into_iter()
        .max_by_key(|&(_, area)| area)
        .map_or(0, |(lbl, _)| lbl);
    if sky_label == 0 {
        return Vec::new();
    }

    // Step 4: per-column lower boundary.
    let mut points = Vec::new();
    for x in 0..w {
        let mut last_sky_y: Option<u32> = None;
        for y in 0..h {
            if labels[(y * w + x) as usize] == sky_label {
                last_sky_y = Some(y);
            }
        }
        if let Some(y) = last_sky_y {
            // Skip columns where the sky reaches all the way to the
            // bottom; that's not a horizon, that's clear sky obscuring
            // whatever's below (or a frame with no sea visible).
            if y < h - 1 {
                points.push((f64::from(x), f64::from(y)));
            }
        }
    }
    points
}

/// Two-pass connected-components labeling for pixels above `threshold`.
/// Returns a label per pixel; 0 means below threshold (background).
///
/// 4-connectivity. Same union-find approach as `centroid::label_components`
/// but works on `&[u16]` so we don't need to construct a `Frame`.
fn connected_components_above(pixels: &[u16], w: u32, h: u32, threshold: u16) -> Vec<u32> {
    let w = w as usize;
    let h = h as usize;
    let mut labels = vec![0u32; w * h];
    let mut parent: Vec<u32> = vec![0]; // index 0 is reserved background
    let mut next_label: u32 = 1;

    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            if pixels[idx] < threshold {
                continue;
            }
            let left = if x > 0 { labels[idx - 1] } else { 0 };
            let above = if y > 0 { labels[idx - w] } else { 0 };
            let lbl = match (left, above) {
                (0, 0) => {
                    let new = next_label;
                    next_label += 1;
                    parent.push(new);
                    new
                }
                (a, 0) | (0, a) => a,
                (a, b) if a == b => a,
                (a, b) => {
                    union_find_union(&mut parent, a, b);
                    a.min(b)
                }
            };
            labels[idx] = lbl;
        }
    }

    for lbl in &mut labels {
        if *lbl > 0 {
            *lbl = union_find_find(&mut parent, *lbl);
        }
    }

    labels
}

fn union_find_find(parent: &mut [u32], x: u32) -> u32 {
    let mut root = x;
    while parent[root as usize] != root {
        root = parent[root as usize];
    }
    let mut cur = x;
    while parent[cur as usize] != root {
        let next = parent[cur as usize];
        parent[cur as usize] = root;
        cur = next;
    }
    root
}

fn union_find_union(parent: &mut [u32], a: u32, b: u32) {
    let ra = union_find_find(parent, a);
    let rb = union_find_find(parent, b);
    if ra != rb {
        let (root, child) = if ra < rb { (ra, rb) } else { (rb, ra) };
        parent[child as usize] = root;
    }
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

    /// Build a frame that simulates the deck-occluded shipboard scene:
    /// bright sky in the upper portion, dark sea below, plus a bright
    /// "deck" rectangle in the lower-left that has stronger horizontal
    /// gradient than the sea-sky boundary itself.
    fn synth_deck_occluded(
        width: u32,
        height: u32,
        sky_horizon_y: u32,
        deck_top_y: u32,
        deck_right_x: u32,
    ) -> Frame {
        let mut pixels = vec![0u16; (width as usize) * (height as usize)];
        for y in 0..height {
            for x in 0..width {
                let v = if y < sky_horizon_y {
                    50_000 // sky
                } else {
                    8_000 // sea
                };
                pixels[(y as usize) * (width as usize) + (x as usize)] = v;
            }
        }
        // Bright deck in the lower-left.
        for y in deck_top_y..height {
            for x in 0..deck_right_x {
                // Deck slightly brighter than sea — this is what fools
                // the gradient detector. The top edge of the deck is a
                // strong horizontal feature competing with the sky-sea
                // boundary.
                pixels[(y as usize) * (width as usize) + (x as usize)] = 35_000;
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
    fn sky_region_detector_finds_horizon_in_deck_occluded_scene() {
        // Sky-sea horizon at y=200; deck top at y=350 covering left
        // half of the frame. The deck top is a strong horizontal edge
        // *below* the true horizon. The sky-region detector should
        // pick the actual sky's lower boundary at y=200, not the deck
        // edge at y=350.
        let frame = synth_deck_occluded(640, 480, 200, 350, 320);
        let line = detect_horizon_via_sky_region(&frame, HorizonConfig::default()).unwrap();
        assert!(
            (line.intercept - 200.0).abs() < 5.0,
            "sky-region detector should find horizon at y=200, got intercept {}",
            line.intercept
        );
        assert!(
            line.slope.abs() < 0.01,
            "horizon should be flat, got slope {}",
            line.slope
        );
    }

    #[test]
    fn sky_region_detector_works_on_simple_horizon() {
        // Should match the gradient detector's behavior on the easy case.
        let frame = synth_horizon_frame(800, 600, 300);
        let line = detect_horizon_via_sky_region(&frame, HorizonConfig::default()).unwrap();
        assert!(
            (line.intercept - 300.0).abs() < 5.0,
            "intercept {} should be near 300",
            line.intercept
        );
        assert!(line.slope.abs() < 0.01);
    }

    #[test]
    fn sky_region_detector_fails_when_no_sky_visible() {
        // A frame where the entire scene is dark (no sky region):
        // every pixel below the percentile threshold means no sky
        // component → InsufficientCandidates.
        let pixels = vec![1_000u16; 200 * 150];
        let frame = Frame::new(
            200,
            150,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(200, 150),
        )
        .unwrap();
        let result = detect_horizon_via_sky_region(&frame, HorizonConfig::default());
        // A uniform frame has no real "sky" but the percentile
        // thresholding will still split it; what we verify is just
        // that the function fails cleanly (either NoCandidates or
        // LowConfidence) rather than fabricating a fit.
        assert!(result.is_err());
    }

    /// Direct comparison: gradient detector picks the wrong line in
    /// the deck-occluded scene; sky-region detector picks the right
    /// one. This is the load-bearing test for the new approach.
    #[test]
    fn sky_region_outperforms_gradient_in_deck_occluded_scene() {
        let frame = synth_deck_occluded(640, 480, 200, 350, 320);
        let gradient_line = detect_horizon(&frame, HorizonConfig::default()).unwrap();
        let sky_line = detect_horizon_via_sky_region(&frame, HorizonConfig::default()).unwrap();
        // The sky-region detector should be closer to the true horizon
        // (y=200) than the gradient detector. We don't assert the
        // gradient detector is wrong (it might luck into the right
        // answer on some configurations), only that the sky-region
        // detector is correct.
        assert!(
            (sky_line.intercept - 200.0).abs() < 5.0,
            "sky-region detector at intercept {} (true=200); \
             gradient detector at intercept {}",
            sky_line.intercept,
            gradient_line.intercept,
        );
    }
}
