//! Integration: Stage E ephemeris-driven cross-frame stitch
//! fallback.
//!
//! Two frames 60 s apart. At J2000 the Sun is up over
//! Greenwich (the `Observer::default_dev` location) and
//! drifts at sidereal rate ~15"/s; in 60 s that's ~4-5 px
//! on the placeholder intrinsics (fx = fy = 1000). The body
//! is placed at the ephemeris-predicted location in each
//! frame, so the actual inter-frame translation matches the
//! prediction.
//!
//! Harris+NCC's `track_rotation` fits a pure 3-D rotation
//! and rejects when the per-correspondence angular residual
//! exceeds `TrackConfig::default().ransac_inlier_rad =
//! 0.003 rad` (~3 px at the placeholder focal length). A
//! 4-5 px pure translation produces residuals above that
//! gate, so the stitcher returns `TrackingFailed` and the
//! ephemeris fallback runs.
//!
//! Asserts (positive path, fallback enabled):
//!
//! - `ephemeris_stitch_attempted >= 1`
//! - `ephemeris_stitch_succeeded >= 1`
//! - `cross_frame_sights_emitted >= 1`
//!
//! Asserts (negative control, fallback disabled):
//!
//! - `ephemeris_stitch_attempted == 0`
//! - `cross_frame_sights_emitted == 0`

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]

use bris_almanac::{body_apparent_place, Observer, SolarSystemBody};
use bris_core::time::{Tt, JD_J2000};
use bris_streaming::{EngineConfig, StoreConfig, StreamingEngine};
use bris_vision::{Frame, Intrinsics};
use tempfile::TempDir;

const W: u32 = 320;
const H: u32 = 240;
const BODY_RADIUS_PX: f64 = 5.0;

/// Frame with a saturated body disk over a uniform sky.
/// The disk-edge gradient ring is too sparse to fit a
/// horizontal line via the gradient horizon RANSAC; no
/// horizon record is produced.
fn body_only_frame(tt: Tt, body_x: f64, body_y: f64) -> Frame {
    let mut pixels = vec![45_000u16; (W * H) as usize];
    paint_disk(&mut pixels, body_x, body_y);
    Frame::new(W, H, pixels, tt, 1000, Intrinsics::placeholder(W, H)).unwrap()
}

/// Frame with the body + a strong horizon (bright sky over
/// dark sea). Produces both a body record and a horizon
/// record.
fn body_and_horizon_frame(tt: Tt, body_x: f64, body_y: f64) -> Frame {
    let mut pixels = vec![0u16; (W * H) as usize];
    for (y, row) in pixels.chunks_mut(W as usize).enumerate() {
        let v = if y < 160 { 45_000 } else { 5_000 };
        for px in row.iter_mut() {
            *px = v;
        }
    }
    paint_disk(&mut pixels, body_x, body_y);
    Frame::new(W, H, pixels, tt, 1000, Intrinsics::placeholder(W, H)).unwrap()
}

fn paint_disk(pixels: &mut [u16], body_x: f64, body_y: f64) {
    let r2 = BODY_RADIUS_PX * BODY_RADIUS_PX;
    for y in 0..H {
        for x in 0..W {
            let dx = f64::from(x) - body_x;
            let dy = f64::from(y) - body_y;
            if dx.mul_add(dx, dy * dy) <= r2 {
                pixels[(y as usize) * (W as usize) + (x as usize)] = u16::MAX;
            }
        }
    }
}

fn cfg_in(dir: &std::path::Path, fallback_enabled: bool) -> EngineConfig {
    let mut cfg = EngineConfig::new(Observer::default_dev());
    cfg.min_fix_publication_interval_ms = 0;
    cfg.enable_ephemeris_stitch_fallback = fallback_enabled;
    // 60 s gap between the two test frames; bump the
    // stitching window so both stay in the ring buffer.
    cfg.stitching_window_seconds = 120.0;
    cfg.store = StoreConfig {
        data_root: dir.to_path_buf(),
        retention_days: 7,
        rotation_size_bytes: 8 * 1024 * 1024,
        enabled: false,
    };
    cfg
}

/// Compute the Sun's predicted pixel displacement between
/// `t0` and `t1` at the default-dev observer on placeholder
/// intrinsics. Mirrors the math inside
/// `ephemeris_stitch::predict_body_pixel_motion`: per-axis
/// projection of (Δalt, Δaz·cos(mean_alt)) through fx/fy.
fn sun_predicted_pixel_delta(t0: Tt, t1: Tt, intrinsics: &Intrinsics) -> (f64, f64) {
    let observer = Observer::default_dev();
    let p0 = body_apparent_place(SolarSystemBody::Sun, t0, t0.julian_date(), observer)
        .expect("Sun above horizon at J2000 Greenwich");
    let p1 = body_apparent_place(SolarSystemBody::Sun, t1, t1.julian_date(), observer)
        .expect("Sun above horizon at J2000 + gap");
    let d_alt = p1.direction.altitude - p0.direction.altitude;
    let d_az_raw = p1.direction.azimuth - p0.direction.azimuth;
    let d_az = ((d_az_raw + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU))
        - std::f64::consts::PI;
    let mean_alt = 0.5 * (p0.direction.altitude + p1.direction.altitude);
    let d_az_eff = d_az * mean_alt.cos();
    (d_az_eff * intrinsics.fx, -d_alt * intrinsics.fy)
}

const GAP_S: f64 = 60.0;

fn run_engine(fallback_enabled: bool) -> bris_streaming::EngineDiagnostics {
    let dir = TempDir::new().unwrap();
    let engine = StreamingEngine::new(cfg_in(dir.path(), fallback_enabled));
    let t0 = Tt::from_julian_date(JD_J2000);
    let t1 = Tt::from_julian_date(JD_J2000 + GAP_S / 86_400.0);
    let intr = Intrinsics::placeholder(W, H);
    let (dx, dy) = sun_predicted_pixel_delta(t0, t1, &intr);
    let body_a_x = 160.0_f64;
    let body_a_y = 80.0_f64;
    engine
        .push_frame(body_and_horizon_frame(t0, body_a_x, body_a_y))
        .unwrap();
    engine
        .push_frame(body_only_frame(t1, body_a_x + dx, body_a_y + dy))
        .unwrap();
    let diag = engine.diagnostics();
    drop(engine);
    drop(dir);
    diag
}

#[test]
fn ephemeris_fallback_accepts_when_harris_ncc_fails() {
    let intr = Intrinsics::placeholder(W, H);
    let t0 = Tt::from_julian_date(JD_J2000);
    let t1 = Tt::from_julian_date(JD_J2000 + GAP_S / 86_400.0);
    let (dx, dy) = sun_predicted_pixel_delta(t0, t1, &intr);
    let displacement_px = dx.hypot(dy);
    assert!(
        displacement_px > 3.0,
        "test prerequisite: predicted Sun motion {displacement_px} px must exceed \
         the Kabsch residual threshold (~3 px at fx=1000) so Harris+NCC declines"
    );

    let diag = run_engine(true);
    assert!(
        diag.ephemeris_stitch_attempted >= 1,
        "ephemeris fallback should have been attempted; diag = {diag:?}"
    );
    assert!(
        diag.ephemeris_stitch_succeeded >= 1,
        "ephemeris fallback should have accepted the predicted correspondence; \
         attempted={}, succeeded={}, no_candidate={}",
        diag.ephemeris_stitch_attempted,
        diag.ephemeris_stitch_succeeded,
        diag.ephemeris_stitch_no_candidate_in_window,
    );
    assert!(
        diag.cross_frame_sights_emitted >= 1,
        "cross-frame sight counter must include the ephemeris-accepted sight"
    );
}

#[test]
fn ephemeris_fallback_disabled_emits_no_cross_frame_sight() {
    let diag = run_engine(false);
    assert_eq!(
        diag.ephemeris_stitch_attempted, 0,
        "fallback disabled: no attempts should be recorded"
    );
    assert_eq!(
        diag.ephemeris_stitch_succeeded, 0,
        "fallback disabled: no successes should be recorded"
    );
    assert_eq!(
        diag.cross_frame_sights_emitted, 0,
        "fallback disabled + Harris+NCC fails: no cross-frame sight"
    );
}
