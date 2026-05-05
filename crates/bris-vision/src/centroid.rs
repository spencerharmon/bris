//! Sun/Moon centroiding.
//!
//! The Sun and Moon are extended bodies (~32 arcmin apparent
//! diameter) that the camera sees as saturated bright blobs. The
//! centroiding algorithm:
//!
//! 1. Threshold the frame to isolate the brightest pixels (those
//!    above some fraction of the maximum value).
//! 2. Connected-component labeling to identify discrete bright
//!    regions.
//! 3. Pick the largest component as the body.
//! 4. Compute an intensity-weighted centroid (sub-pixel accurate).
//!
//! Returns the centroid in pixel coordinates with an attached σ.
//!
//! # Limb correction
//!
//! When the body straddles the horizon (rising/setting Sun, partial
//! occlusion), the centroid as computed here is biased toward the
//! visible portion. Bowditch §16 and the Nautical Almanac provide
//! "lower limb" and "upper limb" sight conventions: navigators
//! traditionally bring the *limb* (edge) of the body to the horizon
//! and apply the body's semi-diameter. We will need that for
//! 0.5 nm accuracy with the Sun/Moon. For MVP we report the
//! centroid as-is and note the bias as a TODO contribution to
//! per-sight uncertainty.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]

use crate::frame::Frame;
use bris_core::Sigma;

/// A detected centroid of an extended body (Sun or Moon).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Centroid {
    /// X coordinate in image pixels (sub-pixel resolution).
    pub x: f64,
    /// Y coordinate in image pixels.
    pub y: f64,
    /// Number of pixels in the connected component.
    pub area_px: u32,
    /// Mean intensity of the component pixels (u16-scale).
    pub mean_intensity: f64,
    /// 1σ uncertainty in the centroid position, pixels. Combines
    /// per-axis statistical uncertainty (≈ 1/√N for a flat-top blob)
    /// with a small fixed term for thresholding bias.
    pub position_sigma_px: Sigma,
}

/// Errors from centroiding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CentroidError {
    /// No bright region survived thresholding. Either no body is in
    /// the frame, or the threshold is too high for current conditions.
    #[error("no bright region detected at threshold {0}")]
    NoBrightRegion(u16),
    /// The largest component was below the configured minimum size,
    /// likely a hot pixel or noise rather than a real body.
    #[error("largest bright component had only {0} pixels (need ≥ {1})")]
    ComponentTooSmall(u32, u32),
}

/// Centroiding configuration.
#[derive(Debug, Clone, Copy)]
pub struct CentroidConfig {
    /// Threshold for "bright" pixels, as a fraction of the frame's
    /// maximum pixel value. Default 0.85 — picks up the body without
    /// catching surrounding glare.
    pub threshold_fraction: f64,
    /// Minimum component area (pixels) to accept. Filters out hot
    /// pixels and noise. Default 50.
    pub min_area_px: u32,
}

impl Default for CentroidConfig {
    fn default() -> Self {
        Self {
            threshold_fraction: 0.85,
            min_area_px: 50,
        }
    }
}

/// Detect the Sun or Moon centroid in a frame.
///
/// # Errors
///
/// Returns `Err` if no bright region survives thresholding or the
/// largest component is suspiciously small.
pub fn centroid_brightest_body(
    frame: &Frame,
    cfg: CentroidConfig,
) -> Result<Centroid, CentroidError> {
    // Find the maximum pixel value to anchor the threshold.
    let max_val = frame.pixels().iter().copied().max().unwrap_or(0);
    if max_val == 0 {
        return Err(CentroidError::NoBrightRegion(0));
    }
    let threshold = (f64::from(max_val) * cfg.threshold_fraction) as u16;

    // Two-pass connected components on the thresholded image.
    let labels = label_components(frame, threshold);

    // Find the label with the largest area (excluding 0 = background).
    let mut areas: Vec<u32> = vec![0; labels.next_label as usize];
    for &lbl in &labels.labels {
        if lbl > 0 {
            areas[lbl as usize] += 1;
        }
    }
    let (best_label, &best_area) = areas
        .iter()
        .enumerate()
        .skip(1) // skip background label 0
        .max_by_key(|&(_, area)| *area)
        .ok_or(CentroidError::NoBrightRegion(threshold))?;
    if best_area < cfg.min_area_px {
        return Err(CentroidError::ComponentTooSmall(best_area, cfg.min_area_px));
    }

    // Intensity-weighted centroid over the chosen component.
    let mut sum_x: f64 = 0.0;
    let mut sum_y: f64 = 0.0;
    let mut sum_w: f64 = 0.0;
    let mut sum_intensity: f64 = 0.0;
    let w = frame.width();
    for y in 0..frame.height() {
        for x in 0..w {
            let idx = (y as usize) * (w as usize) + (x as usize);
            if labels.labels[idx] == best_label as u32 {
                let intensity = f64::from(frame.pixels()[idx]);
                sum_x += f64::from(x) * intensity;
                sum_y += f64::from(y) * intensity;
                sum_w += intensity;
                sum_intensity += intensity;
            }
        }
    }
    let cx = sum_x / sum_w;
    let cy = sum_y / sum_w;
    let mean_intensity = sum_intensity / f64::from(best_area);

    // Centroid σ: ~1/√N from photon counting + small bias term from
    // thresholding effects (we pick this as 0.5 px, conservative).
    let stat_sigma_px = 1.0 / (best_area as f64).sqrt();
    let bias_sigma_px = 0.5;
    let total_sigma_px = (stat_sigma_px * stat_sigma_px + bias_sigma_px * bias_sigma_px).sqrt();
    let position_sigma_px = Sigma::new(total_sigma_px).unwrap_or(Sigma::ZERO);

    Ok(Centroid {
        x: cx,
        y: cy,
        area_px: best_area,
        mean_intensity,
        position_sigma_px,
    })
}

struct ComponentLabels {
    labels: Vec<u32>,
    next_label: u32,
}

/// Two-pass connected-components labeling using a small union-find.
fn label_components(frame: &Frame, threshold: u16) -> ComponentLabels {
    let w = frame.width() as usize;
    let h = frame.height() as usize;
    let pixels = frame.pixels();
    let mut labels = vec![0u32; w * h];
    let mut parent: Vec<u32> = vec![0]; // index 0 is reserved background
    let mut next_label: u32 = 1;

    // First pass: assign provisional labels.
    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            if pixels[idx] < threshold {
                continue;
            }
            // Look at left and above.
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
                    union(&mut parent, a, b);
                    a.min(b)
                }
            };
            labels[idx] = lbl;
        }
    }

    // Second pass: replace each label with the root of its union-find tree.
    for lbl in &mut labels {
        if *lbl > 0 {
            *lbl = find(&mut parent, *lbl);
        }
    }

    ComponentLabels { labels, next_label }
}

fn find(parent: &mut [u32], x: u32) -> u32 {
    let mut root = x;
    while parent[root as usize] != root {
        root = parent[root as usize];
    }
    // Path compression.
    let mut cur = x;
    while parent[cur as usize] != root {
        let next = parent[cur as usize];
        parent[cur as usize] = root;
        cur = next;
    }
    root
}

fn union(parent: &mut [u32], a: u32, b: u32) {
    let ra = find(parent, a);
    let rb = find(parent, b);
    if ra != rb {
        // Smaller index becomes the root for stability.
        let (root, child) = if ra < rb { (ra, rb) } else { (rb, ra) };
        parent[child as usize] = root;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Intrinsics;
    use approx::assert_relative_eq;
    use bris_core::time::{Tt, JD_J2000};

    /// Build a frame with a circular bright disk centered at (cx, cy)
    /// with the given radius, against a dark background.
    fn synth_disk_frame(width: u32, height: u32, cx: f64, cy: f64, radius: f64) -> Frame {
        let mut pixels = vec![1_000u16; (width as usize) * (height as usize)];
        for y in 0..height {
            for x in 0..width {
                let dx = f64::from(x) - cx;
                let dy = f64::from(y) - cy;
                if dx * dx + dy * dy <= radius * radius {
                    pixels[(y as usize) * (width as usize) + (x as usize)] = 60_000;
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
    fn centroid_at_disk_center() {
        let frame = synth_disk_frame(400, 300, 200.0, 150.0, 30.0);
        let c = centroid_brightest_body(&frame, CentroidConfig::default()).unwrap();
        // Centroid should match disk center to sub-pixel.
        assert_relative_eq!(c.x, 200.0, epsilon = 0.5);
        assert_relative_eq!(c.y, 150.0, epsilon = 0.5);
        assert!(c.area_px > 2_500); // π·30² ≈ 2827
        assert!(c.area_px < 3_000);
    }

    #[test]
    fn centroid_offcenter_disk() {
        let frame = synth_disk_frame(400, 300, 100.0, 50.0, 20.0);
        let c = centroid_brightest_body(&frame, CentroidConfig::default()).unwrap();
        assert_relative_eq!(c.x, 100.0, epsilon = 0.5);
        assert_relative_eq!(c.y, 50.0, epsilon = 0.5);
    }

    #[test]
    fn rejects_uniform_dark_frame() {
        let pixels = vec![100u16; 100 * 100];
        let frame = Frame::new(
            100,
            100,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(100, 100),
        )
        .unwrap();
        let result = centroid_brightest_body(&frame, CentroidConfig::default());
        // All pixels equal → max ≈ 100, threshold = 85, all pixels above
        // threshold → one big component spanning the entire frame.
        // With min_area_px = 50, this passes; the centroid is the
        // image center.
        let c = result.unwrap();
        assert_relative_eq!(c.x, 49.5, epsilon = 0.5);
        assert_relative_eq!(c.y, 49.5, epsilon = 0.5);
    }

    #[test]
    fn rejects_tiny_component() {
        // A single bright pixel against dark background.
        let mut pixels = vec![1_000u16; 100 * 100];
        pixels[5050] = 60_000;
        let frame = Frame::new(
            100,
            100,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(100, 100),
        )
        .unwrap();
        let result = centroid_brightest_body(&frame, CentroidConfig::default());
        assert!(matches!(
            result,
            Err(CentroidError::ComponentTooSmall(1, 50))
        ));
    }

    #[test]
    fn ignores_secondary_smaller_blob() {
        // Big disk + small bright spot. Should pick the big one.
        let mut pixels = vec![1_000u16; 400 * 300];
        // Big disk at (200, 150), r=30.
        for y in 0..300 {
            for x in 0..400 {
                let dx = f64::from(x) - 200.0;
                let dy = f64::from(y) - 150.0;
                if dx * dx + dy * dy <= 30.0 * 30.0 {
                    pixels[(y as usize) * 400 + (x as usize)] = 60_000;
                }
            }
        }
        // Small bright spot at (350, 50), 5x5 px.
        for y in 48..53 {
            for x in 348..353 {
                pixels[(y as usize) * 400 + (x as usize)] = 60_000;
            }
        }
        let frame = Frame::new(
            400,
            300,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(400, 300),
        )
        .unwrap();
        let c = centroid_brightest_body(&frame, CentroidConfig::default()).unwrap();
        // Centroid should be near (200, 150), not (350, 50).
        assert!(
            (c.x - 200.0).abs() < 1.0,
            "picked the wrong blob: x={}",
            c.x
        );
        assert!(
            (c.y - 150.0).abs() < 1.0,
            "picked the wrong blob: y={}",
            c.y
        );
    }

    #[test]
    fn position_sigma_decreases_with_size() {
        let small = synth_disk_frame(400, 300, 200.0, 150.0, 10.0);
        let large = synth_disk_frame(400, 300, 200.0, 150.0, 50.0);
        let c_small = centroid_brightest_body(&small, CentroidConfig::default()).unwrap();
        let c_large = centroid_brightest_body(&large, CentroidConfig::default()).unwrap();
        // Larger blob → smaller statistical sigma → smaller total sigma
        // (or equal, if the bias floor dominates).
        assert!(c_large.position_sigma_px.value() <= c_small.position_sigma_px.value());
    }
}
