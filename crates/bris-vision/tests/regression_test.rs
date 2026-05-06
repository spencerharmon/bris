//! Vision regression tests against a corpus of real captured frames.
//!
//! See `crates/bris-vision/tests/regression/README.md` for what's in
//! the corpus and how it's organized. Each subdirectory holds one
//! case described by a `case.toml` file; the build script
//! (`build.rs`) walks the corpus and emits one `mod case_<name>`
//! containing one `#[test] fn` per declared check.
//!
//! The point of this harness is that **adding a new case is a
//! TOML-write, not a Rust-write**. The schema is documented in
//! `mod harness` below; the build script that reads it lives in
//! `crates/bris-vision/build.rs`.
//!
//! These tests are not validation: they don't prove the pipeline
//! produces correct fixes. They prove it doesn't *change* its
//! outputs unexpectedly. When intentionally improving an algorithm,
//! update the recorded values in the relevant `case.toml` and the
//! algorithm in the same commit; the commit message must explain
//! why the change is an improvement.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::similar_names,
    clippy::items_after_statements,
    clippy::module_name_repetitions,
    // The harness's pub items are reachable from the build-script-
    // generated case modules at the bottom of this file via
    // `super::harness::*`. The lint can't see across the
    // include!()-d module boundary.
    unreachable_pub
)]

mod harness {
    //! Schema and runner for TOML-driven regression cases.
    //!
    //! Each subdirectory of `tests/regression/` is a single case. Its
    //! `case.toml` is parsed into [`CaseSpec`]; the runner functions
    //! in this module each take a `CaseSpec` and perform one
    //! well-named assertion.
    //!
    //! The build script enumerates which runners to call for which
    //! cases based on which expectation tables are present.

    #![allow(dead_code)] // Build-time discovery decides which entries are used.

    use std::path::{Path, PathBuf};

    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{
        centroid_brightest_body, classify, detect_horizon, detect_horizon_via_sky_region,
        load_frame_from_path_with_rotation, CentroidConfig, Condition, ConditionConfig, Frame,
        HorizonConfig, HorizonError, HorizonLine, Intrinsics, Rotation,
    };

    #[cfg(feature = "segmentation")]
    use bris_vision::{detect_horizon_via_segmentation, load_model, SegmentError};

    /// Root of the regression corpus, relative to the bris-vision crate.
    pub const REGRESSION_DIR: &str = "tests/regression";

    // -----------------------------------------------------------------
    // Schema (parsed at runtime from case.toml)
    // -----------------------------------------------------------------

    /// Top-level parsed `case.toml`.
    #[derive(Debug, serde::Deserialize)]
    pub struct CaseSpec {
        pub case: CaseMeta,
        #[serde(default)]
        pub reference_observer: Option<ReferenceObserver>,
        #[serde(default)]
        pub expected_classifier: Option<ClassifierExpectation>,
        #[serde(default)]
        pub expected_centroid_frame0: Option<CentroidExpectation>,
        #[serde(default)]
        pub horizon: HorizonExpectations,
        #[serde(default)]
        pub segmentation: Option<SegmentationExpectations>,
        #[serde(default)]
        pub fix: Option<FixExpectation>,
    }

    #[derive(Debug, serde::Deserialize)]
    pub struct CaseMeta {
        pub name: String,
        pub description: String,
        pub kind: CaseKind,
        pub frame_count: u32,
        /// Frame width *after* rotation (the dimensions the pipeline
        /// sees). For a fixture whose bytes are stored sideways and
        /// declared with `source_rotation_deg = 90`, this is the
        /// post-rotation width (= source height).
        pub frame_width: u32,
        /// Frame height *after* rotation.
        pub frame_height: u32,
        /// Rotation to apply to the source pixels at load time, in
        /// degrees clockwise. Accepts 0, 90, 180, 270. Defaults to
        /// 0 (no rotation): we trust that the saved bytes are in
        /// viewing orientation, which is true for any phone-encoded
        /// JPEG/PNG and any conventionally-saved camera image.
        /// Override only for fixtures whose bytes are stored in
        /// sensor-native orientation or otherwise off-axis.
        #[serde(default)]
        pub source_rotation_deg: u16,
        /// Optional list of frame filenames in capture order. Defaults
        /// to `["frame.png"]` if absent.
        #[serde(default)]
        pub frames: Option<Vec<String>>,
    }

    /// What the case is testing for. This is documentation; the
    /// actual assertions are per-table.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CaseKind {
        Working,
        ExpectedFailure,
        ExpectedLowConfidence,
    }

    #[derive(Debug, serde::Deserialize)]
    pub struct ReferenceObserver {
        pub lat_deg: f64,
        pub lon_deg: f64,
        pub eye_height_m: f64,
        pub capture_utc: String,
        pub body: String,
    }

    /// Day/night/twilight classifier expectation. When present in
    /// `case.toml`, the harness runs the image-only classifier
    /// (no astronomical prior) on frame 0 and asserts the resulting
    /// [`Condition`] matches `condition` and the confidence meets
    /// `min_confidence` if set.
    ///
    /// `condition` strings: `"day"`, `"twilight"`, `"night"`,
    /// `"unusable"`. Match is case-insensitive.
    #[derive(Debug, serde::Deserialize)]
    pub struct ClassifierExpectation {
        pub condition: String,
        #[serde(default)]
        pub min_confidence: Option<f64>,
    }

    #[derive(Debug, serde::Deserialize)]
    #[allow(clippy::struct_field_names)] // _px postfix is meaningful (units).
    pub struct CentroidExpectation {
        pub x_px: f64,
        pub y_px: f64,
        pub tolerance_px: f64,
    }

    #[derive(Debug, Default, serde::Deserialize)]
    pub struct HorizonExpectations {
        #[serde(default)]
        pub gradient: Option<HorizonExpectation>,
        #[serde(default)]
        pub sky_region: Option<HorizonExpectation>,
        #[serde(default)]
        pub segmentation: Option<HorizonExpectation>,
    }

    /// Per-method horizon expectation. `outcome` selects what the
    /// assertion does: `ok` requires Ok-and-matches, `err` requires
    /// Err (optionally matching `error_variant` as a substring of
    /// the Display text).
    #[derive(Debug, serde::Deserialize)]
    pub struct HorizonExpectation {
        pub outcome: Outcome,
        #[serde(default)]
        pub slope: Option<f64>,
        #[serde(default)]
        pub intercept: Option<f64>,
        #[serde(default = "default_slope_tolerance")]
        pub slope_tolerance: f64,
        #[serde(default = "default_intercept_tolerance")]
        pub intercept_tolerance: f64,
        #[serde(default)]
        pub inlier_count_min: Option<u32>,
        #[serde(default)]
        pub error_variant: Option<String>,
        /// Documentation: whether this method finds the *correct*
        /// horizon for the scene. Doesn't gate the assertion.
        #[serde(default)]
        pub correctness: Option<String>,
        #[serde(default)]
        pub notes: Option<String>,
    }

    fn default_slope_tolerance() -> f64 {
        0.05
    }
    fn default_intercept_tolerance() -> f64 {
        15.0
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Outcome {
        Ok,
        Err,
    }

    #[derive(Debug, serde::Deserialize)]
    pub struct SegmentationExpectations {
        #[serde(default)]
        pub transition_counts: Option<TransitionCounts>,
    }

    #[derive(Debug, serde::Deserialize)]
    pub struct TransitionCounts {
        pub col_sky_to_sea_min: u32,
        pub col_sky_to_obstr_to_sea_min: u32,
        #[serde(default)]
        pub notes: Option<String>,
    }

    /// End-to-end fix expectation. Currently a schema-only stub: the
    /// harness doesn't yet drive the full sight-reduction pipeline.
    /// Cases can declare expectations now; the runner will assert
    /// them when the wiring lands.
    #[derive(Debug, serde::Deserialize)]
    pub struct FixExpectation {
        pub outcome: FixOutcome,
        #[serde(default)]
        pub sigma_nm_min: Option<f64>,
        #[serde(default)]
        pub sigma_nm_max: Option<f64>,
        #[serde(default)]
        pub dominant_source_in: Option<Vec<String>>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum FixOutcome {
        Ok,
        Err,
        LowConfidence,
    }

    // -----------------------------------------------------------------
    // Loading
    // -----------------------------------------------------------------

    /// Read and parse a case's `case.toml`. Panics with a useful
    /// message if missing or malformed; this is a test-only helper
    /// and a malformed case.toml is a build/test bug.
    pub fn load_case(case_name: &str) -> CaseSpec {
        let path = Path::new(REGRESSION_DIR).join(case_name).join("case.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
    }

    /// Resolve the effective rotation for a case from its declared
    /// `source_rotation_deg`. Default is no rotation; only explicit
    /// 90/180/270 trigger a rotation.
    pub fn resolve_rotation(case: &CaseSpec) -> Rotation {
        Rotation::from_degrees(case.case.source_rotation_deg).unwrap_or_else(|d| {
            panic!(
                "case {}: source_rotation_deg must be 0|90|180|270, got {d}",
                case.case.name
            )
        })
    }

    /// Load a frame from a case directory. Honors the case's
    /// declared `source_rotation_deg`. The returned `Frame` records
    /// the applied rotation.
    pub fn load_case_frame(case: &CaseSpec, filename: &str) -> Frame {
        let path: PathBuf = Path::new(REGRESSION_DIR)
            .join(&case.case.name)
            .join(filename);
        let (src_w, src_h) = image::image_dimensions(&path)
            .unwrap_or_else(|e| panic!("dims {}: {e}", path.display()));
        let rotation = resolve_rotation(case);
        // Intrinsics are placeholder for the regression corpus
        // (uncalibrated cameras). They must describe the post-
        // rotation frame so the principal point lands at the
        // post-rotation center.
        let (post_w, post_h) = match rotation {
            Rotation::Deg0 | Rotation::Deg180 => (src_w, src_h),
            Rotation::Deg90 | Rotation::Deg270 => (src_h, src_w),
        };
        let intrinsics = Intrinsics::placeholder(post_w, post_h);
        load_frame_from_path_with_rotation(
            &path,
            Tt::from_julian_date(JD_J2000),
            0,
            intrinsics,
            rotation,
        )
        .unwrap_or_else(|e| panic!("load {}: {e}", path.display()))
        .with_source_path(path)
    }

    /// First frame in a case (default `frame.png` unless `frames =
    /// [...]` is set in `[case]`).
    pub fn first_frame_filename(case: &CaseSpec) -> String {
        case.case
            .frames
            .as_ref()
            .and_then(|f| f.first().cloned())
            .unwrap_or_else(|| "frame.png".to_string())
    }

    // -----------------------------------------------------------------
    // Per-check runners
    // -----------------------------------------------------------------

    /// Assert each declared frame loads at the declared dimensions.
    /// Dimensions in `case.toml` are *post-rotation*: for a portrait
    /// 1080×1920 source loaded with 90° rotation, declare
    /// `frame_width = 1920`, `frame_height = 1080`.
    pub fn check_frames_load(case: &CaseSpec) {
        let filenames: Vec<String> = case
            .case
            .frames
            .clone()
            .unwrap_or_else(|| vec!["frame.png".to_string()]);
        assert_eq!(
            filenames.len() as u32,
            case.case.frame_count,
            "case.frame_count = {} but frames list has {} entries",
            case.case.frame_count,
            filenames.len()
        );
        for filename in &filenames {
            let f = load_case_frame(case, filename);
            assert_eq!(
                f.width(),
                case.case.frame_width,
                "{}: width = {} expected {} (post-rotation)",
                filename,
                f.width(),
                case.case.frame_width
            );
            assert_eq!(
                f.height(),
                case.case.frame_height,
                "{}: height = {} expected {} (post-rotation)",
                filename,
                f.height(),
                case.case.frame_height
            );
        }
    }

    /// Run the image-only day/night classifier on frame 0 and
    /// assert it reports the expected condition (and meets
    /// `min_confidence` if declared).
    pub fn check_classifier(case: &CaseSpec) {
        let exp = case
            .expected_classifier
            .as_ref()
            .expect("check_classifier called with no [expected_classifier]");
        let want = parse_condition(&exp.condition).unwrap_or_else(|| {
            panic!(
                "case {}: unknown condition string {:?}; expected day/twilight/night/unusable",
                case.case.name, exp.condition
            )
        });
        let frame = load_case_frame(case, &first_frame_filename(case));
        let got = classify(&frame, None, ConditionConfig::default());
        assert_eq!(
            got.condition,
            want,
            "classifier reported {:?} (confidence {:.2}); expected {:?}. \
             Image evidence: mean_luma = {:.4}, saturated_fraction = {:.4}.",
            got.condition,
            got.confidence,
            want,
            got.image_evidence.mean_luma,
            got.image_evidence.saturated_fraction
        );
        if let Some(min) = exp.min_confidence {
            assert!(
                got.confidence >= min,
                "classifier confidence {:.3} below declared minimum {:.3}",
                got.confidence,
                min
            );
        }
    }

    fn parse_condition(s: &str) -> Option<Condition> {
        match s.to_ascii_lowercase().as_str() {
            "day" => Some(Condition::Day),
            "twilight" => Some(Condition::Twilight),
            "night" => Some(Condition::Night),
            "unusable" => Some(Condition::Unusable),
            _ => None,
        }
    }

    /// Assert the unmasked centroid lands within tolerance of the
    /// recorded position. Uses frame 0.
    pub fn check_centroid(case: &CaseSpec) {
        let exp = case
            .expected_centroid_frame0
            .as_ref()
            .expect("check_centroid called with no [expected_centroid_frame0]");
        let frame = load_case_frame(case, &first_frame_filename(case));
        let centroid = centroid_brightest_body(&frame, CentroidConfig::default())
            .expect("centroid_brightest_body returned Err on frame 0");
        let dx = (centroid.x - exp.x_px).abs();
        let dy = (centroid.y - exp.y_px).abs();
        assert!(
            dx <= exp.tolerance_px,
            "centroid x = {:.2} not within {} of recorded {:.2}",
            centroid.x,
            exp.tolerance_px,
            exp.x_px
        );
        assert!(
            dy <= exp.tolerance_px,
            "centroid y = {:.2} not within {} of recorded {:.2}",
            centroid.y,
            exp.tolerance_px,
            exp.y_px
        );
    }

    /// Run the gradient horizon detector and assert its declared outcome.
    pub fn check_horizon_gradient(case: &CaseSpec) {
        let exp = case
            .horizon
            .gradient
            .as_ref()
            .expect("check_horizon_gradient called with no [horizon.gradient]");
        let frame = load_case_frame(case, &first_frame_filename(case));
        let result = detect_horizon(&frame, HorizonConfig::default());
        assert_horizon_outcome("gradient", &result, exp);
    }

    /// Run the sky-region horizon detector and assert its declared outcome.
    pub fn check_horizon_sky_region(case: &CaseSpec) {
        let exp = case
            .horizon
            .sky_region
            .as_ref()
            .expect("check_horizon_sky_region called with no [horizon.sky_region]");
        let frame = load_case_frame(case, &first_frame_filename(case));
        let result = detect_horizon_via_sky_region(&frame, HorizonConfig::default());
        assert_horizon_outcome("sky_region", &result, exp);
    }

    /// Run the segmentation horizon detector and assert its declared
    /// outcome. Skips with an `eprintln!` if the segmentation model
    /// file isn't present (gitignored at 14.5 MB; regenerate with
    /// `scripts/export_segformer_ade.py`).
    #[cfg(feature = "segmentation")]
    pub fn check_horizon_segmentation(case: &CaseSpec) {
        if !ensure_segmentation_model_loaded() {
            return;
        }
        let exp = case
            .horizon
            .segmentation
            .as_ref()
            .expect("check_horizon_segmentation called with no [horizon.segmentation]");
        let frame = load_case_frame(case, &first_frame_filename(case));
        let result = detect_horizon_via_segmentation(&frame, HorizonConfig::default())
            .map_err(SegToHorizonError);
        assert_horizon_outcome("segmentation", &result, exp);
    }

    #[cfg(not(feature = "segmentation"))]
    pub fn check_horizon_segmentation(_case: &CaseSpec) {
        eprintln!("segmentation feature disabled; skipping horizon.segmentation check");
    }

    /// Wraps `SegmentError` to satisfy the `Display` bound on
    /// `assert_horizon_outcome`. Deliberately not impl'd as
    /// `From<SegmentError> for HorizonError`: `SegmentError` covers
    /// strictly more failure modes (model load, inference, output
    /// shape) and collapsing them would lose information.
    #[cfg(feature = "segmentation")]
    struct SegToHorizonError(SegmentError);

    #[cfg(feature = "segmentation")]
    impl std::fmt::Display for SegToHorizonError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Display::fmt(&self.0, f)
        }
    }

    /// Generic outcome assertion shared by all three horizon
    /// methods. Generic over `E: Display` so the segmentation
    /// detector's `SegmentError` works without conversion.
    fn assert_horizon_outcome<E: std::fmt::Display>(
        method: &str,
        result: &Result<HorizonLine, E>,
        exp: &HorizonExpectation,
    ) {
        match (exp.outcome, result) {
            (Outcome::Ok, Ok(line)) => {
                if let Some(slope) = exp.slope {
                    let d = (line.slope - slope).abs();
                    assert!(
                        d <= exp.slope_tolerance,
                        "{method}: slope {:.4} not within {} of recorded {:.4}",
                        line.slope,
                        exp.slope_tolerance,
                        slope
                    );
                }
                if let Some(intercept) = exp.intercept {
                    let d = (line.intercept - intercept).abs();
                    assert!(
                        d <= exp.intercept_tolerance,
                        "{method}: intercept {:.2} not within {} of recorded {:.2}",
                        line.intercept,
                        exp.intercept_tolerance,
                        intercept
                    );
                }
                if let Some(min_inliers) = exp.inlier_count_min {
                    assert!(
                        line.inlier_count >= min_inliers,
                        "{method}: inlier_count {} below recorded floor {}",
                        line.inlier_count,
                        min_inliers
                    );
                }
            }
            (Outcome::Ok, Err(e)) => {
                panic!("{method}: expected Ok, got Err: {e}");
            }
            (Outcome::Err, Ok(line)) => {
                panic!(
                    "{method}: expected Err, got Ok({{ slope: {:.4}, intercept: {:.2}, \
                     inliers: {} }})",
                    line.slope, line.intercept, line.inlier_count
                );
            }
            (Outcome::Err, Err(e)) => {
                if let Some(want) = &exp.error_variant {
                    let msg = format!("{e}");
                    assert!(
                        msg.contains(want.as_str()),
                        "{method}: expected error containing {want:?}, got {msg:?}"
                    );
                }
            }
        }
    }

    /// Segmentation transition-count check. Currently a no-op stub:
    /// the public API doesn't expose per-source candidate counts.
    /// The schema lands first so cases can declare expectations; the
    /// assertion will tighten when the API surfaces the counts.
    #[cfg(feature = "segmentation")]
    pub fn check_segmentation_transition_counts(case: &CaseSpec) {
        let _ = case
            .segmentation
            .as_ref()
            .and_then(|s| s.transition_counts.as_ref())
            .expect("check_segmentation_transition_counts called without table");
        eprintln!(
            "segmentation transition-count check stubbed for {}",
            case.case.name
        );
    }

    #[cfg(not(feature = "segmentation"))]
    pub fn check_segmentation_transition_counts(_case: &CaseSpec) {
        eprintln!("segmentation feature disabled; skipping transition_counts check");
    }

    /// Load the segmentation model from the conventional location,
    /// or emit a skip message and return false.
    #[cfg(feature = "segmentation")]
    fn ensure_segmentation_model_loaded() -> bool {
        let model_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join("segmentation.onnx");
        if !model_path.exists() {
            eprintln!(
                "skipping: segmentation model not present at {}. \
                 Regenerate with scripts/export_segformer_ade.py.",
                model_path.display()
            );
            return false;
        }
        load_model(&model_path).expect("segmentation model should load");
        true
    }

    /// Suppress unused-import lints in builds that disable the
    /// segmentation feature (`HorizonError` appears in the
    /// `pub use` line above but isn't otherwise referenced here).
    #[allow(unused)]
    fn _force_use_horizon_error(_e: HorizonError) {}
}

include!(concat!(env!("OUT_DIR"), "/cases_generated.rs"));

// ---------------------------------------------------------------------------
// Static tests: properties of detectors that happen to use a corpus
// frame, but don't fit the per-case schema. These don't go through
// the build-script dispatch.
// ---------------------------------------------------------------------------

/// When the segmentation model is present but a frame's
/// `source_path` is None, the detector should return a typed error
/// rather than panicking or producing silently wrong output.
#[cfg(feature = "segmentation")]
#[test]
fn segmentation_detector_errors_cleanly_without_source_path() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{
        detect_horizon_via_segmentation, load_frame_from_path, load_model, HorizonConfig,
        Intrinsics,
    };
    use std::path::Path;

    let model_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join("segmentation.onnx");
    if !model_path.exists() {
        eprintln!(
            "skipping: segmentation model not present at {}.",
            model_path.display()
        );
        return;
    }
    load_model(&model_path).expect("model should load");

    let path = Path::new(harness::REGRESSION_DIR)
        .join("sailing_sun_upper_left")
        .join("frame.png");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");
    // Note: deliberately *not* calling .with_source_path(path); this
    // is the condition under test.
    let result = detect_horizon_via_segmentation(&frame, HorizonConfig::default());
    assert!(
        result.is_err(),
        "expected SegmentError when source_path is None, got Ok"
    );
}

/// End-to-end ML-assisted centroiding on `sailing_sun_upper_left`:
/// segment the frame, build a sky-only mask, run the *extended-disk*
/// centroider with that mask. Two assertions:
///   1. The masked centroid lands inside the sky mask (load-bearing
///      — proves the masking actually constrains output).
///   2. The masked centroid is plausibly near the Sun, accepting
///      that area-weighted centroiding over "all bright sky
///      pixels" pulls the answer toward whichever side has brighter
///      haze.
///
/// **Documented limitation.** The relative-threshold extended-disk
/// centroider catches bright haze around the Sun and biases the
/// centroid toward whichever side has more haze. The
/// `centroid_saturated_body_in_mask` entry point with no mask is the
/// recommended approach for Sun/Moon localization on saturated
/// bodies; it lands at ~(99, 45) on this scene, sub-pixel close to
/// the visual sun. See the `sailing_sun_upper_left_saturated_centroid`
/// test below for that path.
#[cfg(feature = "segmentation")]
#[test]
fn sailing_sun_upper_left_sky_mask_centroids_to_sky_region() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{
        centroid_brightest_body_in_mask, load_frame_from_path, load_model, segment, CentroidConfig,
        Intrinsics,
    };
    use std::path::Path;

    let model_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join("segmentation.onnx");
    if !model_path.exists() {
        return;
    }
    load_model(&model_path).expect("model should load");

    let path = Path::new(harness::REGRESSION_DIR)
        .join("sailing_sun_upper_left")
        .join("frame.png");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame")
        .with_source_path(path.clone());

    let mask = segment(&path).expect("segmentation should succeed");
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

/// Saturated-body centroiding on `sailing_sun_upper_left`. This is
/// the *recommended* path for Sun/Moon centroiding when the body is
/// saturated: thresholds at an absolute saturation level (95% of
/// `u16::MAX`) rather than a fraction of the frame's brightest
/// pixel, so the bright haze around the Sun is excluded.
///
/// **No mask is used.** A surprising finding from the corpus pass:
/// the ADE20K-trained segmentation model classifies the saturated
/// Sun *as something other than sky* — likely "light" or one of the
/// indoor classes. Constraining the saturated centroider to the
/// sky mask therefore *excludes* the actual Sun pixels and lands
/// the centroid on a smaller saturated haze region nearby
/// (~(117, 54)) instead of the Sun core (~(99, 47)).
///
/// Saturation thresholding alone is restrictive enough to exclude
/// most non-body pixels; the mask is overkill on scenes with one
/// dominant saturated body and harmful when the model's "sky"
/// class doesn't include the body itself. A future Bris-trained
/// segmentation model with a "sky-or-bright-body" class would
/// resolve this; until then the unmasked saturated centroider is
/// the right tool.
///
/// On this scene: lands at ~(99, 45) — sub-pixel close on x,
/// ~3 px high in y because the saturated disk extends slightly
/// further into the brighter sky above the Sun than below.
#[test]
fn sailing_sun_upper_left_saturated_centroid() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{
        centroid_saturated_body_in_mask, load_frame_from_path, Intrinsics, SaturatedBodyConfig,
    };
    use std::path::Path;

    let path = Path::new(harness::REGRESSION_DIR)
        .join("sailing_sun_upper_left")
        .join("frame.png");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    let centroid = centroid_saturated_body_in_mask(&frame, SaturatedBodyConfig::default(), None)
        .expect("saturated centroider should succeed on a saturated-Sun scene");

    // The visual Sun is at ~(99, 48). The saturated centroider lands
    // sub-pixel close in x (~99) and a few px high in y (~45) because
    // saturation extends slightly further into the upper sky than
    // below. 5 px tolerance accommodates this and any future
    // sub-pixel refinement work.
    const SUN_X: f64 = 99.0;
    const SUN_Y: f64 = 48.0;
    const TOL_PX: f64 = 5.0;
    let dist = ((centroid.x - SUN_X).powi(2) + (centroid.y - SUN_Y).powi(2)).sqrt();
    assert!(
        dist < TOL_PX,
        "saturated centroid at ({:.2}, {:.2}) is {:.2} px from Sun at ({}, {}); \
         expected within {} px",
        centroid.x,
        centroid.y,
        dist,
        SUN_X,
        SUN_Y,
        TOL_PX,
    );
    // Saturated-disk area should be in the few-thousand-pixel range
    // for this scene's Sun size.
    assert!(
        centroid.area_px > 1500 && centroid.area_px < 4000,
        "expected saturated-disk area in [1500, 4000], got {}",
        centroid.area_px
    );
}

/// Saturated-body centroiding on `marina`. This scene has no body
/// (dusk harbor with no celestial source); the saturated centroider
/// must refuse cleanly with `NoBrightRegion` or `ComponentTooSmall`.
/// Load-bearing assertion: pipeline doesn't fabricate a body when
/// none is present.
#[test]
fn marina_saturated_centroid_refuses_cleanly() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{
        centroid_saturated_body_in_mask, load_frame_from_path, CentroidError, Intrinsics,
        SaturatedBodyConfig,
    };
    use std::path::Path;

    let path = Path::new(harness::REGRESSION_DIR)
        .join("marina")
        .join("frame.png");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    let result = centroid_saturated_body_in_mask(&frame, SaturatedBodyConfig::default(), None);
    assert!(
        matches!(
            result,
            Err(CentroidError::NoBrightRegion(_) | CentroidError::ComponentTooSmall(_, _))
        ),
        "expected clean refusal, got {result:?}",
    );
}

/// Saturated-body centroiding on `night_test_lowres` finds the Moon.
/// Load-bearing: the user's stated success criterion for this
/// scene is "moon centroiding *should* work." It does, and the
/// saturated centroider gets it sub-pixel close to the same
/// position the unmasked extended-disk centroider does — confirming
/// that this is the *right* algorithm for the scene, not just a
/// coincidence of which pixels are picked up.
#[test]
fn night_test_lowres_saturated_centroid_finds_moon() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{
        centroid_saturated_body_in_mask, load_frame_from_path, Intrinsics, SaturatedBodyConfig,
    };
    use std::path::Path;

    let path = Path::new(harness::REGRESSION_DIR)
        .join("night_test_lowres")
        .join("frame.jpg");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    let centroid = centroid_saturated_body_in_mask(&frame, SaturatedBodyConfig::default(), None)
        .expect("saturated centroider should find the Moon");

    // The Moon is at approximately (454, 350) per the corpus probe.
    const MOON_X: f64 = 454.0;
    const MOON_Y: f64 = 350.0;
    const TOL_PX: f64 = 3.0;
    let dist = ((centroid.x - MOON_X).powi(2) + (centroid.y - MOON_Y).powi(2)).sqrt();
    assert!(
        dist < TOL_PX,
        "saturated centroid at ({:.2}, {:.2}) is {:.2} px from Moon at ({}, {}); \
         expected within {} px",
        centroid.x,
        centroid.y,
        dist,
        MOON_X,
        MOON_Y,
        TOL_PX,
    );
}

/// `sunrise` revisited: with the body-excluding column mask AND a
/// relaxed inlier-fraction RANSAC config, the gradient detector
/// finds the sea horizon. Without the mask + relaxed config (the
/// default-config path tested by the `case_sunrise::*` generated
/// tests), all three detectors fail because the saturated sun on
/// the horizon blots out clean candidates and a third of the
/// remaining candidates support the real horizon — strong absolute
/// consensus but below the default 50% inlier-fraction floor.
///
/// This test demonstrates the path forward for low-altitude-body
/// scenes: combine [`body_column_mask`] with a per-scene
/// `min_inlier_fraction` adjustment. The resulting fix should be
/// flagged with appropriately wide σ (large RMS residual) so the
/// operator sees that low-altitude sights carry elevated
/// uncertainty — which they correctly do.
///
/// The recorded values:
///   - sun centroid: (311.74, 225.67)
///   - horizon: y ≈ 241 (sun is ~15 px above horizon)
///   - 71 inliers / ~190 candidates after body exclusion
#[test]
fn sunrise_horizon_findable_with_body_exclusion_and_relaxed_ransac() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{
        body_column_mask, centroid_saturated_body_in_mask, detect_horizon_with_column_mask,
        load_frame_from_path, HorizonConfig, Intrinsics, SaturatedBodyConfig,
    };
    use std::path::Path;

    let path = Path::new(harness::REGRESSION_DIR)
        .join("sunrise")
        .join("frame.png");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    // Find the saturated sun.
    let sun = centroid_saturated_body_in_mask(&frame, SaturatedBodyConfig::default(), None)
        .expect("saturated sun should be detectable on this scene");

    // Build a column mask that excludes the sun's columns + 8 px pad.
    let radius_px = (f64::from(sun.area_px) / std::f64::consts::PI).sqrt();
    let col_mask = body_column_mask(frame.width(), sun.x, radius_px, 8.0);
    // Body should occupy a small fraction of the frame (~5% here).
    let excluded = col_mask.iter().filter(|&&b| !b).count();
    assert!(
        excluded < 60,
        "body-exclusion took out too many columns: {excluded}",
    );

    // Relaxed RANSAC config: 30% inlier fraction. The default 50%
    // is too strict for low-altitude-body scenes where lens flare
    // and sky-haze produce many spurious candidates.
    let cfg = HorizonConfig {
        min_inlier_fraction: 0.3,
        ..HorizonConfig::default()
    };
    let line = detect_horizon_with_column_mask(&frame, cfg, Some(&col_mask))
        .expect("body-excluding gradient detector with relaxed RANSAC should find the horizon");

    // Recorded values: intercept ≈ 241, slope near zero.
    assert!(
        (line.intercept - 241.0).abs() < 5.0,
        "horizon intercept {} not near recorded value 241",
        line.intercept
    );
    assert!(
        line.slope.abs() < 0.05,
        "horizon slope {} not near horizontal",
        line.slope
    );
    // Sun is above horizon (lower y is higher in image-space, and
    // sun.y < line.intercept means above).
    assert!(
        sun.y < line.intercept,
        "sun at y={} should be above horizon at y={}",
        sun.y,
        line.intercept
    );
    // Residual RMS is the load-bearing diagnostic that this scene
    // produces a *low-confidence* horizon, even when the algorithm
    // succeeds. ~2 px RMS at full resolution is on the high side;
    // this would translate to elevated altitude σ in the eventual
    // sight.
    assert!(
        line.residual_rms_px > 1.0,
        "expected meaningful residual RMS (this is a hard scene); \
         got {:.2} px",
        line.residual_rms_px
    );
}

/// `night_test_lowres` revisited: with a manually-tuned
/// `search_row_range` that skips the moon's halo, the night
/// detector finds the actual sea-sky horizon at ~69% of the
/// frame height (around y = 1324 of 1920).
///
/// The default-config night detector lands on the moon halo's
/// edge at y ≈ 258 — the strongest luma transition in the frame.
/// The body-excluding variant alone doesn't help much because the
/// moon's halo extends well beyond the body's column range. The
/// fix is to restrict the global gradient search to the lower
/// portion of the frame (below the halo): `search_row_range:
/// (0.55, 1.0)` works for this scene.
///
/// In the eventual streaming engine this kind of per-scene tuning
/// would come from either:
///   - A multi-pass detector (find strongest gradient, mask its
///     neighborhood, find next-strongest).
///   - Combining with the segmentation detector to get a sky/sea
///     class prior.
///   - Operator-supplied scene context.
///
/// For now this test documents that the night detector *can* find
/// the right horizon when given the right search range — the
/// algorithm is sound; the autoconfig is the missing piece.
#[test]
fn night_test_lowres_horizon_findable_with_tuned_search_range() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{detect_horizon_night, load_frame_from_path, Intrinsics, NightHorizonConfig};
    use std::path::Path;

    let path = Path::new(harness::REGRESSION_DIR)
        .join("night_test_lowres")
        .join("frame.jpg");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    let cfg = NightHorizonConfig {
        search_row_range: (0.55, 1.0),
        min_inlier_fraction: 0.2,
        ..NightHorizonConfig::default()
    };
    let line =
        detect_horizon_night(&frame, cfg).expect("tuned night detector should find a horizon");

    // Recorded: intercept ≈ 1324, slope near zero, ~110 inliers.
    // ~69% of the 1920-tall frame, which is where the actual
    // moonlit-sky-to-sea transition sits.
    assert!(
        (line.intercept - 1324.0).abs() < 30.0,
        "horizon intercept {} not near recorded ~1324",
        line.intercept
    );
    assert!(
        line.slope.abs() < 0.05,
        "horizon slope {} not near horizontal",
        line.slope
    );
    assert!(
        line.inlier_count >= 80,
        "expected at least 80 inliers, got {}",
        line.inlier_count
    );
}

/// Synthetic "moonlit sea brighter than dark sky" scene: confirms
/// the night detector handles the **sea-brighter-than-sky** case
/// (the daylight detectors all assume sky-brighter-than-sea).
/// Strictly synthetic; complementary to the real-scene
/// `night_test_lowres` test above.
#[test]
fn night_detector_handles_sea_brighter_than_sky() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{detect_horizon_night, Frame, Intrinsics, NightHorizonConfig};

    // Build a frame with dark sky on top (luma 800) and brighter
    // moonlit sea on the bottom (luma 1500), transition at y=200.
    let w: u32 = 640;
    let h: u32 = 360;
    let mut pixels = vec![0u16; (w * h) as usize];
    for y in 0..h {
        let v = if y < 200 { 800 } else { 1500 };
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

    let line = detect_horizon_night(&frame, NightHorizonConfig::default())
        .expect("night detector should handle sea-brighter-than-sky");
    assert!(
        (line.intercept - 200.0).abs() < 5.0,
        "intercept {} not near true horizon row 200",
        line.intercept
    );
}

/// `marina_with_body` exercises the **peak detector** for a
/// non-saturated body (the dusk Moon visible in the marina scene).
/// Three frames captured at different points of the rigging-sway
/// cycle:
///
///   - `frame_visible.png`: Moon clearly detectable.
///   - `frame_partial.png`: Moon barely above peak threshold;
///     rigging passing across.
///   - `frame_obscured.png`: Moon below peak threshold; rigging
///     fully covers it.
///
/// Demonstrates two complementary properties:
/// 1. Peak detection finds non-saturated bodies that
///    `centroid_brightest_body` misses (the body's connected
///    component is too small at the `0.85·frame_max` threshold).
/// 2. Single-frame detection is **not enough** when something
///    intermittently obscures the body — the future streaming
///    engine needs cross-frame tracking to maintain a body
///    position estimate through obscured frames so a fix can be
///    computed when the body briefly reappears. The Phase 2
///    panorama stitching machinery is the foundation; predictive
///    tracking is the missing piece.
///
/// Recorded position: Moon at ~(415.88, 111.77), intensity ~43000
/// in the visible frame. The same peak appears at ~(415.22,
/// 111.15) in the partial frame at lower intensity (~41167).
#[test]
fn marina_with_body_peak_detector_finds_moon_when_visible() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{detect_peaks, load_frame_from_path, Intrinsics, PeakConfig};
    use std::path::Path;

    let path = Path::new(harness::REGRESSION_DIR)
        .join("marina_with_body")
        .join("frame_visible.png");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    let peaks = detect_peaks(&frame, PeakConfig::default());

    // The Moon is at ~(415.88, 111.77). It must be in the top peaks.
    const MOON_X: f64 = 415.88;
    const MOON_Y: f64 = 111.77;
    const TOL_PX: f64 = 5.0;
    let moon_peak = peaks
        .iter()
        .find(|p| ((p.x - MOON_X).powi(2) + (p.y - MOON_Y).powi(2)).sqrt() < TOL_PX);
    assert!(
        moon_peak.is_some(),
        "no peak within {TOL_PX} px of Moon at ({MOON_X}, {MOON_Y}); top peaks: {:?}",
        peaks.iter().take(5).collect::<Vec<_>>(),
    );
}

#[test]
fn marina_with_body_peak_detector_sees_moon_dim_when_rigging_obscures() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{detect_peaks, load_frame_from_path, Intrinsics, PeakConfig};
    use std::path::Path;

    fn moon_intensity(filename: &str) -> Option<f64> {
        let path = Path::new(harness::REGRESSION_DIR)
            .join("marina_with_body")
            .join(filename);
        let dims = image::image_dimensions(&path).expect("dims");
        let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
        let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
            .expect("load frame");
        let peaks = detect_peaks(&frame, PeakConfig::default());
        const MOON_X: f64 = 415.88;
        const MOON_Y: f64 = 111.77;
        const TOL_PX: f64 = 5.0;
        peaks
            .iter()
            .find(|p| ((p.x - MOON_X).powi(2) + (p.y - MOON_Y).powi(2)).sqrt() < TOL_PX)
            .map(|p| p.intensity)
    }

    let visible = moon_intensity("frame_visible.png").expect("Moon visible in frame_visible.png");
    let obscured =
        moon_intensity("frame_obscured.png").expect("Moon still detectable in frame_obscured.png");

    // The rigging dims the Moon's apparent intensity. Visible
    // frame has intensity ~43000; obscured frame ~29000 (recorded
    // values, ±5%). The drop is the load-bearing assertion: the
    // peak detector sees the body fade as the rigging swings
    // across, which is the signal a temporal-tracking algorithm
    // would use to know the body is being intermittently obscured.
    assert!(
        obscured < visible * 0.8,
        "expected obscured intensity ({obscured:.0}) to be substantially \
         below visible intensity ({visible:.0}); rigging should dim it",
    );
}

/// Multi-pass night-horizon detector on `night_test_highres`:
/// finds the actual sea-sky horizon at y ≈ 77 even though the
/// single-pass detector lands on the wake region (y ≈ 180). The
/// load-bearing assertion: with multi-pass, the
/// most-inliers candidate is the real horizon.
///
/// This is a real-data demonstration of why multi-pass matters:
/// the strongest horizontal luma transition isn't always the
/// horizon. Multi-pass enumerates the top-N transitions; the
/// caller picks by additional context (inlier count, position
/// in the frame, segmentation prior).
#[test]
fn night_test_highres_multi_pass_finds_actual_horizon() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{
        detect_horizon_night_multi_pass, load_frame_from_path, Intrinsics, NightHorizonConfig,
    };
    use std::path::Path;

    let path = Path::new(harness::REGRESSION_DIR)
        .join("night_test_highres")
        .join("frame.png");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    let candidates = detect_horizon_night_multi_pass(&frame, NightHorizonConfig::default(), None);
    assert!(
        !candidates.is_empty(),
        "multi-pass should find at least one horizon"
    );
    // The actual sea-sky horizon is around y=85 (visible in the
    // frame as the dark-sky / dark-sea boundary). Multi-pass
    // sorted by inlier count puts it first.
    let top = &candidates[0];
    assert!(
        (top.intercept - 77.0).abs() < 15.0,
        "top candidate should be near y=77 (actual horizon); got intercept {:.1}",
        top.intercept,
    );
    assert!(
        top.inlier_count > 150,
        "top candidate should have strong inlier consensus; got {}",
        top.inlier_count,
    );
}

/// Multi-pass on `container_ship_night`: the deck-top is the
/// strongest single-pass match (y ≈ 329), but multi-pass finds a
/// stronger consensus near the actual sea horizon at y ≈ 247.
#[test]
fn container_ship_night_multi_pass_finds_horizon_below_deck() {
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{
        detect_horizon_night_multi_pass, load_frame_from_path, Intrinsics, NightHorizonConfig,
    };
    use std::path::Path;

    let path = Path::new(harness::REGRESSION_DIR)
        .join("container_ship_night")
        .join("frame.png");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    let candidates = detect_horizon_night_multi_pass(&frame, NightHorizonConfig::default(), None);
    assert!(
        candidates.len() >= 2,
        "multi-pass should find at least 2 candidates on this scene; got {}",
        candidates.len(),
    );
    // The top candidate (sorted by inlier count) is the actual
    // sea-sky horizon near y=247 (164 inliers); the deck top
    // appears as a secondary candidate near y=329 (105 inliers).
    let top = &candidates[0];
    assert!(
        (top.intercept - 247.0).abs() < 15.0,
        "top candidate should be near y=247 (sea horizon); got {:.1}",
        top.intercept,
    );
    assert!(
        top.inlier_count > candidates[1].inlier_count,
        "top candidate should have more inliers than secondary",
    );
}
