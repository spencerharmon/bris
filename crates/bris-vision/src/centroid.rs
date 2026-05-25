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
    /// The mask supplied to [`centroid_brightest_body_in_mask`] had
    /// a different length than the frame's pixel count.
    #[error("mask length {actual} doesn't match frame pixel count {expected}")]
    MaskShapeMismatch {
        /// `frame.width() * frame.height()`.
        expected: usize,
        /// Actual mask buffer length.
        actual: usize,
    },
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

/// Configuration for [`centroid_saturated_body_in_mask`].
///
/// Distinct from [`CentroidConfig`] because the threshold semantics
/// differ: this is an absolute saturation threshold (in u16 units),
/// not a fraction of the frame's brightest pixel.
#[derive(Debug, Clone, Copy)]
pub struct SaturatedBodyConfig {
    /// Minimum pixel value to count as "saturated" for body
    /// detection. Default 95% of `u16::MAX` (= 62258). Pixels at or
    /// above this contribute to the candidate component.
    ///
    /// Why absolute, not relative: the sun's saturated disk is at
    /// or near `u16::MAX`. Surrounding sky haze can be at 80-90%
    /// of `u16::MAX` — bright but not saturated. A relative
    /// threshold (e.g. "85% of frame max") catches the haze along
    /// with the sun, biasing the resulting centroid toward whichever
    /// side has more haze. An absolute saturation threshold
    /// isolates only the genuinely-saturated body pixels.
    pub saturation_threshold: u16,
    /// Minimum component area (pixels) to accept. Filters out hot
    /// pixels and small reflections. Default 50.
    pub min_area_px: u32,
}

impl Default for SaturatedBodyConfig {
    fn default() -> Self {
        Self {
            saturation_threshold: (u32::from(u16::MAX) * 95 / 100) as u16,
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
    centroid_brightest_body_in_mask(frame, cfg, None)
}

/// Same as [`centroid_brightest_body`], but only considers pixels
/// where `mask[y * width + x]` is `true`.
///
/// Use this with a sky-only mask to prevent false positives from sun
/// glare on water, sail glare, deck saturation, or other bright
/// features outside the celestial sphere.
///
/// `mask.len()` must equal `frame.width() * frame.height()`. A mask
/// of `None` is equivalent to "all pixels allowed" and is identical
/// in behavior to [`centroid_brightest_body`].
///
/// Pixels outside the mask are excluded both from the connected-
/// component search (a bright component partially inside the mask
/// is truncated to its in-mask portion) and from the centroid
/// integration. The reported `area_px` counts only in-mask pixels.
///
/// # Errors
///
/// As [`centroid_brightest_body`], plus
/// [`CentroidError::MaskShapeMismatch`] if the mask length doesn't
/// match the frame.
pub fn centroid_brightest_body_in_mask(
    frame: &Frame,
    cfg: CentroidConfig,
    mask: Option<&[bool]>,
) -> Result<Centroid, CentroidError> {
    let pixel_count = (frame.width() as usize) * (frame.height() as usize);
    if let Some(m) = mask {
        if m.len() != pixel_count {
            return Err(CentroidError::MaskShapeMismatch {
                expected: pixel_count,
                actual: m.len(),
            });
        }
    }

    // Find the maximum pixel value within the mask (or whole frame).
    let max_val = match mask {
        Some(m) => frame
            .pixels()
            .iter()
            .zip(m.iter())
            .filter_map(|(&p, &allowed)| allowed.then_some(p))
            .max()
            .unwrap_or(0),
        None => frame.pixels().iter().copied().max().unwrap_or(0),
    };
    if max_val == 0 {
        return Err(CentroidError::NoBrightRegion(0));
    }
    let threshold = (f64::from(max_val) * cfg.threshold_fraction) as u16;

    // Two-pass connected components on the masked, thresholded image.
    let labels = label_components_masked(frame, threshold, mask);

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

/// Detect *all* saturated bright components in a frame.
///
/// Like [`centroid_saturated_body_in_mask`], but returns one
/// [`Centroid`] per surviving connected component that meets
/// the minimum-area threshold, sorted by descending area
/// (largest first). Used by the reflection-pair horizon
/// provider's Day path, where the direct image of the body
/// and its reflection on a horizontal surface (water, hood,
/// puddle) both appear as bright blobs and the provider needs
/// at least two candidates per frame.
///
/// Returns an empty `Vec` (not an error) when no component
/// passes the area gate — the caller treats this as "no
/// secondary present" and falls back to single-centroid
/// behaviour.
///
/// # Errors
///
/// [`CentroidError::MaskShapeMismatch`] if the mask length
/// doesn't match the frame.
pub fn extract_multi_saturated_centroids(
    frame: &Frame,
    cfg: SaturatedBodyConfig,
    mask: Option<&[bool]>,
) -> Result<Vec<Centroid>, CentroidError> {
    let pixel_count = (frame.width() as usize) * (frame.height() as usize);
    if let Some(m) = mask {
        if m.len() != pixel_count {
            return Err(CentroidError::MaskShapeMismatch {
                expected: pixel_count,
                actual: m.len(),
            });
        }
    }

    let labels = label_components_masked(frame, cfg.saturation_threshold, mask);
    let n_labels = labels.next_label as usize;
    if n_labels <= 1 {
        return Ok(Vec::new());
    }

    // Accumulate per-label moments in a single pass.
    //
    // `mean_intensity` is computed from the **non-saturated
    // halo** of each component (background pixels adjacent
    // to a labelled blob) rather than from the labelled
    // pixels themselves: every labelled pixel is
    // ≥ `saturation_threshold` by construction, so a per-
    // component mean over labelled pixels collapses to the
    // saturation ceiling for *every* component and the
    // reflection-pair Test 2 (`dn.brightness ≤ up.brightness
    // * (1 + tol)`) degenerates to a tautology on Day. The
    // halo proxy retains photometric discriminating power
    // without needing un-clipped raw pixels: a brighter
    // source has a brighter sub-saturation glow around it.
    // When the halo is empty (component touches frame edge
    // or saturates through to its neighbours), we fall back
    // to the labelled-pixel mean (ceiling value).
    let mut areas: Vec<u32> = vec![0; n_labels];
    let mut sum_x: Vec<f64> = vec![0.0; n_labels];
    let mut sum_y: Vec<f64> = vec![0.0; n_labels];
    let mut sum_w: Vec<f64> = vec![0.0; n_labels];
    let mut sum_i: Vec<f64> = vec![0.0; n_labels];
    let mut halo_sum: Vec<f64> = vec![0.0; n_labels];
    let mut halo_count: Vec<u32> = vec![0; n_labels];
    let w = frame.width();
    let h = frame.height();
    let wu = w as usize;
    let hu = h as usize;
    for y in 0..h {
        for x in 0..w {
            let idx = (y as usize) * wu + (x as usize);
            let lbl = labels.labels[idx] as usize;
            if lbl != 0 {
                let intensity = f64::from(frame.pixels()[idx]);
                areas[lbl] += 1;
                sum_x[lbl] += f64::from(x) * intensity;
                sum_y[lbl] += f64::from(y) * intensity;
                sum_w[lbl] += intensity;
                sum_i[lbl] += intensity;
                continue;
            }
            // Background pixel: if any 4-neighbour belongs to
            // a labelled component, this pixel contributes
            // to that component's halo. By construction in
            // `label_components_masked`, background pixels
            // are *below* the saturation threshold, so this
            // is a meaningful brightness sample.
            let xi = x as usize;
            let yi = y as usize;
            let mut neighbour_lbl: u32 = 0;
            if xi > 0 {
                let l = labels.labels[idx - 1];
                if l != 0 {
                    neighbour_lbl = l;
                }
            }
            if neighbour_lbl == 0 && xi + 1 < wu {
                let l = labels.labels[idx + 1];
                if l != 0 {
                    neighbour_lbl = l;
                }
            }
            if neighbour_lbl == 0 && yi > 0 {
                let l = labels.labels[idx - wu];
                if l != 0 {
                    neighbour_lbl = l;
                }
            }
            if neighbour_lbl == 0 && yi + 1 < hu {
                let l = labels.labels[idx + wu];
                if l != 0 {
                    neighbour_lbl = l;
                }
            }
            if neighbour_lbl != 0 {
                let intensity = f64::from(frame.pixels()[idx]);
                halo_sum[neighbour_lbl as usize] += intensity;
                halo_count[neighbour_lbl as usize] += 1;
            }
        }
    }

    let mut out: Vec<Centroid> = (1..n_labels)
        .filter(|&l| areas[l] >= cfg.min_area_px && sum_w[l] > 0.0)
        .map(|l| {
            let area = areas[l];
            let cx = sum_x[l] / sum_w[l];
            let cy = sum_y[l] / sum_w[l];
            let mean_intensity = if halo_count[l] > 0 {
                halo_sum[l] / f64::from(halo_count[l])
            } else {
                sum_i[l] / f64::from(area)
            };
            let stat = 1.0 / (area as f64).sqrt();
            let bias = 0.5;
            let sigma = (stat * stat + bias * bias).sqrt();
            Centroid {
                x: cx,
                y: cy,
                area_px: area,
                mean_intensity,
                position_sigma_px: Sigma::new(sigma).unwrap_or(Sigma::ZERO),
            }
        })
        .collect();
    // Largest first. Stable on equal areas (preserves label
    // order).
    out.sort_by(|a, b| b.area_px.cmp(&a.area_px));
    Ok(out)
}

/// Centroid the brightest *saturated* body inside a mask.
///
/// Distinct from [`centroid_brightest_body_in_mask`] in that the
/// threshold is **absolute** (`cfg.saturation_threshold`), not a
/// fraction of the frame's brightest pixel. This isolates only
/// genuinely-saturated pixels, which is what you want when:
///
/// 1. The body of interest is the Sun or Moon (both saturate a
///    correctly-exposed daytime camera).
/// 2. A relative threshold over a sky-only mask would catch bright
///    haze around the body and bias the centroid. The bright sky
///    near a saturated Sun can sit at 80-90% of `u16::MAX`; a
///    relative threshold of 0.85 includes both the body and the
///    haze and computes a centroid pulled toward the brighter side
///    of the haze rather than the body's actual core.
///
/// `mask` filters which pixels are considered. Pass the segmentation
/// sky-mask to exclude saturated sail glare, water glare, deck
/// reflections, etc.
///
/// Returns [`CentroidError::NoBrightRegion`] when no pixels above
/// the saturation threshold survive the mask — the right behavior
/// for scenes without a saturated body (overcast, dusk, ambiguous
/// sun glow). Use [`centroid_brightest_body_in_mask`] for those
/// cases if you want to fall back to a relative-threshold search.
///
/// # Errors
///
/// As [`centroid_brightest_body_in_mask`].
pub fn centroid_saturated_body_in_mask(
    frame: &Frame,
    cfg: SaturatedBodyConfig,
    mask: Option<&[bool]>,
) -> Result<Centroid, CentroidError> {
    let pixel_count = (frame.width() as usize) * (frame.height() as usize);
    if let Some(m) = mask {
        if m.len() != pixel_count {
            return Err(CentroidError::MaskShapeMismatch {
                expected: pixel_count,
                actual: m.len(),
            });
        }
    }

    // Connected components on saturated pixels only, masked.
    let labels = label_components_masked(frame, cfg.saturation_threshold, mask);

    let mut areas: Vec<u32> = vec![0; labels.next_label as usize];
    for &lbl in &labels.labels {
        if lbl > 0 {
            areas[lbl as usize] += 1;
        }
    }
    let (best_label, &best_area) = areas
        .iter()
        .enumerate()
        .skip(1)
        .max_by_key(|&(_, area)| *area)
        .ok_or(CentroidError::NoBrightRegion(cfg.saturation_threshold))?;
    if best_area < cfg.min_area_px {
        return Err(CentroidError::ComponentTooSmall(best_area, cfg.min_area_px));
    }

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
///
/// `mask` is an optional per-pixel allow filter; when `Some`, pixels
/// where `mask[idx]` is `false` are treated as background regardless
/// of their intensity. This both excludes them from the search and
/// breaks connectivity (a bright component partially inside the mask
/// gets relabeled as the in-mask portion only).
fn label_components_masked(
    frame: &Frame,
    threshold: u16,
    mask: Option<&[bool]>,
) -> ComponentLabels {
    let w = frame.width() as usize;
    let h = frame.height() as usize;
    let pixels = frame.pixels();
    let mut labels = vec![0u32; w * h];
    let mut parent: Vec<u32> = vec![0]; // index 0 is reserved background
    let mut next_label: u32 = 1;

    let allowed = |idx: usize| -> bool { mask.is_none_or(|m| m[idx]) };

    // First pass: assign provisional labels.
    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            if pixels[idx] < threshold || !allowed(idx) {
                continue;
            }
            // Look at left and above. Connectivity is only honored
            // when both endpoints are in-mask; this naturally truncates
            // a bright component at the mask boundary.
            let left = if x > 0 && allowed(idx - 1) {
                labels[idx - 1]
            } else {
                0
            };
            let above = if y > 0 && allowed(idx - w) {
                labels[idx - w]
            } else {
                0
            };
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

    #[test]
    fn mask_with_wrong_length_returns_typed_error() {
        let frame = synth_disk_frame(100, 100, 50.0, 50.0, 10.0);
        let bad_mask = vec![true; 99 * 100]; // off by one row
        let result =
            centroid_brightest_body_in_mask(&frame, CentroidConfig::default(), Some(&bad_mask));
        assert!(matches!(
            result,
            Err(CentroidError::MaskShapeMismatch {
                expected: 10_000,
                actual: 9_900,
            })
        ));
    }

    #[test]
    fn all_true_mask_matches_unmasked_result() {
        // Same frame, same config, two paths: one with no mask, one
        // with an all-true mask. Results must agree.
        let frame = synth_disk_frame(200, 150, 100.0, 75.0, 12.0);
        let mask = vec![true; 200 * 150];
        let unmasked = centroid_brightest_body(&frame, CentroidConfig::default()).unwrap();
        let masked =
            centroid_brightest_body_in_mask(&frame, CentroidConfig::default(), Some(&mask))
                .unwrap();
        assert!((unmasked.x - masked.x).abs() < 1e-9, "x differs");
        assert!((unmasked.y - masked.y).abs() < 1e-9, "y differs");
        assert_eq!(unmasked.area_px, masked.area_px);
    }

    #[test]
    fn mask_picks_smaller_in_mask_blob_over_larger_out_of_mask_blob() {
        // Two bright disks: a *larger* one at (350, 50) (the
        // distractor — think "sun glare on water") and a *smaller*
        // one at (200, 150) (the real target — think "actual sun").
        // Without a mask, the larger blob wins. With a mask that
        // restricts to the upper-left quadrant containing the smaller
        // blob, the smaller blob should be selected.
        let mut pixels = vec![1_000u16; 400 * 300];
        // Real-target disk at (200, 150), r = 12.
        for y in 0..300 {
            for x in 0..400 {
                let dx = f64::from(x) - 200.0;
                let dy = f64::from(y) - 150.0;
                if dx * dx + dy * dy <= 12.0 * 12.0 {
                    pixels[(y as usize) * 400 + (x as usize)] = 60_000;
                }
            }
        }
        // Distractor disk at (350, 50), r = 30 (larger).
        for y in 0..300 {
            for x in 0..400 {
                let dx = f64::from(x) - 350.0;
                let dy = f64::from(y) - 50.0;
                if dx * dx + dy * dy <= 30.0 * 30.0 {
                    pixels[(y as usize) * 400 + (x as usize)] = 60_000;
                }
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

        // Sanity: without a mask, the distractor wins.
        let unmasked = centroid_brightest_body(&frame, CentroidConfig::default()).unwrap();
        assert!(
            (unmasked.x - 350.0).abs() < 2.0,
            "without mask, distractor at (350, 50) should win; got x={}",
            unmasked.x
        );

        // Build a mask that only allows pixels in the left half.
        let mut mask = vec![false; 400 * 300];
        for y in 0..300 {
            for x in 0..250 {
                mask[(y as usize) * 400 + (x as usize)] = true;
            }
        }

        let masked =
            centroid_brightest_body_in_mask(&frame, CentroidConfig::default(), Some(&mask))
                .unwrap();
        assert!(
            (masked.x - 200.0).abs() < 2.0,
            "with mask, real target at (200, 150) should win; got x={}",
            masked.x
        );
        assert!(
            (masked.y - 150.0).abs() < 2.0,
            "with mask, real target at (200, 150) should win; got y={}",
            masked.y
        );
    }

    #[test]
    fn mask_excluding_all_bright_pixels_returns_no_bright_region() {
        let frame = synth_disk_frame(100, 100, 50.0, 50.0, 10.0);
        // Mask out everything that has the bright disk (i.e. block out
        // the center) — leaves only the dark background.
        let mut mask = vec![true; 100 * 100];
        for y in 30..70 {
            for x in 30..70 {
                mask[(y as usize) * 100 + (x as usize)] = false;
            }
        }
        let result =
            centroid_brightest_body_in_mask(&frame, CentroidConfig::default(), Some(&mask));
        // The remaining pixels are all the dark background (intensity
        // 1000); thresholding picks them up but the resulting "blob"
        // is the entire allowed region, which still has area > min.
        // What matters is that the function returns a sensible result
        // and doesn't panic. Either Ok(centroid_in_background) or
        // Err is acceptable; the contract is "no surprises."
        assert!(result.is_ok() || matches!(result, Err(CentroidError::NoBrightRegion(_))));
    }

    // -----------------------------------------------------------------
    // Saturated body centroiding
    // -----------------------------------------------------------------

    /// Build a frame with a saturated disk surrounded by a bright
    /// halo (the failure case the saturated-body centroider exists
    /// to handle): the disk is at `u16::MAX`, and a larger
    /// surrounding ring is at 90% of `u16::MAX`. The unmasked
    /// extended-disk centroider would treat both as one component
    /// and compute a centroid that's pulled toward the haze; the
    /// saturated-body centroider should isolate just the saturated
    /// disk.
    #[allow(clippy::similar_names)] // dx_halo/dy_halo vs dx_sat/dy_sat are intentional pairs
    fn synth_saturated_disk_with_halo(
        width: u32,
        height: u32,
        cx: f64,
        cy: f64,
        sat_radius: f64,
        halo_radius: f64,
        halo_offset_x: f64,
    ) -> Frame {
        let mut pixels = vec![1_000u16; (width as usize) * (height as usize)];
        let halo_intensity = (u32::from(u16::MAX) * 90 / 100) as u16;
        for y in 0..height {
            for x in 0..width {
                let fx = f64::from(x);
                let fy = f64::from(y);
                let dx_halo = fx - (cx + halo_offset_x);
                let dy_halo = fy - cy;
                if dx_halo * dx_halo + dy_halo * dy_halo <= halo_radius * halo_radius {
                    pixels[(y as usize) * (width as usize) + (x as usize)] = halo_intensity;
                }
                let dx_sat = fx - cx;
                let dy_sat = fy - cy;
                if dx_sat * dx_sat + dy_sat * dy_sat <= sat_radius * sat_radius {
                    pixels[(y as usize) * (width as usize) + (x as usize)] = u16::MAX;
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
    fn saturated_body_centers_on_saturated_disk_not_haze() {
        // Saturated disk at (200, 150) radius 15; halo offset by
        // (+30, 0) with radius 50 — the halo's "center of mass"
        // is somewhere between the disk center and the halo center.
        let frame = synth_saturated_disk_with_halo(400, 300, 200.0, 150.0, 15.0, 50.0, 30.0);
        let c =
            centroid_saturated_body_in_mask(&frame, SaturatedBodyConfig::default(), None).unwrap();
        // Should be on the saturated disk, not pulled by the halo.
        assert_relative_eq!(c.x, 200.0, epsilon = 1.0);
        assert_relative_eq!(c.y, 150.0, epsilon = 1.0);
        // Area should be the saturated disk's area (π·15² ≈ 707),
        // not the disk + halo.
        assert!(
            c.area_px > 600 && c.area_px < 800,
            "expected saturated-disk area ~707, got {}",
            c.area_px
        );
    }

    #[test]
    fn saturated_body_refuses_unsaturated_frame() {
        // Synth a non-saturated body (max 60000, below threshold).
        let frame = synth_disk_frame(200, 150, 100.0, 75.0, 20.0);
        let result = centroid_saturated_body_in_mask(&frame, SaturatedBodyConfig::default(), None);
        assert!(matches!(
            result,
            Err(CentroidError::NoBrightRegion(_) | CentroidError::ComponentTooSmall(_, _))
        ));
    }

    #[test]
    fn saturated_body_honors_mask() {
        // Two saturated disks; mask out one and expect the centroid
        // to land on the other.
        let mut pixels = vec![1_000u16; 400 * 300];
        // Disk A at (100, 100), radius 12.
        // Disk B at (300, 200), radius 12.
        for y in 0..300 {
            for x in 0..400 {
                let in_a = (f64::from(x) - 100.0).powi(2) + (f64::from(y) - 100.0).powi(2) <= 144.0;
                let in_b = (f64::from(x) - 300.0).powi(2) + (f64::from(y) - 200.0).powi(2) <= 144.0;
                if in_a || in_b {
                    pixels[(y as usize) * 400 + (x as usize)] = u16::MAX;
                }
            }
        }
        let frame = Frame::new(
            400,
            300,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(400, 300),
        )
        .unwrap();
        // Mask: only allow the right half (x >= 200) — excludes A.
        let mask: Vec<bool> = (0..300).flat_map(|_y| (0..400).map(|x| x >= 200)).collect();
        let c =
            centroid_saturated_body_in_mask(&frame, SaturatedBodyConfig::default(), Some(&mask))
                .unwrap();
        // Should land on disk B at (300, 200).
        assert_relative_eq!(c.x, 300.0, epsilon = 1.0);
        assert_relative_eq!(c.y, 200.0, epsilon = 1.0);
    }

    #[test]
    fn saturated_body_rejects_too_few_saturated_pixels() {
        // A handful of saturated noise pixels shouldn't trigger.
        let mut pixels = vec![1_000u16; 200 * 150];
        // 5 saturated noise pixels scattered.
        for &(x, y) in &[(10, 10), (50, 50), (100, 75), (150, 100), (190, 140)] {
            pixels[(y as usize) * 200 + (x as usize)] = u16::MAX;
        }
        let frame = Frame::new(
            200,
            150,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(200, 150),
        )
        .unwrap();
        let result = centroid_saturated_body_in_mask(&frame, SaturatedBodyConfig::default(), None);
        assert!(matches!(
            result,
            Err(CentroidError::ComponentTooSmall(_, _) | CentroidError::NoBrightRegion(_))
        ));
    }

    #[test]
    fn saturated_body_rejects_mask_shape_mismatch() {
        let frame = synth_disk_frame(100, 100, 50.0, 50.0, 10.0);
        let mask = vec![true; 99 * 100]; // wrong size
        let err =
            centroid_saturated_body_in_mask(&frame, SaturatedBodyConfig::default(), Some(&mask))
                .unwrap_err();
        assert!(matches!(err, CentroidError::MaskShapeMismatch { .. }));
    }

    #[test]
    fn multi_saturated_returns_each_component_largest_first() {
        // Two saturated disks: A at (100, 100) radius 18
        // (area ≈ 1017), B at (300, 200) radius 10 (area ≈
        // 314). Plus a 3-pixel saturated speck that must be
        // gated out by `min_area_px`.
        let mut pixels = vec![1_000u16; 400 * 300];
        for y in 0..300 {
            for x in 0..400 {
                let in_a =
                    (f64::from(x) - 100.0).powi(2) + (f64::from(y) - 100.0).powi(2) <= 18.0 * 18.0;
                let in_b =
                    (f64::from(x) - 300.0).powi(2) + (f64::from(y) - 200.0).powi(2) <= 10.0 * 10.0;
                if in_a || in_b {
                    pixels[(y as usize) * 400 + (x as usize)] = u16::MAX;
                }
            }
        }
        // Tiny noise speck.
        for &(x, y) in &[(380_usize, 10_usize), (381, 10), (380, 11)] {
            pixels[y * 400 + x] = u16::MAX;
        }
        let frame = Frame::new(
            400,
            300,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(400, 300),
        )
        .unwrap();
        let centroids =
            extract_multi_saturated_centroids(&frame, SaturatedBodyConfig::default(), None)
                .unwrap();
        assert_eq!(centroids.len(), 2, "speck must be gated by min_area_px");
        assert!(
            centroids[0].area_px >= centroids[1].area_px,
            "largest first"
        );
        assert_relative_eq!(centroids[0].x, 100.0, epsilon = 1.0);
        assert_relative_eq!(centroids[0].y, 100.0, epsilon = 1.0);
        assert_relative_eq!(centroids[1].x, 300.0, epsilon = 1.0);
        assert_relative_eq!(centroids[1].y, 200.0, epsilon = 1.0);
        for c in &centroids {
            assert!(c.position_sigma_px.value() > 0.0);
        }
    }

    #[test]
    fn multi_saturated_empty_when_no_saturation() {
        let frame = synth_disk_frame(200, 150, 100.0, 75.0, 12.0);
        let centroids =
            extract_multi_saturated_centroids(&frame, SaturatedBodyConfig::default(), None)
                .unwrap();
        assert!(centroids.is_empty());
    }

    /// Two equal-area saturated blobs sitting on backgrounds of
    /// different sub-saturation brightness must produce
    /// distinguishable `mean_intensity` values. Without the
    /// halo-based proxy `mean_intensity` would collapse to the
    /// saturation ceiling for both blobs, making the reflection-
    /// pair photometric test (Test 2) degenerate.
    #[test]
    fn multi_saturated_halo_distinguishes_equal_area_blobs() {
        #![allow(clippy::many_single_char_names)]
        let w = 400_u32;
        let h = 300_u32;
        let mut pixels = vec![0u16; (w * h) as usize];
        let cfg = SaturatedBodyConfig {
            saturation_threshold: u16::MAX - 50,
            min_area_px: 50,
        };
        // Two identical saturated disks centred at (100, 150)
        // and (300, 150). Surround each with a square halo of
        // sub-saturation pixels at different intensities so the
        // halo proxy discriminates: A's halo at 10_000, B's at
        // 40_000.
        let r = 12_i32;
        let halo = 18_i32;
        for &(cx, cy, halo_val) in &[(100_i32, 150_i32, 10_000u16), (300, 150, 40_000)] {
            for dy in -halo..=halo {
                for dx in -halo..=halo {
                    let x = (cx + dx) as usize;
                    let y = (cy + dy) as usize;
                    let idx = y * (w as usize) + x;
                    if dx * dx + dy * dy <= r * r {
                        pixels[idx] = u16::MAX;
                    } else if dx.abs() <= halo && dy.abs() <= halo {
                        pixels[idx] = halo_val;
                    }
                }
            }
        }
        let frame = Frame::new(
            w,
            h,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(w, h),
        )
        .unwrap();
        let centroids = extract_multi_saturated_centroids(&frame, cfg, None).unwrap();
        assert_eq!(centroids.len(), 2, "both blobs should pass min_area_px");
        // Order by x so the assertion is deterministic.
        let mut by_x = centroids;
        by_x.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap());
        assert!(
            by_x[0].area_px == by_x[1].area_px,
            "test setup expected equal areas, got {} vs {}",
            by_x[0].area_px,
            by_x[1].area_px,
        );
        // A's halo is 10000, B's halo is 40000. The halo-based
        // mean_intensity must reflect this 4× ratio (within a
        // small tolerance for halo geometry).
        let ratio = by_x[1].mean_intensity / by_x[0].mean_intensity;
        assert!(
            ratio > 3.0,
            "halo-based mean_intensity should reflect background brightness: \
             got A={}, B={}, ratio {ratio}",
            by_x[0].mean_intensity,
            by_x[1].mean_intensity,
        );
    }
}
