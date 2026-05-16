//! Pose-coverage estimator for an interactive calibration session.
//!
//! Zhang's planar method is well-known to be brittle when
//! all views share an axis: if the operator captured 30
//! frames all roughly fronto-parallel to the board, the
//! solve will technically converge but the recovered focal
//! length is poorly constrained. The
//! [`crate::doctor`] post-solve diagnostic catches the
//! resulting pathologies (asymmetric `fx`/`fy`, tangential
//! distortion absorbing noise) but only after the fact.
//!
//! The coverage estimator below operates on the same
//! [`crate::DetectedView`]s the solver consumes, but
//! *before* the solve. It bins the labelled chessboard
//! corners into a grid over image space and reports which
//! grid cells the operator has not yet sampled, plus a
//! coarse "tilt diversity" estimate based on the
//! aspect-ratio distribution of the per-view bounding
//! boxes.
//!
//! The output is intended for two consumers:
//!
//! - **CLI** — run as a one-shot summary after the
//!   directory walk; print "you covered 40% of the FOV;
//!   the upper-left corner has no samples" before the
//!   solve runs.
//! - **Android** — refresh after every successful capture
//!   so the operator has a live "where to point next"
//!   indicator during the session.
//!
//! # Tilt diversity (placeholder)
//!
//! Without running the solve we don't know per-view
//! extrinsics. The aspect-ratio of the labelled-corner
//! bounding box is a cheap proxy: a fronto-parallel
//! capture of an `R × C` board has bbox aspect close to
//! `R/C`; a board tilted around its short axis squashes
//! the bbox towards a smaller aspect ratio. Spread of
//! the per-view aspect ratios approximates pose diversity.
//! This is heuristic; the real fix is post-solve pose
//! analysis, but that requires either a partial solve
//! per capture (expensive) or extracting per-view
//! homographies from the detector (deferred).

use crate::detect::DetectedView;

/// Configurable knobs for [`coverage`].
#[derive(Debug, Clone, Copy)]
pub struct CoverageConfig {
    /// Number of grid cells along the X axis (image width).
    pub grid_cols: u32,
    /// Number of grid cells along the Y axis (image height).
    pub grid_rows: u32,
}

impl Default for CoverageConfig {
    /// 4 × 4 — coarse enough that "all cells covered" is a
    /// reachable goal in 16-30 captures, fine enough that
    /// it catches operators who only point at the center.
    fn default() -> Self {
        Self {
            grid_cols: 4,
            grid_rows: 4,
        }
    }
}

/// Image-space coverage report.
///
/// `cell_counts[r * grid_cols + c]` is the number of views
/// whose labelled-corner bounding box overlaps grid cell
/// `(r, c)` (row-major, top-left origin).
#[derive(Debug, Clone)]
pub struct CoverageReport {
    /// Image width the report was computed against.
    pub image_width: u32,
    /// Image height the report was computed against.
    pub image_height: u32,
    /// Grid configuration used.
    pub config: CoverageConfig,
    /// Row-major counts.
    pub cell_counts: Vec<u32>,
    /// Fraction of cells with at least one view contributing.
    /// `0.0..=1.0`; the headline "how covered is the FOV?"
    /// number.
    pub covered_fraction: f64,
    /// Number of cells with zero views. Renders to
    /// "covered 12/16 cells; 4 still empty (point at upper
    /// left, lower middle, …)".
    pub empty_cells: u32,
    /// Per-view bounding-box aspect-ratio distribution.
    /// Used for tilt-diversity heuristics (see module docs).
    pub aspect_ratios: Vec<f64>,
    /// Standard deviation of `aspect_ratios`. Higher = more
    /// pose diversity (heuristic; see module docs).
    pub aspect_ratio_stddev: f64,
}

impl CoverageReport {
    /// Returns `true` if all grid cells have at least one
    /// view contributing.
    #[must_use]
    pub fn fully_covered(&self) -> bool {
        self.empty_cells == 0
    }

    /// Cells (row, col) with zero views, in row-major order.
    /// Useful for an Android UI that wants to highlight
    /// missing regions on the preview.
    #[must_use]
    pub fn empty_cell_coords(&self) -> Vec<(u32, u32)> {
        let cols = self.config.grid_cols;
        self.cell_counts
            .iter()
            .enumerate()
            .filter(|(_, &c)| c == 0)
            .map(|(idx, _)| {
                #[allow(clippy::cast_possible_truncation)]
                let i = idx as u32;
                (i / cols, i % cols)
            })
            .collect()
    }
}

/// Compute a [`CoverageReport`] over a slice of detected
/// views.
///
/// All views must share width × height; the report is
/// computed against the first view's dimensions. Views with
/// different dimensions are ignored (this mirrors the
/// solver's behaviour, which would reject the dataset
/// outright).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    // i64 → u32 casts below are bounded by clamp(0, grid_*) so
    // the value always fits and is non-negative.
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
)]
#[must_use]
pub fn coverage(views: &[DetectedView], config: CoverageConfig) -> Option<CoverageReport> {
    let first = views.first()?;
    let w = first.width;
    let h = first.height;
    if w == 0 || h == 0 || config.grid_cols == 0 || config.grid_rows == 0 {
        return None;
    }
    let cell_w = f64::from(w) / f64::from(config.grid_cols);
    let cell_h = f64::from(h) / f64::from(config.grid_rows);
    let n_cells = (config.grid_cols * config.grid_rows) as usize;
    let mut counts = vec![0_u32; n_cells];
    let mut aspect_ratios = Vec::with_capacity(views.len());

    for view in views {
        if view.width != w || view.height != h {
            continue;
        }
        if view.correspondences.is_empty() {
            continue;
        }
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for c in &view.correspondences {
            min_x = min_x.min(c.pixel_x);
            min_y = min_y.min(c.pixel_y);
            max_x = max_x.max(c.pixel_x);
            max_y = max_y.max(c.pixel_y);
        }
        let bw = (max_x - min_x).max(1.0);
        let bh = (max_y - min_y).max(1.0);
        aspect_ratios.push(bw / bh);

        let c0 = ((min_x / cell_w).floor() as i64).clamp(0, i64::from(config.grid_cols - 1)) as u32;
        let c1 = ((max_x / cell_w).floor() as i64).clamp(0, i64::from(config.grid_cols - 1)) as u32;
        let r0 = ((min_y / cell_h).floor() as i64).clamp(0, i64::from(config.grid_rows - 1)) as u32;
        let r1 = ((max_y / cell_h).floor() as i64).clamp(0, i64::from(config.grid_rows - 1)) as u32;
        for r in r0..=r1 {
            for c in c0..=c1 {
                let idx = (r * config.grid_cols + c) as usize;
                counts[idx] = counts[idx].saturating_add(1);
            }
        }
    }
    let empty_cells = counts.iter().filter(|&&c| c == 0).count() as u32;
    let covered_fraction = 1.0 - (empty_cells as f64 / n_cells as f64);
    let aspect_ratio_stddev = stddev(&aspect_ratios);

    Some(CoverageReport {
        image_width: w,
        image_height: h,
        config,
        cell_counts: counts,
        covered_fraction,
        empty_cells,
        aspect_ratios,
        aspect_ratio_stddev,
    })
}

fn stddev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    var.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Correspondence;

    fn view_with_bbox(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> DetectedView {
        DetectedView {
            source: "x.png".into(),
            width: 640,
            height: 480,
            correspondences: vec![
                Correspondence {
                    pixel_x: min_x,
                    pixel_y: min_y,
                    board_x_m: 0.0,
                    board_y_m: 0.0,
                },
                Correspondence {
                    pixel_x: max_x,
                    pixel_y: max_y,
                    board_x_m: 0.025,
                    board_y_m: 0.025,
                },
            ],
        }
    }

    #[test]
    fn no_views_returns_none() {
        assert!(coverage(&[], CoverageConfig::default()).is_none());
    }

    #[test]
    fn single_centered_view_covers_one_cell_only() {
        // 4×4 grid over 640×480 ⇒ cells are 160×120.
        // A bbox entirely interior to row 1 col 1 is
        // x ∈ (160, 320) and y ∈ (120, 240). Pick a 40×40
        // bbox centered there.
        let v = view_with_bbox(220.0, 160.0, 260.0, 200.0);
        let r = coverage(&[v], CoverageConfig::default()).unwrap();
        let covered: u32 = r.cell_counts.iter().filter(|&&c| c > 0).sum();
        assert_eq!(
            covered, 1,
            "small centered bbox should hit 1 cell, counts={:?}",
            r.cell_counts
        );
        assert_eq!(r.empty_cells, 15);
    }

    #[test]
    fn full_frame_view_covers_all_cells() {
        let v = view_with_bbox(5.0, 5.0, 635.0, 475.0);
        let r = coverage(&[v], CoverageConfig::default()).unwrap();
        assert_eq!(r.empty_cells, 0);
        assert!((r.covered_fraction - 1.0).abs() < 1e-9);
        assert!(r.fully_covered());
    }

    #[test]
    fn empty_cell_coords_returns_row_col_pairs() {
        let v = view_with_bbox(220.0, 160.0, 260.0, 200.0);
        let r = coverage(&[v], CoverageConfig::default()).unwrap();
        let empties = r.empty_cell_coords();
        assert_eq!(empties.len(), 15);
        // Sanity: (0,0) (top-left) should be empty.
        assert!(empties.contains(&(0, 0)));
    }

    #[test]
    fn aspect_ratio_stddev_zero_for_identical_views() {
        let v1 = view_with_bbox(100.0, 100.0, 300.0, 200.0); // aspect = 2.0
        let v2 = view_with_bbox(200.0, 150.0, 400.0, 250.0); // also 2.0
        let r = coverage(&[v1, v2], CoverageConfig::default()).unwrap();
        assert!(r.aspect_ratio_stddev < 1e-9);
    }

    #[test]
    fn aspect_ratio_stddev_positive_for_diverse_views() {
        let v1 = view_with_bbox(100.0, 100.0, 300.0, 200.0); // aspect = 2.0
        let v2 = view_with_bbox(100.0, 100.0, 200.0, 300.0); // aspect = 0.5
        let r = coverage(&[v1, v2], CoverageConfig::default()).unwrap();
        assert!(r.aspect_ratio_stddev > 0.5);
    }
}
