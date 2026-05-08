//! Generate a synthetic catalog projection PNG and run the
//! full vision + plate-solve pipeline on it.
//!
//! Bridges the gap between the existing in-memory synthetic
//! `round_trip` unit test and the real-frame regression cases:
//!   - synthetic `round_trip`: peak positions are computed and
//!     handed directly to `plate_solve`, no peak detector in
//!     the loop. Confirms the solver math.
//!   - this binary: catalog stars are projected to a PNG, the
//!     PNG is loaded and `detect_peaks` runs on it, then the
//!     detected peaks are handed to `plate_solve`. Confirms the
//!     full chain end-to-end on input where every "peak" is a
//!     real catalog star (no wake, no JPEG noise, no unknown fx).
//!   - real frames: `detect_peaks_above_horizon` then
//!     `plate_solve`. Failing today.
//!
//! If this binary succeeds, the chain works end-to-end and the
//! gap is in either (1) real-frame peak quality after horizon
//! masking or (2) the unknown camera intrinsics. If it fails,
//! we have a bug in the pipeline integration that the
//! `round_trip` test doesn't catch (most likely something at
//! the peak-detection / unit-conversion seam).

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::uninlined_format_args,
    clippy::doc_markdown
)]

use bris_almanac::all_stars;
use bris_core::time::{Tt, JD_J2000};
use bris_platesolve::{
    plate_solve, ra_dec_to_unit_vec, rotate_vec, PlateSolveConfig, StarHashDb, StarHashDbConfig,
};
use bris_vision::{
    detect_peaks, lens, load_frame_from_path, save_frame_as_png, Frame, Intrinsics, PeakConfig,
};

const W: u32 = 640;
const H: u32 = 480;
const FX: f64 = 800.0; // ~44° HFOV at 640 px

fn main() {
    let mag_cutoff: f64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4.0);
    // Render-side distortion (what the "camera" applies). Solve-
    // side intrinsics will use SOLVE_K1; if they differ, that
    // simulates "real camera has distortion we haven't
    // calibrated."
    let render_k1: f64 = std::env::var("RENDER_K1")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let solve_k1: f64 = std::env::var("SOLVE_K1")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    eprintln!(
        "render_k1 = {render_k1} (camera-side), solve_k1 = {solve_k1} (assumed in plate_solve)"
    );

    // Aim at the Big Dipper region (RA = 12h, Dec = +55°).
    // The catalog is dense here; an attitude that brings this
    // sky region to camera +Z gives many in-FOV stars.
    let aim_ra_rad = 12.0 * 15.0_f64.to_radians();
    let aim_dec_rad = 55.0_f64.to_radians();
    let aim_vec = ra_dec_to_unit_vec(aim_ra_rad, aim_dec_rad);
    let attitude = aim_to_z_attitude(aim_vec);
    eprintln!("aim: RA=180.0° Dec=+55.0°, fx={FX}, frame={W}x{H}");
    eprintln!("attitude (row-major): {attitude:?}");

    let intrinsics_render = Intrinsics {
        fx: FX,
        fy: FX,
        cx: f64::from(W) / 2.0,
        cy: f64::from(H) / 2.0,
        k1: render_k1,
        ..Intrinsics::placeholder(W, H)
    };
    let intrinsics_solve = Intrinsics {
        fx: FX,
        fy: FX,
        cx: f64::from(W) / 2.0,
        cy: f64::from(H) / 2.0,
        k1: solve_k1,
        ..Intrinsics::placeholder(W, H)
    };

    // Project all catalog stars (mag <= cutoff) through the
    // attitude. Render visible ones as Gaussian blobs into a
    // u16 frame. Background is dark (10 / 65535 luma).
    let bg = 800_u16;
    let mut pixels = vec![bg; (W * H) as usize];
    let stars = all_stars();
    let mut projected: Vec<(f64, f64, f64)> = Vec::new(); // (px, py, vmag)
    for s in stars {
        if s.vmag > mag_cutoff {
            continue;
        }
        let cv = ra_dec_to_unit_vec(s.ra_rad, s.dec_rad);
        let r = rotate_vec(&attitude, cv);
        if r[2] <= 0.0 {
            continue;
        }
        // Normalized image-plane coordinate (pinhole projection)
        // BEFORE distortion.
        let xn = r[0] / r[2];
        let yn = r[1] / r[2];
        // Apply the camera-side distortion model.
        let (xd, yd) = lens::distort_normalized(intrinsics_render, xn, yn);
        let (px, py) = lens::project_pinhole(intrinsics_render, xd, yd);
        if !(0.0..f64::from(W)).contains(&px) || !(0.0..f64::from(H)).contains(&py) {
            continue;
        }
        projected.push((px, py, s.vmag));
        // Render: 5×5 Gaussian, intensity scaled to magnitude.
        // Brightest visible star (mag 0) → ~50000 counts, each
        // mag step ÷ 2.5. Stars below mag 6 will be near
        // background.
        let peak_intensity = 50000.0 * (10.0_f64).powf(-0.4 * s.vmag);
        let sigma = 1.2_f64;
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                let x = px.round() as i32 + dx;
                let y = py.round() as i32 + dy;
                if x < 0 || y < 0 || x >= W as i32 || y >= H as i32 {
                    continue;
                }
                let r2 = (px - x as f64).powi(2) + (py - y as f64).powi(2);
                let g = (-r2 / (2.0 * sigma * sigma)).exp();
                let v = (peak_intensity * g) as u32;
                let idx = (y as usize) * (W as usize) + (x as usize);
                pixels[idx] = pixels[idx].saturating_add(v.min(u16::MAX as u32) as u16);
            }
        }
    }
    eprintln!(
        "rendered {} in-frame stars (mag ≤ {mag_cutoff})",
        projected.len()
    );

    let frame = Frame::new(
        W,
        H,
        pixels,
        Tt::from_julian_date(JD_J2000),
        1000,
        intrinsics_render,
    )
    .expect("frame ctor");

    let out_path = "/tmp/synth_starfield.png";
    save_frame_as_png(&frame, out_path).expect("save");
    eprintln!("saved {out_path}");

    // Reload from disk with the SOLVE-side intrinsics (the
    // ones plate_solve will actually use). This is the analog
    // of "we open a real PNG and we have only an
    // approximation of the lens model."
    let reloaded = load_frame_from_path(
        std::path::Path::new(out_path),
        Tt::from_julian_date(JD_J2000),
        0,
        intrinsics_solve,
    )
    .expect("reload");
    eprintln!("reloaded; {}x{}", reloaded.width(), reloaded.height());

    let peaks = detect_peaks(&reloaded, PeakConfig::default());
    eprintln!("peaks detected: {}", peaks.len());
    eprintln!("top 8 peaks (intensity, x, y):");
    for (i, p) in peaks.iter().take(8).enumerate() {
        eprintln!("  {i:2}: {:8.0} ({:6.1}, {:6.1})", p.intensity, p.x, p.y);
    }

    eprintln!("\nbuilding hash db (mag <= {mag_cutoff})...");
    let db = StarHashDb::build(StarHashDbConfig {
        mag_cutoff,
        max_pattern_diameter_rad: 60.0_f64.to_radians(),
        bin_count: 50,
        neighbor_limit: 20,
    });
    eprintln!("done: {} patterns", db.pattern_count());

    let solve_cfg = PlateSolveConfig {
        max_peaks_to_match: 12,
        min_verifications: 3,
        verify_match_radius_rad: 1.0_f64.to_radians(),
        max_rms_residual_rad: (60.0 / 3600.0_f64).to_radians(),
        max_tuple_diameter_rad: 60.0_f64.to_radians(),
        exhaustive_permutations: false,
    };

    eprintln!("\nsolving...");
    let t = std::time::Instant::now();
    match plate_solve(&peaks, &intrinsics_solve, &db, solve_cfg) {
        Ok(r) => {
            eprintln!(
                "MATCHED in {:?} with {} stars",
                t.elapsed(),
                r.identified.len()
            );
            // Recover aim point: the recovered attitude should
            // map aim_vec near +Z.
            let recovered_aim = rotate_vec(&r.attitude.matrix, aim_vec);
            eprintln!(
                "aim_vec recovered → {:?} (z component should be ≈ 1)",
                recovered_aim
            );
            eprintln!("first 8 identified stars:");
            for s in r.identified.iter().take(8) {
                eprintln!(
                    "  hr={} mag={:.2} pixel=({:.1}, {:.1})",
                    s.hr, s.vmag, s.pixel_x, s.pixel_y
                );
            }
        }
        Err(e) => {
            eprintln!("FAILED in {:?}: {e}", t.elapsed());
        }
    }
}

/// Build an attitude (rotation matrix, row-major) that maps
/// the given unit vector to +Z (camera optical axis).
fn aim_to_z_attitude(aim: [f64; 3]) -> [f64; 9] {
    let z = [0.0, 0.0, 1.0];
    let axis = [
        aim[1] * z[2] - aim[2] * z[1],
        aim[2] * z[0] - aim[0] * z[2],
        aim[0] * z[1] - aim[1] * z[0],
    ];
    let axis_norm = (axis[0].powi(2) + axis[1].powi(2) + axis[2].powi(2)).sqrt();
    if axis_norm < 1e-12 {
        if aim[2] > 0.0 {
            return [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        }
        return [1.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, -1.0];
    }
    let axis = [
        axis[0] / axis_norm,
        axis[1] / axis_norm,
        axis[2] / axis_norm,
    ];
    let angle = aim[2].clamp(-1.0, 1.0).acos();
    let (s, c) = (angle.sin(), angle.cos());
    let one_minus_c = 1.0 - c;
    let (x, y, zz) = (axis[0], axis[1], axis[2]);
    [
        c + x * x * one_minus_c,
        x * y * one_minus_c - zz * s,
        x * zz * one_minus_c + y * s,
        y * x * one_minus_c + zz * s,
        c + y * y * one_minus_c,
        y * zz * one_minus_c - x * s,
        zz * x * one_minus_c - y * s,
        zz * y * one_minus_c + x * s,
        c + zz * zz * one_minus_c,
    ]
}
