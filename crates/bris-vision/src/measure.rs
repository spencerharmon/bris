//! Angle measurement: convert a body centroid + horizon line + camera
//! intrinsics into a measured altitude.
//!
//! This is the bridge from pixel-space vision to sky-space astronomy.
//! Given:
//! - A horizon line in image coordinates (from [`crate::horizon::detect_horizon`]).
//! - A body centroid in image coordinates (from
//!   [`crate::centroid::centroid_brightest_body`] or, eventually, plate
//!   solving).
//! - The camera intrinsics with which the frame was captured.
//!
//! Compute the angle between the body's ray direction and the
//! horizon plane the camera sees. This is the celestial *apparent
//! altitude* (Hs in navigation notation), modulo the corrections
//! the apparent-place pipeline subsequently applies (refraction,
//! horizon dip, etc., which are all already factored into the
//! observer/refraction modules in `bris-almanac`).
//!
//! # Algorithm
//!
//! 1. Pick two horizon points and convert each pixel → ray direction
//!    (undistortion + pinhole inverse). Cross-product gives a vector
//!    *normal to the horizon plane in camera coordinates*.
//! 2. Convert the body-centroid pixel → ray direction the same way.
//! 3. Altitude = π/2 − angle(`body_ray`, `horizon_normal`). When the body
//!    is *above* the horizon plane (the normal points "up"), this is
//!    the body's elevation above the visible horizon.
//!
//! # Uncertainty
//!
//! Combine in quadrature:
//! - Horizon altitude σ from the line fit (already provided).
//! - Centroid position σ converted to angular σ via the camera's
//!   instantaneous angular resolution (1 / fy radians per vertical
//!   pixel, valid for non-extreme FOVs).

use crate::centroid::Centroid;
use crate::frame::Intrinsics;
use crate::horizon::HorizonLine;
use crate::lens::pixel_ray_direction;
use bris_core::{Sigma, Uncertain};

/// Errors from angle measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MeasurementError {
    /// The body's computed altitude was below the horizon (negative
    /// or wraps unphysically). Caller should reject the sight.
    #[error("computed altitude is below the horizon")]
    BelowHorizon,
    /// Internal arithmetic produced a non-finite result.
    #[error("non-finite arithmetic in altitude computation")]
    NonFinite,
}

/// Compute the apparent altitude of a body from its centroid in a frame
/// where the horizon has been detected.
///
/// Returns the altitude (radians, positive above horizon) paired with
/// a 1σ uncertainty.
///
/// # Errors
///
/// See [`MeasurementError`].
pub fn measure_altitude(
    intr: Intrinsics,
    horizon: HorizonLine,
    body: Centroid,
) -> Result<Uncertain<f64>, MeasurementError> {
    // Convert body pixel to camera-frame ray.
    let body_ray = pixel_ray_direction(intr, body.x, body.y);
    let body_vec = (body_ray.0, body_ray.1, body_ray.2);

    // Pick two well-separated points on the horizon line.
    // The line is y = slope·x + intercept in pixel coords. Use x=0
    // and x = (image_width estimate). We don't actually know the
    // image width here, but slope is small (camera roughly level)
    // so x = 1000 is fine; the cross product is independent of
    // distance.
    let p1_x = 0.0;
    let p1_y = horizon.intercept;
    let p2_x = 1000.0;
    let p2_y = horizon.slope * p2_x + horizon.intercept;
    let p1_ray = pixel_ray_direction(intr, p1_x, p1_y);
    let p2_ray = pixel_ray_direction(intr, p2_x, p2_y);
    let p1_vec = (p1_ray.0, p1_ray.1, p1_ray.2);
    let p2_vec = (p2_ray.0, p2_ray.1, p2_ray.2);

    // Normal to the horizon plane: cross product. The sign convention
    // is chosen so the normal points toward image-y < horizon (i.e.
    // upward in the image, which corresponds to "up" in the world for
    // a roughly level camera).
    let normal = cross(p1_vec, p2_vec);
    let normal_norm = norm(normal);
    if !normal_norm.is_finite() || normal_norm < 1e-12 {
        return Err(MeasurementError::NonFinite);
    }
    let mut normal = (
        normal.0 / normal_norm,
        normal.1 / normal_norm,
        normal.2 / normal_norm,
    );

    // The cross product's sign depends on point order. We need the
    // normal pointing "up" (away from where the body should be when
    // above the horizon). For the camera convention (image +y down),
    // p1.x < p2.x, the cross product points in +y if both rays are
    // near +z. Flip if the resulting normal has negative y in image
    // space — equivalently, ensure the normal's "up" direction points
    // away from the body when the body is above horizon.
    //
    // Heuristic: sample a point well above the horizon line in image
    // coords (smaller y) and verify dot(normal, sample_ray) > 0.
    let sample_y = horizon.intercept - 100.0;
    let sample_ray = pixel_ray_direction(intr, 500.0, sample_y);
    let sample_vec = (sample_ray.0, sample_ray.1, sample_ray.2);
    if dot(normal, sample_vec) < 0.0 {
        normal = (-normal.0, -normal.1, -normal.2);
    }

    // Altitude = π/2 − angle(body, normal) when normal is the "up"
    // direction. Equivalently, altitude = arcsin(dot(body, normal)).
    let cos_complement = dot(body_vec, normal).clamp(-1.0, 1.0);
    let altitude = cos_complement.asin();

    if !altitude.is_finite() {
        return Err(MeasurementError::NonFinite);
    }
    if altitude < -1.0_f64.to_radians() {
        return Err(MeasurementError::BelowHorizon);
    }

    // Uncertainty: horizon altitude σ + centroid σ converted to angular σ.
    let centroid_sigma_rad = body.position_sigma_px.value() / intr.fy;
    let centroid_sigma = Sigma::new(centroid_sigma_rad).unwrap_or(Sigma::ZERO);
    let total_sigma = horizon.altitude_sigma.combine(centroid_sigma);

    Ok(Uncertain::new(altitude, total_sigma))
}

#[inline]
fn cross(a: (f64, f64, f64), b: (f64, f64, f64)) -> (f64, f64, f64) {
    (
        a.1 * b.2 - a.2 * b.1,
        a.2 * b.0 - a.0 * b.2,
        a.0 * b.1 - a.1 * b.0,
    )
}

#[inline]
fn dot(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    a.0 * b.0 + a.1 * b.1 + a.2 * b.2
}

#[inline]
fn norm(a: (f64, f64, f64)) -> f64 {
    (a.0 * a.0 + a.1 * a.1 + a.2 * a.2).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::centroid::CentroidConfig;
    use crate::frame::{Frame, Intrinsics};
    use crate::horizon::HorizonConfig;
    use approx::assert_relative_eq;
    use bris_core::time::{Tt, JD_J2000};

    /// Build a frame with a horizontal horizon at the given row and a
    /// bright disk at the given (cx, cy).
    #[allow(clippy::similar_names)] // body_cx, body_cy are domain-standard
    fn synth_frame_with_horizon_and_body(
        width: u32,
        height: u32,
        horizon_y: u32,
        body_cx: f64,
        body_cy: f64,
        body_radius: f64,
    ) -> Frame {
        let mut pixels = vec![0u16; (width as usize) * (height as usize)];
        for y in 0..height {
            for x in 0..width {
                let v = if y < horizon_y { 50_000 } else { 5_000 };
                pixels[(y as usize) * (width as usize) + (x as usize)] = v;
            }
        }
        // Bright disk (saturated).
        for y in 0..height {
            for x in 0..width {
                let dx = f64::from(x) - body_cx;
                let dy = f64::from(y) - body_cy;
                if dx * dx + dy * dy <= body_radius * body_radius {
                    pixels[(y as usize) * (width as usize) + (x as usize)] = 65_000;
                }
            }
        }
        Frame::new(
            width,
            height,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    #[test]
    fn body_directly_above_horizon_at_known_altitude() {
        // Camera with fy=1000, horizon at y=400, body centroid at y=200
        // (200 px above horizon). Expected altitude ≈ 200 / 1000 rad
        // ≈ 0.2 rad ≈ 11.5°.
        let frame = synth_frame_with_horizon_and_body(800, 600, 400, 400.0, 200.0, 25.0);
        let horizon = crate::detect_horizon(&frame, HorizonConfig::default()).unwrap();
        let centroid = crate::centroid_brightest_body(&frame, CentroidConfig::default()).unwrap();
        let altitude = measure_altitude(frame.intrinsics, horizon, centroid).unwrap();
        let alt_deg = altitude.value.to_degrees();
        // Expected ≈ atan(200/1000) ≈ 11.31°. Tolerance accommodates
        // the synthetic horizon detection precision.
        assert_relative_eq!(alt_deg, 11.31, epsilon = 0.5);
    }

    #[test]
    fn body_at_horizon_has_zero_altitude() {
        // Body centered ON the horizon. Altitude should be ~0.
        let frame = synth_frame_with_horizon_and_body(800, 600, 400, 400.0, 400.0, 25.0);
        let horizon = crate::detect_horizon(&frame, HorizonConfig::default()).unwrap();
        let centroid = crate::centroid_brightest_body(&frame, CentroidConfig::default()).unwrap();
        let altitude = measure_altitude(frame.intrinsics, horizon, centroid).unwrap();
        let alt_deg = altitude.value.to_degrees();
        // Body blob occludes horizon → centroid+horizon will land near
        // each other but not exactly. Tolerance ~1°.
        assert!(alt_deg.abs() < 2.0, "alt = {alt_deg}° should be near 0");
    }

    #[test]
    fn altitude_uncertainty_is_finite_and_positive() {
        let frame = synth_frame_with_horizon_and_body(800, 600, 400, 400.0, 200.0, 25.0);
        let horizon = crate::detect_horizon(&frame, HorizonConfig::default()).unwrap();
        let centroid = crate::centroid_brightest_body(&frame, CentroidConfig::default()).unwrap();
        let altitude = measure_altitude(frame.intrinsics, horizon, centroid).unwrap();
        assert!(altitude.sigma.value().is_finite());
        assert!(altitude.sigma.value() > 0.0);
        // For a synthetic noise-free frame the σ should be well below
        // 1° (no atmospheric / refraction effects in this stage).
        assert!(altitude.sigma.value().to_degrees() < 1.0);
    }
}
