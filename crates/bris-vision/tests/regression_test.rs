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
        centroid_brightest_body, detect_horizon, detect_horizon_via_sky_region,
        load_frame_from_path, CentroidConfig, Frame, HorizonConfig, HorizonError, HorizonLine,
        Intrinsics,
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
        pub frame_width: u32,
        pub frame_height: u32,
        /// Rotation applied to the source image at load time, in
        /// degrees clockwise. 0 for landscape captures; 90 / 180 /
        /// 270 for portrait or otherwise-rotated captures. Pixel
        /// coordinates in `expected_centroid_frame0` and `horizon.*`
        /// are after rotation. Defaults to 0.
        ///
        /// Loader-side rotation is a separate follow-up commit; for
        /// now any case with a non-zero value will trip an assertion
        /// in the loader. The schema lands first so cases can declare
        /// rotation when they need it.
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

    /// Day/night/twilight classifier expectation. Asserted only if
    /// `[expected_classifier]` is present. Until the classifier
    /// module lands this is a no-op stub; the schema is in place so
    /// cases can declare expectations ahead of the implementation.
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

    /// Load a frame from a case directory. Honors the case's
    /// `source_rotation_deg` (currently 0 only; non-zero traps).
    pub fn load_case_frame(case: &CaseSpec, filename: &str) -> Frame {
        let path: PathBuf = Path::new(REGRESSION_DIR)
            .join(&case.case.name)
            .join(filename);
        let dims = image::image_dimensions(&path)
            .unwrap_or_else(|e| panic!("dims {}: {e}", path.display()));
        // Rotation lands in a follow-up commit; for now any case
        // with non-zero rotation will trip this assertion until the
        // loader honors it.
        assert_eq!(
            case.case.source_rotation_deg, 0,
            "case {}: source_rotation_deg = {} but loader rotation \
             isn't wired in yet",
            case.case.name, case.case.source_rotation_deg,
        );
        let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
        load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
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
        let (expected_w, expected_h) = match case.case.source_rotation_deg {
            0 | 180 => (case.case.frame_width, case.case.frame_height),
            90 | 270 => (case.case.frame_height, case.case.frame_width),
            other => panic!(
                "case {}: source_rotation_deg must be 0|90|180|270, got {other}",
                case.case.name
            ),
        };
        for filename in &filenames {
            let f = load_case_frame(case, filename);
            assert_eq!(
                f.width(),
                expected_w,
                "{}: width = {} expected {}",
                filename,
                f.width(),
                expected_w
            );
            assert_eq!(
                f.height(),
                expected_h,
                "{}: height = {} expected {}",
                filename,
                f.height(),
                expected_h
            );
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
/// segment the frame, build a sky-only mask, run masked centroid.
/// Two assertions:
///   1. The masked centroid lands inside the sky mask (load-bearing
///      — proves the masking actually constrains output).
///   2. The masked centroid is plausibly near the Sun, accepting
///      that area-weighted centroiding over "all bright sky
///      pixels" pulls the answer toward whichever side has brighter
///      haze.
///
/// **Known limitation.** For tight Sun/Moon centroids inside a sky
/// mask, the right algorithm is the peak detector (`detect_peaks`)
/// rather than the connected-component centroider, because Sun/Moon
/// are *peaks* of brightness rather than largest connected regions.
/// Tracked as a follow-up; see plan.org "Switch Sun/Moon centroiding
/// to peak detection inside sky mask."
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
