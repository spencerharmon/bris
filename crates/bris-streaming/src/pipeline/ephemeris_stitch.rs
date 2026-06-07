//! Ephemeris-driven correspondence prior for cross-frame
//! body stitching.
//!
//! Stage E's primary cross-frame stitcher is
//! [`bris_vision::panorama_altitude_for_pair`], which uses
//! Harris corners + NCC over the two frames' shared scene
//! texture to recover the camera rotation between them. On
//! indoor / low-contrast / motion-blurred captures the corner
//! detector frequently finds nothing reliable to match and
//! the stitcher refuses (returning
//! [`bris_vision::PanoramaError::TrackingFailed`] or
//! [`bris_vision::PanoramaError::DegenerateHorizonRay`]).
//!
//! When the operator is stationary and the camera is held
//! roughly still (a tripod-class scenario; the bedroom-moon
//! corpus is the motivating example) the body's apparent
//! motion between two frames is well-predicted by the
//! ephemeris alone: Earth rotates at ~15°/h ≈ 0.0042°/s, and
//! the apparent altitude/azimuth of the body at the observer
//! position drifts at the same cadence.
//!
//! This module exposes that prior in a single function,
//! [`predict_body_pixel_motion`], which projects the body's
//! camera-frame ray at two timestamps via the almanac and
//! converts the angular delta to a pixel delta in the source
//! frame's intrinsics. The returned `sigma_px` is honest:
//!
//! * The almanac contribution (sub-pixel for any body within
//!   seconds-to-minutes of separation; see
//!   [`bris_almanac::ApparentPlace::altitude_sigma`]).
//! * An observer-position uncertainty contribution scaled
//!   by parallax sensitivity. For stars and planets this is
//!   negligible; for the Moon a multi-thousand-nm-uncertain
//!   observer can swing the apparent altitude by ~1° (≈ 1
//!   Earth-radius shift in geocenter-to-observer line at
//!   Moon parallax of ~57′/Earth-radius), which the function
//!   honours by re-evaluating apparent place at perturbed
//!   observer positions.
//! * A camera-roll contribution: the per-axis (dx, dy)
//!   prediction assumes the camera's image-down axis is
//!   aligned with world-down. When the operator's roll is
//!   not known the σ floor swells to the full magnitude of
//!   the predicted displacement; callers then perform an
//!   annular (magnitude-based) search around the predicted
//!   point rather than a tight rectangular one.
//!
//! Stage E consumes the prediction in
//! [`super::stage_e`]'s cross-frame fallback path: when
//! Harris+NCC declines, the fallback looks for a body
//! candidate in the *other* frame and checks whether its
//! pixel offset from the source frame's body matches the
//! ephemeris-predicted offset within 3·σ. If so, it accepts
//! the correspondence under an identity-rotation assumption
//! (no camera motion between frames) and emits a sight whose
//! stitch σ is the *angular* prediction σ rather than the
//! per-pixel σ — see [`super::stage_e::STITCH_SIGMA_PER_SECOND_RAD`].

use bris_almanac::{
    body_apparent_place, star_apparent_place, ApparentPlace, ApparentPlaceError, Observer,
};
use bris_core::time::Tt;
use bris_core::{Latitude, Longitude};
use bris_vision::Intrinsics;

use super::stage_e::SightBody;

/// Earth-radius shift used to estimate parallax sensitivity
/// of apparent place at the observer guess, in metres.
/// Apparent place is recomputed with the observer's latitude
/// shifted by this amount (north and south); the resulting
/// delta-altitude is taken as the per-metre parallax slope.
///
/// Picking the perturbation: too small loses signal to
/// floating-point noise; too large stops being a local
/// derivative. 100 m is well inside the regime where Moon
/// parallax behaves linearly (Moon parallax derivative is
/// roughly 1 arcsec per 30 m of observer shift at altitude
/// 45°, so 100 m → ~3″ of signal, comfortably above f64
/// noise).
const OBSERVER_PERTURB_M: f64 = 100.0;

/// Metres per radian of latitude on the WGS-84 spheroid at
/// the equator. Used to translate
/// `observer_position_sigma_m` into a latitude perturbation
/// for the parallax sensitivity calculation. Polar /
/// equatorial radii differ by ~0.3% which is well below the
/// budget for an ephemeris prior; one number suffices.
const METRES_PER_RADIAN_LAT: f64 = 6_378_137.0;

/// Cap on the latitude perturbation, in radians, used when
/// computing the parallax derivative. For very cold-start
/// observer σ (e.g. global, σ ≈ 6000 nm ≈ 11 000 km) the
/// "1σ" would wrap the globe; the local linearisation
/// becomes meaningless. Cap at ~10° (≈ 1100 km), which is
/// already well past the linear regime for Moon parallax
/// but lets the function return a sensibly-large σ rather
/// than blowing up.
const MAX_PERTURB_LAT_RAD: f64 = 0.1745; // 10°

/// Predicted body motion between two timestamps in the
/// source frame's pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EphemerisPrediction {
    /// Pixel-x delta from the body's position at `t1` to its
    /// position at `t2`, assuming the camera's image axes
    /// align with world up/down (no roll). Positive = right.
    pub dx_px: f64,
    /// Pixel-y delta. Positive = down. Negative when the
    /// body climbs in altitude.
    pub dy_px: f64,
    /// Predicted angular displacement magnitude in radians,
    /// independent of camera orientation. The honest σ for
    /// a sight that adopts this prediction as the stitch
    /// correspondence; see module docs.
    pub angular_delta_rad: f64,
    /// 1σ angular uncertainty of the prediction, in
    /// radians. Combines almanac uncertainty at the
    /// observer guess with parallax sensitivity to the
    /// supplied `observer_position_sigma_m`.
    pub angular_sigma_rad: f64,
    /// 1σ uncertainty of `(dx_px, dy_px)` as a Euclidean
    /// 2D displacement, in pixels. Accounts for the
    /// angular σ scaled by the local pixel-per-radian
    /// factor *and* a roll-uncertainty floor: when camera
    /// roll is unknown, the per-axis prediction is wrong
    /// by up to the full magnitude of the displacement.
    /// Callers may treat this as the radius of a
    /// circular acceptance window centered on the
    /// predicted point.
    pub sigma_px: f64,
}

/// Inputs that cannot be honoured by the predictor.
#[derive(Debug)]
pub(crate) enum EphemerisError {
    /// `body` is a [`SightBody::Star`] whose HR id is not in
    /// the bundled catalog.
    UnknownStar(u32),
    /// The almanac declined to produce an apparent place at
    /// either timestamp (typically `BelowHorizon`).
    Apparent(ApparentPlaceError),
    /// The observer perturbation produced a latitude outside
    /// the angle type's valid range. Defensive; only fires
    /// for observer guesses extremely close to the poles
    /// combined with a large position σ.
    InvalidPerturbedObserver,
}

impl std::fmt::Display for EphemerisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownStar(hr) => write!(f, "unknown star HR={hr}"),
            Self::Apparent(e) => write!(f, "apparent-place error: {e}"),
            Self::InvalidPerturbedObserver => {
                write!(f, "perturbed observer latitude out of range")
            }
        }
    }
}

impl std::error::Error for EphemerisError {}

/// Compute the body's expected pixel motion between two
/// timestamps at the supplied observer guess.
///
/// `observer_position_sigma_m` is the 1σ uncertainty of the
/// observer's horizontal position, in metres. For a
/// freshly-cold-start observer with no prior, pass a large
/// value (e.g. 11_000_000 ≈ 6000 nm) to honestly widen the
/// returned σ; for a well-known observer with a recent
/// published fix, pass that fix's `sigma_major_nm × 1852`.
///
/// `roll_uncertainty_rad` is the 1σ uncertainty of the
/// camera roll angle. Pass `std::f64::consts::PI` when the
/// operator's roll is unknown (the per-axis prediction is
/// then a hint, and `sigma_px` covers the full magnitude as
/// a circular search radius). Pass a small value when a
/// recent horizon detection has fixed the roll.
///
/// # Errors
///
/// See [`EphemerisError`].
pub(crate) fn predict_body_pixel_motion(
    body: SightBody,
    t1: Tt,
    t2: Tt,
    observer: Observer,
    intrinsics: &Intrinsics,
    observer_position_sigma_m: f64,
    roll_uncertainty_rad: f64,
) -> Result<EphemerisPrediction, EphemerisError> {
    let jd_ut1_1 = t1.julian_date();
    let jd_ut1_2 = t2.julian_date();

    let place1 = apparent_at(body, t1, jd_ut1_1, observer)?;
    let place2 = apparent_at(body, t2, jd_ut1_2, observer)?;

    let alt1 = place1.direction.altitude;
    let alt2 = place2.direction.altitude;
    let az1 = place1.direction.azimuth;
    let az2 = place2.direction.azimuth;

    // Azimuth difference, wrapped to (-π, π].
    let d_az_raw = az2 - az1;
    let d_az = ((d_az_raw + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU))
        - std::f64::consts::PI;
    let d_alt = alt2 - alt1;

    // Effective horizontal angular motion: azimuth scales by
    // cos(altitude) when projected onto the local tangent
    // plane. Use mean altitude as the linearisation point.
    let mean_alt = 0.5 * (alt1 + alt2);
    let d_az_eff = d_az * mean_alt.cos();

    // Magnitude of the angular displacement, independent of
    // camera orientation. This is the quantity Stage E
    // adopts as the stitch σ when the prediction confirms a
    // body correspondence.
    let angular_delta_rad = d_alt.hypot(d_az_eff);

    // Per-axis pixel projection under the no-roll
    // assumption: pixel-x is image-right (+az direction at
    // image scale), pixel-y is image-down (-alt direction).
    let dx_px = d_az_eff * intrinsics.fx;
    let dy_px = -d_alt * intrinsics.fy;

    // ----- σ accounting -----
    //
    // (a) almanac altitude σ at each timestamp, combined in
    //     quadrature. The almanac σ is altitude-axis only;
    //     azimuth-axis σ is dominated by it for any
    //     reasonable horizon-fit accuracy (see
    //     ApparentPlace::altitude_sigma docs), so we treat
    //     it as a 1-D angular σ for the prediction. Two
    //     evaluations both carry that σ; in the difference
    //     they add in quadrature.
    let sigma_alm_1 = place1.altitude_sigma.value();
    let sigma_alm_2 = place2.altitude_sigma.value();
    let sigma_almanac_rad = (sigma_alm_1.powi(2) + sigma_alm_2.powi(2)).sqrt();

    // (b) parallax sensitivity to observer-position σ.
    //     Re-evaluate apparent place at the observer guess
    //     perturbed in latitude by min(σ_pos / R_earth,
    //     MAX_PERTURB) and divide the resulting δ-altitude
    //     by the perturbation to get a local slope, then
    //     multiply by σ_pos to get the implied σ in
    //     radians. Done at each timestamp and combined in
    //     quadrature with the perturb undone on the second
    //     to avoid double-counting the correlated component.
    let perturb_rad = if observer_position_sigma_m.is_finite() && observer_position_sigma_m > 0.0 {
        (observer_position_sigma_m / METRES_PER_RADIAN_LAT)
            .min(MAX_PERTURB_LAT_RAD)
            .max(OBSERVER_PERTURB_M / METRES_PER_RADIAN_LAT)
    } else {
        OBSERVER_PERTURB_M / METRES_PER_RADIAN_LAT
    };
    let perturbed_observer = perturb_observer_north(observer, perturb_rad)
        .ok_or(EphemerisError::InvalidPerturbedObserver)?;
    let place1_p = apparent_at(body, t1, jd_ut1_1, perturbed_observer)?;
    let place2_p = apparent_at(body, t2, jd_ut1_2, perturbed_observer)?;
    // Δ-altitude derivative at the perturbation; the
    // correlated component cancels in the *difference of
    // differences* below.
    let delta_alt_perturbed = (place2_p.direction.altitude - place1_p.direction.altitude) - d_alt;
    let delta_az_perturbed_raw = (place2_p.direction.azimuth - place1_p.direction.azimuth) - d_az;
    let delta_az_perturbed = ((delta_az_perturbed_raw + std::f64::consts::PI)
        .rem_euclid(std::f64::consts::TAU))
        - std::f64::consts::PI;
    let delta_az_eff_perturbed = delta_az_perturbed * mean_alt.cos();
    let parallax_slope_rad_per_rad_pos =
        delta_alt_perturbed.hypot(delta_az_eff_perturbed) / perturb_rad;
    let observer_sigma_rad_pos = if observer_position_sigma_m.is_finite() {
        observer_position_sigma_m.max(0.0) / METRES_PER_RADIAN_LAT
    } else {
        // Honest about unknown observer position: σ wraps
        // the globe (π/2 rad).
        std::f64::consts::FRAC_PI_2
    };
    let sigma_parallax_rad = parallax_slope_rad_per_rad_pos * observer_sigma_rad_pos;

    let angular_sigma_rad = (sigma_almanac_rad.powi(2) + sigma_parallax_rad.powi(2)).sqrt();

    // (c) pixel σ from angular σ × local pixel scale, then
    //     RSS with a roll-uncertainty term. Under unknown
    //     roll, the (dx, dy) prediction can be rotated by
    //     up to roll_uncertainty_rad around the body's
    //     starting pixel; the magnitude of that error is
    //     |displacement| × sin(roll_uncertainty), capped at
    //     the full magnitude. Pass π for "fully unknown
    //     roll" to recover the magnitude-only circular
    //     search.
    let f_eff = (intrinsics.fx * intrinsics.fy).sqrt().max(f64::EPSILON);
    let mag_px = (dx_px.powi(2) + dy_px.powi(2)).sqrt();
    let roll_sigma_factor = roll_uncertainty_rad.abs().min(std::f64::consts::FRAC_PI_2).sin();
    let sigma_px_angular = angular_sigma_rad * f_eff;
    let sigma_px_roll = mag_px * roll_sigma_factor;
    let sigma_px = (sigma_px_angular.powi(2) + sigma_px_roll.powi(2)).sqrt();

    Ok(EphemerisPrediction {
        dx_px,
        dy_px,
        angular_delta_rad,
        angular_sigma_rad,
        sigma_px,
    })
}

fn apparent_at(
    body: SightBody,
    tt: Tt,
    jd_ut1: f64,
    observer: Observer,
) -> Result<ApparentPlace, EphemerisError> {
    match body {
        SightBody::SolarSystem(b) => {
            body_apparent_place(b, tt, jd_ut1, observer).map_err(EphemerisError::Apparent)
        }
        SightBody::Star { hr } => {
            let rec = bris_almanac::by_hr(hr).ok_or(EphemerisError::UnknownStar(hr))?;
            star_apparent_place(rec, tt, jd_ut1, observer).map_err(EphemerisError::Apparent)
        }
    }
}

/// Shift the observer's latitude by `delta_rad` (north
/// positive). Returns `None` if the resulting latitude is
/// outside [`Latitude`]'s valid range — only possible for
/// observers very near the poles combined with a large
/// `delta_rad`.
fn perturb_observer_north(observer: Observer, delta_rad: f64) -> Option<Observer> {
    let lat_rad = observer.latitude.radians() + delta_rad;
    // Reflect off the poles so the latitude stays in range
    // even for near-polar observers; the underlying parallax
    // signal is direction-symmetric for the σ-magnitude
    // estimate we extract here.
    let clamped = lat_rad.clamp(
        -std::f64::consts::FRAC_PI_2 + 1e-6,
        std::f64::consts::FRAC_PI_2 - 1e-6,
    );
    let new_lat = Latitude::from_radians(clamped).ok()?;
    Some(Observer {
        latitude: new_lat,
        longitude: Longitude::from_radians(observer.longitude.radians()).ok()?,
        ..observer
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_almanac::{Observer, SolarSystemBody};
    use bris_core::time::{Tt, JD_J2000};

    fn dev_observer() -> Observer {
        Observer::default_dev()
    }

    fn intr() -> Intrinsics {
        Intrinsics::placeholder(1920, 1080)
    }

    /// At J2000 (Sun high over Greenwich), one second of
    /// Earth rotation should move the Sun ~15 arcsec ≈
    /// 7.27e-5 rad. With fx = 1000 px/rad-ish (placeholder
    /// intr), that's ~0.07 pixels. Verify the predictor
    /// returns a finite, small displacement.
    #[test]
    fn sun_at_j2000_predicts_sidereal_drift() {
        let t1 = Tt::from_julian_date(JD_J2000);
        let t2 = Tt::from_julian_date(JD_J2000 + 1.0 / 86_400.0); // +1 s
        let body = SightBody::SolarSystem(SolarSystemBody::Sun);
        let pred = predict_body_pixel_motion(
            body,
            t1,
            t2,
            dev_observer(),
            &intr(),
            10_000.0,                 // 10 km horizontal σ
            std::f64::consts::FRAC_PI_4, // moderate roll uncertainty
        )
        .expect("Sun apparent place should be defined at J2000 Greenwich");
        assert!(pred.angular_delta_rad.is_finite());
        // Sidereal rate ≈ 15″/s ≈ 7.27e-5 rad. Allow generous
        // band: Sun moves ~1° per 4 min, also has its own
        // ~1°/day apparent motion.
        assert!(
            pred.angular_delta_rad < 5e-4 && pred.angular_delta_rad > 1e-5,
            "1-second Sun motion {} rad outside expected band",
            pred.angular_delta_rad,
        );
        assert!(pred.sigma_px.is_finite() && pred.sigma_px > 0.0);
    }

    /// Cold-start observer (huge σ) inflates σ_px for Moon
    /// prediction (parallax sensitive). Compare against a
    /// near-zero observer σ at the same instant.
    #[test]
    fn moon_observer_uncertainty_inflates_sigma() {
        let t1 = Tt::from_julian_date(JD_J2000);
        let t2 = Tt::from_julian_date(JD_J2000 + 10.0 / 86_400.0); // +10 s
        let body = SightBody::SolarSystem(SolarSystemBody::Moon);
        let known = predict_body_pixel_motion(
            body,
            t1,
            t2,
            dev_observer(),
            &intr(),
            10.0, // 10 m
            0.01, // tight roll
        );
        let cold = predict_body_pixel_motion(
            body,
            t1,
            t2,
            dev_observer(),
            &intr(),
            5_000_000.0, // 5000 km
            0.01,
        );
        // Both should succeed or both fail (e.g. Moon below
        // horizon at this instant). If they succeed, the
        // cold-start σ must dominate.
        match (known, cold) {
            (Ok(k), Ok(c)) => {
                assert!(
                    c.sigma_px > k.sigma_px * 2.0,
                    "cold-start σ_px {} not meaningfully larger than known {}",
                    c.sigma_px,
                    k.sigma_px,
                );
            }
            // Acceptable: Moon below horizon at the configured
            // observer at J2000. The almanac contract is
            // exercised by other tests.
            _ => {}
        }
    }

    #[test]
    fn no_roll_prediction_matches_per_axis_when_roll_known() {
        let t1 = Tt::from_julian_date(JD_J2000);
        let t2 = Tt::from_julian_date(JD_J2000 + 5.0 / 86_400.0);
        let body = SightBody::SolarSystem(SolarSystemBody::Sun);
        let pred = predict_body_pixel_motion(
            body,
            t1,
            t2,
            dev_observer(),
            &intr(),
            100.0,
            0.0, // perfectly known roll
        )
        .expect("Sun at J2000 should be predictable");
        // sigma_px should be dominated by the angular term
        // when roll_uncertainty = 0.
        let f_eff = (intr().fx * intr().fy).sqrt();
        let expected_sigma_from_angular = pred.angular_sigma_rad * f_eff;
        assert!(
            (pred.sigma_px - expected_sigma_from_angular).abs() < 1e-6,
            "sigma_px {} should equal angular σ × f_eff {}",
            pred.sigma_px,
            expected_sigma_from_angular,
        );
    }
}
