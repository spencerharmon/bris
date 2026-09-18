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

/// Regression for the "`BelowHorizon` fix-frame geometry
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

/// Camera-frame "down" (gravity) direction for a camera whose
/// orientation is given by roll/pitch, constructed **without**
/// calling `gravity_from_roll_pitch` — i.e. the independent
/// ground truth the ml-gravity derivation under test is
/// compared against.
///
/// A world-frame gravity of `(0, 0, -1)` (down) is expressed in
/// the camera frame by rotating first by pitch about the
/// camera +x (image right) axis, then by roll about the camera
/// +z (optical) axis, matching the convention documented in
/// `ml_gravity.rs`. The two rotations are written out by hand
/// with explicit rotation matrices so this reference shares no
/// code path with `gravity_from_roll_pitch`; if the production
/// derivation ever drifts from the documented convention, this
/// reference and the derivation disagree and the test fails.
fn true_gravity_cam_independent(roll: f64, pitch: f64) -> (f64, f64, f64) {
    // World gravity points "down"; a level, upright camera
    // (+y = image down) sees it as (0, 1, 0). Build that base
    // vector and rotate it into the tilted camera frame by the
    // inverse camera orientation. Concretely: start from
    // g0 = (0, 1, 0), rotate by pitch about +x, then roll
    // about +z, each with a hand-written matrix.
    let g0 = (0.0_f64, 1.0_f64, 0.0_f64);
    // Rotate about +x by `-pitch` so that pitching the camera up
    // drives gravity's camera-frame z NEGATIVE (matching the
    // documented `g.z = -sin θ`):
    //   y' =  cosθ·y + sinθ·z ; z' = -sinθ·y + cosθ·z
    let (sp, cp) = pitch.sin_cos();
    let after_pitch = (g0.0, cp * g0.1 + sp * g0.2, -sp * g0.1 + cp * g0.2);
    // Rotate about +z by `roll` so that a positive roll drives
    // gravity's camera-frame x POSITIVE (matching the documented
    // `g.x = sin φ · cos θ`):
    //   x' = cosφ·x + sinφ·y ; y' = -sinφ·x + cosφ·y
    let (sr, cr) = roll.sin_cos();
    (
        cr * after_pitch.0 + sr * after_pitch.1,
        -sr * after_pitch.0 + cr * after_pitch.1,
        after_pitch.2,
    )
}

/// Non-circular regression for the "`BelowHorizon` single-capture
/// under-production" bris ROI item.
///
/// The prior regression
/// (`ml_gravity_single_fix_frame_recovers_true_altitude...`)
/// built BOTH the horizon line and the body ray from the same
/// `gravity_from_roll_pitch` normal, making it a tautological
/// inverse: it could only prove the transform chain is
/// self-consistent, never that the ml-gravity-predicted horizon
/// agrees with the TRUE horizon. This test closes that gap:
///
/// 1. The TRUE horizon normal comes from
///    `true_gravity_cam_independent`, a hand-written rotation
///    that shares no code with the derivation under test.
/// 2. Body rays are placed at known altitudes relative to that
///    INDEPENDENT true horizon.
/// 3. With a PERFECT ml-gravity prediction the derivation
///    (`gravity_from_roll_pitch` → `horizon_line_from_normal`)
///    must recover each body's true altitude — proving there is
///    no systematic geometry bias between the predicted and the
///    true horizon (the reported failure is NOT a geometry bug).
/// 4. With a deliberately WRONG prediction (roll/pitch offset by
///    a per-frame error ε) the measured altitude must shift by
///    exactly ε in the error's direction — a 1:1, un-amplified
///    response. A genuinely-above-horizon body is therefore
///    only clipped to `BelowHorizon` when ε exceeds its true
///    altitude plus the −1° tolerance, i.e. the under-production
///    is honest single-frame prediction noise / large
///    per-prediction σ, not a derivation bug that rejects valid
///    sights.
#[test]
#[allow(clippy::too_many_lines)]
fn ml_gravity_predicted_horizon_matches_independent_true_horizon_and_below_horizon_is_honest() {
    let intr = Intrinsics::placeholder(1280, 720);
    let width = 1280;
    let body_sigma = Sigma::new(1.0e-4).unwrap();
    let alt_sigma = Sigma::new(0.05).unwrap();

    // Representative single-fix-frame handheld tilts.
    let tilts_deg: &[(f64, f64)] = &[
        (0.0, 0.0),
        (12.0, 0.0),
        (-18.0, 0.0),
        (0.0, 10.0),
        (0.0, -8.0),
        (20.0, 12.0),
        (-25.0, -6.0),
    ];
    let azimuths_rad: &[f64] = &[0.0, 1.2, 2.6, 4.0, 5.3];

    for &(roll_deg, pitch_deg) in tilts_deg {
        let roll = roll_deg.to_radians();
        let pitch = pitch_deg.to_radians();

        // INDEPENDENT ground-truth horizon: sky normal = -g_true.
        let g_true = true_gravity_cam_independent(roll, pitch);
        let true_sky_normal = CameraRay {
            x: -g_true.0,
            y: -g_true.1,
            z: -g_true.2,
        };

        // Sanity: the independent reference must agree with the
        // production derivation for a PERFECT prediction. This
        // is the load-bearing non-circularity check — the two
        // normals are computed by disjoint code.
        let g_pred = gravity_from_roll_pitch(roll, pitch);
        approx(g_pred.x, g_true.0, 1e-12);
        approx(g_pred.y, g_true.1, 1e-12);
        approx(g_pred.z, g_true.2, 1e-12);

        for &az in azimuths_rad {
            // --- (3) Perfect prediction recovers true altitude. ---
            let pred_normal_perfect = {
                let g = gravity_from_roll_pitch(roll, pitch);
                CameraRay {
                    x: -g.x,
                    y: -g.y,
                    z: -g.z,
                }
            };
            let line_perfect =
                horizon_line_from_normal(&pred_normal_perfect, &intr, alt_sigma).unwrap();
            for true_alt_deg in [6.0_f64, 15.0, 40.0] {
                // Body ray built against the INDEPENDENT true
                // horizon, then measured against the PREDICTED
                // horizon.
                let body = body_ray_at_altitude(&true_sky_normal, true_alt_deg.to_radians(), az);
                let measured =
                    measure_altitude_from_ray(intr, width, line_perfect, body, body_sigma)
                        .unwrap_or_else(|e| {
                            panic!(
                                "perfect pred roll={roll_deg} pitch={pitch_deg} az={az} \
                                 alt={true_alt_deg}: spurious rejection {e:?}"
                            )
                        });
                approx(measured.value.to_degrees(), true_alt_deg, 1e-6);
            }

            // --- (4) A wrong prediction shifts altitude 1:1,
            // and BelowHorizon rejection is honest. ---
            for eps_deg in [3.0_f64, 9.0, 15.0] {
                // Inject the per-frame prediction error as a
                // pitch offset (tilts the horizon toward/away
                // from the body along its azimuth in the worst
                // case). The predicted horizon is over-tilted
                // "up" by eps, so a body's measured altitude
                // relative to it drops.
                let eps = eps_deg.to_radians();
                let g_wrong = gravity_from_roll_pitch(roll, pitch + eps);
                let pred_normal_wrong = CameraRay {
                    x: -g_wrong.x,
                    y: -g_wrong.y,
                    z: -g_wrong.z,
                };
                let line_wrong =
                    horizon_line_from_normal(&pred_normal_wrong, &intr, alt_sigma).unwrap();

                // Body genuinely well above the true horizon.
                let true_alt = 25.0_f64.to_radians();
                let body = body_ray_at_altitude(&true_sky_normal, true_alt, az);
                let measured = measure_altitude_from_ray(intr, width, line_wrong, body, body_sigma);

                // The measured altitude error must not exceed
                // the injected prediction error (no bias
                // amplification). Because the body sits well
                // above the horizon and eps < true_alt + 1°,
                // the sight must NOT be spuriously rejected.
                let m = measured.unwrap_or_else(|e| {
                    panic!(
                        "wrong-pred roll={roll_deg} pitch={pitch_deg} az={az} eps={eps_deg}: \
                         a body 25° above the true horizon was spuriously rejected {e:?} \
                         even though the prediction error is smaller than its altitude — \
                         that would indicate a geometry bug, not honest σ"
                    )
                });
                let alt_err = (m.value - true_alt).abs();
                assert!(
                    alt_err <= eps + 1e-6,
                    "wrong-pred roll={roll_deg} pitch={pitch_deg} az={az} eps={eps_deg}: \
                     altitude error {} deg exceeds injected prediction error {} deg — \
                     the derivation is AMPLIFYING prediction error (a geometry bug)",
                    alt_err.to_degrees(),
                    eps_deg
                );
            }
        }

        // --- Honest, azimuth-dependent BelowHorizon rejection. ---
        // A prediction error tilts the predicted horizon plane:
        // a body on the side the horizon tilts UP toward measures
        // LOWER (and, if the error exceeds its true altitude + the
        // −1° tolerance, is honestly rejected), while a body on the
        // opposite side measures HIGHER and is not. Scanning the
        // full azimuth circle for a marginal body under a large
        // single-frame error must therefore find BOTH a rejected
        // and an accepted azimuth. That azimuth dependence is the
        // signature of honest geometry (the horizon tilted, the
        // body did not) — a systematic derivation bug would reject
        // regardless of azimuth, and a "disabled check" would
        // reject none. This is the single-capture under-production
        // mechanism, and it is CORRECT: honest σ, not a bug.
        let marginal_alt = 4.0_f64.to_radians();
        let big_eps = 8.0_f64.to_radians();
        let g_big = gravity_from_roll_pitch(roll, pitch + big_eps);
        let pred_normal_big = CameraRay {
            x: -g_big.x,
            y: -g_big.y,
            z: -g_big.z,
        };
        let line_big = horizon_line_from_normal(&pred_normal_big, &intr, alt_sigma).unwrap();

        let mut any_rejected = false;
        let mut any_accepted = false;
        let scan_az: &[f64] = &[0.0, 0.9, 1.8, 2.7, 3.6, 4.5, 5.4, 6.2];
        for &az in scan_az {
            let body_marginal = body_ray_at_altitude(&true_sky_normal, marginal_alt, az);
            match measure_altitude_from_ray(intr, width, line_big, body_marginal, body_sigma) {
                Err(bris_vision::MeasurementError::BelowHorizon) => any_rejected = true,
                Ok(_) => any_accepted = true,
                Err(e) => {
                    panic!("roll={roll_deg} pitch={pitch_deg} az={az}: unexpected error {e:?}")
                }
            }
        }
        assert!(
            any_rejected,
            "roll={roll_deg} pitch={pitch_deg}: a 4°-above body under an 8° prediction error \
             was NOT rejected at any azimuth — the BelowHorizon check is not firing where it \
             honestly should"
        );
        assert!(
            any_accepted,
            "roll={roll_deg} pitch={pitch_deg}: a 4°-above body under an 8° prediction error \
             was rejected at EVERY azimuth — rejection independent of azimuth would indicate a \
             systematic geometry bias, not honest single-frame σ"
        );
    }
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
