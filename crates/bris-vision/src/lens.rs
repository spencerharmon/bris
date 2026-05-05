//! Lens model: pinhole + Brown-Conrady distortion, with undistortion.
//!
//! The pinhole model maps a 3D ray direction to a normalized image
//! plane (z = 1) coordinate; the Brown-Conrady distortion model warps
//! that ideal coordinate to where the real lens actually places it on
//! the sensor; the intrinsics map normalized → pixel.
//!
//! For Bris's use, we need the inverse: given an observed pixel of a
//! body, recover the ideal (undistorted) ray direction. That direction
//! combined with horizon geometry gives the body's altitude. The
//! [`undistort_pixel`] function does this iteratively because the
//! Brown-Conrady model is not analytically invertible.
//!
//! # Calibration accuracy and our budget
//!
//! Sub-pixel calibration residuals are achievable with a checkerboard
//! workflow (~30 frames). For our 0.5 nm stretch goal, calibration
//! residuals of ~0.5 px contribute ~0.5 arcmin at typical FOVs (~1
//! arcmin/px), which is the dominant error after refraction at high
//! altitude. So the calibration workflow needs to deliver < 0.5 px
//! RMS to honor the budget. That's well within reach.
//!
//! The calibration *workflow* (capture, corner detection, parameter
//! solve) is a separate task; this module is just the math.

use crate::frame::Intrinsics;

/// Apply the lens distortion model to an ideal normalized image-plane
/// coordinate `(x, y)` (with z = 1), producing the distorted normalized
/// coordinate.
///
/// Brown-Conrady model:
/// ```text
/// r² = x² + y²
/// radial = 1 + k1 r² + k2 r⁴ + k3 r⁶
/// x_d = x · radial + 2 p1 x y + p2 (r² + 2 x²)
/// y_d = y · radial + p1 (r² + 2 y²) + 2 p2 x y
/// ```
#[must_use]
pub fn distort_normalized(intr: Intrinsics, x: f64, y: f64) -> (f64, f64) {
    let r2 = x * x + y * y;
    let radial = 1.0 + intr.k1 * r2 + intr.k2 * r2 * r2 + intr.k3 * r2 * r2 * r2;
    let x_d = x * radial + 2.0 * intr.p1 * x * y + intr.p2 * (r2 + 2.0 * x * x);
    let y_d = y * radial + intr.p1 * (r2 + 2.0 * y * y) + 2.0 * intr.p2 * x * y;
    (x_d, y_d)
}

/// Map an ideal normalized coordinate to a pixel position via the
/// intrinsics (focal length + principal point), without distortion.
#[must_use]
pub fn project_pinhole(intr: Intrinsics, x: f64, y: f64) -> (f64, f64) {
    (intr.fx * x + intr.cx, intr.fy * y + intr.cy)
}

/// Map a pixel to its ideal normalized coordinate (inverse of
/// [`project_pinhole`]). This step is purely linear.
#[must_use]
pub fn unproject_pinhole(intr: Intrinsics, u: f64, v: f64) -> (f64, f64) {
    ((u - intr.cx) / intr.fx, (v - intr.cy) / intr.fy)
}

/// Recover the ideal (undistorted) normalized image-plane coordinate
/// from an observed pixel.
///
/// Brown-Conrady has no closed-form inverse; we iterate. Five
/// iterations of fixed-point inversion are enough for sub-0.01-px
/// accuracy on typical lenses (|k1| < 0.5).
///
/// Returns `(x, y)` such that `distort_normalized(intr, x, y)` then
/// `project_pinhole(intr, ...)` reproduces the input pixel `(u, v)`.
#[must_use]
#[allow(clippy::similar_names)] // x_d, y_d, dx_tan, dy_tan are domain-standard.
pub fn undistort_pixel(intr: Intrinsics, u: f64, v: f64) -> (f64, f64) {
    // Start from the linear unprojection (assumes no distortion).
    let (x_d, y_d) = unproject_pinhole(intr, u, v);
    let mut x = x_d;
    let mut y = y_d;
    for _ in 0..5 {
        let r2 = x * x + y * y;
        let radial = 1.0 + intr.k1 * r2 + intr.k2 * r2 * r2 + intr.k3 * r2 * r2 * r2;
        let dx_tan = 2.0 * intr.p1 * x * y + intr.p2 * (r2 + 2.0 * x * x);
        let dy_tan = intr.p1 * (r2 + 2.0 * y * y) + 2.0 * intr.p2 * x * y;
        // Subtract distortion to recover ideal coords.
        x = (x_d - dx_tan) / radial;
        y = (y_d - dy_tan) / radial;
    }
    (x, y)
}

/// Convert an undistorted normalized image-plane coordinate to a unit
/// ray direction in camera coordinates.
///
/// Returns a unit vector `(dx, dy, dz)` pointing from the camera origin
/// toward the world point that landed at this pixel. Useful for
/// converting body centroid pixels to ray directions for downstream
/// angle computation.
#[must_use]
pub fn pixel_ray_direction(intr: Intrinsics, u: f64, v: f64) -> (f64, f64, f64) {
    let (x, y) = undistort_pixel(intr, u, v);
    let norm = (x * x + y * y + 1.0).sqrt();
    (x / norm, y / norm, 1.0 / norm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    fn intrinsics_zero_distortion() -> Intrinsics {
        Intrinsics {
            fx: 1000.0,
            fy: 1000.0,
            cx: 320.0,
            cy: 240.0,
            k1: 0.0,
            k2: 0.0,
            k3: 0.0,
            p1: 0.0,
            p2: 0.0,
        }
    }

    fn intrinsics_realistic_distortion() -> Intrinsics {
        Intrinsics {
            fx: 1000.0,
            fy: 1000.0,
            cx: 320.0,
            cy: 240.0,
            k1: -0.10,
            k2: 0.05,
            k3: 0.0,
            p1: 0.001,
            p2: -0.002,
        }
    }

    #[test]
    fn pinhole_round_trips() {
        let intr = intrinsics_zero_distortion();
        let (x, y) = unproject_pinhole(intr, 100.0, 150.0);
        let (u, v) = project_pinhole(intr, x, y);
        assert_relative_eq!(u, 100.0);
        assert_relative_eq!(v, 150.0);
    }

    #[test]
    fn distort_undistort_round_trips_with_distortion() {
        let intr = intrinsics_realistic_distortion();
        // Pick several pixels across the field.
        for &(u, v) in &[
            (320.0, 240.0), // center: distortion is zero by symmetry
            (400.0, 300.0),
            (100.0, 400.0),
            (600.0, 50.0),
        ] {
            let (xn, yn) = undistort_pixel(intr, u, v);
            let (xd, yd) = distort_normalized(intr, xn, yn);
            let (u2, v2) = project_pinhole(intr, xd, yd);
            assert_relative_eq!(u2, u, epsilon = 0.001);
            assert_relative_eq!(v2, v, epsilon = 0.001);
        }
    }

    #[test]
    fn zero_distortion_undistort_is_identity() {
        let intr = intrinsics_zero_distortion();
        let (x, y) = undistort_pixel(intr, 500.0, 100.0);
        let (x_lin, y_lin) = unproject_pinhole(intr, 500.0, 100.0);
        assert_relative_eq!(x, x_lin, epsilon = 1e-12);
        assert_relative_eq!(y, y_lin, epsilon = 1e-12);
    }

    #[test]
    fn ray_direction_is_unit_length() {
        let intr = intrinsics_realistic_distortion();
        let (dx, dy, dz) = pixel_ray_direction(intr, 400.0, 300.0);
        let norm = (dx * dx + dy * dy + dz * dz).sqrt();
        assert_relative_eq!(norm, 1.0, epsilon = 1e-12);
    }

    #[test]
    fn principal_point_ray_is_straight_ahead() {
        let intr = intrinsics_zero_distortion();
        let (dx, dy, dz) = pixel_ray_direction(intr, intr.cx, intr.cy);
        // Should point straight along +z.
        assert_relative_eq!(dx, 0.0, epsilon = 1e-12);
        assert_relative_eq!(dy, 0.0, epsilon = 1e-12);
        assert_relative_eq!(dz, 1.0, epsilon = 1e-12);
    }
}
