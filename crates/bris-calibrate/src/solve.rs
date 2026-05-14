//! Calibration solve: detected views → camera intrinsics.
//!
//! Wraps the `vision-calibration` planar-intrinsics workflow
//! (Zhang's closed-form initialization + non-linear bundle
//! adjustment) into a single function returning Bris's
//! [`bris_vision::Intrinsics`] plus a quality summary.
//!
//! # Solver behaviour
//!
//! - **Init**: Zhang's homography-based method estimates
//!   `(fx, fy, cx, cy)` and an iterative distortion seed.
//! - **Optimize**: Levenberg-Marquardt over all parameters
//!   (intrinsics + distortion + per-view extrinsics)
//!   minimizing reprojection residuals.
//!
//! The returned `mean_reproj_error` is the RMS pixel
//! residual over every observed corner. Sub-pixel residuals
//! (< 0.5 px) on a clean capture are routine; > 1.0 px
//! suggests a problem (the [`crate::doctor`] module
//! flags this and other failure modes).

use bris_vision::Intrinsics;
use thiserror::Error;
use tracing::{debug, info};
use vision_calibration::core::{CorrespondenceView, NoMeta, PlanarDataset, Pt2, Pt3, View};
use vision_calibration::planar_intrinsics::{
    run_calibration as run_planar, PlanarIntrinsicsProblem,
};
use vision_calibration::session::CalibrationSession;

use crate::detect::DetectedView;

/// Result of a calibration solve.
#[derive(Debug, Clone)]
pub struct CalibrationResult {
    /// Fitted camera intrinsics, ready to drop into
    /// [`bris_vision::Frame::new`].
    pub intrinsics: Intrinsics,
    /// Image dimensions (width × height) the calibration
    /// was performed against. Persisted alongside the
    /// intrinsics so consumers can sanity-check that they
    /// don't accidentally apply a 640×480 calibration to a
    /// 1920×1080 frame.
    pub image_width: u32,
    /// Image height (see `image_width`).
    pub image_height: u32,
    /// Mean RMS reprojection error in pixels across all
    /// labelled corners across all views. Sub-pixel is the
    /// target.
    pub mean_reproj_error_px: f64,
    /// Number of views (frames) that contributed to the
    /// solve.
    pub view_count: usize,
    /// Total number of corner observations across all views.
    pub observation_count: usize,
}

/// Errors during the calibration solve.
#[derive(Debug, Error)]
pub enum SolveError {
    /// Too few views supplied. Zhang's planar method needs
    /// ≥ 3 distinct views for a well-conditioned solve.
    #[error("calibration needs ≥ 3 views, got {0}")]
    TooFewViews(usize),
    /// Views have inconsistent dimensions; the solve
    /// requires every view to come from the same image
    /// size.
    #[error("inconsistent view dimensions: first {first_w}×{first_h}, other {other_w}×{other_h}")]
    InconsistentDimensions {
        /// First view's width.
        first_w: u32,
        /// First view's height.
        first_h: u32,
        /// Mismatching width.
        other_w: u32,
        /// Mismatching height.
        other_h: u32,
    },
    /// `vision-calibration` rejected the input or the
    /// solve diverged.
    #[error("calibration solver: {0}")]
    Solver(String),
}

/// Run the calibration solve on the supplied detected views.
///
/// Wraps `vision-calibration`'s session API. The session
/// internally runs `step_init` (Zhang) then `step_optimize`
/// (LM); we call the convenience `run_calibration` wrapper
/// that does both with default options.
///
/// # Errors
///
/// See [`SolveError`].
pub fn calibrate(views: &[DetectedView]) -> Result<CalibrationResult, SolveError> {
    if views.len() < 3 {
        return Err(SolveError::TooFewViews(views.len()));
    }
    let (image_width, image_height) = (views[0].width, views[0].height);
    for v in views.iter().skip(1) {
        if v.width != image_width || v.height != image_height {
            return Err(SolveError::InconsistentDimensions {
                first_w: image_width,
                first_h: image_height,
                other_w: v.width,
                other_h: v.height,
            });
        }
    }

    // Build the vision-calibration dataset.
    let mut vc_views: Vec<View<NoMeta>> = Vec::with_capacity(views.len());
    let mut total_observations = 0_usize;
    for v in views {
        let mut points_3d: Vec<Pt3> = Vec::with_capacity(v.correspondences.len());
        let mut points_2d: Vec<Pt2> = Vec::with_capacity(v.correspondences.len());
        for c in &v.correspondences {
            // The board lies in the z = 0 plane in its own
            // frame; the per-view extrinsics rotate/translate
            // it into the camera frame.
            points_3d.push(Pt3::new(c.board_x_m, c.board_y_m, 0.0));
            points_2d.push(Pt2::new(c.pixel_x, c.pixel_y));
            total_observations += 1;
        }
        let cv = CorrespondenceView::new(points_3d, points_2d)
            .map_err(|e| SolveError::Solver(format!("CorrespondenceView::new: {e}")))?;
        vc_views.push(View::without_meta(cv));
    }
    let dataset = PlanarDataset::new(vc_views)
        .map_err(|e| SolveError::Solver(format!("PlanarDataset::new: {e}")))?;

    // Session + solve.
    let mut session = CalibrationSession::<PlanarIntrinsicsProblem>::new();
    session
        .set_input(dataset)
        .map_err(|e| SolveError::Solver(format!("set_input: {e}")))?;
    debug!(views = views.len(), "bris-calibrate: starting solve");
    run_planar(&mut session).map_err(|e| SolveError::Solver(format!("run_calibration: {e}")))?;
    let export = session
        .export()
        .map_err(|e| SolveError::Solver(format!("export: {e}")))?;

    let k = export.params.intrinsics();
    let dist = export.params.distortion();
    let intrinsics = Intrinsics {
        fx: k.fx,
        fy: k.fy,
        cx: k.cx,
        cy: k.cy,
        k1: dist.k1,
        k2: dist.k2,
        k3: dist.k3,
        p1: dist.p1,
        p2: dist.p2,
    };
    info!(
        fx = intrinsics.fx,
        fy = intrinsics.fy,
        cx = intrinsics.cx,
        cy = intrinsics.cy,
        k1 = intrinsics.k1,
        k2 = intrinsics.k2,
        rms_px = export.mean_reproj_error,
        views = views.len(),
        observations = total_observations,
        "bris-calibrate: solve complete"
    );
    Ok(CalibrationResult {
        intrinsics,
        image_width,
        image_height,
        mean_reproj_error_px: export.mean_reproj_error,
        view_count: views.len(),
        observation_count: total_observations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_too_few_views() {
        let views: Vec<DetectedView> = Vec::new();
        let err = calibrate(&views).unwrap_err();
        assert!(matches!(err, SolveError::TooFewViews(0)));
    }

    #[test]
    fn rejects_inconsistent_dimensions() {
        let views = vec![
            DetectedView {
                source: "a.png".into(),
                width: 640,
                height: 480,
                correspondences: vec![],
            },
            DetectedView {
                source: "b.png".into(),
                width: 640,
                height: 480,
                correspondences: vec![],
            },
            DetectedView {
                source: "c.png".into(),
                width: 1280,
                height: 720,
                correspondences: vec![],
            },
        ];
        let err = calibrate(&views).unwrap_err();
        assert!(matches!(err, SolveError::InconsistentDimensions { .. }));
    }

    // End-to-end solve tests need either real captures or
    // synthetic checkerboard frames; both are deferred to
    // hardware bring-up.
}
