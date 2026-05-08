//! One-off probe: try a handful of fx values on the plate-solve
//! regression cases to find intrinsics that produce a stable
//! match. Bail at the first successful configuration per case
//! to keep total runtime manageable (each `plate_solve` call is
//! ~30s in release on these scenes).

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::uninlined_format_args,
    clippy::single_match_else,
    clippy::match_wildcard_for_single_variants,
    clippy::needless_pass_by_value,
    clippy::doc_markdown
)]

use bris_core::time::{Tt, JD_J2000};
use bris_platesolve::{plate_solve, PlateSolveConfig, StarHashDb, StarHashDbConfig};
use bris_vision::{
    detect_horizon_night_multi_pass, detect_peaks, detect_peaks_above_horizon,
    load_frame_from_path, Intrinsics, NightHorizonConfig, PeakConfig,
};

fn main() {
    let case = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "night_test_highres".to_string());
    // Allow overriding the frame path (so we can test a higher-
    // resolution capture without restructuring the corpus
    // layout).
    let path_str = std::env::var("FRAME_PATH")
        .unwrap_or_else(|_| format!("crates/bris-vision/tests/regression/{case}/frame.png"));
    let path = std::path::Path::new(&path_str);
    eprintln!("frame: {path_str}");

    let peaks_only = std::env::var("PEAKS_ONLY").is_ok();

    let mag_cutoff: f64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5.0);
    let db = if peaks_only {
        eprintln!("PEAKS_ONLY set: skipping hash db build");
        None
    } else {
        let cfg = StarHashDbConfig {
            mag_cutoff,
            max_pattern_diameter_rad: 60.0_f64.to_radians(),
            bin_count: 50,
            neighbor_limit: 20,
        };
        eprintln!("Building hash db (mag <= {mag_cutoff})...");
        let db = StarHashDb::build(cfg);
        eprintln!("Done: {} patterns", db.pattern_count());
        Some(db)
    };

    let dims = image::image_dimensions(path).unwrap();
    eprintln!("\n=== {} ({}x{}) ===", case, dims.0, dims.1);
    let placeholder = Intrinsics::placeholder(dims.0, dims.1);

    let frame = load_frame_from_path(path, Tt::from_julian_date(JD_J2000), 0, placeholder).unwrap();

    // Multi-pass night-horizon detector. Returns candidates
    // sorted by inlier count; the top one is the actual sea
    // horizon for this corpus (pre-recorded in case.toml).
    let horizon_candidates =
        detect_horizon_night_multi_pass(&frame, NightHorizonConfig::default(), None);
    // Sort by inlier count desc; the first is the actual sea
    // horizon for this corpus (pre-recorded in case.toml).
    let mut sorted = horizon_candidates;
    sorted.sort_by_key(|h| std::cmp::Reverse(h.inlier_count));
    let horizon = sorted.first().copied();
    if let Some(h) = horizon {
        eprintln!(
            "  horizon: slope={:.4} intercept={:.2} inliers={}",
            h.slope, h.intercept, h.inlier_count
        );
    } else {
        eprintln!("  horizon: NONE (running peak detection unmasked)");
    }

    let peak_cfg = PeakConfig {
        min_intensity: std::env::var("MIN_INTENSITY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2000),
        ..PeakConfig::default()
    };
    let peaks = match horizon {
        Some(h) => detect_peaks_above_horizon(&frame, peak_cfg, h, 5),
        None => detect_peaks(&frame, peak_cfg),
    };
    eprintln!("  {} peaks", peaks.len());
    eprintln!("  top 12 peaks (intensity, x, y):");
    for (i, p) in peaks.iter().take(12).enumerate() {
        eprintln!("    {i:2}: {:8.0}  ({:6.1}, {:6.1})", p.intensity, p.x, p.y);
    }
    if peaks.len() >= 20 {
        eprintln!("  intensity at idx 20: {:.0}", peaks[19].intensity);
    }
    if peaks.len() >= 50 {
        eprintln!("  intensity at idx 50: {:.0}", peaks[49].intensity);
    }
    if peaks.len() >= 100 {
        eprintln!("  intensity at idx 100: {:.0}", peaks[99].intensity);
    }

    if peaks_only {
        eprintln!("PEAKS_ONLY set: skipping plate-solve sweep");
        return;
    }
    let db = db.expect("db is Some when not PEAKS_ONLY");

    // Try fx values from very-wide fisheye to mild telephoto.
    // For a 640px-wide frame: 200-290 ≈ GoPro wide; 600-800 ≈
    // smartphone/normal. Multiply ranges by 3 for 1920px-wide,
    // 2 for 1280px-wide, etc. A `single_fx` arg overrides.
    let single_fx: Option<f64> = std::env::args().nth(3).and_then(|s| s.parse().ok());
    let fx_values: Vec<f64> = match single_fx {
        Some(f) => vec![f],
        None => {
            // Adapt the sweep to the frame's actual width so a
            // single sweep covers fisheye-to-mild-telephoto on
            // any input resolution. The factors are calibrated
            // for 640 px.
            let scale = f64::from(dims.0) / 640.0;
            [
                150.0, 200.0, 250.0, 300.0, 400.0, 500.0, 700.0, 1000.0, 1500.0, 2000.0, 3000.0,
            ]
            .iter()
            .map(|&v| v * scale)
            .collect()
        }
    };
    let exhaustive = std::env::var("ALL_PERMS").is_ok();
    if exhaustive {
        eprintln!("  ALL_PERMS set: trying all 24 perms (slower fallback)");
    }
    eprintln!(
        "  sweeping {} fx value(s){}",
        fx_values.len(),
        if exhaustive {
            " (~12× slower per fx with ALL_PERMS)"
        } else {
            ""
        }
    );

    for &f in &fx_values {
        let intrinsics = Intrinsics {
            fx: f,
            fy: f,
            cx: f64::from(dims.0) / 2.0,
            cy: f64::from(dims.1) / 2.0,
            ..placeholder
        };
        let solve_cfg = PlateSolveConfig {
            max_peaks_to_match: 12,
            min_verifications: 3,
            verify_match_radius_rad: 1.5_f64.to_radians(),
            max_rms_residual_rad: (60.0_f64 / 3600.0).to_radians(),
            max_tuple_diameter_rad: 60.0_f64.to_radians(),
            exhaustive_permutations: exhaustive,
        };
        eprintln!(
            "  trying fx={f} ({:.0}° HFOV)...",
            2.0 * (f64::from(dims.0) / (2.0 * f)).atan().to_degrees()
        );
        match plate_solve(&peaks, &intrinsics, &db, solve_cfg) {
            Ok(r) => {
                eprintln!("  → MATCHED with {} stars", r.identified.len(),);
                for s in r.identified.iter().take(5) {
                    eprintln!(
                        "      hr={} ra={:.3} dec={:.3} mag={:.2} pixel=({:.1}, {:.1})",
                        s.hr,
                        s.ra_rad.to_degrees(),
                        s.dec_rad.to_degrees(),
                        s.vmag,
                        s.pixel_x,
                        s.pixel_y
                    );
                }
                eprintln!(
                    "  → matrix: [{:.4} {:.4} {:.4}; {:.4} {:.4} {:.4}; {:.4} {:.4} {:.4}]",
                    r.attitude.matrix[0],
                    r.attitude.matrix[1],
                    r.attitude.matrix[2],
                    r.attitude.matrix[3],
                    r.attitude.matrix[4],
                    r.attitude.matrix[5],
                    r.attitude.matrix[6],
                    r.attitude.matrix[7],
                    r.attitude.matrix[8],
                );
                return;
            }
            Err(e) => eprintln!("  → no match: {e}"),
        }
    }
    eprintln!("\nNo intrinsics in the swept range produced a match for {case}.");
}
