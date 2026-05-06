//! Night-horizon detection.
//!
//! The daylight horizon detectors ([`crate::detect_horizon`],
//! [`crate::detect_horizon_via_sky_region`],
//! [`crate::detect_horizon_via_segmentation`]) all rely on
//! daytime-scene assumptions: a bright sky (top half of frame) above
//! a darker sea, segmentation classes trained on color daylight
//! imagery, or a sea-sky brightness contrast that's strong enough to
//! produce per-column gradient peaks. None of these hold at night.
//!
//! This module provides [`detect_horizon_night`], a detector that
//! works on the smoothed *per-row* mean luma profile rather than
//! per-column gradients. It's robust to:
//!
//! - Uniformly dark scenes where individual-pixel gradients are
//!   dominated by noise.
//! - Saturated point sources (Moon, planets, bright stars,
//!   harbor lights) that would create spurious per-column gradient
//!   peaks elsewhere in the frame *when combined with*
//!   [`detect_horizon_night_excluding_body`].
//! - Scenes where the sea is *brighter* than the sky (city-light
//!   reflections on water; the daylight detectors assume
//!   sky-brighter-than-sea).
//!
//! # Limitations
//!
//! The detector finds the **strongest horizontal luma transition**
//! in the search row range. On real shipboard footage this is often
//! a deck-to-sky boundary or a wake/glint feature, *not* the
//! sea-sky horizon. Distinguishing these requires either:
//!
//! - A `search_row_range` that's manually constrained based on
//!   scene context (e.g. to skip a known deck region or a saturated
//!   body's halo).
//! - Combining with the segmentation detector to get a sky/sea
//!   class prior.
//! - Multi-pass: find the strongest gradient, exclude its
//!   neighborhood, find the next-strongest. The horizon is often
//!   the second-strongest when a body or deck is in frame.
//!
//! For the moonlit `night_test_lowres` scene specifically, the
//! moon's halo extends to roughly 35% of the (1920-tall) frame
//! height; the actual horizon is around 70%. Default config
//! restricted to "below moon" still catches the halo edge.
//! Restricting `search_row_range` to e.g. `(0.55, 1.0)` would
//! skip the halo entirely.
//!
//! On the `container_ship_night*` corpus scenes the detector lands
//! on the deck top rather than the sea horizon. A future deck-
//! excluding mask (parallel to [`crate::body_column_mask`] but for
//! row ranges) would resolve this.
//!
//! # Algorithm
//!
//! 1. Downsample the frame to a working width (default 200).
//! 2. Compute per-row mean luma over a horizontal center band
//!    (avoids vignetting edge effects). Optionally exclude masked
//!    columns (essential when a saturated body is present).
//! 3. Smooth the per-row profile with a small box filter (kernel
//!    size derived from the working height).
//! 4. Find the row index `y*` in the configured `search_row_range`
//!    where `|profile[y+1] - profile[y-1]|` is maximum.
//! 5. For each column, find the row in a window around `y*` where
//!    the per-column vertical gradient is largest. Emit as
//!    candidates if the gradient exceeds a low threshold.
//! 6. RANSAC-fit a line through the candidates using the standard
//!    [`crate::horizon::HorizonError`]-returning machinery.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use crate::frame::Frame;
use crate::horizon::{
    body_column_mask, finalize_horizon, HorizonConfig, HorizonError, HorizonLine,
};

/// Configuration for [`detect_horizon_night`].
#[derive(Debug, Clone, Copy)]
pub struct NightHorizonConfig {
    /// Working resolution width (pixels). Frames are downsampled to
    /// this width before profile computation. Default 200, matching
    /// the daylight detectors.
    pub working_width: u32,
    /// Vertical extent of the per-column gradient search window
    /// around the globally-detected horizon row, in working-image
    /// pixels. Default 8 (16-pixel total window). Smaller windows
    /// reject more outliers; larger windows tolerate more horizon
    /// curvature / camera tilt.
    pub search_half_height: u32,
    /// Minimum per-column gradient magnitude (in u16-difference
    /// units) for a column to contribute a candidate. Default 80,
    /// well below the daylight detector's 800 — at night the sea-
    /// sky luma difference is a few percent of full scale, not the
    /// 50%+ of a daylight scene.
    pub gradient_threshold: u16,
    /// Fraction of frame width used as the horizontal center band
    /// for per-row mean luma. Default 0.6. Avoids vignetting on
    /// wide-angle lenses.
    pub center_band_fraction: f64,
    /// Optional restriction on which rows are considered when
    /// searching for the global luma transition. Expressed as a
    /// fraction-of-frame-height range `(lo, hi)`, both in `[0, 1]`.
    /// Default `(0.0, 1.0)` (search everywhere).
    ///
    /// Use this when a saturated body (Moon) is in the upper part
    /// of the frame and its halo would otherwise dominate the
    /// global gradient search. Setting `(0.5, 1.0)` restricts to
    /// the lower half of the frame, which is where the horizon
    /// usually lives in night photography.
    pub search_row_range: (f64, f64),
    /// Maximum number of candidate horizons to return from
    /// [`detect_horizon_night_multi_pass`]. After finding the
    /// strongest, mask its row neighborhood and search again, up
    /// to this many passes. Default 3.
    pub max_passes: usize,
    /// Half-height of the row range to mask out after each found
    /// horizon, in working-image pixels. Should be large enough to
    /// cover the smoothed luma transition's vertical extent.
    /// Default = `2 * search_half_height` (so the mask covers the
    /// full search window plus a margin).
    pub multi_pass_mask_half_height: u32,
    /// Number of RANSAC iterations.
    pub ransac_iterations: u32,
    /// RANSAC inlier distance threshold (pixels at working
    /// resolution).
    pub ransac_inlier_px: f64,
    /// Minimum inlier count to accept a fit, as a fraction of
    /// candidates. Default 0.3 — looser than the daylight 0.5
    /// because night scenes have lower SNR and the per-column
    /// gradient at the horizon is intrinsically weaker, so a
    /// smaller fraction of columns produce strong votes.
    pub min_inlier_fraction: f64,
}

impl Default for NightHorizonConfig {
    fn default() -> Self {
        Self {
            working_width: 200,
            search_half_height: 8,
            gradient_threshold: 80,
            center_band_fraction: 0.6,
            search_row_range: (0.0, 1.0),
            max_passes: 3,
            multi_pass_mask_half_height: 16,
            ransac_iterations: 200,
            ransac_inlier_px: 2.0,
            min_inlier_fraction: 0.3,
        }
    }
}

/// Detect the sea horizon in a low-light scene.
///
/// Works by finding the row index of largest vertical luma
/// transition in a smoothed per-row mean profile, then refining
/// to per-column candidates in a window around that row.
///
/// # Errors
///
/// Returns [`HorizonError::InsufficientCandidates`] if too few
/// columns produce above-threshold gradients in the search window,
/// or [`HorizonError::LowConfidence`] if RANSAC's inlier count
/// falls below the configured fraction.
pub fn detect_horizon_night(
    frame: &Frame,
    cfg: NightHorizonConfig,
) -> Result<HorizonLine, HorizonError> {
    detect_horizon_night_with_column_mask(frame, cfg, None)
}

/// As [`detect_horizon_night`] but skips columns where
/// `column_mask[x] == false`. The mask is in *frame-resolution*
/// columns; pass `None` to consider every column.
///
/// Use with [`body_column_mask`] when a bright Moon (or other
/// saturated body) sits near the horizon and would otherwise
/// generate spurious gradient votes in the search window. The
/// `night_test_lowres` corpus scene is the motivating example —
/// the Moon at y ≈ 350 is well above the horizon (which is in the
/// lower portion of the portrait frame) but its brightness halo
/// could confuse the per-row profile if not excluded.
///
/// # Errors
///
/// As [`detect_horizon_night`].
pub fn detect_horizon_night_with_column_mask(
    frame: &Frame,
    cfg: NightHorizonConfig,
    column_mask: Option<&[bool]>,
) -> Result<HorizonLine, HorizonError> {
    let mut row_exclusions: Vec<(u32, u32)> = Vec::new();
    detect_horizon_night_inner(frame, cfg, column_mask, &mut row_exclusions)
}

/// Multi-pass variant: run the detector multiple times, each time
/// masking out a row range around the previous pass's horizon row.
/// Returns up to `cfg.max_passes` candidate horizons sorted by
/// inlier count (best first).
///
/// Useful when the strongest luma transition isn't the sea-sky
/// horizon — e.g. on `container_ship_night*` scenes where the
/// deck-to-sky boundary outvotes the actual horizon, or
/// `night_test_lowres` where the moon halo's edge dominates. The
/// caller picks the best candidate based on additional context
/// (expected horizon row from IMU, segmentation prior, or by
/// taking the candidate with the most inliers in the lower half
/// of the frame).
///
/// Returns an empty Vec if no pass produces a fit.
#[must_use]
pub fn detect_horizon_night_multi_pass(
    frame: &Frame,
    cfg: NightHorizonConfig,
    column_mask: Option<&[bool]>,
) -> Vec<HorizonLine> {
    let mut row_exclusions: Vec<(u32, u32)> = Vec::new();
    let mut results: Vec<HorizonLine> = Vec::new();

    let scale = f64::from(frame.width()) / f64::from(cfg.working_width);
    let working_height = (f64::from(frame.height()) / scale).round() as u32;

    for _ in 0..cfg.max_passes.max(1) {
        match detect_horizon_night_inner(frame, cfg, column_mask, &mut row_exclusions) {
            Ok(line) => {
                // Convert the found line's full-resolution
                // intercept back to a working-image row, then mask
                // a window around it for the next pass.
                let working_row = (line.intercept / scale) as u32;
                let half = cfg.multi_pass_mask_half_height;
                let lo = working_row.saturating_sub(half);
                let hi = (working_row + half).min(working_height.saturating_sub(1));
                row_exclusions.push((lo, hi));
                results.push(line);
            }
            Err(_) => break,
        }
    }

    // Sort best (most inliers) first.
    results.sort_by_key(|h| std::cmp::Reverse(h.inlier_count));
    results
}

/// Internal: shared single-pass logic. The `row_exclusions`
/// parameter holds working-row ranges that should be masked from
/// the global gradient-row search; pass an empty vec for the
/// single-pass entry, or a non-empty one to suppress already-
/// found horizons in the multi-pass entry.
fn detect_horizon_night_inner(
    frame: &Frame,
    cfg: NightHorizonConfig,
    column_mask: Option<&[bool]>,
    row_exclusions: &[(u32, u32)],
) -> Result<HorizonLine, HorizonError> {
    if let Some(m) = column_mask {
        if m.len() != frame.width() as usize {
            return Err(HorizonError::InsufficientCandidates(0));
        }
    }
    let scale = f64::from(frame.width()) / f64::from(cfg.working_width);
    let working_height = (f64::from(frame.height()) / scale).round() as u32;
    let work = downsample(frame, cfg.working_width, working_height);

    // Per-row mean luma over the center band, applying the column
    // mask if present. Excluding masked columns from the row-mean
    // computation is essential when the mask covers a saturated
    // body: otherwise the body's bright pixels skew the per-row
    // profile and the global gradient peak lands on the body's
    // row instead of the horizon's.
    let band_lo = ((1.0 - cfg.center_band_fraction) / 2.0 * f64::from(work.width)) as u32;
    let band_hi = work.width - band_lo;
    // The column mask is in full-resolution columns; downsample it
    // to working-resolution by sampling at the working column's
    // center mapped back to full resolution.
    let work_mask: Option<Vec<bool>> = column_mask.map(|m| {
        (0..work.width)
            .map(|wx| {
                let full_x = ((f64::from(wx) + 0.5) * scale) as usize;
                m.get(full_x).copied().unwrap_or(false)
            })
            .collect()
    });
    let row_means = per_row_mean_masked(&work, band_lo, band_hi, work_mask.as_deref());

    // Smooth and find the row with maximum vertical gradient,
    // restricted to the configured search row range and excluding
    // any row ranges from previous passes.
    let smoothed = smooth_1d(&row_means, smoothing_kernel_size(work.height));
    let row_lo = (cfg.search_row_range.0.clamp(0.0, 1.0) * f64::from(work.height)) as u32;
    let row_hi = (cfg.search_row_range.1.clamp(0.0, 1.0) * f64::from(work.height)) as u32;
    let horizon_row =
        max_gradient_row_in_range_excluding(&smoothed, row_lo, row_hi, row_exclusions)
            .ok_or(HorizonError::InsufficientCandidates(0))?;

    // Per-column candidates in a window around horizon_row.
    let candidates = column_candidates_in_window(
        &work,
        horizon_row,
        cfg.search_half_height,
        cfg.gradient_threshold,
    );

    // Map candidates back to full-resolution coords and apply the
    // optional column mask.
    let candidates_full: Vec<(f64, f64)> = candidates
        .into_iter()
        .map(|(x, y)| (x * scale, y * scale))
        .filter(|&(x, _)| match column_mask {
            None => true,
            Some(m) => m.get(x as usize).copied().unwrap_or(false),
        })
        // Map back to working coords for finalize_horizon.
        .map(|(x, y)| (x / scale, y / scale))
        .collect();

    let horizon_cfg = HorizonConfig {
        working_width: cfg.working_width,
        ransac_iterations: cfg.ransac_iterations,
        ransac_inlier_px: cfg.ransac_inlier_px,
        min_inlier_fraction: cfg.min_inlier_fraction,
        ..HorizonConfig::default()
    };
    finalize_horizon(frame, &candidates_full, scale, &horizon_cfg)
}

/// Convenience: build a body-excluding column mask, restrict the
/// global row search to "below the body" (assuming the body is
/// above the horizon, the usual case), and run
/// [`detect_horizon_night_with_column_mask`].
///
/// `body_centroid_x`, `body_centroid_y`, and `body_radius_px` are
/// in frame-resolution pixels; `pad_px` adds a margin around the
/// body's column range.
///
/// The search-row restriction is critical when a saturated body
/// (Moon) is in the upper part of the frame: its halo's luma
/// transition is far steeper than the sea-sky horizon's, so
/// without restriction the global gradient peak lands on the
/// body's halo at the body's row rather than the sea-sky
/// horizon's row. Restricting the search to rows strictly below
/// the body resolves this.
///
/// # Errors
///
/// As [`detect_horizon_night`].
pub fn detect_horizon_night_excluding_body(
    frame: &Frame,
    cfg: NightHorizonConfig,
    body_centroid_x: f64,
    body_centroid_y: f64,
    body_radius_px: f64,
    pad_px: f64,
) -> Result<HorizonLine, HorizonError> {
    let mask = body_column_mask(frame.width(), body_centroid_x, body_radius_px, pad_px);
    // Restrict the global row search to "starting just below the
    // body's lower edge." 5% pad below the body keeps the search
    // away from the halo.
    let body_lower_y = body_centroid_y + body_radius_px + pad_px;
    let frame_h = f64::from(frame.height());
    let row_lo = ((body_lower_y / frame_h) + 0.05).clamp(0.0, 1.0);
    let cfg_with_range = NightHorizonConfig {
        search_row_range: (row_lo, 1.0),
        ..cfg
    };
    detect_horizon_night_with_column_mask(frame, cfg_with_range, Some(&mask))
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Box-average downsample (same as the daylight detectors). Local
/// copy to avoid making the daylight version `pub(crate)` for one
/// caller; if a third caller emerges, factor out.
fn downsample(frame: &Frame, out_w: u32, out_h: u32) -> WorkingImage {
    let in_w = frame.width();
    let in_h = frame.height();
    let pixels = frame.pixels();
    let scale_x = f64::from(in_w) / f64::from(out_w);
    let scale_y = f64::from(in_h) / f64::from(out_h);
    let mut out = vec![0u16; (out_w as usize) * (out_h as usize)];
    for oy in 0..out_h {
        let y0 = (f64::from(oy) * scale_y) as u32;
        let y1 = ((f64::from(oy + 1) * scale_y) as u32).min(in_h);
        for ox in 0..out_w {
            let x0 = (f64::from(ox) * scale_x) as u32;
            let x1 = ((f64::from(ox + 1) * scale_x) as u32).min(in_w);
            let mut sum: u64 = 0;
            let mut count: u64 = 0;
            for y in y0..y1 {
                let row_off = (y as usize) * (in_w as usize);
                for x in x0..x1 {
                    sum += u64::from(pixels[row_off + (x as usize)]);
                    count += 1;
                }
            }
            let v = if count == 0 { 0 } else { (sum / count) as u16 };
            out[(oy as usize) * (out_w as usize) + (ox as usize)] = v;
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

/// Per-row mean luma over columns `[x_lo, x_hi)`, optionally
/// honoring a working-resolution column mask. When `column_mask` is
/// supplied (length must equal `img.width`), columns where
/// `column_mask[x] == false` are excluded from the mean.
fn per_row_mean_masked(
    img: &WorkingImage,
    x_lo: u32,
    x_hi: u32,
    column_mask: Option<&[bool]>,
) -> Vec<f64> {
    let w = img.width;
    let h = img.height;
    let lo = x_lo.min(w);
    let hi = x_hi.min(w);
    if lo >= hi {
        return vec![0.0; h as usize];
    }
    let mut out = vec![0.0_f64; h as usize];
    for y in 0..h {
        let row_off = (y as usize) * (w as usize);
        let mut sum: u64 = 0;
        let mut count: u64 = 0;
        for x in lo..hi {
            if let Some(m) = column_mask {
                if !m.get(x as usize).copied().unwrap_or(false) {
                    continue;
                }
            }
            sum += u64::from(img.pixels[row_off + (x as usize)]);
            count += 1;
        }
        out[y as usize] = if count == 0 {
            0.0
        } else {
            (sum as f64) / (count as f64)
        };
    }
    out
}

/// Box smoothing (1D, replicate edges) with kernel half-size `half`.
/// Returns a Vec of the same length.
fn smooth_1d(profile: &[f64], half: usize) -> Vec<f64> {
    let n = profile.len();
    let mut out = vec![0.0; n];
    for (i, slot) in out.iter_mut().enumerate() {
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(n);
        let count = (hi - lo) as f64;
        let mut sum = 0.0;
        for v in &profile[lo..hi] {
            sum += v;
        }
        *slot = sum / count;
    }
    out
}

/// Choose a smoothing kernel size proportional to the working
/// height. Default ~3% of height; minimum 2 to do meaningful
/// smoothing.
fn smoothing_kernel_size(working_height: u32) -> usize {
    ((f64::from(working_height) * 0.03) as usize).max(2)
}

/// Same as [`max_gradient_row_in_range`] but additionally skips
/// row indices that fall within any `(lo, hi)` exclusion range
/// (inclusive on both ends). Used by the multi-pass detector to
/// suppress already-found horizon rows.
fn max_gradient_row_in_range_excluding(
    profile: &[f64],
    lo: u32,
    hi: u32,
    exclusions: &[(u32, u32)],
) -> Option<u32> {
    let n = profile.len();
    if n < 3 {
        return None;
    }
    let lo = lo.max(1) as usize;
    let hi = (hi as usize).min(n.saturating_sub(1));
    if lo >= hi {
        return None;
    }
    let mut best: f64 = 0.0;
    let mut best_y: u32 = 0;
    for y in lo..hi {
        let y_u32 = y as u32;
        if exclusions
            .iter()
            .any(|&(elo, ehi)| y_u32 >= elo && y_u32 <= ehi)
        {
            continue;
        }
        let g = (profile[y + 1] - profile[y - 1]).abs();
        if g > best {
            best = g;
            best_y = y_u32;
        }
    }
    if best > 0.0 {
        Some(best_y)
    } else {
        None
    }
}

/// Find the row index in `[lo, hi)` with the largest
/// `|profile[y+1] - profile[y-1]|`. Returns None if the profile is
/// too short or the range is empty.
fn max_gradient_row_in_range(profile: &[f64], lo: u32, hi: u32) -> Option<u32> {
    max_gradient_row_in_range_excluding(profile, lo, hi, &[])
}

/// For each column, find the row in `[horizon_row - half,
/// horizon_row + half]` where the absolute vertical gradient is
/// largest. Emit as a candidate if it exceeds `threshold`.
fn column_candidates_in_window(
    img: &WorkingImage,
    horizon_row: u32,
    half: u32,
    threshold: u16,
) -> Vec<(f64, f64)> {
    let w = img.width;
    let h = img.height;
    if h < 3 {
        return Vec::new();
    }
    let row_lo = horizon_row.saturating_sub(half).max(1);
    let row_hi = (horizon_row + half).min(h.saturating_sub(2));
    if row_lo >= row_hi {
        return Vec::new();
    }
    let mut points = Vec::new();
    for x in 0..w {
        let mut best_grad: i32 = 0;
        let mut best_y: u32 = horizon_row;
        for y in row_lo..=row_hi {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Intrinsics;
    use bris_core::time::{Tt, JD_J2000};

    /// Synthesize a night scene: dark sea at the bottom, slightly
    /// brighter sky at the top, transition at row `horizon_y`.
    fn synth_night_scene(
        width: u32,
        height: u32,
        horizon_y: u32,
        sea_luma: u16,
        sky_luma: u16,
    ) -> Frame {
        let mut pixels = vec![0u16; (width as usize) * (height as usize)];
        for y in 0..height {
            let v = if y < horizon_y { sky_luma } else { sea_luma };
            for x in 0..width {
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
    fn finds_horizon_in_dim_two_band_scene() {
        // Sea at luma 800, sky at luma 1500 — small absolute
        // difference but consistent across the frame.
        let frame = synth_night_scene(640, 360, 200, 800, 1500);
        let line = detect_horizon_night(&frame, NightHorizonConfig::default()).unwrap();
        assert!(
            (line.intercept - 200.0).abs() < 5.0,
            "intercept {} not near true horizon row 200",
            line.intercept
        );
        assert!(
            line.slope.abs() < 0.05,
            "slope {} not near horizontal",
            line.slope
        );
    }

    #[test]
    fn finds_horizon_when_sea_is_brighter_than_sky() {
        // City-lights-on-water case: sea is brighter than sky.
        let frame = synth_night_scene(640, 360, 250, 1500, 800);
        let line = detect_horizon_night(&frame, NightHorizonConfig::default()).unwrap();
        assert!(
            (line.intercept - 250.0).abs() < 5.0,
            "intercept {} not near true horizon row 250",
            line.intercept
        );
    }

    #[test]
    fn fails_cleanly_on_uniformly_dark_frame() {
        let pixels = vec![100u16; 640 * 360];
        let frame = Frame::new(
            640,
            360,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(640, 360),
        )
        .unwrap();
        let result = detect_horizon_night(&frame, NightHorizonConfig::default());
        assert!(
            result.is_err(),
            "expected failure on uniform dark frame, got {result:?}",
        );
    }

    #[test]
    fn body_excluding_variant_skips_body_columns() {
        // Two-band scene with a "moon" (saturated bright spot) added.
        // Without the body-excluding variant the bright spot would
        // produce gradient votes in its column at a row different
        // from the true horizon. With the body excluded, RANSAC
        // ignores those votes.
        let mut pixels = vec![800u16; 640 * 360];
        for y in 0..200 {
            for x in 0..640 {
                pixels[(y as usize) * 640 + (x as usize)] = 1500;
            }
        }
        // Moon at (320, 100), 10 px radius, saturated.
        for y in 90..110 {
            for x in 310..330 {
                pixels[(y as usize) * 640 + (x as usize)] = u16::MAX;
            }
        }
        let frame = Frame::new(
            640,
            360,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(640, 360),
        )
        .unwrap();
        let line = detect_horizon_night_excluding_body(
            &frame,
            NightHorizonConfig::default(),
            320.0,
            100.0, // body y
            10.0,
            8.0,
        )
        .unwrap();
        // Should still find the true horizon at y=200.
        assert!(
            (line.intercept - 200.0).abs() < 5.0,
            "intercept {} not near true horizon row 200 (body should not have moved it)",
            line.intercept
        );
    }

    #[test]
    fn shape_mismatch_in_column_mask_returns_typed_error() {
        let frame = synth_night_scene(640, 360, 200, 800, 1500);
        let bad_mask = vec![true; 100];
        let result = detect_horizon_night_with_column_mask(
            &frame,
            NightHorizonConfig::default(),
            Some(&bad_mask),
        );
        assert!(matches!(
            result,
            Err(HorizonError::InsufficientCandidates(0))
        ));
    }

    /// Multi-pass detection on a synthetic three-region scene:
    /// dark sea (bottom) → dim sky (middle) → bright deck-glow
    /// (top). The single-pass detector finds the strongest
    /// transition (deck→sky); the multi-pass detector finds *both*
    /// the deck-edge and the sea-sky horizon, ranked by inlier
    /// count.
    ///
    /// This is the synthetic analog of `container_ship_night*`:
    /// the scene has multiple horizontal luma transitions, and
    /// only the lower one is the actual sea-sky horizon.
    #[test]
    fn multi_pass_finds_secondary_horizon() {
        let w: u32 = 640;
        let h: u32 = 360;
        let mut pixels = vec![0u16; (w * h) as usize];
        // Region 1: bright deck glow at top, y in [0, 100), luma 4000.
        // Region 2: dim sky in y in [100, 250), luma 600.
        // Region 3: dark sea in y in [250, 360), luma 200.
        for y in 0..h {
            let v = if y < 100 {
                4000
            } else if y < 250 {
                600
            } else {
                200
            };
            for x in 0..w {
                pixels[(y as usize) * (w as usize) + (x as usize)] = v;
            }
        }
        let frame = Frame::new(
            w,
            h,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(w, h),
        )
        .unwrap();

        // Single-pass: should find the deck→sky transition at y ≈ 100
        // (much stronger than the sea-sky transition at y ≈ 250).
        let single = detect_horizon_night(&frame, NightHorizonConfig::default()).unwrap();
        assert!(
            (single.intercept - 100.0).abs() < 10.0,
            "single-pass should find the deck transition near y=100, got {}",
            single.intercept,
        );

        // Multi-pass with 2+ passes should find both transitions.
        let cfg = NightHorizonConfig {
            max_passes: 3,
            ..NightHorizonConfig::default()
        };
        let candidates = detect_horizon_night_multi_pass(&frame, cfg, None);
        assert!(
            candidates.len() >= 2,
            "multi-pass should find at least 2 candidates; got {}",
            candidates.len()
        );
        // One candidate should be near y=100 (deck), another near y=250 (sea horizon).
        let near_deck = candidates
            .iter()
            .any(|h| (h.intercept - 100.0).abs() < 10.0);
        let near_horizon = candidates
            .iter()
            .any(|h| (h.intercept - 250.0).abs() < 15.0);
        assert!(
            near_deck && near_horizon,
            "multi-pass should find both deck (y~100) and sea horizon (y~250); \
             got intercepts {:?}",
            candidates.iter().map(|h| h.intercept).collect::<Vec<_>>(),
        );
    }

    /// Multi-pass on a simple two-band scene returns only one
    /// candidate (the second-pass row exclusion eats the only
    /// real transition).
    #[test]
    fn multi_pass_on_simple_scene_returns_one_candidate() {
        let frame = synth_night_scene(640, 360, 200, 800, 1500);
        let candidates =
            detect_horizon_night_multi_pass(&frame, NightHorizonConfig::default(), None);
        assert!(!candidates.is_empty(), "should find at least one horizon");
        assert!(
            (candidates[0].intercept - 200.0).abs() < 5.0,
            "primary candidate should be near true horizon"
        );
    }
}
