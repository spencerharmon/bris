//! End-to-end plate-solving integration tests against real
//! captured footage.
//!
//! These tests load frames from the `bris-vision` regression
//! corpus, run peak detection, and feed the peaks into the plate
//! solver. They exercise the *current state* of the algorithm
//! against real night-sky imagery — not a correctness assertion,
//! more a "what does it actually do" probe in test form.
//!
//! Each test is `#[ignore]` because the database build at the
//! magnitude cutoff needed for real footage takes long enough
//! (~10-30 seconds in release, longer in debug) that we don't
//! want it in the routine CI loop. Run with:
//!
//! ```text
//! cargo test --release -p bris-platesolve --test real_data -- --ignored
//! ```

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::path::Path;

use bris_core::time::{Tt, JD_J2000};
use bris_platesolve::{plate_solve, PlateSolveConfig, StarHashDb, StarHashDbConfig};
use bris_vision::{detect_peaks, load_frame_from_path, Intrinsics, PeakConfig};

const CORPUS_DIR: &str = "../bris-vision/tests/regression";

fn frame_path(case: &str, filename: &str) -> std::path::PathBuf {
    Path::new(CORPUS_DIR).join(case).join(filename)
}

/// Probe-style test: load the `night_test_highres` frame, run peak
/// detection, run plate solving with a moderately-deep catalog
/// (mag 5.0). Print the result.
///
/// Pass criterion: the solver runs without panicking. Whether it
/// finds a match depends on actual scene content vs. catalog
/// coverage; this test documents the current behavior rather than
/// asserting a specific outcome.
#[test]
#[ignore = "slow; loads catalog and runs full solver against real footage"]
fn night_test_highres_real_pipeline() {
    let path = frame_path("night_test_highres", "frame.png");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    let peaks = detect_peaks(&frame, PeakConfig::default());
    eprintln!(
        "night_test_highres: detected {} peaks (top intensity = {})",
        peaks.len(),
        peaks.first().map_or(0.0, |p| p.intensity),
    );

    let cfg = StarHashDbConfig {
        mag_cutoff: 5.0,
        max_pattern_diameter_rad: 60.0_f64.to_radians(),
        bin_count: 50,
        neighbor_limit: 20,
    };
    let db = StarHashDb::build(cfg);
    eprintln!(
        "db: {} unique hashes, {} patterns from {} stars",
        db.bin_count_used(),
        db.pattern_count(),
        bris_almanac::all_stars()
            .iter()
            .filter(|s| s.vmag <= 5.0)
            .count(),
    );

    let result = plate_solve(
        &peaks,
        &intrinsics,
        &db,
        PlateSolveConfig {
            max_peaks_to_match: 12,
            min_verifications: 3,
            verify_match_radius_rad: 1.5_f64.to_radians(),
            max_rms_residual_rad: (60.0 / 3600.0_f64).to_radians(), // 1 arcmin
            max_tuple_diameter_rad: 60.0_f64.to_radians(),
        },
    );
    match result {
        Ok(r) => {
            eprintln!("MATCHED. {} stars identified.", r.identified.len());
            for s in &r.identified {
                eprintln!(
                    "  hr={} ra={:.3} dec={:.3} mag={:.2} pixel=({:.1}, {:.1})",
                    s.hr,
                    s.ra_rad.to_degrees(),
                    s.dec_rad.to_degrees(),
                    s.vmag,
                    s.pixel_x,
                    s.pixel_y,
                );
            }
        }
        Err(e) => {
            eprintln!("no match: {e}");
        }
    }
}

/// Same probe against `container_ship_night`.
#[test]
#[ignore = "slow; loads catalog and runs full solver against real footage"]
fn container_ship_night_real_pipeline() {
    let path = frame_path("container_ship_night", "frame.png");
    let dims = image::image_dimensions(&path).expect("dims");
    let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(&path, Tt::from_julian_date(JD_J2000), 0, intrinsics)
        .expect("load frame");

    let peaks = detect_peaks(&frame, PeakConfig::default());
    eprintln!(
        "container_ship_night: detected {} peaks (top intensity = {})",
        peaks.len(),
        peaks.first().map_or(0.0, |p| p.intensity),
    );

    let cfg = StarHashDbConfig {
        mag_cutoff: 5.0,
        max_pattern_diameter_rad: 60.0_f64.to_radians(),
        bin_count: 50,
        neighbor_limit: 20,
    };
    let db = StarHashDb::build(cfg);

    let result = plate_solve(
        &peaks,
        &intrinsics,
        &db,
        PlateSolveConfig {
            max_peaks_to_match: 16,
            min_verifications: 3,
            verify_match_radius_rad: 1.5_f64.to_radians(),
            max_rms_residual_rad: (60.0 / 3600.0_f64).to_radians(), // 1 arcmin
            max_tuple_diameter_rad: 60.0_f64.to_radians(),
        },
    );
    match result {
        Ok(r) => {
            eprintln!("MATCHED. {} stars identified.", r.identified.len());
        }
        Err(e) => {
            eprintln!("no match: {e}");
        }
    }
}
