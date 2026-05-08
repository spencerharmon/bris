//! Profile plate_solve hot path: count tuples, hash lookups,
//! candidates, permutations attempted, and time each phase.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::uninlined_format_args,
    clippy::single_match_else,
    clippy::doc_markdown
)]

use bris_core::time::{Tt, JD_J2000};
use bris_platesolve::{plate_solve, PlateSolveConfig, StarHashDb, StarHashDbConfig};
use bris_vision::{
    detect_horizon_night_multi_pass, detect_peaks_above_horizon, load_frame_from_path, Intrinsics,
    NightHorizonConfig, PeakConfig,
};
use std::time::Instant;

fn main() {
    let path =
        std::path::Path::new("crates/bris-vision/tests/regression/night_test_highres/frame.png");
    let dims = image::image_dimensions(path).unwrap();
    let intr_pl = Intrinsics::placeholder(dims.0, dims.1);
    let frame = load_frame_from_path(path, Tt::from_julian_date(JD_J2000), 0, intr_pl).unwrap();

    let mut horizons = detect_horizon_night_multi_pass(&frame, NightHorizonConfig::default(), None);
    horizons.sort_by_key(|h| std::cmp::Reverse(h.inlier_count));
    let horizon = *horizons.first().expect("scene has a horizon");

    let peaks = detect_peaks_above_horizon(&frame, PeakConfig::default(), horizon, 5);
    eprintln!("peaks: {}", peaks.len());

    let t_db = Instant::now();
    let db = StarHashDb::build(StarHashDbConfig {
        mag_cutoff: 5.0,
        max_pattern_diameter_rad: 60.0_f64.to_radians(),
        bin_count: 50,
        neighbor_limit: 20,
    });
    eprintln!(
        "db build: {:?}, {} patterns",
        t_db.elapsed(),
        db.pattern_count()
    );

    let intrinsics = Intrinsics {
        fx: 300.0,
        fy: 300.0,
        cx: f64::from(dims.0) / 2.0,
        cy: f64::from(dims.1) / 2.0,
        ..intr_pl
    };

    // Warm + time three runs
    for run in 0..3 {
        let t = Instant::now();
        let res = plate_solve(
            &peaks,
            &intrinsics,
            &db,
            PlateSolveConfig {
                max_peaks_to_match: 12,
                min_verifications: 3,
                verify_match_radius_rad: 1.0_f64.to_radians(),
                max_rms_residual_rad: (60.0 / 3600.0_f64).to_radians(),
                max_tuple_diameter_rad: 60.0_f64.to_radians(),
                exhaustive_permutations: false,
            },
        );
        eprintln!(
            "run {run}: {:?}, result = {}",
            t.elapsed(),
            match res {
                Ok(r) => format!("Ok ({} stars)", r.identified.len()),
                Err(e) => format!("Err ({e})"),
            }
        );
    }
}
