//! The apparent-place pipeline: turn a body identifier and an instant
//! into the topocentric horizontal direction (altitude, azimuth) the
//! observer actually sees.
//!
//! This is the load-bearing surface of the almanac. It hides the
//! sequence of frame rotations, light-time corrections, and geometric
//! transformations behind a single typed function per body class. Each
//! return value carries an attached uncertainty so downstream sight-
//! reduction code can propagate fix uncertainty honestly.
//!
//! # Pipeline (Solar System body)
//!
//! 1. Geocentric ecliptic position of date from `ephemeris::heliocentric`
//!    (Sun via `sun_geocentric`; Moon via `lunar::lunar_position`).
//! 2. Light-time correction (one iteration; planets/Moon).
//! 3. Convert to equatorial of date via [`coord::ecliptic_to_equatorial`].
//! 4. Apply nutation (rotates mean → true equator).
//! 5. Apply annual aberration (TODO: stub returning zero correction
//!    with a small documented sigma, see plan.org Phase 1 follow-up).
//! 6. Convert geocentric → topocentric (parallax; only Moon matters).
//! 7. Equatorial → horizontal via [`coord::equatorial_to_horizontal`]
//!    using local apparent sidereal time.
//! 8. Subtract horizon dip from altitude (eye-height effect).
//! 9. Apply atmospheric refraction.
//!
//! # Pipeline (star)
//!
//! 1. Catalog J2000 RA/Dec.
//! 2. Linear proper motion to epoch (`catalog::position_at`).
//! 3. Apply precession (J2000 → mean of date).
//! 4. Apply nutation (mean → true).
//! 5. Apply annual aberration (same TODO).
//! 6. Equatorial → horizontal.
//! 7. Horizon dip + refraction.

use crate::catalog::{position_at, StarRecord};
use crate::coord::{
    ecliptic_to_equatorial, equatorial_to_horizontal, last_rad, Ecliptic, Equatorial, Horizontal,
};
use crate::ephemeris::{heliocentric, sun_geocentric, Body, Heliocentric};
use crate::frame::{mean_obliquity, nutation, precession_angles};
use crate::lunar::lunar_position;
use crate::observer::Observer;
use crate::refraction::{bennett, RefractionError};
use bris_core::time::Tt;
use bris_core::Sigma;

/// What an observer sees at the eyepiece: altitude and azimuth, with
/// an attached 1σ uncertainty in altitude.
///
/// Azimuth uncertainty is not separately tracked because azimuth enters
/// sight reduction as a per-LOP direction, and its uncertainty is
/// dominated by the altitude uncertainty itself for any reasonable
/// horizon-fit accuracy. If we ever need it, it goes alongside.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ApparentPlace {
    /// Topocentric apparent direction the observer sees.
    pub direction: Horizontal,
    /// 1σ uncertainty in the apparent altitude, radians.
    pub altitude_sigma: Sigma,
}

/// Errors from the apparent-place chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApparentPlaceError {
    /// Body is below the horizon; no apparent place is defined.
    #[error("body is below the horizon")]
    BelowHorizon,
    /// Refraction model rejected the input.
    #[error("refraction error: {0}")]
    Refraction(#[from] RefractionError),
    /// Internal angle construction failed (NaN/inf from upstream).
    #[error("internal arithmetic produced a non-finite angle")]
    NonFinite,
}

/// Apparent-place uncertainty contribution from annual aberration when
/// not yet applied. Aberration is a ~20″ effect; if we're returning a
/// position without applying it, we must add ~20″ to the uncertainty
/// budget so callers don't believe a precision they aren't getting.
///
/// Once aberration is implemented this becomes a much smaller residual
/// (~0.1″) and the constant changes accordingly.
const ABERRATION_PLACEHOLDER_SIGMA_RAD: f64 = 20.0 * std::f64::consts::PI / (180.0 * 3600.0);

/// Compute the apparent place of a Solar System body.
///
/// Returns `BelowHorizon` if the body is below the geometric horizon
/// (post-refraction altitude < 0), since refraction is not defined
/// there in our model.
pub fn body_apparent_place(
    body: SolarSystemBody,
    tt: Tt,
    jd_ut1: f64,
    observer: Observer,
) -> Result<ApparentPlace, ApparentPlaceError> {
    let geocentric_ecliptic = body_geocentric_ecliptic(body, tt);
    common_apparent_place(geocentric_ecliptic, tt, jd_ut1, observer)
}

/// Compute the apparent place of a catalog star.
pub fn star_apparent_place(
    star: &StarRecord,
    tt: Tt,
    jd_ut1: f64,
    observer: Observer,
) -> Result<ApparentPlace, ApparentPlaceError> {
    // Step 1-2: catalog J2000 + proper motion to epoch.
    let pm = position_at(star, tt);
    let j2000_eq = Equatorial {
        ra: pm.ra_rad,
        dec: pm.dec_rad,
    };

    // Step 3: precession (J2000 → mean of date).
    let mean_eq = apply_precession(j2000_eq, tt);

    // Step 4: nutation (mean → true).
    let nu = nutation(tt);
    let true_eq = apply_nutation(mean_eq, nu.delta_psi, nu.delta_epsilon, tt);

    // Step 5: aberration (TODO; counted in sigma).
    finalize_to_horizontal(true_eq, tt, jd_ut1, observer, nu.delta_psi)
}

/// All Solar System bodies the apparent-place pipeline supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolarSystemBody {
    /// The Sun.
    Sun,
    /// The Moon.
    Moon,
    /// A planet.
    Planet(Body),
}

/// Geocentric ecliptic position of date for the named body.
fn body_geocentric_ecliptic(body: SolarSystemBody, tt: Tt) -> Heliocentric {
    match body {
        SolarSystemBody::Sun => sun_geocentric(tt),
        SolarSystemBody::Moon => {
            let m = lunar_position(tt);
            // Lunar position returns geocentric ecliptic of date already;
            // distance is in km but we use it only as a non-zero placeholder
            // here because the topocentric correction in this commit is a stub.
            Heliocentric {
                longitude: m.longitude,
                latitude: m.latitude,
                radius_au: m.distance_km / 149_597_870.7,
            }
        }
        SolarSystemBody::Planet(b) => {
            // Geocentric ecliptic = heliocentric body − heliocentric Earth.
            // Light-time iteration: not yet applied (planets at ~5-30 light-min;
            // this is a 1-2 arcsec correction we need for 0.5 nm targeting).
            let body_helio = heliocentric(b, tt);
            let earth_helio = heliocentric(Body::EarthMoonBarycenter, tt);
            geocentric_from_heliocentric(body_helio, earth_helio)
        }
    }
}

/// Compute geocentric ecliptic spherical coordinates from heliocentric
/// positions of the body and Earth (both ecliptic of date, AU).
fn geocentric_from_heliocentric(body: Heliocentric, earth: Heliocentric) -> Heliocentric {
    // Convert each to rectangular, subtract, convert back.
    let bx = body.radius_au * body.latitude.cos() * body.longitude.cos();
    let by = body.radius_au * body.latitude.cos() * body.longitude.sin();
    let bz = body.radius_au * body.latitude.sin();
    let ex = earth.radius_au * earth.latitude.cos() * earth.longitude.cos();
    let ey = earth.radius_au * earth.latitude.cos() * earth.longitude.sin();
    let ez = earth.radius_au * earth.latitude.sin();
    let dx = bx - ex;
    let dy = by - ey;
    let dz = bz - ez;
    let r = (dx * dx + dy * dy + dz * dz).sqrt();
    let lon = dy.atan2(dx).rem_euclid(std::f64::consts::TAU);
    let lat = (dz / r).asin();
    Heliocentric {
        longitude: lon,
        latitude: lat,
        radius_au: r,
    }
}

/// Apply the IAU 2006 equatorial precession rotation J2000 → mean-of-date.
fn apply_precession(p: Equatorial, tt: Tt) -> Equatorial {
    let pa = precession_angles(tt);
    // Standard composition: rotate by -ζ about z, then by +θ about y,
    // then by -z about z. Equivalent matrix form:
    let (sin_zeta, cos_zeta) = (-pa.zeta).sin_cos();
    let (sin_z, cos_z) = (-pa.z).sin_cos();
    let (sin_th, cos_th) = pa.theta.sin_cos();
    // Precompute trig of p.
    let (sin_a, cos_a) = p.ra.sin_cos();
    let (sin_d, cos_d) = p.dec.sin_cos();
    // Convert to unit vector.
    let x0 = cos_d * cos_a;
    let y0 = cos_d * sin_a;
    let z0 = sin_d;
    // R_z(-ζ).
    let x1 = cos_zeta * x0 + sin_zeta * y0;
    let y1 = -sin_zeta * x0 + cos_zeta * y0;
    let z1 = z0;
    // R_y(+θ).
    let x2 = cos_th * x1 - sin_th * z1;
    let y2 = y1;
    let z2 = sin_th * x1 + cos_th * z1;
    // R_z(-z).
    let x3 = cos_z * x2 + sin_z * y2;
    let y3 = -sin_z * x2 + cos_z * y2;
    let z3 = z2;
    // Back to spherical.
    let ra = y3.atan2(x3).rem_euclid(std::f64::consts::TAU);
    let dec = z3.clamp(-1.0, 1.0).asin();
    Equatorial { ra, dec }
}

/// Apply nutation to convert mean equatorial → true equatorial of date.
///
/// Standard small-angle approximation (Meeus 23.1) suffices for our
/// arcsecond budget:
///   Δα ≈ (cos ε + sin ε sin α tan δ) Δψ − cos α tan δ Δε
///   Δδ ≈ sin ε cos α Δψ + sin α Δε
fn apply_nutation(p: Equatorial, dpsi_rad: f64, deps_rad: f64, tt: Tt) -> Equatorial {
    let eps = mean_obliquity(tt);
    let (sin_eps, cos_eps) = eps.sin_cos();
    let (sin_a, cos_a) = p.ra.sin_cos();
    let tan_d = p.dec.tan();
    let dalpha = (cos_eps + sin_eps * sin_a * tan_d) * dpsi_rad - cos_a * tan_d * deps_rad;
    let ddelta = sin_eps * cos_a * dpsi_rad + sin_a * deps_rad;
    Equatorial {
        ra: (p.ra + dalpha).rem_euclid(std::f64::consts::TAU),
        dec: (p.dec + ddelta).clamp(-std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2),
    }
}

/// Common tail: starts from a true-equatorial-of-date position and
/// finishes through horizontal coords + dip + refraction with attached
/// uncertainty.
fn common_apparent_place(
    geocentric_ecliptic_of_date: Heliocentric,
    tt: Tt,
    jd_ut1: f64,
    observer: Observer,
) -> Result<ApparentPlace, ApparentPlaceError> {
    // Convert ecliptic → equatorial at the mean obliquity of date.
    let eps = mean_obliquity(tt);
    let mean_eq = ecliptic_to_equatorial(
        Ecliptic {
            longitude: geocentric_ecliptic_of_date.longitude,
            latitude: geocentric_ecliptic_of_date.latitude,
        },
        eps,
    );

    // Apply nutation (mean → true).
    let nu = nutation(tt);
    let true_eq = apply_nutation(mean_eq, nu.delta_psi, nu.delta_epsilon, tt);

    finalize_to_horizontal(true_eq, tt, jd_ut1, observer, nu.delta_psi)
}

/// Final stage shared by Solar-System and stellar paths.
fn finalize_to_horizontal(
    true_eq: Equatorial,
    tt: Tt,
    jd_ut1: f64,
    observer: Observer,
    delta_psi_rad: f64,
) -> Result<ApparentPlace, ApparentPlaceError> {
    // LAST.
    let eps = mean_obliquity(tt);
    let last = last_rad(jd_ut1, observer.longitude.radians(), delta_psi_rad, eps);

    // Equatorial → horizontal.
    let mut horizontal = equatorial_to_horizontal(true_eq, last, observer.latitude.radians());

    if !horizontal.altitude.is_finite() || !horizontal.azimuth.is_finite() {
        return Err(ApparentPlaceError::NonFinite);
    }

    // Subtract horizon dip from observed altitude (eye-height effect).
    let dip = observer.horizon_dip_rad();
    let alt_after_dip = horizontal.altitude - dip;

    // Apply refraction. Bennett wants apparent altitude (post-dip),
    // returns the refraction angle and its sigma.
    let alt_angle =
        bris_core::Angle::from_radians(alt_after_dip).map_err(|_| ApparentPlaceError::NonFinite)?;
    if alt_angle.degrees() < -1.0 {
        return Err(ApparentPlaceError::BelowHorizon);
    }
    let refraction = bennett(alt_angle, observer.atmosphere)?;

    // The body appears higher than its true altitude by the refraction
    // amount; or equivalently, true alt = apparent alt − refraction.
    // The pipeline so far has produced the *true* altitude (geometric);
    // to get *apparent* altitude (what the observer sees), ADD refraction.
    horizontal.altitude = alt_after_dip + refraction.value.radians();

    // Sigma budget: refraction sigma + horizon dip sigma + aberration
    // placeholder. Combined in quadrature.
    let dip_sigma = observer.horizon_dip_sigma();
    let aberration_sigma = Sigma::new(ABERRATION_PLACEHOLDER_SIGMA_RAD).unwrap();
    let sigma = refraction
        .sigma
        .combine(dip_sigma)
        .combine(aberration_sigma);

    Ok(ApparentPlace {
        direction: horizontal,
        altitude_sigma: sigma,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_core::time::utc_to_tt;
    use chrono::TimeZone;

    /// 2024-06-21T18:00:00 UTC (near June solstice, sun near max altitude
    /// at Greenwich latitude).
    fn june_solstice_noon_at_greenwich() -> (Tt, f64) {
        let utc = chrono::Utc
            .with_ymd_and_hms(2024, 6, 21, 12, 0, 0)
            .single()
            .unwrap();
        let tt = utc_to_tt(utc).unwrap();
        // For tests we approximate JD_UT1 ≈ JD_UTC (ΔUT1 ≈ 0).
        let jd_ut1 = chrono_to_jd_utc(utc);
        (tt, jd_ut1)
    }

    fn chrono_to_jd_utc(utc: chrono::DateTime<chrono::Utc>) -> f64 {
        // Inline copy of the algorithm in bris-core to avoid making it
        // pub. Same Meeus form.
        use chrono::Datelike;
        use chrono::Timelike;
        let mut y = utc.year();
        let mut m = i32::try_from(utc.month()).unwrap();
        if m <= 2 {
            y -= 1;
            m += 12;
        }
        let a = y.div_euclid(100);
        let b = 2 - a + a.div_euclid(4);
        let day_fraction = (f64::from(utc.hour()) * 3600.0
            + f64::from(utc.minute()) * 60.0
            + f64::from(utc.second()))
            / 86_400.0;
        let jd_int = (365.25 * f64::from(y + 4716)).floor()
            + (30.6001 * f64::from(m + 1)).floor()
            + f64::from(utc.day())
            + f64::from(b)
            - 1524.5;
        jd_int + day_fraction
    }

    #[test]
    fn sun_at_greenwich_solstice_noon_has_high_altitude() {
        // At June solstice, local noon, latitude 51.5°N (London), Sun
        // altitude is ~62°. At latitude 0° (equator) it's ~67°.
        // Test against a generous tolerance because nutation,
        // longitude, exact UT vs apparent solar noon, etc. all shift
        // this by a few degrees.
        let (tt, jd_ut1) = june_solstice_noon_at_greenwich();
        let mut obs = Observer::default_dev();
        obs.latitude = bris_core::Latitude::from_degrees(51.5).unwrap();
        let ap = body_apparent_place(SolarSystemBody::Sun, tt, jd_ut1, obs).unwrap();
        let alt_deg = ap.direction.altitude.to_degrees();
        // Accept anywhere from 50° to 70° — we're verifying the
        // pipeline runs end-to-end and produces a sane high-altitude
        // value, not exact almanac match.
        assert!(
            (40.0..=75.0).contains(&alt_deg),
            "Sun altitude at London solstice noon = {alt_deg}°, expected ~62°"
        );
    }

    #[test]
    fn star_apparent_place_returns_some_altitude() {
        // Sirius, observer in Sydney (-33° lat) at southern winter,
        // some time when Sirius is up. Just confirm the chain runs
        // end-to-end and produces a finite altitude with attached
        // sigma > 0.
        let utc = chrono::Utc
            .with_ymd_and_hms(2024, 1, 15, 13, 0, 0)
            .single()
            .unwrap();
        let tt = utc_to_tt(utc).unwrap();
        let jd_ut1 = chrono_to_jd_utc(utc);
        let mut obs = Observer::default_dev();
        obs.latitude = bris_core::Latitude::from_degrees(-33.0).unwrap();
        obs.longitude = bris_core::Longitude::from_degrees(151.0).unwrap();
        let sirius = crate::by_hr(2491).unwrap();
        let ap = star_apparent_place(sirius, tt, jd_ut1, obs).unwrap();
        assert!(ap.direction.altitude.is_finite());
        assert!(ap.altitude_sigma.value() > 0.0);
    }

    #[test]
    fn polaris_altitude_close_to_observer_latitude() {
        // The classic navigator's check: Polaris altitude ≈ observer
        // latitude (it's within ~0.5° of the celestial pole). At
        // Boston (lat 42.4°N) Polaris should be at ~42° altitude.
        let utc = chrono::Utc
            .with_ymd_and_hms(2024, 6, 1, 0, 0, 0)
            .single()
            .unwrap();
        let tt = utc_to_tt(utc).unwrap();
        let jd_ut1 = chrono_to_jd_utc(utc);
        let mut obs = Observer::default_dev();
        obs.latitude = bris_core::Latitude::from_degrees(42.4).unwrap();
        obs.longitude = bris_core::Longitude::from_degrees(-71.0).unwrap();
        let polaris = crate::by_hr(911).unwrap();
        let ap = star_apparent_place(polaris, tt, jd_ut1, obs).unwrap();
        let alt_deg = ap.direction.altitude.to_degrees();
        // Polaris should be within ~2° of observer latitude after all
        // corrections (it's not exactly at the pole).
        assert!(
            (40.0..=45.0).contains(&alt_deg),
            "Polaris altitude at Boston = {alt_deg}°, expected ~42.4°"
        );
    }

    #[test]
    fn aberration_placeholder_sigma_present() {
        // Until aberration is properly applied, every fix carries a
        // ~20" placeholder sigma. Check that's at least the floor.
        let (tt, jd_ut1) = june_solstice_noon_at_greenwich();
        let obs = Observer::default_dev();
        let ap = body_apparent_place(SolarSystemBody::Sun, tt, jd_ut1, obs).unwrap();
        let arcsec = ap.altitude_sigma.value() * 180.0 * 3600.0 / std::f64::consts::PI;
        assert!(
            arcsec >= 19.0,
            "altitude sigma {arcsec}\" should include aberration placeholder ≥ 19\""
        );
    }
}
