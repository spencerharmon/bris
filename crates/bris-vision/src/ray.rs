//! Camera-space ray representations of detector outputs.
//!
//! The pixel-coordinate output types of each detector
//! ([`crate::HorizonLine`], [`crate::Centroid`], [`crate::Peak`])
//! are tied to the resolution they were produced at. Combining
//! detections from different resolutions in pixel space requires
//! the consumer to know exactly how each was scaled, which is
//! both error-prone and brittle.
//!
//! This module's types live in **camera coordinate space** —
//! unit 3-vectors expressed in the camera's frame of reference,
//! independent of resolution. Two detections at different
//! resolutions, once converted to camera-space rays, can be
//! composed directly: stitched, intersected, or measured against
//! the horizon plane without any further coordinate manipulation.
//!
//! Convention for the camera frame:
//!
//!  * +x — right in image space
//!  * +y — down in image space
//!  * +z — out of the lens, toward the scene
//!
//! All rays in this module are unit vectors. The conversion from
//! pixel coordinates uses the lens model (pinhole + Brown-
//! Conrady distortion); see [`crate::pixel_ray_direction`] and
//! [`crate::undistort_pixel`].

use bris_core::Sigma;

use crate::frame::Intrinsics;
use crate::horizon::HorizonLine;
use crate::lens::pixel_ray_direction;

/// A unit 3-vector in the camera's coordinate frame.
///
/// `[x, y, z]` with `x² + y² + z² = 1`. Construction goes
/// through [`pixel_ray_direction`] so the lens model is applied
/// consistently. Direct construction from raw components is
/// reserved for tests and for code that has already done its
/// own undistortion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraRay {
    /// Component along the image-right axis.
    pub x: f64,
    /// Component along the image-down axis.
    pub y: f64,
    /// Component along the lens axis (toward the scene).
    pub z: f64,
}

impl CameraRay {
    /// Compute a camera ray from a pixel coordinate, applying
    /// the lens model. Equivalent to [`pixel_ray_direction`]
    /// with a strongly-typed return.
    #[must_use]
    pub fn from_pixel(intrinsics: &Intrinsics, px_x: f64, px_y: f64) -> Self {
        let (x, y, z) = pixel_ray_direction(*intrinsics, px_x, px_y);
        Self { x, y, z }
    }

    /// Direct construction from already-computed components.
    /// Caller asserts `x² + y² + z² = 1` (within float
    /// tolerance); no normalization performed.
    #[must_use]
    pub const fn from_unit_components(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    /// As `[x, y, z]`. Convenience for code expecting a slice.
    #[must_use]
    pub const fn as_array(&self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }

    /// Dot product with another ray.
    #[must_use]
    pub fn dot(&self, other: &Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// Cross product with another ray.
    #[must_use]
    pub fn cross(&self, other: &Self) -> Self {
        Self {
            x: self.y * other.z - self.z * other.y,
            y: self.z * other.x - self.x * other.z,
            z: self.x * other.y - self.y * other.x,
        }
    }

    /// Vector magnitude.
    #[must_use]
    pub fn norm(&self) -> f64 {
        self.dot(self).sqrt()
    }

    /// Unit-length copy. Returns `None` if the vector has zero
    /// magnitude.
    #[must_use]
    pub fn normalize(&self) -> Option<Self> {
        let n = self.norm();
        if n == 0.0 || !n.is_finite() {
            return None;
        }
        Some(Self {
            x: self.x / n,
            y: self.y / n,
            z: self.z / n,
        })
    }
}

/// Camera-space representation of a detected horizon line.
///
/// A horizon in pixel space is a line `y = m·x + b`. In the
/// camera frame it is the **plane** containing the lens center
/// and the horizon line; that plane is fully described by its
/// unit normal vector. Altitudes are computed by projecting the
/// body ray onto this normal: `sin(altitude) = -dot(body_ray,
/// normal)` (sign convention: normal points toward the sky;
/// body above horizon has positive altitude).
///
/// The normal is derived by undistorting two pixel points on
/// the line through the lens model and taking the cross product
/// of their resulting rays.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HorizonRay {
    /// Unit normal to the horizon plane, in camera coordinates.
    /// Points toward the sky side.
    pub normal: CameraRay,
    /// 1σ altitude uncertainty, in radians. Carried over from
    /// the source [`HorizonLine::altitude_sigma`].
    pub altitude_sigma: Sigma,
}

impl HorizonRay {
    /// Lift a pixel-coordinate horizon line to a camera-space
    /// horizon plane.
    ///
    /// The conversion samples two well-separated points on the
    /// line (at x = 0 and x = `image_width - 1`), undistorts
    /// each via the lens model, and takes their cross product
    /// to get the plane normal. The sign is flipped if needed
    /// so the normal points toward the sky (+z half-space:
    /// since the camera looks down the +z axis and the horizon
    /// is below the optical axis in a typical capture, the
    /// sky-pointing normal has positive `y` after the cross
    /// product when the line slopes nearly horizontal).
    ///
    /// # Errors
    ///
    /// Returns `None` if the constructed normal is degenerate
    /// (zero magnitude after the cross product), which only
    /// happens for an unrealistic line through the principal
    /// point.
    #[must_use]
    pub fn from_line(
        line: &HorizonLine,
        intrinsics: &Intrinsics,
        image_width: u32,
    ) -> Option<Self> {
        if image_width < 2 {
            return None;
        }
        let x0 = 0.0_f64;
        let x1 = f64::from(image_width - 1);
        let y0 = line.slope.mul_add(x0, line.intercept);
        let y1 = line.slope.mul_add(x1, line.intercept);
        let r0 = CameraRay::from_pixel(intrinsics, x0, y0);
        let r1 = CameraRay::from_pixel(intrinsics, x1, y1);
        let mut normal = r0.cross(&r1).normalize()?;
        // Sign convention: sky-pointing normal. The horizon
        // line is below the optical axis in the typical
        // capture, so the cross product r0 × r1 points
        // "up" in image space → negative y in camera coords
        // (image-y increases downward). Flip if positive.
        if normal.y > 0.0 {
            normal = CameraRay {
                x: -normal.x,
                y: -normal.y,
                z: -normal.z,
            };
        }
        Some(Self {
            normal,
            altitude_sigma: line.altitude_sigma,
        })
    }
}

/// Camera-space representation of a detected body centroid (Sun
/// / Moon / planet / star).
///
/// The body's location reduces to a single unit ray. The σ
/// captured here is the **angular** uncertainty in radians,
/// computed by combining the source pixel-position σ with the
/// lens's local pixels-per-radian factor at the centroid
/// location. Foreign callers using the ray for sight reduction
/// consume `σ` directly without needing the intrinsics again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyRay {
    /// Unit ray pointing at the body in camera coordinates.
    pub ray: CameraRay,
    /// 1σ angular uncertainty in radians. Combines the source
    /// pixel-position σ with the local lens scale.
    pub direction_sigma: Sigma,
}

impl BodyRay {
    /// Lift a pixel-coordinate centroid + per-pixel σ to a
    /// camera-space ray + angular σ.
    ///
    /// Angular σ ≈ pixel σ / `focal_length_eff`, where
    /// `focal_length_eff = sqrt(fx · fy)` is the geometric mean
    /// of the per-axis focal lengths in pixels. Good to first
    /// order at any centroid location away from heavy
    /// distortion.
    #[must_use]
    pub fn from_pixel(
        intrinsics: &Intrinsics,
        px_x: f64,
        px_y: f64,
        position_sigma_px: Sigma,
    ) -> Self {
        let ray = CameraRay::from_pixel(intrinsics, px_x, px_y);
        let f_eff = (intrinsics.fx * intrinsics.fy).sqrt();
        // Sigma::new rejects non-positive; guard.
        let sigma_value = position_sigma_px.value() / f_eff.max(f64::EPSILON);
        let direction_sigma = Sigma::new(sigma_value).unwrap_or(position_sigma_px);
        Self {
            ray,
            direction_sigma,
        }
    }
}

/// Compute the altitude (angle above the horizon plane) of a
/// body ray.
///
/// `sin(altitude) = dot(body_ray, horizon_normal)` under the
/// sign convention that the horizon normal points toward the
/// sky. Body in the zenith → ray parallel to normal → altitude
/// = π/2; body on the horizon → ray perpendicular → altitude
/// = 0; body below horizon → opposite hemisphere → altitude
/// negative.
///
/// The total altitude σ combines the body's angular σ and the
/// horizon's altitude σ in quadrature. Both are 1σ values in
/// radians; the result is a 1σ value in radians.
#[must_use]
pub fn altitude_from_rays(body: &BodyRay, horizon: &HorizonRay) -> AltitudeMeasurement {
    let sin_alt = body.ray.dot(&horizon.normal);
    let altitude_rad = sin_alt.clamp(-1.0, 1.0).asin();
    let combined_sigma_value =
        (body.direction_sigma.value().powi(2) + horizon.altitude_sigma.value().powi(2)).sqrt();
    let altitude_sigma = Sigma::new(combined_sigma_value).unwrap_or(horizon.altitude_sigma);
    AltitudeMeasurement {
        altitude_rad,
        altitude_sigma,
    }
}

/// Output of [`altitude_from_rays`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AltitudeMeasurement {
    /// Altitude above the horizon plane, radians. Positive when
    /// the body is above the horizon.
    pub altitude_rad: f64,
    /// 1σ uncertainty on `altitude_rad`, radians.
    pub altitude_sigma: Sigma,
}

/// Compute the angular bisector of two camera rays, normalized.
///
/// `(a + b).normalize()`. Returns `None` if the vectors are
/// anti-parallel (sum is zero). Used by the reflection-pair
/// horizon provider to infer the local vertical from a body
/// and its reflection: the bisector points from the camera
/// toward the horizon plane, so `-bisector` is the inferred
/// gravity direction in camera frame.
#[must_use]
pub fn bisector_normal(a: &CameraRay, b: &CameraRay) -> Option<CameraRay> {
    CameraRay {
        x: a.x + b.x,
        y: a.y + b.y,
        z: a.z + b.z,
    }
    .normalize()
}

/// Build a pixel-coordinate [`HorizonLine`] from a horizon-plane
/// normal expressed in the camera frame.
///
/// `normal` should point toward the sky. The line is
/// parameterised as `y = slope * x + intercept`; pixels on the
/// line have rays perpendicular to `normal` under the pinhole
/// model. Tangential / radial distortion are ignored at the
/// line-construction step (the reflection-pair provider uses
/// this on near-axis horizons where the small-distortion
/// approximation is good to well under an arcminute on the
/// `Intrinsics::placeholder` used by tests).
///
/// `altitude_sigma` is propagated unchanged into the resulting
/// line.
///
/// Returns `None` when the line is degenerate (vertical in the
/// image, i.e. `normal.y` is zero), which doesn't correspond to
/// any horizon a forward-pointing camera could see.
#[must_use]
pub fn horizon_line_from_normal(
    normal: &CameraRay,
    intrinsics: &Intrinsics,
    altitude_sigma: Sigma,
) -> Option<HorizonLine> {
    if normal.y.abs() < 1e-12 {
        return None;
    }
    // ray·normal = 0  with ray = ((x-cx)/fx, (y-cy)/fy, 1)
    //   nx*(x-cx)/fx + ny*(y-cy)/fy + nz = 0
    //   y = cy - (fy/ny) * (nx*(x-cx)/fx + nz)
    let slope = -(normal.x * intrinsics.fy) / (normal.y * intrinsics.fx);
    let intercept = intrinsics.cy
        - (intrinsics.fy / normal.y) * (-normal.x * intrinsics.cx / intrinsics.fx + normal.z);
    Some(HorizonLine {
        slope,
        intercept,
        inlier_count: 0,
        candidate_count: 0,
        residual_rms_px: 0.0,
        altitude_sigma,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholder_intr() -> Intrinsics {
        Intrinsics::placeholder(1280, 720)
    }

    #[test]
    fn camera_ray_from_principal_point_is_optical_axis() {
        let intr = placeholder_intr();
        let ray = CameraRay::from_pixel(&intr, intr.cx, intr.cy);
        // The principal point projects to the +z optical axis.
        assert!(ray.x.abs() < 1e-9, "x = {}", ray.x);
        assert!(ray.y.abs() < 1e-9, "y = {}", ray.y);
        assert!((ray.z - 1.0).abs() < 1e-9, "z = {}", ray.z);
    }

    #[test]
    fn camera_ray_normalization() {
        let intr = placeholder_intr();
        let ray = CameraRay::from_pixel(&intr, 100.0, 200.0);
        assert!((ray.norm() - 1.0).abs() < 1e-9, "norm = {}", ray.norm());
    }

    #[test]
    fn camera_ray_dot_and_cross_basics() {
        let a = CameraRay::from_unit_components(1.0, 0.0, 0.0);
        let b = CameraRay::from_unit_components(0.0, 1.0, 0.0);
        assert!((a.dot(&b)).abs() < 1e-12, "orthogonal");
        let c = a.cross(&b);
        assert!((c.x).abs() < 1e-12);
        assert!((c.y).abs() < 1e-12);
        assert!((c.z - 1.0).abs() < 1e-12, "right-handed");
    }

    #[test]
    fn horizon_ray_from_horizontal_line_has_normal_pointing_up() {
        // A perfectly horizontal line at the principal-point
        // y becomes a horizon plane through the lens center
        // perpendicular to the +y axis (sky-pointing normal
        // = -y).
        let intr = placeholder_intr();
        let line = HorizonLine {
            slope: 0.0,
            intercept: intr.cy,
            inlier_count: 100,
            candidate_count: 100,
            residual_rms_px: 0.0,
            altitude_sigma: Sigma::new(0.001).unwrap(),
        };
        let h = HorizonRay::from_line(&line, &intr, 1280).unwrap();
        // Normal must be (0, ±1, 0) to numerical precision; we
        // expect -y (sky is "up", image-y is "down").
        assert!(h.normal.x.abs() < 1e-9);
        assert!(h.normal.z.abs() < 1e-9);
        assert!(
            h.normal.y < -0.999,
            "expected sky-up (normal.y ≈ -1), got y = {}",
            h.normal.y,
        );
    }

    #[test]
    fn altitude_from_rays_computes_principal_axis_altitude_to_horizon_below() {
        // Body ray on the optical axis (+z); horizon plane has
        // normal -y; altitude must be 0 because +z is
        // perpendicular to -y → sin(alt) = -dot(z, -y) = 0.
        let body = BodyRay {
            ray: CameraRay::from_unit_components(0.0, 0.0, 1.0),
            direction_sigma: Sigma::new(1e-6).unwrap(),
        };
        let horizon = HorizonRay {
            normal: CameraRay::from_unit_components(0.0, -1.0, 0.0),
            altitude_sigma: Sigma::new(1e-6).unwrap(),
        };
        let m = altitude_from_rays(&body, &horizon);
        assert!(m.altitude_rad.abs() < 1e-9, "altitude = {}", m.altitude_rad);
    }

    #[test]
    fn altitude_from_rays_positive_when_body_above_horizon() {
        // Body 30° above the horizon plane.
        let alt = std::f64::consts::FRAC_PI_6;
        let body = BodyRay {
            ray: CameraRay::from_unit_components(0.0, -alt.sin(), alt.cos()),
            direction_sigma: Sigma::new(1e-6).unwrap(),
        };
        let horizon = HorizonRay {
            normal: CameraRay::from_unit_components(0.0, -1.0, 0.0),
            altitude_sigma: Sigma::new(1e-6).unwrap(),
        };
        let m = altitude_from_rays(&body, &horizon);
        assert!(
            (m.altitude_rad - alt).abs() < 1e-9,
            "expected {alt}, got {}",
            m.altitude_rad,
        );
    }

    #[test]
    fn altitude_sigma_combines_in_quadrature() {
        let body_s = 0.001_f64;
        let horizon_s = 0.002_f64;
        let body = BodyRay {
            ray: CameraRay::from_unit_components(0.0, 0.0, 1.0),
            direction_sigma: Sigma::new(body_s).unwrap(),
        };
        let horizon = HorizonRay {
            normal: CameraRay::from_unit_components(0.0, -1.0, 0.0),
            altitude_sigma: Sigma::new(horizon_s).unwrap(),
        };
        let m = altitude_from_rays(&body, &horizon);
        let expected = (body_s * body_s + horizon_s * horizon_s).sqrt();
        assert!(
            (m.altitude_sigma.value() - expected).abs() < 1e-12,
            "expected {expected}, got {}",
            m.altitude_sigma.value(),
        );
    }

    // --- Non-circular ml-gravity BelowHorizon geometry regression ---
    //
    // The `ml-gravity` provider synthesises its horizon from a
    // model-predicted camera-frame gravity via
    // `gravity_from_roll_pitch` → `horizon_line_from_normal`.
    // The integration copy of these assertions lives in
    // `tests/ml_gravity_math.rs`, but that whole file is behind
    // `#[cfg(feature = "ml-gravity")]`, so the definition-of-done
    // check `cargo test -p bris-vision` (default features) never
    // runs it. This module-level copy exercises the SAME geometry
    // with default features so the DoD check actually proves the
    // reflection/gravity horizon derivation does not spuriously
    // reject valid single-fix-frame sights. It reproduces the
    // provider's (roll, pitch) → g_cam formula locally (pure math,
    // no ONNX / `ort` / `ndarray` dependency) and compares it,
    // and the horizon it yields, against an INDEPENDENTLY
    // constructed true horizon — so it is not the tautological
    // inverse the prior regression was.

    /// The provider's documented derivation, reproduced with no
    /// feature gate: `g_cam = (sinφcosθ, cosφcosθ, -sinθ)`.
    fn ml_gravity_g_cam(roll: f64, pitch: f64) -> (f64, f64, f64) {
        let (sr, cr) = roll.sin_cos();
        let (sp, cp) = pitch.sin_cos();
        (sr * cp, cr * cp, -sp)
    }

    /// Independent camera-frame gravity for a roll/pitch tilt,
    /// built from hand-written +x then +z rotations of the level
    /// image-down vector — sharing no code with
    /// `ml_gravity_g_cam`. The two must agree for a perfect
    /// prediction; that agreement is the load-bearing
    /// non-circularity check.
    fn independent_true_g_cam(roll: f64, pitch: f64) -> (f64, f64, f64) {
        let g0 = (0.0_f64, 1.0_f64, 0.0_f64);
        let (sp, cp) = pitch.sin_cos();
        let after_pitch = (g0.0, cp * g0.1 + sp * g0.2, -sp * g0.1 + cp * g0.2);
        let (sr, cr) = roll.sin_cos();
        (
            cr * after_pitch.0 + sr * after_pitch.1,
            -sr * after_pitch.0 + cr * after_pitch.1,
            after_pitch.2,
        )
    }

    fn v_cross(a: (f64, f64, f64), b: (f64, f64, f64)) -> (f64, f64, f64) {
        (
            a.1 * b.2 - a.2 * b.1,
            a.2 * b.0 - a.0 * b.2,
            a.0 * b.1 - a.1 * b.0,
        )
    }
    fn v_norm(a: (f64, f64, f64)) -> (f64, f64, f64) {
        let n = (a.0 * a.0 + a.1 * a.1 + a.2 * a.2).sqrt();
        (a.0 / n, a.1 / n, a.2 / n)
    }

    /// Unit body ray at `altitude` above the local horizontal
    /// defined by `sky_normal`, offset by `azimuth` around it.
    fn body_ray_at(sky_normal: (f64, f64, f64), altitude: f64, azimuth: f64) -> (f64, f64, f64) {
        let sn = v_norm(sky_normal);
        let seed = if sn.0.abs() < 0.9 {
            (1.0, 0.0, 0.0)
        } else {
            (0.0, 1.0, 0.0)
        };
        let e1 = v_norm(v_cross(sn, seed));
        let e2 = v_cross(sn, e1);
        let (sa, ca) = altitude.sin_cos();
        let (saz, caz) = azimuth.sin_cos();
        (
            sa * sn.0 + ca * (caz * e1.0 + saz * e2.0),
            sa * sn.1 + ca * (caz * e1.1 + saz * e2.1),
            sa * sn.2 + ca * (caz * e1.2 + saz * e2.2),
        )
    }

    #[test]
    fn ml_gravity_derivation_has_no_below_horizon_geometry_bias() {
        let tilts_deg: &[(f64, f64)] = &[
            (0.0, 0.0),
            (12.0, 0.0),
            (-18.0, 0.0),
            (0.0, 10.0),
            (0.0, -8.0),
            (20.0, 12.0),
            (-25.0, -6.0),
        ];
        for &(roll_deg, pitch_deg) in tilts_deg {
            assert_no_geometry_bias_for_tilt(roll_deg, pitch_deg);
        }
    }

    fn assert_no_geometry_bias_for_tilt(roll_deg: f64, pitch_deg: f64) {
        use crate::measure::{measure_altitude_from_ray, MeasurementError};
        let intr = Intrinsics::placeholder(1280, 720);
        let width = 1280;
        let body_sigma = Sigma::new(1.0e-4).unwrap();
        let alt_sigma = Sigma::new(0.05).unwrap();
        let azimuths: &[f64] = &[0.0, 1.2, 2.6, 4.0, 5.3];

        {
            let roll = roll_deg.to_radians();
            let pitch = pitch_deg.to_radians();

            // Non-circularity: independent true gravity must match
            // the derivation under test for a perfect prediction.
            let g_true = independent_true_g_cam(roll, pitch);
            let g_pred = ml_gravity_g_cam(roll, pitch);
            assert!((g_pred.0 - g_true.0).abs() < 1e-12);
            assert!((g_pred.1 - g_true.1).abs() < 1e-12);
            assert!((g_pred.2 - g_true.2).abs() < 1e-12);

            let true_sky_normal = (-g_true.0, -g_true.1, -g_true.2);
            let pred_normal = CameraRay {
                x: -g_pred.0,
                y: -g_pred.1,
                z: -g_pred.2,
            };
            let line_perfect = horizon_line_from_normal(&pred_normal, &intr, alt_sigma).unwrap();

            for &az in azimuths {
                // Perfect prediction recovers true altitude — a
                // valid sight is never spuriously rejected.
                for true_alt_deg in [6.0_f64, 15.0, 40.0] {
                    let body = body_ray_at(true_sky_normal, true_alt_deg.to_radians(), az);
                    let m = measure_altitude_from_ray(intr, width, line_perfect, body, body_sigma)
                        .unwrap_or_else(|e| {
                            panic!(
                                "roll={roll_deg} pitch={pitch_deg} az={az} \
                             alt={true_alt_deg}: spurious rejection {e:?}"
                            )
                        });
                    assert!(
                        (m.value.to_degrees() - true_alt_deg).abs() < 1e-6,
                        "roll={roll_deg} pitch={pitch_deg} az={az}: recovered \
                         {} deg, expected {true_alt_deg}",
                        m.value.to_degrees()
                    );
                }

                // A prediction error of eps shifts measured
                // altitude by AT MOST eps (no amplification) — a
                // body 25° up is never rejected when eps < 25°.
                for eps_deg in [3.0_f64, 9.0, 15.0] {
                    let eps = eps_deg.to_radians();
                    let gw = ml_gravity_g_cam(roll, pitch + eps);
                    let nw = CameraRay {
                        x: -gw.0,
                        y: -gw.1,
                        z: -gw.2,
                    };
                    let line_w = horizon_line_from_normal(&nw, &intr, alt_sigma).unwrap();
                    let true_alt = 25.0_f64.to_radians();
                    let body = body_ray_at(true_sky_normal, true_alt, az);
                    let m = measure_altitude_from_ray(intr, width, line_w, body, body_sigma)
                        .unwrap_or_else(|e| {
                            panic!(
                                "roll={roll_deg} pitch={pitch_deg} az={az} \
                                 eps={eps_deg}: a 25°-above body was spuriously \
                                 rejected {e:?} under a smaller prediction error — \
                                 a geometry bug"
                            )
                        });
                    assert!(
                        (m.value - true_alt).abs() <= eps + 1e-6,
                        "roll={roll_deg} pitch={pitch_deg} az={az} eps={eps_deg}: \
                         altitude error {} deg exceeds injected {} deg — derivation \
                         amplifies prediction error",
                        (m.value - true_alt).abs().to_degrees(),
                        eps_deg
                    );
                }
            }

            // Honest, azimuth-DEPENDENT BelowHorizon rejection: a
            // 4°-above body under an 8° single-frame prediction
            // error is rejected on the down-tilted side and
            // accepted on the up-tilted side. Both outcomes must
            // appear across the azimuth circle — a systematic bug
            // would reject at every azimuth; a disabled check at
            // none. This is the single-capture under-production
            // mechanism, and it is CORRECT (honest σ).
            let marginal = 4.0_f64.to_radians();
            let big = 8.0_f64.to_radians();
            let gb = ml_gravity_g_cam(roll, pitch + big);
            let nb = CameraRay {
                x: -gb.0,
                y: -gb.1,
                z: -gb.2,
            };
            let line_b = horizon_line_from_normal(&nb, &intr, alt_sigma).unwrap();
            let mut rejected = false;
            let mut accepted = false;
            for &az in &[0.0, 0.9, 1.8, 2.7, 3.6, 4.5, 5.4, 6.2] {
                let body = body_ray_at(true_sky_normal, marginal, az);
                match measure_altitude_from_ray(intr, width, line_b, body, body_sigma) {
                    Err(MeasurementError::BelowHorizon) => rejected = true,
                    Ok(_) => accepted = true,
                    Err(e) => panic!("unexpected error {e:?}"),
                }
            }
            assert!(
                rejected,
                "roll={roll_deg} pitch={pitch_deg}: BelowHorizon never fired where it \
                 honestly should"
            );
            assert!(
                accepted,
                "roll={roll_deg} pitch={pitch_deg}: rejected at EVERY azimuth — that is a \
                 systematic geometry bias, not honest single-frame σ"
            );
        }
    }
}
