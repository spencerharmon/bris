//! Per-star altitude extraction from a plate-solve result.
//!
//! Once [`plate_solve`] has identified stars and recovered the
//! camera attitude, each identified star can be turned into an
//! altitude observation against an independently-measured horizon
//! line. This is the bridge from "what stars are these" to
//! "altitudes I can plug into sight reduction."
//!
//! # Math
//!
//! For each identified star:
//! 1. Catalog J2000 unit vector → camera-frame unit ray via
//!    `attitude · catalog_vec`.
//! 2. Camera-frame ray + horizon line + intrinsics → measured
//!    altitude via [`bris_vision::measure_altitude_from_ray`].
//!
//! The same horizon line is used for all stars in the frame
//! (they share a camera frame). Per-star σ is the combination of
//! the horizon-fit σ and a per-star pose σ derived from the
//! plate-solve refinement residual.
//!
//! # Limitations
//!
//! Same intrinsics-calibration story as the rest of the pipeline:
//! placeholder `fx = fy = 1000` makes per-star altitudes wrong by
//! the same calibration factor as Sun/Moon altitudes. Real
//! intrinsics are required for absolute accuracy.

use bris_core::{Sigma, Uncertain};
use bris_vision::{measure_altitude_from_ray, HorizonLine, Intrinsics, MeasurementError};

use crate::hash::ra_dec_to_unit_vec;
use crate::solve::{IdentifiedStar, PlateSolveResult};
use bris_math::rotate_vec;

/// One identified star's altitude observation.
#[derive(Debug, Clone, Copy)]
pub struct StarAltitude {
    /// HR id (Yale BSC catalog).
    pub hr: u32,
    /// Identified star's J2000 RA, radians.
    pub ra_rad: f64,
    /// Identified star's J2000 Dec, radians.
    pub dec_rad: f64,
    /// Measured apparent altitude above the horizon line, with σ.
    /// Positive when above horizon.
    pub altitude: Uncertain<f64>,
}

/// Convert a plate-solve result into per-star altitude observations
/// against the supplied horizon line.
///
/// `per_star_sigma` is a 1σ angular uncertainty (radians) added in
/// quadrature to the horizon-fit σ and to the lens-model-propagated
/// per-star plate-solve residual for each star. It captures sources
/// the plate solver doesn't know about (refraction-model residual,
/// star-catalog position σ, etc.).
///
/// The per-star plate-solve refinement residual (stored on each
/// [`IdentifiedStar::pixel_residual`]) is propagated through the
/// lens model into an angular σ contribution ∂alt/∂pixel · `σ_pixel`
/// ≈ `σ_pixel` / fy, then combined in quadrature with the other
/// terms. This is the same Jacobian path that
/// [`bris_vision::measure_altitude`] uses for the body-centroid
/// σ contribution.
///
/// Stars that fall below the horizon under the recovered attitude
/// are skipped silently (they don't contribute observations); other
/// errors propagate as `Result`.
///
/// # Errors
///
/// Returns the first non-`BelowHorizon` measurement error
/// encountered. `BelowHorizon` for individual stars is a normal
/// occurrence (some identified stars may be just below the cutoff
/// after pose refinement) and is skipped, not propagated.
pub fn star_altitudes(
    result: &PlateSolveResult,
    intrinsics: Intrinsics,
    horizon: HorizonLine,
    per_star_sigma: Sigma,
) -> Result<Vec<StarAltitude>, MeasurementError> {
    let mut out = Vec::with_capacity(result.identified.len());
    for ident in &result.identified {
        match star_altitude(
            ident,
            &result.attitude.matrix,
            intrinsics,
            horizon,
            per_star_sigma,
        ) {
            Ok(s) => out.push(s),
            // Below-horizon stars don't produce observations but
            // also aren't an error condition for the batch.
            Err(MeasurementError::BelowHorizon) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Compute one star's altitude. Exposed primarily for testing; in
/// most code paths [`star_altitudes`] is the right entry.
///
/// # Errors
///
/// See [`MeasurementError`].
pub fn star_altitude(
    star: &IdentifiedStar,
    attitude: &[f64; 9],
    intrinsics: Intrinsics,
    horizon: HorizonLine,
    per_star_sigma: Sigma,
) -> Result<StarAltitude, MeasurementError> {
    let catalog_vec = ra_dec_to_unit_vec(star.ra_rad, star.dec_rad);
    let cam = rotate_vec(attitude, catalog_vec);
    // Normalize defensively — Kabsch output should be a true
    // rotation but accumulated numerical error from refinement
    // can leave |cam| slightly off.
    let n = (cam[0].powi(2) + cam[1].powi(2) + cam[2].powi(2)).sqrt();
    let body_ray = if n > 0.0 {
        (cam[0] / n, cam[1] / n, cam[2] / n)
    } else {
        return Err(MeasurementError::NonFinite);
    };
    // Propagate the per-star plate-solve refinement residual through
    // the lens model into an angular σ contribution. The Jacobian
    // ∂alt/∂pixel ≈ 1/fy is the same one bris-vision uses to convert
    // a centroid position σ (pixels) into a ray-angle σ (radians);
    // see `bris_vision::measure_altitude`. Combine in quadrature
    // with the caller-supplied per-star σ (which captures sources
    // the plate solver doesn't know about).
    //
    // Defensive: a NaN/non-finite residual (e.g. a behind-camera
    // projection at refinement time) propagates to a non-finite
    // body_ray_sigma rather than being silently treated as zero.
    let lens_sigma_value = if star.pixel_residual.is_finite() {
        star.pixel_residual / intrinsics.fy
    } else {
        return Err(MeasurementError::NonFinite);
    };
    let lens_sigma = Sigma::new(lens_sigma_value).map_err(|_| MeasurementError::NonFinite)?;
    let body_ray_sigma = per_star_sigma.combine(lens_sigma);
    let altitude = measure_altitude_from_ray(intrinsics, horizon, body_ray, body_ray_sigma)?;
    Ok(StarAltitude {
        hr: star.hr,
        ra_rad: star.ra_rad,
        dec_rad: star.dec_rad,
        altitude,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solve::{Attitude, PlateSolveResult};
    use bris_vision::HorizonLine;

    fn identity_attitude() -> [f64; 9] {
        [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]
    }

    /// A simple horizon at y = 240 (image center), no slope.
    fn level_horizon() -> HorizonLine {
        HorizonLine {
            slope: 0.0,
            intercept: 240.0,
            inlier_count: 100,
            candidate_count: 200,
            residual_rms_px: 1.0,
            altitude_sigma: Sigma::new(1e-4).unwrap_or(Sigma::ZERO),
        }
    }

    #[test]
    fn star_above_horizon_yields_positive_altitude() {
        // Set up a star whose J2000 unit vector, under identity
        // attitude, projects to a camera-frame ray that lands
        // above the horizon (y < intercept).
        //
        // For placeholder intrinsics (cx=320, cy=240, fx=fy=1000)
        // and horizon intercept 240, "above horizon" means image
        // y < 240. We want a ray (X, Y, Z) with Y < 0 and Z > 0
        // (in front of camera). Pick (0, -0.2, 0.9798) (unit), which
        // projects to (320, 240 + (-0.2/0.9798)*1000) ≈ (320, 36).
        //
        // The J2000 vector matching this ray under identity is
        // ra = atan2(-0.2, 0), dec = arcsin(0.9798).
        let intrinsics = Intrinsics::placeholder(640, 480);
        let dec = 0.9798_f64.asin();
        let ra = (-0.2_f64).atan2(0.0); // = -π/2
        let star = IdentifiedStar {
            pixel_x: 320.0,
            pixel_y: 36.0,
            hr: 0,
            ra_rad: ra,
            dec_rad: dec,
            vmag: 0.0,
            pixel_residual: 0.0,
        };
        let alt = star_altitude(
            &star,
            &identity_attitude(),
            intrinsics,
            level_horizon(),
            Sigma::new(1e-5).unwrap_or(Sigma::ZERO),
        )
        .unwrap();
        assert!(
            alt.altitude.value > 0.0,
            "expected positive altitude (above horizon), got {}",
            alt.altitude.value
        );
        assert!(alt.altitude.value.is_finite());
        assert_eq!(alt.hr, 0);
    }

    #[test]
    fn star_below_horizon_is_skipped_in_batch() {
        // Two synthetic stars: one above the horizon, one below.
        // The above-horizon star has the same setup as the test
        // above; the below-horizon star uses positive Y in the
        // ray (which projects to image-y > 240 = below horizon).
        let intrinsics = Intrinsics::placeholder(640, 480);
        let dec_above = 0.9798_f64.asin();
        let ra_above = (-0.2_f64).atan2(0.0);
        let above = IdentifiedStar {
            pixel_x: 320.0,
            pixel_y: 36.0,
            hr: 1,
            ra_rad: ra_above,
            dec_rad: dec_above,
            vmag: 0.0,
            pixel_residual: 0.0,
        };
        // Below: ray (0, +0.5, 0.866) → image y = 240 + 577 ≈ 817
        // (off-frame downward, but well below horizon).
        let dec_below = 0.866_f64.asin();
        let ra_below = 0.5_f64.atan2(0.0); // = π/2
        let below = IdentifiedStar {
            pixel_x: 320.0,
            pixel_y: 817.0,
            hr: 2,
            ra_rad: ra_below,
            dec_rad: dec_below,
            vmag: 0.0,
            pixel_residual: 0.0,
        };
        let result = PlateSolveResult {
            attitude: Attitude {
                matrix: identity_attitude(),
            },
            identified: vec![above, below],
        };
        let alts = star_altitudes(
            &result,
            intrinsics,
            level_horizon(),
            Sigma::new(1e-5).unwrap_or(Sigma::ZERO),
        )
        .unwrap();
        // Only the above-horizon star should produce an
        // observation. The below-horizon one is silently skipped.
        assert_eq!(
            alts.len(),
            1,
            "expected 1 altitude (above-horizon star); got {} ({:?})",
            alts.len(),
            alts.iter()
                .map(|a| (a.hr, a.altitude.value))
                .collect::<Vec<_>>(),
        );
        assert_eq!(alts[0].hr, 1);
    }

    #[test]
    fn altitude_uncertainty_combines_horizon_and_per_star_sigma() {
        let intrinsics = Intrinsics::placeholder(640, 480);
        // Same above-horizon setup as the positive-altitude test.
        let dec = 0.9798_f64.asin();
        let ra = (-0.2_f64).atan2(0.0);
        let star = IdentifiedStar {
            pixel_x: 320.0,
            pixel_y: 36.0,
            hr: 0,
            ra_rad: ra,
            dec_rad: dec,
            vmag: 0.0,
            pixel_residual: 0.0,
        };
        let horizon_sigma = 2e-4;
        let per_star_sigma = 1e-4;
        let horizon = HorizonLine {
            altitude_sigma: Sigma::new(horizon_sigma).unwrap(),
            ..level_horizon()
        };
        let alt = star_altitude(
            &star,
            &identity_attitude(),
            intrinsics,
            horizon,
            Sigma::new(per_star_sigma).unwrap(),
        )
        .unwrap();
        // Combined sigma should be sqrt(horizon^2 + per_star^2) when
        // the per-star pixel residual is zero (no lens-model term).
        let expected = (horizon_sigma.powi(2) + per_star_sigma.powi(2)).sqrt();
        let got = alt.altitude.sigma.value();
        assert!(
            (got - expected).abs() < 1e-12,
            "expected combined sigma {expected}, got {got}",
        );
    }

    /// Helper: build an above-horizon identified star with a chosen
    /// per-star plate-solve pixel residual.
    fn star_with_residual(pixel_residual: f64) -> IdentifiedStar {
        let dec = 0.9798_f64.asin();
        let ra = (-0.2_f64).atan2(0.0);
        IdentifiedStar {
            pixel_x: 320.0,
            pixel_y: 36.0,
            hr: 0,
            ra_rad: ra,
            dec_rad: dec,
            vmag: 0.0,
            pixel_residual,
        }
    }

    fn altitude_sigma_with(
        intrinsics: Intrinsics,
        pixel_residual: f64,
        caller_sigma: f64,
        horizon_sigma: f64,
    ) -> f64 {
        let horizon = HorizonLine {
            altitude_sigma: Sigma::new(horizon_sigma).unwrap(),
            ..level_horizon()
        };
        let alt = star_altitude(
            &star_with_residual(pixel_residual),
            &identity_attitude(),
            intrinsics,
            horizon,
            Sigma::new(caller_sigma).unwrap(),
        )
        .unwrap();
        alt.altitude.sigma.value()
    }

    /// Doubling the focal length halves the angular contribution of a
    /// per-star pixel residual through the lens-model Jacobian
    /// (∂alt/∂pixel ≈ 1/fy). With horizon σ and caller σ both zero,
    /// the altitude σ is purely that lens-model term, so the σ ratio
    /// equals 1/2.
    #[test]
    fn doubling_focal_length_halves_per_star_sigma_contribution() {
        let base = Intrinsics::placeholder(640, 480);
        let mut doubled = base;
        doubled.fx *= 2.0;
        doubled.fy *= 2.0;

        let pixel_residual = 0.5;
        let sigma_base = altitude_sigma_with(base, pixel_residual, 0.0, 0.0);
        let sigma_doubled = altitude_sigma_with(doubled, pixel_residual, 0.0, 0.0);

        assert!(sigma_base > 0.0, "baseline lens-model sigma must be > 0");
        let ratio = sigma_doubled / sigma_base;
        assert!(
            (ratio - 0.5).abs() < 1e-6,
            "expected sigma to halve when focal length doubles: ratio = {ratio}",
        );
    }

    /// Doubling the per-star pixel residual doubles the lens-model
    /// σ contribution.
    #[test]
    fn doubling_pixel_residual_doubles_per_star_sigma_contribution() {
        let intrinsics = Intrinsics::placeholder(640, 480);
        let sigma_small = altitude_sigma_with(intrinsics, 0.5, 0.0, 0.0);
        let sigma_large = altitude_sigma_with(intrinsics, 1.0, 0.0, 0.0);
        assert!(sigma_small > 0.0);
        let ratio = sigma_large / sigma_small;
        assert!(
            (ratio - 2.0).abs() < 1e-6,
            "expected sigma to double when pixel residual doubles: ratio = {ratio}",
        );
    }

    /// Regression sanity: with typical placeholder intrinsics
    /// (fy = 1000) and a small per-star pixel residual (0.05 px)
    /// added on top of typical horizon and caller sigmas, the total
    /// σ stays within ±10% of the pre-fix value (which was just
    /// sqrt(horizon² + caller²)). The lens-model term is small at
    /// this residual (5e-5 rad), so the bound is comfortable.
    #[test]
    fn typical_residual_within_ten_percent_of_pre_fix_sigma() {
        let intrinsics = Intrinsics::placeholder(640, 480);
        let horizon_sigma = 2e-4_f64;
        let caller_sigma = 1e-4_f64;
        let pre_fix = (horizon_sigma.powi(2) + caller_sigma.powi(2)).sqrt();
        let got = altitude_sigma_with(intrinsics, 0.05, caller_sigma, horizon_sigma);
        let drift = (got - pre_fix).abs() / pre_fix;
        assert!(
            drift < 0.10,
            "expected sigma within ±10% of pre-fix value (pre = {pre_fix}, got = {got}, drift = {drift})",
        );
    }
}
