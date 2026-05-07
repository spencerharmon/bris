//! End-to-end plate-solving regression tests against real
//! captured footage from the bris-vision corpus.
//!
//! Each case in `../bris-vision/tests/regression/*/` that
//! declares a `[plate_solve]` table in its `case.toml` gets a
//! generated `plate_solve` test (see `build.rs`). The runner
//! `run_plate_solve_case` reads the expectation, runs
//! [`plate_solve`] with any per-case config overrides, and
//! asserts the declared outcome.
//!
//! All generated tests are `#[ignore]` because the geometric-
//! hash database build is ~10-30s in release mode. Run with:
//!
//! ```text
//! cargo test --release -p bris-platesolve --test real_data \
//!     -- --ignored --include-ignored
//! ```
//!
//! The database is cached across tests in the same process via
//! `OnceLock` keyed on the config hash, so a full corpus pass
//! pays the build cost once.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bris_core::time::{Tt, JD_J2000};
use bris_platesolve::{
    plate_solve, PlateSolveConfig, PlateSolveError, StarHashDb, StarHashDbConfig,
};
use bris_vision::{detect_peaks, load_frame_from_path, Intrinsics, PeakConfig};

const CORPUS_DIR: &str = "../bris-vision/tests/regression";

/// Top-level case.toml schema (only the parts the plate-solve
/// runner cares about).
#[derive(Debug, serde::Deserialize)]
struct CaseSpec {
    case: CaseMeta,
    plate_solve: PlateSolveExpectation,
}

#[derive(Debug, serde::Deserialize)]
struct CaseMeta {
    #[serde(default)]
    frames: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize)]
struct PlateSolveExpectation {
    outcome: Outcome,
    #[serde(default)]
    min_identified_stars: Option<usize>,
    #[serde(default)]
    error_variant: Option<String>,
    /// Documentation only.
    #[serde(default)]
    #[allow(dead_code)]
    correctness: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    notes: Option<String>,
    /// Optional config overrides.
    #[serde(default = "default_mag_cutoff")]
    config_mag_cutoff: f64,
    #[serde(default = "default_max_rms_residual_arcsec")]
    config_max_rms_residual_arcsec: f64,
    #[serde(default = "default_min_verifications")]
    config_min_verifications: usize,
    #[serde(default = "default_max_pattern_diameter_deg")]
    config_max_pattern_diameter_deg: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Ok,
    Err,
}

fn default_mag_cutoff() -> f64 {
    5.0
}
fn default_max_rms_residual_arcsec() -> f64 {
    60.0 // looser than the library default (30) to allow for placeholder-intrinsics calibration error
}
fn default_min_verifications() -> usize {
    3
}
fn default_max_pattern_diameter_deg() -> f64 {
    60.0
}

fn case_dir(name: &str) -> PathBuf {
    Path::new(CORPUS_DIR).join(name)
}

fn load_case_spec(name: &str) -> CaseSpec {
    let path = case_dir(name).join("case.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn first_frame_path(name: &str, spec: &CaseSpec) -> PathBuf {
    let frame_name = spec
        .case
        .frames
        .as_ref()
        .and_then(|f| f.first().cloned())
        .unwrap_or_else(|| "frame.png".to_string());
    case_dir(name).join(frame_name)
}

/// Cache the hash database keyed on its config (mag cutoff +
/// pattern diameter). All cases probed in one cargo-test
/// invocation share the database build.
type DbCacheEntry = (StarHashDbConfig, std::sync::Arc<StarHashDb>);
static DB_CACHE: OnceLock<std::sync::Mutex<Vec<DbCacheEntry>>> = OnceLock::new();

fn db_for(cfg: StarHashDbConfig) -> std::sync::Arc<StarHashDb> {
    let cache = DB_CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut g = cache.lock().expect("db cache mutex");
    // Find an existing db with matching cfg fields.
    for (existing_cfg, existing_db) in g.iter() {
        // Compare all fields.
        if (existing_cfg.mag_cutoff - cfg.mag_cutoff).abs() < 1e-6
            && (existing_cfg.max_pattern_diameter_rad - cfg.max_pattern_diameter_rad).abs() < 1e-9
            && existing_cfg.bin_count == cfg.bin_count
            && existing_cfg.neighbor_limit == cfg.neighbor_limit
        {
            return existing_db.clone();
        }
    }
    let db = std::sync::Arc::new(StarHashDb::build(cfg));
    g.push((cfg, db.clone()));
    db
}

/// Runner invoked by the build-script-generated per-case tests.
pub fn run_plate_solve_case(case_name: &str) {
    let spec = load_case_spec(case_name);
    let frame_path = first_frame_path(case_name, &spec);
    let dims = image::image_dimensions(&frame_path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&frame_path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    let peaks = detect_peaks(&frame, PeakConfig::default());
    eprintln!(
        "{case_name}: detected {} peaks (top intensity = {})",
        peaks.len(),
        peaks.first().map_or(0.0, |p| p.intensity),
    );

    let exp = &spec.plate_solve;
    let db_cfg = StarHashDbConfig {
        mag_cutoff: exp.config_mag_cutoff,
        max_pattern_diameter_rad: exp.config_max_pattern_diameter_deg.to_radians(),
        bin_count: 50,
        neighbor_limit: 20,
    };
    let db = db_for(db_cfg);

    let solve_cfg = PlateSolveConfig {
        max_peaks_to_match: 12,
        min_verifications: exp.config_min_verifications,
        verify_match_radius_rad: 1.5_f64.to_radians(),
        max_rms_residual_rad: (exp.config_max_rms_residual_arcsec / 3600.0).to_radians(),
        max_tuple_diameter_rad: exp.config_max_pattern_diameter_deg.to_radians(),
    };
    let result = plate_solve(&peaks, &intrinsics, &db, solve_cfg);

    match (exp.outcome, &result) {
        (Outcome::Ok, Ok(r)) => {
            eprintln!(
                "{case_name}: MATCHED, {} stars identified.",
                r.identified.len()
            );
            if let Some(min) = exp.min_identified_stars {
                assert!(
                    r.identified.len() >= min,
                    "{case_name}: expected ≥ {min} identified stars, got {}",
                    r.identified.len()
                );
            }
        }
        (Outcome::Ok, Err(e)) => {
            panic!("{case_name}: expected Ok plate-solve, got Err: {e}");
        }
        (Outcome::Err, Ok(r)) => {
            panic!(
                "{case_name}: expected Err plate-solve, got Ok with {} identified stars",
                r.identified.len()
            );
        }
        (Outcome::Err, Err(e)) => {
            eprintln!("{case_name}: refused as expected: {e}");
            if let Some(want) = &exp.error_variant {
                let msg = format!("{e}");
                assert!(
                    msg.contains(want.as_str()),
                    "{case_name}: expected error containing {want:?}, got {msg:?}"
                );
            }
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/plate_solve_cases_generated.rs"));

// ---------------------------------------------------------------------------
// Static (non-generated) tests for properties not expressible per
// case in the TOML schema.
// ---------------------------------------------------------------------------

/// Demonstrate that `PlateSolveError::InsufficientPeaks` is
/// reachable via the public API. Synthetic input; not a corpus
/// test.
#[test]
fn rejects_fewer_than_4_peaks_via_public_api() {
    use bris_vision::Peak;
    let peaks = vec![
        Peak {
            x: 100.0,
            y: 100.0,
            intensity: 50_000.0,
        },
        Peak {
            x: 200.0,
            y: 100.0,
            intensity: 40_000.0,
        },
        Peak {
            x: 100.0,
            y: 200.0,
            intensity: 30_000.0,
        },
    ];
    let intr = Intrinsics::placeholder(640, 480);
    // Use a tiny db (mag 1.5, ~20 stars) to keep this test fast
    // even when run as part of the default suite.
    let db = StarHashDb::build(StarHashDbConfig {
        mag_cutoff: 1.5,
        ..StarHashDbConfig::default()
    });
    let result = plate_solve(&peaks, &intr, &db, PlateSolveConfig::default());
    assert!(
        matches!(result, Err(PlateSolveError::InsufficientPeaks(3))),
        "expected InsufficientPeaks(3), got {result:?}",
    );
}
