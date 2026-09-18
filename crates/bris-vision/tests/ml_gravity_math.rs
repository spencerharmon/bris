//! Unit / integration tests for the ML-gravity provider.
//!
//! The tract-onnx end-to-end test path is exercised by the
//! corpus smoke harness in `tools/ml-gravity-smoke/`; here we
//! cover the conversion math, σ propagation, and provider-trait
//! plumbing without requiring the 30+ MB ONNX file.

#![cfg(feature = "ml-gravity")]

use bris_core::Sigma;
use bris_vision::horizon_providers::ml_gravity::gravity_from_roll_pitch;
use bris_vision::horizon_providers::ml_gravity::sigma::{
    altitude_sigma_at_ray, gravity_axis_sigmas,
};
use bris_vision::ray::{horizon_line_from_normal, CameraRay};
use bris_vision::{measure_altitude_from_ray, Intrinsics};
use std::f64::consts::PI;

fn approx(a: f64, b: f64, eps: f64) {
    assert!((a - b).abs() <= eps, "expected ~{b}, got {a} (tol {eps})");
}

#[test]
fn gravity_upright_is_image_down() {
    let g = gravity_from_roll_pitch(0.0, 0.0);
    approx(g.x, 0.0, 1e-12);
    approx(g.y, 1.0, 1e-12);
    approx(g.z, 0.0, 1e-12);
}

#[test]
fn gravity_rolled_quarter_turn_points_image_right() {
    let g = gravity_from_roll_pitch(PI / 2.0, 0.0);
    approx(g.x, 1.0, 1e-12);
    approx(g.y, 0.0, 1e-12);
    approx(g.z, 0.0, 1e-12);
}

#[test]
fn gravity_pitched_up_acquires_negative_z() {
    let g = gravity_from_roll_pitch(0.0, PI / 4.0);
    approx(g.x, 0.0, 1e-12);
    approx(g.y, (PI / 4.0).cos(), 1e-12);
    approx(g.z, -(PI / 4.0).sin(), 1e-12);
}

#[test]
fn gravity_upside_down() {
    let g = gravity_from_roll_pitch(PI, 0.0);
    approx(g.x, 0.0, 1e-12);
    approx(g.y, -1.0, 1e-12);
    approx(g.z, 0.0, 1e-12);
}

#[test]
fn sigma_jacobian_at_zero_orientation() {
    let (sx, sy, sz) = gravity_axis_sigmas(0.0, 0.0, 0.1, 0.05);
    approx(sx, 0.1, 1e-12);
    approx(sy, 0.0, 1e-12);
    approx(sz, 0.05, 1e-12);
}

#[test]
fn altitude_sigma_on_optical_axis() {
    let r = CameraRay {
        x: 0.0,
        y: 0.0,
        z: 1.0,
    };
    let g = CameraRay {
        x: 0.0,
        y: 1.0,
        z: 0.0,
    };
    let s = altitude_sigma_at_ray(&r, &g, 0.0, 0.0, 0.02);
    approx(s, 0.02, 1e-12);
}

#[test]
fn altitude_sigma_clamps_near_zenith() {
    // Body aligned with -g (zenith): cos(α) → 0; clamp at 0.05.
    let r = CameraRay {
        x: 0.0,
        y: -1.0,
        z: 0.0,
    };
    let g = CameraRay {
        x: 0.0,
        y: 1.0,
        z: 0.0,
    };
    let s = altitude_sigma_at_ray(&r, &g, 0.0, 0.01, 0.0);
    approx(s, 0.01 / 0.05, 1e-9);
}

#[test]
fn full_jacobian_propagation_45_degree_roll() {
    // Roll 45°, pitch 0. σ_roll = 0.1, σ_pitch = 0.0.
    // g = (sin45, cos45, 0) = (0.707, 0.707, 0).
    // ∂g_x/∂φ = cos45 · cos0 = 0.707
    // ∂g_y/∂φ = -sin45 · cos0 = -0.707
    // σ_gx = 0.707 · 0.1 = 0.0707
    // σ_gy = 0.707 · 0.1 = 0.0707
    let (sx, sy, sz) = gravity_axis_sigmas(PI / 4.0, 0.0, 0.1, 0.0);
    approx(sx, (PI / 4.0).cos() * 0.1, 1e-12);
    approx(sy, (PI / 4.0).sin() * 0.1, 1e-12);
    approx(sz, 0.0, 1e-12);
}

/// Regression for the "BelowHorizon fix-frame geometry
/// mismatch" bris ROI item: a **single fix-frame capture**
/// (no cross-frame stacking/averaging to smooth a bad
/// per-frame prediction) must recover the body's true altitude
/// from the ml-gravity-derived horizon without spuriously
/// clipping it to `BelowHorizon`, across the range of
/// handheld camera roll/pitch a real capture exercises.
///
/// End-to-end wires exactly what
/// `MlGravityProvider::detect_with_stats` and
/// `bris_streaming::pipeline::stage_e::reduce_to_sight` do in
/// production: (roll, pitch) → `g_cam`
/// (`gravity_from_roll_pitch`) → sky-normal horizon line
/// (`horizon_line_from_normal`) → observed altitude
/// (`measure_altitude_from_ray`, the same call Stage E makes
/// for a same-frame candidate). A body genuinely a comfortable
/// margin above the true horizon must measure as such; a body
/// genuinely below it must still be honestly rejected.
#[test]
fn ml_gravity_single_fix_frame_recovers_true_altitude_without_spurious_below_horizon() {
    let intr = Intrinsics::placeholder(1280, 720);
    let width = 1280;

    // Representative single-fix-frame handheld tilts: level,
    // rolled either way, pitched either way, and a combined
    // roll+pitch tilt — the geometry space a phone held up to
    // sight a body actually covers.
    let tilts_deg: &[(f64, f64)] = &[
        (0.0, 0.0),
        (12.0, 0.0),
        (-18.0, 0.0),
        (0.0, 10.0),
        (0.0, -8.0),
        (20.0, 12.0),
        (-25.0, -6.0),
    ];
    // Azimuths (radians, measured around the gravity axis)
    // sampled around the full circle so the test isn't
    // accidentally tuned to a single lucky direction.
    let azimuths_rad: &[f64] = &[0.0, 0.6, 1.8, 3.0, 4.4, 5.5];

    for &(roll_deg, pitch_deg) in tilts_deg {
        let roll = roll_deg.to_radians();
        let pitch = pitch_deg.to_radians();
        let g = gravity_from_roll_pitch(roll, pitch);
        let sky_normal = CameraRay {
            x: -g.x,
            y: -g.y,
            z: -g.z,
        };
        let altitude_sigma = Sigma::new(0.05).unwrap();
        let line = horizon_line_from_normal(&sky_normal, &intr, altitude_sigma)
            .expect("horizon line must exist for these moderate tilts");

        for &az in azimuths_rad {
            for true_alt_deg in [8.0_f64, 20.0, 45.0] {
                let body_ray = body_ray_at_altitude(&sky_normal, true_alt_deg.to_radians(), az);
                let body_sigma = Sigma::new(1.0e-4).unwrap();
                let measured = measure_altitude_from_ray(intr, width, line, body_ray, body_sigma)
                    .unwrap_or_else(|e| {
                        panic!(
                            "roll={roll_deg} pitch={pitch_deg} az={az} alt={true_alt_deg}: \
                                 expected a valid altitude, got spurious rejection {e:?}"
                        )
                    });
                approx(measured.value.to_degrees(), true_alt_deg, 1e-6);
            }

            // A body genuinely below the true horizon (by more
            // than the `BelowHorizon` tolerance) must still be
            // honestly rejected — the fix is about not
            // over-rejecting valid sights, not about disabling
            // the check.
            let below_ray = body_ray_at_altitude(&sky_normal, (-5.0_f64).to_radians(), az);
            let below_sigma = Sigma::new(1.0e-4).unwrap();
            let result = measure_altitude_from_ray(intr, width, line, below_ray, below_sigma);
            assert!(
                matches!(result, Err(bris_vision::MeasurementError::BelowHorizon)),
                "roll={roll_deg} pitch={pitch_deg} az={az}: expected BelowHorizon for a \
                 genuinely-below-horizon body, got {result:?}"
            );
        }
    }
}

/// Construct a unit camera-frame ray for a body at `altitude`
/// (radians above the local horizontal defined by
/// `sky_normal`) and `azimuth` (radians, arbitrary reference
/// around `sky_normal`).
fn body_ray_at_altitude(sky_normal: &CameraRay, altitude: f64, azimuth: f64) -> (f64, f64, f64) {
    let sn = normalize((sky_normal.x, sky_normal.y, sky_normal.z));
    // Arbitrary vector not parallel to `sn` to seed the
    // in-plane basis.
    let seed = if sn.0.abs() < 0.9 {
        (1.0, 0.0, 0.0)
    } else {
        (0.0, 1.0, 0.0)
    };
    let e1 = normalize(cross(sn, seed));
    let e2 = cross(sn, e1);
    let (sa, ca) = altitude.sin_cos();
    let (saz, caz) = azimuth.sin_cos();
    (
        sa * sn.0 + ca * (caz * e1.0 + saz * e2.0),
        sa * sn.1 + ca * (caz * e1.1 + saz * e2.1),
        sa * sn.2 + ca * (caz * e1.2 + saz * e2.2),
    )
}

fn cross(a: (f64, f64, f64), b: (f64, f64, f64)) -> (f64, f64, f64) {
    (
        a.1 * b.2 - a.2 * b.1,
        a.2 * b.0 - a.0 * b.2,
        a.0 * b.1 - a.1 * b.0,
    )
}

fn normalize(a: (f64, f64, f64)) -> (f64, f64, f64) {
    let n = (a.0 * a.0 + a.1 * a.1 + a.2 * a.2).sqrt();
    (a.0 / n, a.1 / n, a.2 / n)
}
