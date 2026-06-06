//! Unit / integration tests for the ML-gravity provider.
//!
//! The tract-onnx end-to-end test path is exercised by the
//! corpus smoke harness in `tools/ml-gravity-smoke/`; here we
//! cover the conversion math, σ propagation, and provider-trait
//! plumbing without requiring the 30+ MB ONNX file.

#![cfg(feature = "ml-gravity")]

use bris_vision::horizon_providers::ml_gravity::gravity_from_roll_pitch;
use bris_vision::horizon_providers::ml_gravity::sigma::{
    altitude_sigma_at_ray, gravity_axis_sigmas,
};
use bris_vision::ray::CameraRay;
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
