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
    let row_exclusions: Vec<(u32, u32)> = Vec::new();
    detect_horizon_night_inner(frame, cfg, column_mask, &row_exclusions)
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
        match detect_horizon_night_inner(frame, cfg, column_mask, &row_exclusions) {
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
// Textured-water variant
// ---------------------------------------------------------------------------

/// Configuration for [`detect_horizon_night_textured`].
///
/// Distinct from [`NightHorizonConfig`] because the signal is
/// fundamentally different: this detector finds the row where
/// per-row *standard deviation* (texture) steps up, not where
/// per-row mean luma changes. The thresholds and RANSAC parameters
/// don't translate one-to-one.
#[derive(Debug, Clone, Copy)]
pub struct TexturedHorizonConfig {
    /// Working resolution width (pixels). Frames are downsampled
    /// before profile computation. Default 200.
    pub working_width: u32,
    /// Fraction of frame width used as the horizontal center band
    /// for per-row statistics. Default 0.6.
    pub center_band_fraction: f64,
    /// Vertical extent of the per-column refinement window around
    /// the globally-detected horizon row, in working-image pixels.
    /// Default 12 (24-pixel total window). Slightly larger than
    /// the mean-gradient detector's 8 because texture transitions
    /// are inherently fuzzier than brightness transitions.
    pub search_half_height: u32,
    /// Vertical extent of the per-column rolling-window in which
    /// each column's local std-dev is computed for refinement, in
    /// working-image pixels. Default 6 (13-pixel window). Should
    /// be small enough to localize the transition but large
    /// enough to give meaningful std-dev statistics.
    pub column_window_half_height: u32,
    /// Optional restriction on which rows are considered when
    /// searching for the global texture-step. Default
    /// `(0.0, 1.0)` (search everywhere).
    pub search_row_range: (f64, f64),
    /// Minimum *step ratio* required to call a row a horizon. The
    /// step ratio at row `y` is `mean_std(y .. y+window) /
    /// mean_std(y-window .. y)`. Values < 1.0 are downward steps
    /// (sea-above-sky); values > 1.0 are upward steps (sky-above-
    /// sea, the moon-glint case). Default 1.5 — accept ≥ 1.5×
    /// step in either direction.
    pub min_step_ratio: f64,
    /// Half-height of the row window over which each side's mean
    /// std-dev is averaged when computing the step ratio. Default
    /// 8 (16 working-image pixels).
    pub step_window_half_height: u32,
    /// Number of RANSAC iterations.
    pub ransac_iterations: u32,
    /// RANSAC inlier distance threshold (pixels at working
    /// resolution).
    pub ransac_inlier_px: f64,
    /// Minimum inlier count to accept a fit, as a fraction of
    /// candidates. Default 0.3 (textured-water scenes are noisy
    /// per column).
    pub min_inlier_fraction: f64,
}

impl Default for TexturedHorizonConfig {
    fn default() -> Self {
        Self {
            working_width: 200,
            center_band_fraction: 0.6,
            search_half_height: 12,
            column_window_half_height: 6,
            search_row_range: (0.0, 1.0),
            min_step_ratio: 1.5,
            step_window_half_height: 8,
            ransac_iterations: 200,
            ransac_inlier_px: 3.0,
            min_inlier_fraction: 0.3,
        }
    }
}

/// Build a 2D pixel mask that excludes a rectangular box around a
/// detected body. `true` = consider this pixel, `false` = exclude.
/// Length is `frame_width * frame_height`.
///
/// Use with [`detect_horizon_night_textured_with_pixel_mask`] when
/// a saturated body is in the frame and its halo extends radially
/// well beyond the body's column range. Unlike the 1D
/// [`body_column_mask`] (which only excludes columns), this 2D
/// mask preserves the *non-body parts* of the moon's columns —
/// crucial for portrait-orientation frames where excluding a 2D
/// box leaves most of the lower frame intact for horizon
/// detection.
#[must_use]
pub fn body_box_mask(
    frame_width: u32,
    frame_height: u32,
    body_centroid_x: f64,
    body_centroid_y: f64,
    body_radius_px: f64,
    pad_px: f64,
) -> Vec<bool> {
    let lo_x = (body_centroid_x - body_radius_px - pad_px).max(0.0) as u32;
    let hi_x = ((body_centroid_x + body_radius_px + pad_px).max(0.0) as u32)
        .min(frame_width.saturating_sub(1));
    let lo_y = (body_centroid_y - body_radius_px - pad_px).max(0.0) as u32;
    let hi_y = ((body_centroid_y + body_radius_px + pad_px).max(0.0) as u32)
        .min(frame_height.saturating_sub(1));
    let n = (frame_width as usize) * (frame_height as usize);
    let mut mask = vec![true; n];
    for y in lo_y..=hi_y {
        let row_off = (y as usize) * (frame_width as usize);
        for x in lo_x..=hi_x {
            mask[row_off + (x as usize)] = false;
        }
    }
    mask
}

/// Detect a sea-sky horizon via per-row texture (std-dev) gradient.
///
/// Optimized for scenes where the sky and sea differ in *texture*
/// rather than mean brightness — the canonical case is moonlit
/// water under dim sky, where the moon glints on the water make
/// the sea visibly textured while the sky stays near-uniform. The
/// existing [`detect_horizon_night`] looks for mean-luma gradient
/// steps and misses these scenes.
///
/// The two detectors are complementary, not competing. The right
/// choice depends on the scene:
///   - Mean-luma gradient: sky and sea have different mean
///     brightness (city-light glow on sea, twilight sky over
///     dark sea, etc.). Use [`detect_horizon_night`].
///   - Texture (std-dev) gradient: sky and sea have similar mean
///     brightness but differ in spatial variance (moonlit water,
///     wave glints). Use this function.
///
/// The streaming engine can run both and keep the best fit; offline
/// callers pick the appropriate one based on scene knowledge.
///
/// # Errors
///
/// See [`HorizonError`].
pub fn detect_horizon_night_textured(
    frame: &Frame,
    cfg: TexturedHorizonConfig,
) -> Result<HorizonLine, HorizonError> {
    detect_horizon_night_textured_with_pixel_mask(frame, cfg, None)
}

/// As [`detect_horizon_night_textured`] but skips pixels where
/// `pixel_mask[y * width + x] == false`. Use [`body_box_mask`] to
/// build the mask from a detected body centroid + radius.
///
/// # Errors
///
/// As [`detect_horizon_night_textured`].
pub fn detect_horizon_night_textured_with_pixel_mask(
    frame: &Frame,
    cfg: TexturedHorizonConfig,
    pixel_mask: Option<&[bool]>,
) -> Result<HorizonLine, HorizonError> {
    if let Some(m) = pixel_mask {
        let expected = (frame.width() as usize) * (frame.height() as usize);
        if m.len() != expected {
            return Err(HorizonError::InsufficientCandidates(0));
        }
    }
    let scale = f64::from(frame.width()) / f64::from(cfg.working_width);
    let working_height = (f64::from(frame.height()) / scale).round() as u32;
    let work = downsample(frame, cfg.working_width, working_height);

    // Downsample the pixel mask to working resolution by
    // sampling the center pixel of each working block.
    let work_mask: Option<Vec<bool>> = pixel_mask.map(|m| {
        let mut out = vec![true; (cfg.working_width as usize) * (working_height as usize)];
        for wy in 0..working_height {
            let full_y = ((f64::from(wy) + 0.5) * scale) as usize;
            for wx in 0..cfg.working_width {
                let full_x = ((f64::from(wx) + 0.5) * scale) as usize;
                let full_idx = full_y * (frame.width() as usize) + full_x;
                let work_idx = (wy as usize) * (cfg.working_width as usize) + (wx as usize);
                out[work_idx] = m.get(full_idx).copied().unwrap_or(true);
            }
        }
        out
    });

    // Per-row std-dev over the center band, mask-aware.
    let band_lo = ((1.0 - cfg.center_band_fraction) / 2.0 * f64::from(work.width)) as u32;
    let band_hi = work.width - band_lo;
    let row_stds = per_row_std_masked(&work, band_lo, band_hi, work_mask.as_deref());

    // Smooth the std-dev profile.
    let smoothed = smooth_1d(&row_stds, smoothing_kernel_size(work.height));

    // Find the row with the largest *step* in std-dev (low → high
    // or high → low). Restricted to the configured search range.
    let row_lo = (cfg.search_row_range.0.clamp(0.0, 1.0) * f64::from(work.height)) as u32;
    let row_hi = (cfg.search_row_range.1.clamp(0.0, 1.0) * f64::from(work.height)) as u32;
    let horizon_row = max_step_row(
        &smoothed,
        row_lo,
        row_hi,
        cfg.step_window_half_height,
        cfg.min_step_ratio,
    )
    .ok_or(HorizonError::InsufficientCandidates(0))?;

    // Per-column refinement: in a window around horizon_row, find
    // each column's row where the local-window std-dev steps up.
    let candidates = per_column_texture_step_in_window(
        &work,
        horizon_row,
        cfg.search_half_height,
        cfg.column_window_half_height,
        work_mask.as_deref(),
    );

    // Map candidates back to full-resolution coords.
    let candidates_full: Vec<(f64, f64)> = candidates
        .into_iter()
        .map(|(x, y)| (x * scale, y * scale))
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

/// Convenience: detect a body, build a 2D box mask around it,
/// restrict search to rows below the body, and run
/// [`detect_horizon_night_textured_with_pixel_mask`].
///
/// `body_centroid_x`, `body_centroid_y`, and `body_radius_px` are
/// in frame-resolution pixels.
///
/// # Errors
///
/// As [`detect_horizon_night_textured`].
pub fn detect_horizon_night_textured_excluding_body(
    frame: &Frame,
    cfg: TexturedHorizonConfig,
    body_centroid_x: f64,
    body_centroid_y: f64,
    body_radius_px: f64,
    pad_px: f64,
) -> Result<HorizonLine, HorizonError> {
    let mask = body_box_mask(
        frame.width(),
        frame.height(),
        body_centroid_x,
        body_centroid_y,
        body_radius_px,
        pad_px,
    );
    let body_lower_y = body_centroid_y + body_radius_px + pad_px;
    let frame_h = f64::from(frame.height());
    let row_lo = ((body_lower_y / frame_h) + 0.05).clamp(0.0, 1.0);
    let cfg_with_range = TexturedHorizonConfig {
        search_row_range: (row_lo, 1.0),
        ..cfg
    };
    detect_horizon_night_textured_with_pixel_mask(frame, cfg_with_range, Some(&mask))
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

/// Per-row standard deviation of luma over columns `[x_lo, x_hi)`,
/// optionally honoring a working-resolution pixel mask. When a
/// pixel mask is supplied, masked-out pixels are excluded from
/// the per-row statistics in *both dimensions* (mask is per-pixel,
/// not per-column).
///
/// Returns 0 for rows with fewer than 2 unmasked pixels (std-dev
/// undefined).
fn per_row_std_masked(
    img: &WorkingImage,
    x_lo: u32,
    x_hi: u32,
    pixel_mask: Option<&[bool]>,
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
        let mut sum: f64 = 0.0;
        let mut sum_sq: f64 = 0.0;
        let mut count: u64 = 0;
        for x in lo..hi {
            if let Some(m) = pixel_mask {
                if !m.get(row_off + x as usize).copied().unwrap_or(false) {
                    continue;
                }
            }
            let v = f64::from(img.pixels[row_off + (x as usize)]);
            sum += v;
            sum_sq += v * v;
            count += 1;
        }
        if count < 2 {
            out[y as usize] = 0.0;
            continue;
        }
        let n = count as f64;
        let mean = sum / n;
        let var = (sum_sq / n) - mean * mean;
        out[y as usize] = var.max(0.0).sqrt();
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

/// Find the row index in `[lo, hi)` with the largest
/// `|profile[y+1] - profile[y-1]|`, additionally skipping any
/// row indices that fall within any `(elo, ehi)` exclusion range
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

/// Find the row index in `[lo, hi)` with the largest *step* in
/// the smoothed profile, where step at row y is defined as
/// `mean(profile[y+1 .. y+1+window]) / mean(profile[y-window .. y])`
/// for upward steps, or its inverse for downward steps. Returns
/// the row whose absolute log-step (i.e. larger of step or 1/step)
/// exceeds `min_step_ratio`, with maximum log-step magnitude.
///
/// Used by the textured-water detector to find the row where
/// per-row std-dev jumps from low (sky) to high (textured sea) or
/// vice versa. Compared to a plain gradient (`max_gradient_row_*`),
/// the step-ratio formulation is robust to the absolute magnitude
/// of the texture signal — it finds *transitions* of any
/// magnitude as long as the ratio between sides exceeds the
/// threshold.
fn max_step_row(
    profile: &[f64],
    lo: u32,
    hi: u32,
    window_half: u32,
    min_step_ratio: f64,
) -> Option<u32> {
    let n = profile.len();
    if n < (2 * window_half as usize + 3) {
        return None;
    }
    let lo = lo.max(window_half + 1) as usize;
    let hi = (hi as usize).min(n.saturating_sub(window_half as usize + 1));
    if lo >= hi {
        return None;
    }
    let half = window_half as usize;
    let mut best_log_step: f64 = 0.0;
    let mut best_y: Option<u32> = None;
    for y in lo..hi {
        // Mean of the window above (lower-y) and below (higher-y).
        let above_lo = y.saturating_sub(half);
        let above_hi = y;
        let below_lo = y + 1;
        let below_hi = (y + 1 + half).min(n);
        if above_hi <= above_lo || below_hi <= below_lo {
            continue;
        }
        let above_mean: f64 =
            profile[above_lo..above_hi].iter().sum::<f64>() / (above_hi - above_lo) as f64;
        let below_mean: f64 =
            profile[below_lo..below_hi].iter().sum::<f64>() / (below_hi - below_lo) as f64;
        // Step ratio: larger over smaller. Add a small floor to
        // avoid division by zero when one side is uniformly zero
        // (a degenerate case for masked rows).
        let eps = 1e-6;
        let larger = above_mean.max(below_mean);
        let smaller = above_mean.min(below_mean).max(eps);
        let ratio = larger / smaller;
        if ratio < min_step_ratio {
            continue;
        }
        let log_step = ratio.ln();
        if log_step > best_log_step {
            best_log_step = log_step;
            best_y = Some(y as u32);
        }
    }
    best_y
}

/// Per-column refinement for the textured-water detector. For each
/// column, in a window around `horizon_row` ± `search_half`, find
/// the y that maximizes the local-window std-dev step. Returns
/// candidates in working-image (column, row) coordinates.
///
/// The local-window std-dev is computed over a vertical window of
/// `column_window_half` pixels above and below each candidate y;
/// "step" is the ratio of below-mean-std to above-mean-std (or
/// vice versa). This is the per-column version of
/// [`max_step_row`].
fn per_column_texture_step_in_window(
    img: &WorkingImage,
    horizon_row: u32,
    search_half: u32,
    column_window_half: u32,
    pixel_mask: Option<&[bool]>,
) -> Vec<(f64, f64)> {
    let w = img.width;
    let h = img.height;
    let mut points = Vec::new();
    if h < (2 * column_window_half as usize + 3) as u32 {
        return points;
    }
    let row_lo = horizon_row
        .saturating_sub(search_half)
        .max(column_window_half + 1);
    let row_hi = (horizon_row + search_half).min(h.saturating_sub(column_window_half + 1));
    if row_lo >= row_hi {
        return points;
    }

    // For efficiency, precompute a per-pixel "is in mask" check
    // once and reuse. The local-std computation per (col, y) walks
    // a small window, so the inner loops are tiny.
    let in_mask =
        |idx: usize| -> bool { pixel_mask.is_none_or(|m| m.get(idx).copied().unwrap_or(false)) };

    for x in 0..w {
        let mut best_log_step = 0.0_f64;
        let mut best_y: Option<u32> = None;
        for y in row_lo..=row_hi {
            // Above window: rows [y - half, y - 1].
            let mut sum_a: f64 = 0.0;
            let mut sum_sq_a: f64 = 0.0;
            let mut count_a: u32 = 0;
            for ay in y.saturating_sub(column_window_half)..y {
                let idx = (ay as usize) * (w as usize) + (x as usize);
                if !in_mask(idx) {
                    continue;
                }
                let v = f64::from(img.pixels[idx]);
                sum_a += v;
                sum_sq_a += v * v;
                count_a += 1;
            }
            // Below window: rows [y + 1, y + half].
            let mut sum_b: f64 = 0.0;
            let mut sum_sq_b: f64 = 0.0;
            let mut count_b: u32 = 0;
            for by in y + 1..=(y + column_window_half).min(h.saturating_sub(1)) {
                let idx = (by as usize) * (w as usize) + (x as usize);
                if !in_mask(idx) {
                    continue;
                }
                let v = f64::from(img.pixels[idx]);
                sum_b += v;
                sum_sq_b += v * v;
                count_b += 1;
            }
            if count_a < 2 || count_b < 2 {
                continue;
            }
            let mean_a = sum_a / f64::from(count_a);
            let var_a = (sum_sq_a / f64::from(count_a)) - mean_a * mean_a;
            let std_a = var_a.max(0.0).sqrt();
            let mean_b = sum_b / f64::from(count_b);
            let var_b = (sum_sq_b / f64::from(count_b)) - mean_b * mean_b;
            let std_b = var_b.max(0.0).sqrt();
            let eps = 1e-6;
            let larger = std_a.max(std_b);
            let smaller = std_a.min(std_b).max(eps);
            let ratio = larger / smaller;
            if ratio < 1.2 {
                // Per-column threshold is laxer than the global
                // step-row threshold: even a small per-column
                // ratio is informative when many columns vote for
                // the same row.
                continue;
            }
            let log_step = ratio.ln();
            if log_step > best_log_step {
                best_log_step = log_step;
                best_y = Some(y);
            }
        }
        if let Some(y) = best_y {
            points.push((f64::from(x), f64::from(y)));
        }
    }
    points
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

    // -----------------------------------------------------------------
    // Textured-water detector
    // -----------------------------------------------------------------

    /// Synthesize a night scene with uniform sky on top and
    /// textured (noisy) sea on the bottom. Both have similar mean
    /// luma; the only signal distinguishing them is per-row
    /// std-dev. The standard detector cannot find this; the
    /// textured detector should.
    fn synth_textured_night_scene(width: u32, height: u32, horizon_y: u32) -> Frame {
        // Use a deterministic pseudo-random pattern (no rand
        // dependency) for the sea texture: alternating bright and
        // dim pixels at high frequency. Mean luma is the same as
        // the uniform sky.
        let mean_luma: u16 = 1200;
        let amplitude: u16 = 400;
        let mut pixels = vec![mean_luma; (width * height) as usize];
        for y in horizon_y..height {
            for x in 0..width {
                // Pseudo-random ±amplitude based on (x, y).
                let h = (x.wrapping_mul(13) ^ y.wrapping_mul(31)) % 8;
                let v = if h < 4 {
                    mean_luma.saturating_sub(amplitude)
                } else {
                    mean_luma.saturating_add(amplitude)
                };
                pixels[(y as usize) * (width as usize) + (x as usize)] = v;
            }
        }
        Frame::new(
            width,
            height,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    #[test]
    fn textured_detector_finds_horizon_when_mean_luma_is_unchanged() {
        let frame = synth_textured_night_scene(640, 360, 200);
        let line = detect_horizon_night_textured(&frame, TexturedHorizonConfig::default())
            .expect("textured detector should find the texture transition");
        assert!(
            (line.intercept - 200.0).abs() < 10.0,
            "expected intercept near 200, got {:.1}",
            line.intercept
        );
    }

    #[test]
    fn textured_detector_fails_on_uniform_frame() {
        let pixels = vec![1200u16; 640 * 360];
        let frame = Frame::new(
            640,
            360,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(640, 360),
        )
        .unwrap();
        let result = detect_horizon_night_textured(&frame, TexturedHorizonConfig::default());
        assert!(result.is_err(), "expected failure on uniform frame");
    }

    #[test]
    fn body_box_mask_excludes_rectangle() {
        let mask = body_box_mask(20, 10, 10.0, 5.0, 2.0, 1.0);
        // Box: x in [7..=13], y in [2..=8]
        for y in 0..10u32 {
            for x in 0..20u32 {
                let idx = (y * 20 + x) as usize;
                let expected = !((7..=13).contains(&x) && (2..=8).contains(&y));
                assert_eq!(
                    mask[idx], expected,
                    "({x}, {y}): expected {expected}, got {}",
                    mask[idx]
                );
            }
        }
    }

    #[test]
    fn textured_detector_with_body_mask_finds_horizon_below_masked_body() {
        // Build a textured scene with horizon at y=250, plus a
        // bright "moon" patch up at (320, 80). The unmasked
        // textured detector might be confused by the moon's
        // single-row brightness; the masked variant should not.
        let mut pixels = vec![1200u16; 640 * 360];
        // Add textured "sea" below y=250.
        for y in 250u32..360 {
            for x in 0u32..640 {
                let h = (x.wrapping_mul(13) ^ y.wrapping_mul(31)) % 8;
                let v = if h < 4 { 800u16 } else { 1600u16 };
                pixels[(y as usize) * 640 + (x as usize)] = v;
            }
        }
        // Add moon at (320, 80), 10 px radius, saturated.
        for y in 70u32..90 {
            for x in 310u32..330 {
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

        let line = detect_horizon_night_textured_excluding_body(
            &frame,
            TexturedHorizonConfig::default(),
            320.0,
            80.0,
            10.0,
            8.0,
        )
        .expect("body-excluding textured detector should find horizon");
        assert!(
            (line.intercept - 250.0).abs() < 10.0,
            "expected intercept near 250, got {:.1}",
            line.intercept
        );
    }
}
