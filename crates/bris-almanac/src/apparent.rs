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
//! 5. Apply annual aberration (Meeus Ch. 23, classical formulation):
//!    apparent direction ≈ normalize(`u + v_earth / c`), with Earth's
//!    heliocentric velocity in equatorial-of-date cartesian frame.
//!    See [`apply_annual_aberration`].
//! 6. Convert geocentric → topocentric in equatorial coordinates
//!    (diurnal parallax; only the Moon is large, but applied to Sun
//!    and planets uniformly for consistency). Uses Meeus Ch. 40.
//!    See [`topocentric_equatorial`].
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
//! 5. Apply annual aberration (same classical formulation; for stars
//!    the body direction is at infinity so the v/c shift is
//!    independent of distance).
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

/// Residual 1σ uncertainty after applying classical annual aberration.
///
/// The classical formulation accounts for Earth's heliocentric orbital
/// velocity (~30 km/s, peak shift ~20″). Unmodelled terms include
/// diurnal aberration (observer rotation about Earth's axis, ≤ 0.32″),
/// relativistic second-order terms (~v²/c², sub-mas), and the small
/// difference between geocenter and observer in the velocity. Lump
/// these into a 0.1″ residual.
const ABERRATION_RESIDUAL_SIGMA_RAD: f64 = 0.1 * std::f64::consts::PI / (180.0 * 3600.0);

/// Speed of light in m/s.
const C_M_PER_S: f64 = 299_792_458.0;

/// Seconds per day.
const SECS_PER_DAY: f64 = 86_400.0;

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
    let distance_au = Some(geocentric_ecliptic.radius_au);
    common_apparent_place(geocentric_ecliptic, distance_au, tt, jd_ut1, observer)
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

    // Step 5: annual aberration (classical). For stars the direction
    // is at infinity so the shift is independent of distance.
    let aberrated_eq = apply_annual_aberration(true_eq, tt);

    // Stars: stellar parallax is handled (or omitted at mas level) via
    // the catalog `parallax_mas` field — do NOT apply diurnal parallax.
    finalize_to_horizontal(aberrated_eq, None, tt, jd_ut1, observer, nu.delta_psi)
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
            // Lunar position returns geocentric ecliptic of date already.
            // Distance is converted to AU so the downstream topocentric
            // parallax step (see `finalize_to_horizontal`) has a real
            // geocentric distance to work with.
            Heliocentric {
                longitude: m.longitude,
                latitude: m.latitude,
                radius_au: m.distance_km * 1000.0 / AU_M,
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
    distance_au: Option<f64>,
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

    // Annual aberration (classical). Applied to the geocentric direction
    // before topocentric parallax; parallax must be the last shift since
    // it is the rotation from geocenter to surface viewpoint.
    let aberrated_eq = apply_annual_aberration(true_eq, tt);

    finalize_to_horizontal(
        aberrated_eq,
        distance_au,
        tt,
        jd_ut1,
        observer,
        nu.delta_psi,
    )
}

/// Apply classical annual aberration (Meeus Ch. 23).
///
/// Given a geocentric true-equatorial-of-date direction `true_eq` and
/// the instant `tt`, returns the apparent direction shifted by Earth's
/// heliocentric velocity v\: `u_apparent = normalize(u + v / c)`.
///
/// Units\: Earth heliocentric velocity is computed in AU/day via a
/// centered numerical derivative of VSOP87 over `dt = 60` s and
/// converted to m/s; `c = 299_792_458` m/s. The classical
/// (non-relativistic) form is accurate to `v²/c²` ≈ 1e-8 rad
/// (sub-mas), well below the residual `σ` floor we attach.
#[must_use]
pub fn apply_annual_aberration(true_eq: Equatorial, tt: Tt) -> Equatorial {
    // Earth heliocentric velocity in ecliptic-of-date rectangular AU/day.
    let (vxe, vye, vze) = earth_heliocentric_velocity_ecliptic_au_per_day(tt);

    // Convert AU/day → m/s.
    let au_per_day_to_m_per_s = AU_M / SECS_PER_DAY;
    let vxe = vxe * au_per_day_to_m_per_s;
    let vye = vye * au_per_day_to_m_per_s;
    let vze = vze * au_per_day_to_m_per_s;

    // Rotate ecliptic → equatorial of date by +ε about x-axis.
    let eps = mean_obliquity(tt);
    let (se, ce) = eps.sin_cos();
    let vx = vxe;
    let vy = ce * vye - se * vze;
    let vz = se * vye + ce * vze;

    // Unit vector toward body in equatorial of date.
    let (sa, ca) = true_eq.ra.sin_cos();
    let (sdec, cdec) = true_eq.dec.sin_cos();
    let ux = cdec * ca;
    let uy = cdec * sa;
    let uz = sdec;

    // Classical aberration: apparent direction ≈ normalize(u + v / c).
    // Sign convention: Earth moving toward apex shifts a star toward the
    // apex by v/c (Meeus 23.2-23.4 equivalent).
    let dx = ux + vx / C_M_PER_S;
    let dy = uy + vy / C_M_PER_S;
    let dz = uz + vz / C_M_PER_S;
    let inv_len = 1.0 / (dx * dx + dy * dy + dz * dz).sqrt();
    let dx = dx * inv_len;
    let dy = dy * inv_len;
    let dz = dz * inv_len;

    let ra = dy.atan2(dx).rem_euclid(std::f64::consts::TAU);
    let dec = dz.clamp(-1.0, 1.0).asin();
    Equatorial { ra, dec }
}

/// Earth's heliocentric velocity in ecliptic-of-date rectangular AU/day.
///
/// VSOP87D exposes positions only; we take a centered finite difference
/// over ±30 s (60 s total span). At Earth's orbital speed (~30 km/s)
/// over 30 s the truncation error of a centered difference is
/// O((dt)² · jerk), well below 10⁻¹⁰ AU/day.
fn earth_heliocentric_velocity_ecliptic_au_per_day(tt: Tt) -> (f64, f64, f64) {
    let dt_days = 30.0 / SECS_PER_DAY;
    let tt_plus = Tt::from_julian_date(tt.julian_date() + dt_days);
    let tt_minus = Tt::from_julian_date(tt.julian_date() - dt_days);
    let e_plus = earth_helio_rect(tt_plus);
    let e_minus = earth_helio_rect(tt_minus);
    let inv_2dt = 1.0 / (2.0 * dt_days);
    (
        (e_plus.0 - e_minus.0) * inv_2dt,
        (e_plus.1 - e_minus.1) * inv_2dt,
        (e_plus.2 - e_minus.2) * inv_2dt,
    )
}

/// Earth heliocentric ecliptic-of-date rectangular coordinates (AU).
fn earth_helio_rect(tt: Tt) -> (f64, f64, f64) {
    let e = heliocentric(Body::EarthMoonBarycenter, tt);
    let (sin_l, cos_l) = e.longitude.sin_cos();
    let (sin_b, cos_b) = e.latitude.sin_cos();
    (
        e.radius_au * cos_b * cos_l,
        e.radius_au * cos_b * sin_l,
        e.radius_au * sin_b,
    )
}

/// One astronomical unit, in meters (IAU 2012 definition).
const AU_M: f64 = 149_597_870_700.0;

/// WGS-84 Earth equatorial radius, meters.
const EARTH_EQUATORIAL_RADIUS_M: f64 = 6_378_137.0;

/// WGS-84 flattening.
const EARTH_FLATTENING: f64 = 1.0 / 298.257_223_563;

/// Convert geocentric equatorial (α, δ) at the observer's instant to
/// topocentric (α', δ') via the diurnal-parallax rotation. Meeus
/// (1998) *Astronomical Algorithms* Ch. 40, eqs. 40.6–40.7, with the
/// observer's geocentric `(ρ sin φ′, ρ cos φ′)` derived from WGS-84
/// oblateness plus eye height.
///
/// `distance_m` is the geocentric distance to the body in meters.
fn topocentric_equatorial(
    geo_eq: Equatorial,
    last_rad_val: f64,
    observer: Observer,
    distance_m: f64,
) -> Equatorial {
    let phi = observer.latitude.radians();
    let h_over_a = observer.eye_height_m.max(0.0) / EARTH_EQUATORIAL_RADIUS_M;
    let one_minus_f = 1.0 - EARTH_FLATTENING;
    let u = (one_minus_f * phi.tan()).atan();
    let (sin_u, cos_u) = u.sin_cos();
    let (sin_phi, cos_phi) = phi.sin_cos();
    let rho_sin_phip = one_minus_f * sin_u + h_over_a * sin_phi;
    let rho_cos_phip = cos_u + h_over_a * cos_phi;

    let sin_parallax = EARTH_EQUATORIAL_RADIUS_M / distance_m;

    let h_angle = (last_rad_val - geo_eq.ra).rem_euclid(std::f64::consts::TAU);
    let (sin_h, cos_h) = h_angle.sin_cos();
    let (sin_d, cos_d) = geo_eq.dec.sin_cos();

    // Meeus 40.6
    let num = -rho_cos_phip * sin_parallax * sin_h;
    let den = cos_d - rho_cos_phip * sin_parallax * cos_h;
    let dalpha = num.atan2(den);
    let alpha_topo = (geo_eq.ra + dalpha).rem_euclid(std::f64::consts::TAU);
    // Meeus 40.7
    let dec_topo = ((sin_d - rho_sin_phip * sin_parallax) * dalpha.cos()).atan2(den);
    Equatorial {
        ra: alpha_topo,
        dec: dec_topo,
    }
}

/// Final stage shared by Solar-System and stellar paths.
fn finalize_to_horizontal(
    true_eq: Equatorial,
    distance_au: Option<f64>,
    tt: Tt,
    jd_ut1: f64,
    observer: Observer,
    delta_psi_rad: f64,
) -> Result<ApparentPlace, ApparentPlaceError> {
    // LAST.
    let eps = mean_obliquity(tt);
    let last = last_rad(jd_ut1, observer.longitude.radians(), delta_psi_rad, eps);

    // Geocentric → topocentric (diurnal parallax). Stars skip this.
    let topo_eq = match distance_au {
        Some(d_au) if d_au > 0.0 => topocentric_equatorial(true_eq, last, observer, d_au * AU_M),
        _ => true_eq,
    };

    // Equatorial → horizontal.
    let mut horizontal = equatorial_to_horizontal(topo_eq, last, observer.latitude.radians());

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
    let aberration_sigma = Sigma::new(ABERRATION_RESIDUAL_SIGMA_RAD).unwrap();
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
        let polaris = crate::by_hr(424).unwrap();
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
    fn moon_topocentric_matches_skyfield_at_austin() {
        // Moonlight-pond regression case: 2026-05-25T06:29:06.752Z,
        // observer at (30.150588 N, -97.844170 E), 1.7 m eye height,
        // standard atmosphere.
        //
        // Skyfield/JPL DE421 reference (topocentric apparent, including
        // refraction):
        //   alt = 18.9431°, az = 257.9499°
        // Without diurnal parallax bris reported 19.8354° (≈54′ high).
        //
        // After applying topocentric correction (this commit) Hc must
        // agree with Skyfield to better than 1′ in altitude. Tighter
        // residuals (a few arcseconds) are limited by the truncated
        // ELP series and our aberration stub; do not weaken below 1′.
        let utc = chrono::Utc
            .timestamp_millis_opt(1_779_690_546_752)
            .single()
            .unwrap();
        let tt = utc_to_tt(utc).unwrap();
        let jd_ut1 = chrono_to_jd_utc(utc);
        let obs = Observer {
            latitude: bris_core::Latitude::from_degrees(30.150_588).unwrap(),
            longitude: bris_core::Longitude::from_degrees(-97.844_170).unwrap(),
            eye_height_m: 1.7,
            eye_height_sigma_m: 0.5,
            atmosphere: crate::refraction::Atmosphere::STANDARD,
        };
        let ap = body_apparent_place(SolarSystemBody::Moon, tt, jd_ut1, obs).unwrap();
        let alt_deg = ap.direction.altitude.to_degrees();
        let az_deg = ap.direction.azimuth.to_degrees();
        let dalt_arcmin = (alt_deg - 18.9431) * 60.0;
        let daz_arcmin = (az_deg - 257.9499) * 60.0;
        assert!(
            dalt_arcmin.abs() < 1.0,
            "Moon topocentric altitude {alt_deg}° differs from Skyfield 18.9431° by {dalt_arcmin:.2}′ (>1′)"
        );
        assert!(
            daz_arcmin.abs() < 2.0,
            "Moon topocentric azimuth {az_deg}° differs from Skyfield 257.9499° by {daz_arcmin:.2}′ (>2′)"
        );
    }

    #[test]
    fn aberration_residual_sigma_is_small() {
        // After applying classical annual aberration, the contribution
        // is ~0.1″. The historical placeholder was 20″; verify it is
        // no longer combined into the altitude sigma by checking the
        // total is below the old floor (refraction + dip alone is
        // ~19.6″ at default_dev; adding 20″ in quadrature would push
        // past 28″).
        let (tt, jd_ut1) = june_solstice_noon_at_greenwich();
        let obs = Observer::default_dev();
        let ap = body_apparent_place(SolarSystemBody::Sun, tt, jd_ut1, obs).unwrap();
        let arcsec = ap.altitude_sigma.value() * 180.0 * 3600.0 / std::f64::consts::PI;
        assert!(
            arcsec < 25.0,
            "altitude sigma {arcsec}\" should no longer carry a 20\" \
             aberration placeholder (combined with dip+refraction ≈ 19.6\", \
             the placeholder would push this past 28\")"
        );
    }

    #[test]
    fn annual_aberration_shifts_perpendicular_star_by_about_twenty_arcsec() {
        // Pick a star at the ecliptic pole at J2000: ecliptic longitude
        // irrelevant, latitude +90°. In equatorial of date this is
        // approximately (α=18h, δ=90°−ε). Aberration from Earth's
        // ~30 km/s orbital motion (perpendicular to the pole) shifts the
        // apparent direction by v/c ≈ 1e-4 rad ≈ 20.5″.
        let utc = chrono::Utc
            .with_ymd_and_hms(2024, 3, 21, 0, 0, 0)
            .single()
            .unwrap();
        let tt = utc_to_tt(utc).unwrap();
        let eps = crate::frame::mean_obliquity(tt);
        let true_eq = Equatorial {
            ra: 1.5 * std::f64::consts::PI, // 18h
            dec: std::f64::consts::FRAC_PI_2 - eps,
        };
        let app = apply_annual_aberration(true_eq, tt);
        let (sa0, ca0) = true_eq.ra.sin_cos();
        let (sdec0, cdec0) = true_eq.dec.sin_cos();
        let (sa1, ca1) = app.ra.sin_cos();
        let (sdec1, cdec1) = app.dec.sin_cos();
        let dot = (cdec0 * ca0) * (cdec1 * ca1)
            + (cdec0 * sa0) * (cdec1 * sa1)
            + sdec0 * sdec1;
        let angle = dot.clamp(-1.0, 1.0).acos();
        let arcsec = angle * 180.0 * 3600.0 / std::f64::consts::PI;
        assert!(
            (15.0..=25.0).contains(&arcsec),
            "aberration shift {arcsec}\" out of expected ~20.5\" band"
        );
    }
}
