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
/// quadrature to the horizon-fit σ for each star. Reasonable values
/// come from the plate-solve refinement RMS (typically a few
/// arcseconds with calibrated intrinsics).
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
    let altitude = measure_altitude_from_ray(intrinsics, horizon, body_ray, per_star_sigma)?;
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
        // Combined sigma should be sqrt(horizon^2 + per_star^2).
        let expected = (horizon_sigma.powi(2) + per_star_sigma.powi(2)).sqrt();
        let got = alt.altitude.sigma.value();
        assert!(
            (got - expected).abs() < 1e-12,
            "expected combined sigma {expected}, got {got}",
        );
    }
}
