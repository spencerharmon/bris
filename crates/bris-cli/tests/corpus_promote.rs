//! `bris corpus promote` end-to-end test.
//!
//! Synthesizes a tiny bris-bundle-shaped capture (a sky/sea split
//! plus a saturated "sun" blob, in the same style as
//! `tests/replay.rs`'s synthetic bundle), promotes it via the CLI,
//! and asserts:
//!
//! 1. the tool exits 0 and writes `case.toml` + `frame.png`,
//! 2. the generated `case.toml` parses against the schema
//!    `crates/bris-vision/tests/regression_test.rs` expects,
//! 3. the derived expectation values agree with independently
//!    re-running the vision pipeline against the emitted frame —
//!    proving the promotion tool doesn't fabricate its stubs.
//!
//! This closes the corpus-promotion-tooling loop: field capture →
//! `case.toml` skeleton → the case can drive `bris replay` /
//! the regression harness.

use std::fs;
use std::process::Command;

use bris_vision::{
    centroid_brightest_body, classify, detect_horizon, CentroidConfig, ConditionConfig,
    HorizonConfig, Intrinsics, Rotation,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const HORIZON_Y: u32 = 120;

/// Sky above `HORIZON_Y` at `30_000`, sea below at `5_000`, plus a
/// saturated `60_000` "sun" disc in the sky so centroiding has a
/// candidate distinct from the general sky brightness.
#[allow(clippy::similar_names)]
fn write_synthetic_pgm(path: &std::path::Path) {
    let mut pixels = vec![0u16; (WIDTH as usize) * (HEIGHT as usize)];
    let (sun_cx, sun_cy, sun_r) = (200i64, 60i64, 12i64);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let dx = i64::from(x) - sun_cx;
            let dy = i64::from(y) - sun_cy;
            let v: u16 = if dx * dx + dy * dy <= sun_r * sun_r {
                60_000
            } else if y < HORIZON_Y {
                30_000
            } else {
                5_000
            };
            pixels[(y as usize) * (WIDTH as usize) + (x as usize)] = v;
        }
    }
    let buf = image::ImageBuffer::<image::Luma<u16>, _>::from_raw(WIDTH, HEIGHT, pixels).unwrap();
    buf.save_with_format(path, image::ImageFormat::Pnm).unwrap();
}

fn write_synthetic_capture(dir: &std::path::Path) {
    let media = dir.join("media");
    fs::create_dir_all(&media).unwrap();
    let pgm = media.join(format!("{:012}.pgm", 0));
    write_synthetic_pgm(&pgm);
    let sidecar = pgm.with_extension("json");
    fs::write(
        &sidecar,
        r#"{"seq":0,"captured_unix_ms":1700000000000,"width":320,"height":240}"#,
    )
    .unwrap();

    let bundle = r#"{
        "schema_version": 1,
        "bundle_id": "synthetic-corpus-promotion-test",
        "device": { "model": "synthetic" },
        "capture": {
            "source_rotation_deg": 0,
            "frame_count": 1,
            "started_unix_ms": 1700000000000,
            "ended_unix_ms": 1700000000000
        },
        "intrinsics": {
            "source": { "kind": "placeholder" },
            "width": 320, "height": 240,
            "fx": 1000.0, "fy": 1000.0, "cx": 160.0, "cy": 120.0,
            "distortion": { "model": "none" }
        }
    }"#;
    fs::write(dir.join("bundle.json"), bundle).unwrap();
}

/// Minimal mirror of the fields
/// `crates/bris-vision/tests/regression_test.rs`'s `CaseSpec` schema
/// declares, enough to assert the generated skeleton is structurally
/// and numerically sound without depending on the `bris-vision` test
/// binary's internal (non-lib) harness types.
#[derive(Debug, serde::Deserialize)]
struct CaseSpec {
    case: CaseMeta,
    expected_classifier: Option<ClassifierExpectation>,
    expected_centroid_frame0: Option<CentroidExpectation>,
    horizon: HorizonExpectations,
}

#[derive(Debug, serde::Deserialize)]
struct CaseMeta {
    name: String,
    frame_width: u32,
    frame_height: u32,
}

#[derive(Debug, serde::Deserialize)]
struct ClassifierExpectation {
    condition: String,
}

#[derive(Debug, serde::Deserialize)]
struct CentroidExpectation {
    x_px: f64,
    y_px: f64,
}

#[derive(Debug, Default, serde::Deserialize)]
struct HorizonExpectations {
    gradient: Option<HorizonExpectation>,
}

#[derive(Debug, serde::Deserialize)]
struct HorizonExpectation {
    outcome: String,
    slope: Option<f64>,
    intercept: Option<f64>,
}

#[test]
fn promote_generates_a_parseable_case_matching_the_actual_pipeline_output() {
    let tmp = tempfile::tempdir().unwrap();
    let capture_dir = tmp.path().join("capture");
    write_synthetic_capture(&capture_dir);

    let case_dir = tmp.path().join("case-output").join("synthetic_case");

    let exe = env!("CARGO_BIN_EXE_bris");
    let out = Command::new(exe)
        .args([
            "corpus",
            "promote",
            "--capture",
            capture_dir.to_str().unwrap(),
            "--output",
            case_dir.to_str().unwrap(),
            "--name",
            "synthetic_case",
        ])
        .output()
        .expect("invoke bris corpus promote");
    assert!(
        out.status.success(),
        "promote exited non-zero.\nSTDOUT:\n{}\nSTDERR:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let case_toml_path = case_dir.join("case.toml");
    let frame_png_path = case_dir.join("frame.png");
    assert!(case_toml_path.is_file(), "case.toml was not written");
    assert!(frame_png_path.is_file(), "frame.png was not written");

    let text = fs::read_to_string(&case_toml_path).unwrap();
    let spec: CaseSpec = toml::from_str(&text)
        .unwrap_or_else(|e| panic!("generated case.toml failed to parse: {e}\n---\n{text}"));

    assert_eq!(spec.case.name, "synthetic_case");
    assert_eq!(spec.case.frame_width, WIDTH);
    assert_eq!(spec.case.frame_height, HEIGHT);

    // Re-run the pipeline independently against the emitted frame
    // and confirm the generated stubs agree with reality (rather
    // than being fabricated/copy-pasted placeholders).
    let intrinsics = Intrinsics::placeholder(WIDTH, HEIGHT);
    let frame = bris_vision::load_frame_from_path_with_rotation(
        &frame_png_path,
        bris_core::time::Tt::from_julian_date(bris_core::time::JD_J2000),
        0,
        intrinsics,
        Rotation::Deg0,
    )
    .expect("reload emitted frame.png");

    let classification = classify(&frame, None, ConditionConfig::default());
    let expected_classifier = spec
        .expected_classifier
        .expect("case.toml should declare [expected_classifier]");
    assert_eq!(
        expected_classifier.condition.to_lowercase(),
        format!("{:?}", classification.condition).to_lowercase(),
        "recorded classifier condition disagrees with a fresh run"
    );

    let centroid =
        centroid_brightest_body(&frame, CentroidConfig::default()).expect("sun blob detected");
    let expected_centroid = spec
        .expected_centroid_frame0
        .expect("case.toml should declare [expected_centroid_frame0] for a scene with a body");
    assert!(
        (expected_centroid.x_px - centroid.x).abs() < 1e-6,
        "centroid x mismatch: recorded {} vs actual {}",
        expected_centroid.x_px,
        centroid.x
    );
    assert!(
        (expected_centroid.y_px - centroid.y).abs() < 1e-6,
        "centroid y mismatch: recorded {} vs actual {}",
        expected_centroid.y_px,
        centroid.y
    );

    let gradient = detect_horizon(&frame, HorizonConfig::default());
    let expected_gradient = spec
        .horizon
        .gradient
        .expect("case.toml should declare [horizon.gradient]");
    match gradient {
        Ok(line) => {
            assert_eq!(expected_gradient.outcome, "ok");
            let slope = expected_gradient.slope.expect("recorded slope");
            let intercept = expected_gradient.intercept.expect("recorded intercept");
            assert!((slope - line.slope).abs() < 1e-3, "slope mismatch");
            assert!((intercept - line.intercept).abs() < 1e-1, "intercept mismatch");
        }
        Err(_) => {
            assert_eq!(expected_gradient.outcome, "err");
        }
    }
}
