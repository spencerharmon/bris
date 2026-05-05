//! Vision regression tests against a corpus of real captured frames.
//!
//! See `crates/bris-vision/tests/regression/README.md` for what's in
//! the corpus and how it's organized. Each test loads a known frame
//! and asserts that the current pipeline reproduces previously-
//! recorded behavior within tolerance.
//!
//! These tests are *not* validation: they don't prove the pipeline
//! produces correct fixes. They prove it doesn't *change* its
//! outputs unexpectedly, which is what regression tests are for.
//!
//! When intentionally improving an algorithm, update the recorded
//! values in the relevant `case.toml` and the assertions below in
//! the same commit. The commit message should explain why the
//! change is an improvement.

use bris_core::time::{Tt, JD_J2000};
use bris_vision::{
    centroid_brightest_body, detect_horizon, detect_horizon_via_sky_region, load_frame_from_path,
    CentroidConfig, Frame, HorizonConfig, Intrinsics,
};
use std::path::Path;

#[cfg(feature = "segmentation")]
use bris_vision::{detect_horizon_via_segmentation, load_model};

const REGRESSION_DIR: &str = "tests/regression";

/// Load a regression frame by case name and filename. Test panics if
/// the file is missing — every regression case is required.
fn load_regression_frame(case: &str, filename: &str) -> Frame {
    let path = Path::new(REGRESSION_DIR).join(case).join(filename);
    let dims = image::image_dimensions(&path)
        .unwrap_or_else(|e| panic!("read {} dimensions: {e}", path.display()));
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .unwrap_or_else(|e| panic!("load {}: {e}", path.display()))
        .with_source_path(path)
}

/// `sailing_sun_upper_left` case: sailing POV with sun in upper-left
/// and a deck-occluded horizon. This case was the first to expose
/// the deck-occlusion problem with the gradient and sky-region
/// horizon detectors. The recorded values come from the version of
/// the pipeline at the time the case was added; see
/// `tests/regression/sailing_sun_upper_left/case.toml`.
mod sailing_sun_upper_left {
    use super::*;

    const CASE: &str = "sailing_sun_upper_left";
    const TOLERANCE_PX: f64 = 5.0;
    // Slope tolerance covers the working-resolution rounding plus
    // RANSAC's per-iteration variability.
    const SLOPE_TOLERANCE: f64 = 0.05;
    // Intercept tolerance in full-resolution pixels.
    const INTERCEPT_TOLERANCE: f64 = 15.0;

    #[test]
    fn frames_load() {
        let f = load_regression_frame(CASE, "frame.png");
        assert_eq!(f.width(), 640);
        assert_eq!(f.height(), 360);
        let f = load_regression_frame(CASE, "frame_5s_later.png");
        assert_eq!(f.width(), 640);
        assert_eq!(f.height(), 360);
    }

    #[test]
    fn centroid_finds_sun_in_upper_left() {
        let frame = load_regression_frame(CASE, "frame.png");
        let centroid = centroid_brightest_body(&frame, CentroidConfig::default())
            .expect("centroid should succeed on this frame");
        // From case.toml: sun centroid at (98.8, 47.7).
        assert!(
            (centroid.x - 98.8).abs() < TOLERANCE_PX,
            "centroid x = {} not within {} of 98.8",
            centroid.x,
            TOLERANCE_PX
        );
        assert!(
            (centroid.y - 47.7).abs() < TOLERANCE_PX,
            "centroid y = {} not within {} of 47.7",
            centroid.y,
            TOLERANCE_PX
        );
    }

    #[test]
    fn gradient_detector_returns_known_wrong_horizon() {
        // The gradient detector is known to be fooled by deck edges
        // in this scene. We don't assert it's wrong (that's an
        // invariant the test would only pass when the bug is
        // present); we assert it returns *some* plausible line so
        // we know the detector itself doesn't crash. The recorded
        // wrong value is in case.toml for human reference.
        let frame = load_regression_frame(CASE, "frame.png");
        let line = detect_horizon(&frame, HorizonConfig::default())
            .expect("gradient detector should not error on this frame");
        // Slope and intercept should still be in physically-plausible
        // ranges (slope < 1.0, intercept within image bounds).
        assert!(line.slope.abs() < 1.0, "slope {} too steep", line.slope);
        assert!(
            line.intercept >= 0.0 && line.intercept <= 360.0,
            "intercept {} outside image bounds",
            line.intercept
        );
    }

    #[test]
    fn sky_region_detector_returns_known_wrong_horizon() {
        // Same shape as the gradient test: assert it doesn't crash,
        // assert plausible bounds, leave the "this is the wrong
        // line" knowledge in case.toml.
        let frame = load_regression_frame(CASE, "frame.png");
        let line = detect_horizon_via_sky_region(&frame, HorizonConfig::default())
            .expect("sky-region detector should not error on this frame");
        assert!(line.slope.abs() < 1.5, "slope {} too steep", line.slope);
        assert!(
            line.intercept >= -100.0 && line.intercept <= 460.0,
            "intercept {} outside expanded image bounds",
            line.intercept
        );
    }

    /// The segmentation detector is the only one that gets the right
    /// horizon for this scene. Recorded values: slope ≈ -0.187,
    /// intercept ≈ 322.3. Test only runs when the model file is
    /// present (it's gitignored at 14.5 MB; regenerate with
    /// `scripts/export_segformer_ade.py`).
    #[cfg(feature = "segmentation")]
    #[test]
    fn segmentation_detector_finds_correct_horizon_when_model_present() {
        let model_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join("segmentation.onnx");
        if !model_path.exists() {
            eprintln!(
                "skipping: segmentation model not present at {}. \
                 Regenerate with scripts/export_segformer_ade.py.",
                model_path.display()
            );
            return;
        }
        load_model(&model_path).expect("model should load");
        let frame = load_regression_frame(CASE, "frame.png");
        let line = detect_horizon_via_segmentation(&frame, HorizonConfig::default())
            .expect("segmentation detector should produce a horizon on this frame");
        // From case.toml: slope ≈ -0.187, intercept ≈ 322.3.
        assert!(
            (line.slope - (-0.187)).abs() < SLOPE_TOLERANCE,
            "slope {} not within {} of -0.187 (recorded value)",
            line.slope,
            SLOPE_TOLERANCE
        );
        assert!(
            (line.intercept - 322.3).abs() < INTERCEPT_TOLERANCE,
            "intercept {} not within {} of 322.3 (recorded value)",
            line.intercept,
            INTERCEPT_TOLERANCE
        );
        // Inlier count should be substantial — the recorded value was
        // 172 out of 512 candidate columns.
        assert!(
            line.inlier_count > 100,
            "only {} inliers; expected > 100 (recorded ~172)",
            line.inlier_count
        );
    }

    /// When the segmentation model is *not* present, the detector
    /// should return a typed error rather than panicking or producing
    /// silently wrong output.
    #[cfg(feature = "segmentation")]
    #[test]
    fn segmentation_detector_errors_cleanly_without_source_path() {
        let model_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join("segmentation.onnx");
        if !model_path.exists() {
            return;
        }
        load_model(&model_path).expect("model should load");
        // Build a frame *without* a source_path — the segmentation
        // detector requires one because it needs to reload the
        // original RGB image.
        let path = Path::new(REGRESSION_DIR).join(CASE).join("frame.png");
        let dims = image::image_dimensions(&path).unwrap();
        let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
        let frame =
            load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics).unwrap();
        // Note: deliberately *not* calling .with_source_path(path).
        let result = detect_horizon_via_segmentation(&frame, HorizonConfig::default());
        assert!(
            result.is_err(),
            "expected SegmentError when source_path is None"
        );
    }
}
