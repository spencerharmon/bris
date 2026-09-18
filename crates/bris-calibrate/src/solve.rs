//! Calibration solve: detected views → camera intrinsics.
//!
//! Wraps the `vision-calibration` planar-intrinsics workflow
//! (Zhang's closed-form initialization + non-linear bundle
//! adjustment) into a single function returning Bris's
//! [`bris_vision::Intrinsics`] plus a quality summary
//! including per-view RMS reprojection residuals.
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
//! flags this and other failure modes). Per-view residuals
//! let the caller (CLI / Android UI) point at the
//! offending frames specifically rather than just reporting
//! an aggregate.

use std::path::PathBuf;

use bris_vision::Intrinsics;
use thiserror::Error;
use tracing::{debug, info};
use vision_calibration::core::{
    make_pinhole_camera, CorrespondenceView, NoMeta, PlanarDataset, Pt2, Pt3, View,
};
use vision_calibration::planar_intrinsics::{
    run_calibration as run_planar, PlanarIntrinsicsProblem,
};
use vision_calibration::session::CalibrationSession;

use crate::detect::DetectedView;

/// Per-view residual statistics extracted from the solve.
///
/// Lets the operator see *which* views are dragging the
/// aggregate RMS up. The CLI prints a sorted list with the
/// worst offenders at the top; the Android UI surfaces the
/// outliers as a list of "remove this frame and re-solve?"
/// suggestions.
#[derive(Debug, Clone)]
pub struct ViewResidual {
    /// Source frame path (copied from the input
    /// [`DetectedView::source`]).
    pub source: PathBuf,
    /// RMS reprojection residual over this view's corners,
    /// in pixels.
    pub rms_px: f64,
    /// Maximum per-corner residual, in pixels — useful for
    /// catching individual mis-labelled corners that the
    /// average smooths out.
    pub max_px: f64,
    /// Number of corner observations contributing.
    pub n_corners: usize,
}

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
    /// Per-view residual statistics in input order. Length
    /// equals `view_count`. Empty if per-view extraction
    /// failed (logged as a `warn!`); the aggregate result
    /// is still trustworthy in that case.
    pub per_view: Vec<ViewResidual>,
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
/// After the solve, the per-view extrinsics are recovered
/// from `session.output()` and used to compute per-view RMS
/// residuals (we project each view's correspondences using
/// the fitted intrinsics + the view's pose and accumulate
/// pixel errors). These per-view stats let the caller
/// surface "frame 12 is the outlier" feedback instead of
/// just an aggregate "rms = 1.4 px".
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

    // Pull per-view residuals from the in-progress output
    // (the export drops them; only the aggregate survives
    // export). If anything goes wrong we still return the
    // aggregate result with an empty `per_view` — the
    // caller's per-view UI just won't have anything to
    // show, but the calibration itself is unaffected.
    let per_view = compute_per_view_residuals(&session, views).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "per-view residual extraction failed; aggregate stats still valid");
        Vec::new()
    });

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
        per_view,
    })
}

/// Compute per-view RMS / max residuals by projecting each
/// view's 3D correspondences using the fitted intrinsics
/// and the view's recovered pose.
fn compute_per_view_residuals(
    session: &CalibrationSession<PlanarIntrinsicsProblem>,
    inputs: &[DetectedView],
) -> Result<Vec<ViewResidual>, String> {
    let estimate = session
        .output()
        .ok_or_else(|| "session.output() unavailable".to_string())?;
    let camera = make_pinhole_camera(estimate.params.intrinsics(), estimate.params.distortion());
    let poses = estimate.params.poses();
    if poses.len() != inputs.len() {
        return Err(format!(
            "pose count {} ≠ input view count {}",
            poses.len(),
            inputs.len(),
        ));
    }
    let mut out = Vec::with_capacity(inputs.len());
    for (view, pose) in inputs.iter().zip(poses.iter()) {
        let mut sum_sq = 0.0_f64;
        let mut max = 0.0_f64;
        let mut n = 0_usize;
        for c in &view.correspondences {
            let p_target = vision_calibration::core::Pt3::new(c.board_x_m, c.board_y_m, 0.0);
            let p_cam = pose * p_target;
            let Some(projected) = camera.project_point_c(&p_cam.coords) else {
                continue;
            };
            let dx = projected.x - c.pixel_x;
            let dy = projected.y - c.pixel_y;
            let err = (dx * dx + dy * dy).sqrt();
            sum_sq += err * err;
            if err > max {
                max = err;
            }
            n += 1;
        }
        let rms = if n > 0 {
            #[allow(clippy::cast_precision_loss)]
            let nf = n as f64;
            (sum_sq / nf).sqrt()
        } else {
            f64::NAN
        };
        out.push(ViewResidual {
            source: view.source.clone(),
            rms_px: rms,
            max_px: max,
            n_corners: n,
        });
    }
    Ok(out)
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

    // ---------------------------------------------------------------
    // End-to-end synthetic target-board fixture.
    //
    // The task's definition of source-of-truth is that the
    // planar-intrinsics workflow (Zhang init + non-linear bundle
    // adjustment) recovers *known* lens parameters. We build that
    // ground truth with a synthetic chessboard: place a planar grid
    // at several known poses, project every corner through a camera
    // with KNOWN intrinsics, feed the resulting pixel↔board
    // correspondences into `calibrate()`, and assert the solver
    // recovers the intrinsics we started from within tolerance.
    //
    // No image I/O or hardware is involved: the fixture is the
    // `vision-calibration` synthetic projection helpers, which are
    // exactly the forward model the solver inverts. This is the
    // regression that pins bris-calibrate as the real source of
    // truth — a wide-angle lens (fx≈fy≈400, not the fx=fy=1000
    // `Intrinsics::placeholder`) is recovered from its own
    // projections.
    // ---------------------------------------------------------------

    use vision_calibration::core::{make_pinhole_camera, BrownConrady5, FxFyCxCySkew};
    use vision_calibration::synthetic::planar::{grid_points, poses_yaw_y_z};

    /// Ground-truth intrinsics for the synthetic fixture: a
    /// wide-angle lens (short focal length) with a slightly
    /// off-centre principal point and mild radial distortion —
    /// representative of the real lenses the ROI calls out
    /// (~350–500 px focal length), and deliberately far from the
    /// `Intrinsics::placeholder` fx=fy=1000.
    fn truth_intrinsics() -> FxFyCxCySkew<f64> {
        FxFyCxCySkew {
            fx: 420.0,
            fy: 418.0,
            cx: 332.0,
            cy: 244.0,
            skew: 0.0,
        }
    }

    /// Build synthetic `DetectedView`s by projecting a planar
    /// chessboard grid through a camera with the given ground-truth
    /// intrinsics from several known poses. Pixel coordinates are
    /// the exact forward-projected corners (noise-free), so a
    /// correct solver must recover the intrinsics to high precision.
    fn synthetic_views(
        k: FxFyCxCySkew<f64>,
        dist: BrownConrady5<f64>,
        n_views: usize,
        width: u32,
        height: u32,
    ) -> Vec<DetectedView> {
        // 9x7 inner-corner grid at 25 mm spacing — a typical
        // calibration board size.
        let (nx, ny, spacing) = (9u32, 7u32, 0.025_f64);
        let board = grid_points(nx as usize, ny as usize, spacing);
        // Centre the board on the optical axis so the whole grid
        // stays in frame across the yaw/depth sweep.
        let board_center_x = (f64::from(nx) - 1.0) * spacing / 2.0;
        let board_center_y = (f64::from(ny) - 1.0) * spacing / 2.0;

        let camera = make_pinhole_camera(k, dist);
        // Sweep yaw and depth so the views are geometrically
        // distinct (Zhang's method needs ≥3 non-degenerate views).
        let poses = poses_yaw_y_z(n_views, -0.30, 0.12, 0.45, 0.02);

        let mut views = Vec::with_capacity(n_views);
        for (view_idx, pose) in poses.iter().enumerate() {
            let mut correspondences = Vec::with_capacity(board.len());
            for p in &board {
                // Recentre the board about the origin before applying
                // the pose so the projected corners land near the
                // image centre.
                let centered = vision_calibration::core::Pt3::new(
                    p.x - board_center_x,
                    p.y - board_center_y,
                    0.0,
                );
                let p_cam = pose * centered;
                let projected = camera
                    .project_point_c(&p_cam.coords)
                    .expect("synthetic board corner must project in front of the camera");
                correspondences.push(crate::detect::Correspondence {
                    pixel_x: projected.x,
                    pixel_y: projected.y,
                    // Report the board coordinate in its own (un-recentred)
                    // frame; the solver estimates the per-view pose that
                    // includes the constant recentring translation.
                    board_x_m: p.x - board_center_x,
                    board_y_m: p.y - board_center_y,
                });
            }
            views.push(DetectedView {
                source: PathBuf::from(format!("synthetic_view_{view_idx:02}.pgm")),
                width,
                height,
                correspondences,
            });
        }
        views
    }

    #[test]
    fn recovers_known_wide_angle_intrinsics_from_synthetic_board() {
        let truth = truth_intrinsics();
        // Noise-free, distortion-free forward model: the pinhole
        // intrinsics must be recovered essentially exactly.
        let dist = BrownConrady5::<f64>::default();
        let views = synthetic_views(truth, dist, 12, 640, 480);

        let result = calibrate(&views).expect("synthetic calibration must succeed");

        // The whole point of the task: real intrinsics, not the
        // fx=fy=1000 placeholder.
        assert!(
            (result.intrinsics.fx - 1000.0).abs() > 100.0,
            "recovered fx={} must not be the placeholder 1000",
            result.intrinsics.fx
        );

        // Recovery tolerance: with a clean forward model the solver
        // should land within 1 px of focal length and principal
        // point.
        assert!(
            (result.intrinsics.fx - truth.fx).abs() < 1.0,
            "fx: recovered {} vs truth {}",
            result.intrinsics.fx,
            truth.fx
        );
        assert!(
            (result.intrinsics.fy - truth.fy).abs() < 1.0,
            "fy: recovered {} vs truth {}",
            result.intrinsics.fy,
            truth.fy
        );
        assert!(
            (result.intrinsics.cx - truth.cx).abs() < 1.0,
            "cx: recovered {} vs truth {}",
            result.intrinsics.cx,
            truth.cx
        );
        assert!(
            (result.intrinsics.cy - truth.cy).abs() < 1.0,
            "cy: recovered {} vs truth {}",
            result.intrinsics.cy,
            truth.cy
        );

        // A correct solve of noise-free data has sub-pixel RMS.
        assert!(
            result.mean_reproj_error_px < 0.5,
            "mean reproj error {} px too high — solve did not converge",
            result.mean_reproj_error_px
        );
        assert_eq!(result.view_count, 12);
        assert!(
            !result.per_view.is_empty(),
            "per-view residuals must be populated"
        );
    }

    #[test]
    fn recovers_intrinsics_with_radial_distortion() {
        let truth = truth_intrinsics();
        // Mild barrel distortion, typical of a wide-angle lens.
        let dist = BrownConrady5::<f64> {
            k1: -0.12,
            k2: 0.03,
            k3: 0.0,
            p1: 0.0,
            p2: 0.0,
            iters: 5,
        };
        let views = synthetic_views(truth, dist, 14, 640, 480);

        let result = calibrate(&views).expect("synthetic calibration must succeed");

        // Focal length / principal point still recovered within a
        // loose pixel tolerance when distortion is present and
        // jointly estimated.
        assert!(
            (result.intrinsics.fx - truth.fx).abs() < 3.0,
            "fx: recovered {} vs truth {}",
            result.intrinsics.fx,
            truth.fx
        );
        assert!(
            (result.intrinsics.fy - truth.fy).abs() < 3.0,
            "fy: recovered {} vs truth {}",
            result.intrinsics.fy,
            truth.fy
        );
        // The recovered radial term must have the right sign and
        // rough magnitude (barrel distortion, k1 < 0).
        assert!(
            result.intrinsics.k1 < -0.02,
            "k1: recovered {} should capture the barrel distortion",
            result.intrinsics.k1
        );
        assert!(
            result.mean_reproj_error_px < 0.5,
            "mean reproj error {} px too high with distortion",
            result.mean_reproj_error_px
        );
    }
}
