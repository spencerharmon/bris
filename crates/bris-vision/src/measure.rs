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
    /// The body's centroid σ was non-finite (NaN or ±∞). Per the
    /// honest-uncertainty rule we refuse to fabricate a σ default;
    /// the caller must produce a finite σ or reject the sight.
    #[error("non-finite centroid sigma — cannot fabricate a default")]
    NonFiniteSigma,
    /// `image_width` passed in was too small to pick two distinct
    /// horizon sample points (`image_width < 2`).
    #[error("image_width {0} is too small to sample the horizon line")]
    ImageTooNarrow(u32),
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
    image_width: u32,
    horizon: HorizonLine,
    body: Centroid,
) -> Result<Uncertain<f64>, MeasurementError> {
    let body_ray = pixel_ray_direction(intr, body.x, body.y);
    let centroid_sigma_rad = body.position_sigma_px.value() / intr.fy;
    let body_sigma =
        Sigma::new(centroid_sigma_rad).map_err(|_| MeasurementError::NonFiniteSigma)?;
    measure_altitude_from_ray(intr, image_width, horizon, body_ray, body_sigma)
}

/// Compute the apparent altitude of a body given its camera-frame
/// unit ray directly (skipping the pixel→ray conversion).
///
/// This is the entry point for plate-solving altitude extraction:
/// after identifying a star, its camera-frame ray comes from the
/// recovered attitude × J2000 unit vector — there's no pixel
/// centroid to convert. The horizon plane and altitude math is
/// identical to [`measure_altitude`].
///
/// `body_ray_sigma` is the 1σ angular uncertainty of the body
/// ray itself, in radians. For a centroid-derived ray it's
/// `position_sigma_px / fy`; for a plate-solved star ray it's the
/// per-star pose residual from the Kabsch refinement.
///
/// # Errors
///
/// See [`MeasurementError`].
pub fn measure_altitude_from_ray(
    intr: Intrinsics,
    image_width: u32,
    horizon: HorizonLine,
    body_ray: (f64, f64, f64),
    body_ray_sigma: Sigma,
) -> Result<Uncertain<f64>, MeasurementError> {
    if image_width < 2 {
        return Err(MeasurementError::ImageTooNarrow(image_width));
    }
    let body_vec = (body_ray.0, body_ray.1, body_ray.2);

    // Pick two endpoints on the horizon line at the actual image
    // bounds. The cross product is independent of baseline length,
    // but using a baseline that doesn't match the real image risks
    // sampling a y-coordinate outside the calibrated principal-
    // point / distortion regime when the horizon has nonzero slope.
    let p1_x = 0.0;
    let p1_y = horizon.intercept;
    let p2_x = f64::from(image_width - 1);
    let p2_y = horizon.slope * p2_x + horizon.intercept;
    let p1_ray = pixel_ray_direction(intr, p1_x, p1_y);
    let p2_ray = pixel_ray_direction(intr, p2_x, p2_y);
    let p1_vec = (p1_ray.0, p1_ray.1, p1_ray.2);
    let p2_vec = (p2_ray.0, p2_ray.1, p2_ray.2);

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

    // Sign convention: the normal should point "up" away from
    // image-space where the body sits when above the horizon.
    // Sample 100 px above the horizon's y at the image's horizontal
    // midpoint (where the principal-point / distortion model is most
    // trustworthy).
    let sample_x = f64::from(image_width - 1) / 2.0;
    let sample_y = horizon.slope * sample_x + horizon.intercept - 100.0;
    let sample_ray = pixel_ray_direction(intr, sample_x, sample_y);
    let sample_vec = (sample_ray.0, sample_ray.1, sample_ray.2);
    if dot(normal, sample_vec) < 0.0 {
        normal = (-normal.0, -normal.1, -normal.2);
    }

    let cos_complement = dot(body_vec, normal).clamp(-1.0, 1.0);
    let altitude = cos_complement.asin();

    if !altitude.is_finite() {
        return Err(MeasurementError::NonFinite);
    }
    if altitude < -1.0_f64.to_radians() {
        return Err(MeasurementError::BelowHorizon);
    }

    let total_sigma = horizon.altitude_sigma.combine(body_ray_sigma);
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
        let altitude =
            measure_altitude(frame.intrinsics, frame.width(), horizon, centroid).unwrap();
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
        let altitude =
            measure_altitude(frame.intrinsics, frame.width(), horizon, centroid).unwrap();
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
        let altitude =
            measure_altitude(frame.intrinsics, frame.width(), horizon, centroid).unwrap();
        assert!(altitude.sigma.value().is_finite());
        assert!(altitude.sigma.value() > 0.0);
        // For a synthetic noise-free frame the σ should be well below
        // 1° (no atmospheric / refraction effects in this stage).
        assert!(altitude.sigma.value().to_degrees() < 1.0);
    }

    #[test]
    fn non_finite_centroid_sigma_returns_explicit_error_not_zero() {
        // Per AGENTS.md rule zero / plan.org L596 audit: a non-finite
        // centroid σ must NOT be silently coerced to Sigma::ZERO
        // (which the WLS fix solver would treat as infinite
        // confidence and let the bad sight dominate). It must
        // surface as an explicit error.
        let intr = Intrinsics::placeholder(800, 600);
        // Build a NaN σ by hand. `Sigma::new` refuses non-finite, so
        // we go through a Centroid that the constructor doesn't
        // validate — the f64 -> Sigma conversion happens inside
        // `measure_altitude` via `value() / intr.fy`. To get a
        // non-finite (intr.fy * NaN), feed a Sigma carrying NaN:
        // construct it by abusing the public API — Sigma::new rejects
        // NaN, so we have to take the equivalent path. The simplest
        // is `intr.fy = 0.0` so the divide produces NaN/inf, but
        // intrinsics with fy=0 is itself nonsense. Instead: cook
        // `position_sigma_px = Sigma::new(f64::MAX).unwrap()` and
        // then make intr.fy small enough that the divide overflows
        // to +inf.
        let mut intr_inf = intr;
        intr_inf.fy = f64::MIN_POSITIVE; // tiny; MAX / tiny = +inf
        let bad = crate::centroid::Centroid {
            x: 400.0,
            y: 200.0,
            area_px: 100,
            mean_intensity: 50_000.0,
            position_sigma_px: Sigma::new(f64::MAX).unwrap(),
        };
        let horizon = HorizonLine {
            slope: 0.0,
            intercept: 400.0,
            inlier_count: 100,
            candidate_count: 200,
            residual_rms_px: 1.0,
            altitude_sigma: Sigma::new(1e-4).unwrap(),
        };
        let result = measure_altitude(intr_inf, 800, horizon, bad);
        assert_eq!(result, Err(MeasurementError::NonFiniteSigma));
    }

    #[test]
    fn horizon_sample_uses_actual_image_width_not_hardcoded_baseline() {
        // Per plan.org L596 audit: the horizon endpoint sample was
        // hardcoded at x = 1000.0 with a comment claiming "we don't
        // know the image width here." With distortion present, the
        // y-coordinate the cross-product is evaluated at moves the
        // resulting normal (and therefore the altitude). Two
        // intrinsics that differ ONLY in image_width must produce
        // different altitudes when the horizon has nonzero slope
        // and the lens has nonzero distortion. The old hardcoded
        // x = 1000.0 path would produce identical altitudes.
        let intr = Intrinsics {
            fx: 1000.0,
            fy: 1000.0,
            cx: 1000.0,
            cy: 750.0,
            k1: -0.10,
            k2: 0.05,
            k3: 0.0,
            p1: 0.001,
            p2: -0.002,
        };
        let horizon = HorizonLine {
            slope: 0.05, // ~3° tilt — nonzero so endpoint y depends on x
            intercept: 600.0,
            inlier_count: 100,
            candidate_count: 200,
            residual_rms_px: 1.0,
            altitude_sigma: Sigma::new(1e-4).unwrap(),
        };
        // A body well above the horizon. Use the ray form so the
        // body geometry is identical in both cases — the ONLY
        // difference between the two calls is `image_width`.
        let body_ray = pixel_ray_direction(intr, 1000.0, 200.0);
        let body_sigma = Sigma::new(1e-5).unwrap();
        let narrow = measure_altitude_from_ray(intr, 800, horizon, body_ray, body_sigma).unwrap();
        let wide = measure_altitude_from_ray(intr, 4000, horizon, body_ray, body_sigma).unwrap();
        assert!(
            (narrow.value - wide.value).abs() > 1e-6,
            "altitude must depend on image_width with nonzero slope + \
             distortion; got narrow={} wide={} (delta {:e})",
            narrow.value,
            wide.value,
            (narrow.value - wide.value).abs()
        );
    }

    #[test]
    fn image_width_too_narrow_is_explicit_error() {
        let intr = Intrinsics::placeholder(800, 600);
        let horizon = HorizonLine {
            slope: 0.0,
            intercept: 400.0,
            inlier_count: 100,
            candidate_count: 200,
            residual_rms_px: 1.0,
            altitude_sigma: Sigma::new(1e-4).unwrap(),
        };
        let body_ray = pixel_ray_direction(intr, 400.0, 200.0);
        let body_sigma = Sigma::new(1e-5).unwrap();
        assert_eq!(
            measure_altitude_from_ray(intr, 1, horizon, body_ray, body_sigma),
            Err(MeasurementError::ImageTooNarrow(1))
        );
    }
}
