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

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::similar_names,
    clippy::items_after_statements
)]

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
            "expected SegError when source_path is None"
        );
    }

    /// End-to-end ML-assisted centroiding: segment the frame, build
    /// a sky-only mask, run masked centroid. Two assertions:
    ///   1. The masked centroid lands inside the sky mask (load-
    ///      bearing — proves the masking actually constrains output).
    ///   2. The masked centroid is plausibly near the Sun, accepting
    ///      that area-weighted centroiding over "all bright sky
    ///      pixels" pulls the answer toward whichever side has
    ///      brighter haze. The unmasked centroid happens to be near
    ///      the Sun *because* the saturated Sun is the strongest
    ///      signal anywhere in the frame; masking restricts the
    ///      search but the area-weighted average over the bright sky
    ///      region is biased.
    ///
    /// **Known limitation** documented here so the next person to
    /// touch this test understands it: for tight Sun/Moon centroids
    /// inside a sky mask, the right algorithm is the peak detector
    /// (`detect_peaks`) rather than the connected-component centroid,
    /// because Sun/Moon are *peaks* of brightness rather than
    /// largest connected regions. Tracked as a follow-up; the
    /// brightness-weighted centroid is "approximately Sun" not
    /// "Sun centroid to sub-pixel."
    #[cfg(feature = "segmentation")]
    #[test]
    fn segmentation_sky_mask_centroids_to_sky_region() {
        use bris_vision::{centroid_brightest_body_in_mask, segment, CentroidConfig};

        let model_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join("segmentation.onnx");
        if !model_path.exists() {
            return;
        }
        load_model(&model_path).expect("model should load");

        let frame = load_regression_frame(CASE, "frame.png");
        let img_path = frame
            .source_path
            .clone()
            .expect("frame should carry source_path");
        let mask = segment(&img_path).expect("segmentation should succeed");
        let allow = mask.sky_mask(frame.width(), frame.height());

        let sky_centroid =
            centroid_brightest_body_in_mask(&frame, CentroidConfig::default(), Some(&allow))
                .expect("masked centroid should succeed");

        // (1) The masked centroid must land on a pixel that the mask
        //     says is sky. Load-bearing invariant.
        let cx_int = sky_centroid.x.round() as u32;
        let cy_int = sky_centroid.y.round() as u32;
        let idx = (cy_int as usize) * (frame.width() as usize) + (cx_int as usize);
        assert!(
            allow[idx],
            "sky-masked centroid at ({cx_int}, {cy_int}) should be inside the sky mask"
        );

        // (2) Plausibly near the Sun. Tolerance accommodates the
        //     known area-weighting bias when the bright sky region
        //     includes haze around the saturated body. See test
        //     docstring for details.
        const SUN_X: f64 = 99.0;
        const SUN_Y: f64 = 48.0;
        const TOL_PX: f64 = 30.0;
        let dist = ((sky_centroid.x - SUN_X).powi(2) + (sky_centroid.y - SUN_Y).powi(2)).sqrt();
        assert!(
            dist < TOL_PX,
            "sky-masked centroid at ({:.1}, {:.1}) is {:.1} px from Sun at ({}, {}); \
             expected within {} px",
            sky_centroid.x,
            sky_centroid.y,
            dist,
            SUN_X,
            SUN_Y,
            TOL_PX,
        );
    }
}

/// `sailing_with_distant_shore` case: sailing POV with a distant
/// shoreline visible between sea and sky. Exercises the
/// obstruction-aware horizon detector (catalog item 3 in plan.org).
mod sailing_with_distant_shore {
    use super::*;

    const CASE: &str = "sailing_with_distant_shore";

    #[test]
    fn frame_loads() {
        let f = load_regression_frame(CASE, "frame.png");
        assert_eq!(f.width(), 640);
        assert_eq!(f.height(), 360);
    }

    #[test]
    fn centroid_finds_sun() {
        let frame = load_regression_frame(CASE, "frame.png");
        let centroid = centroid_brightest_body(&frame, CentroidConfig::default())
            .expect("centroid should succeed");
        // Recorded values: Sun at (429.7, 46.5).
        assert!(
            (centroid.x - 429.7).abs() < 10.0,
            "centroid x = {} not within 10 of 429.7",
            centroid.x
        );
        assert!(
            (centroid.y - 46.5).abs() < 10.0,
            "centroid y = {} not within 10 of 46.5",
            centroid.y
        );
    }

    /// The load-bearing assertion for this case: the obstruction-
    /// aware horizon detector finds substantially more candidate
    /// columns than the strict sky-to-sea version would. On this
    /// scene we observe ~162 sky→sea + ~168 sky→thin-shore→sea
    /// columns; the test asserts a healthy lower bound on each.
    #[cfg(feature = "segmentation")]
    #[test]
    fn obstruction_aware_horizon_finds_robust_fit() {
        let model_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join("segmentation.onnx");
        if !model_path.exists() {
            eprintln!("skipping: segmentation model not present");
            return;
        }
        load_model(&model_path).expect("model should load");

        let frame = load_regression_frame(CASE, "frame.png");
        let line = detect_horizon_via_segmentation(&frame, HorizonConfig::default())
            .expect("segmentation detector should produce a horizon");

        // Recorded: slope ≈ -0.06, intercept ≈ 188.5, inliers >= 200.
        assert!(
            (line.slope - (-0.06)).abs() < 0.05,
            "slope {} not within 0.05 of -0.06",
            line.slope
        );
        assert!(
            (line.intercept - 188.5).abs() < 20.0,
            "intercept {} not within 20 of 188.5",
            line.intercept
        );
        assert!(
            line.inlier_count >= 200,
            "inlier count {} below recorded floor of 200",
            line.inlier_count
        );
    }
}
