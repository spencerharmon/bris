//! Coordinate transformations: ecliptic ↔ equatorial, equatorial ↔
//! horizontal, and helpers used by the apparent-place pipeline.
//!
//! All transformations are explicit, non-allocating, and operate on
//! plain spherical coordinates. The apparent-place chain in `apparent`
//! composes these into the full geocentric→topocentric→horizontal
//! pipeline.

use core::f64::consts::TAU;

/// A point on the celestial sphere, equatorial frame.
///
/// Right ascension is measured eastward along the celestial equator
/// from the equinox; declination is measured north from the equator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Equatorial {
    /// Right ascension, radians, normalized to `[0, 2π)`.
    pub ra: f64,
    /// Declination, radians, in `[-π/2, π/2]`.
    pub dec: f64,
}

/// A point on the celestial sphere, ecliptic frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ecliptic {
    /// Ecliptic longitude, radians, normalized to `[0, 2π)`.
    pub longitude: f64,
    /// Ecliptic latitude, radians.
    pub latitude: f64,
}

/// A direction in the local horizontal frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Horizontal {
    /// Azimuth measured clockwise from north, radians, in `[0, 2π)`.
    pub azimuth: f64,
    /// Altitude above the horizon, radians, in `[-π/2, π/2]`.
    /// Negative values mean below the horizon.
    pub altitude: f64,
}

/// Rotate ecliptic coordinates to equatorial, given the obliquity of
/// the ecliptic for the relevant frame.
#[must_use]
pub fn ecliptic_to_equatorial(p: Ecliptic, obliquity_rad: f64) -> Equatorial {
    let (sin_b, cos_b) = p.latitude.sin_cos();
    let (sin_l, cos_l) = p.longitude.sin_cos();
    let (sin_eps, cos_eps) = obliquity_rad.sin_cos();

    let sin_dec = sin_b * cos_eps + cos_b * sin_eps * sin_l;
    let dec = sin_dec.clamp(-1.0, 1.0).asin();

    let y = sin_l * cos_eps - sin_b / cos_b * sin_eps;
    let x = cos_l;
    let ra = y.atan2(x).rem_euclid(TAU);

    Equatorial { ra, dec }
}

/// Rotate equatorial coordinates to local horizontal, given local
/// hour angle of the equinox (i.e. local apparent sidereal time, LAST,
/// in radians) and observer geodetic latitude.
///
/// Convention: azimuth increases clockwise from north (the meteorological
/// and most navigation convention). Note: some astronomy texts measure
/// azimuth from south increasing west; we use the navigation convention
/// because the entire downstream pipeline is for marine navigation.
#[must_use]
pub fn equatorial_to_horizontal(p: Equatorial, last_rad: f64, observer_lat_rad: f64) -> Horizontal {
    // Hour angle of the body: H = LAST − α, then reduced.
    let h = (last_rad - p.ra).rem_euclid(TAU);
    let (sin_h, cos_h) = h.sin_cos();
    let (sin_phi, cos_phi) = observer_lat_rad.sin_cos();
    let (sin_dec, cos_dec) = p.dec.sin_cos();

    let sin_alt = sin_phi * sin_dec + cos_phi * cos_dec * cos_h;
    let altitude = sin_alt.clamp(-1.0, 1.0).asin();

    // Standard formula: azimuth A measured from north clockwise.
    //   sin A = -sin H cos δ / cos a
    //   cos A = (sin δ - sin φ sin a) / (cos φ cos a)
    let cos_alt = altitude.cos();
    let denom_az = (cos_phi * cos_alt).max(1e-12);
    let sin_az = -sin_h * cos_dec / cos_alt.max(1e-12);
    let cos_az = (sin_dec - sin_phi * sin_alt) / denom_az;
    let mut azimuth = sin_az.atan2(cos_az).rem_euclid(TAU);
    // rem_euclid can yield exactly TAU for inputs like -0.0; pin that
    // case to 0.0 to keep azimuth in the half-open [0, TAU) range.
    if azimuth >= TAU {
        azimuth -= TAU;
    }

    Horizontal { azimuth, altitude }
}

/// Greenwich Mean Sidereal Time (radians) at the given UT1 Julian Date.
///
/// IAU 2006 expression (close to the older Aoki formula but consistent
/// with the IAU 2006 precession). For our budget the older expression
/// would suffice; we use the modern one for consistency with the rest
/// of the frame chain.
///
/// Reference: IERS TN 36, eq. 5.32.
#[must_use]
pub fn gmst_rad(jd_ut1: f64) -> f64 {
    let t = (jd_ut1 - 2_451_545.0) / 36525.0;
    // Earth Rotation Angle (radians) at UT1.
    let du = jd_ut1 - 2_451_545.0;
    let frac = du - du.floor();
    let theta = TAU * (0.779_057_273_264 + 1.002_737_811_911_354 * du) - TAU * frac.floor();
    let theta = theta.rem_euclid(TAU);

    // Equation of the origins / GMST polynomial, in arcseconds.
    let arcsec = 0.014_506 + 4_612.156_534 * t + 1.391_581_7 * t.powi(2) - 0.000_000_44 * t.powi(3);
    let arcsec_to_rad = std::f64::consts::PI / (180.0 * 3600.0);
    (theta + arcsec * arcsec_to_rad).rem_euclid(TAU)
}

/// Local Apparent Sidereal Time (radians) at the given UT1 JD,
/// observer longitude, and nutation in longitude (Δψ).
#[must_use]
pub fn last_rad(jd_ut1: f64, observer_lon_rad: f64, delta_psi_rad: f64, obliquity_rad: f64) -> f64 {
    // Apparent sidereal time = mean sidereal time + Δψ × cos ε.
    let gast = (gmst_rad(jd_ut1) + delta_psi_rad * obliquity_rad.cos()).rem_euclid(TAU);
    (gast + observer_lon_rad).rem_euclid(TAU)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn ecliptic_to_equatorial_at_equinox() {
        // Ecliptic point at λ=0, β=0 should map to RA=0, Dec=0
        // regardless of obliquity.
        let p = Ecliptic {
            longitude: 0.0,
            latitude: 0.0,
        };
        let q = ecliptic_to_equatorial(p, 23.4_f64.to_radians());
        assert_relative_eq!(q.ra, 0.0, epsilon = 1e-12);
        assert_relative_eq!(q.dec, 0.0, epsilon = 1e-12);
    }

    #[test]
    fn ecliptic_to_equatorial_at_pole_of_ecliptic() {
        // Ecliptic latitude 90° (north pole of the ecliptic) should
        // give Dec = 90° - ε.
        let eps = 23.4_f64.to_radians();
        let p = Ecliptic {
            longitude: 0.0,
            latitude: std::f64::consts::FRAC_PI_2,
        };
        let q = ecliptic_to_equatorial(p, eps);
        assert_relative_eq!(q.dec, std::f64::consts::FRAC_PI_2 - eps, epsilon = 1e-9);
    }

    #[test]
    fn body_at_zenith_has_altitude_pi_over_2() {
        // A body at the same RA as LAST and Dec = observer_lat is
        // at the zenith: altitude = π/2.
        let observer_lat = 45.0_f64.to_radians();
        let last = 1.234; // arbitrary
        let p = Equatorial {
            ra: last,
            dec: observer_lat,
        };
        let h = equatorial_to_horizontal(p, last, observer_lat);
        assert_relative_eq!(h.altitude, std::f64::consts::FRAC_PI_2, epsilon = 1e-9);
    }

    #[test]
    fn body_at_celestial_pole_has_altitude_eq_observer_lat() {
        // A body at Dec = +90° (north celestial pole) appears at
        // altitude equal to observer geodetic latitude, due north.
        let observer_lat = 47.6_f64.to_radians(); // Seattle-ish
        let p = Equatorial {
            ra: 0.0,
            dec: std::f64::consts::FRAC_PI_2,
        };
        let h = equatorial_to_horizontal(p, 1.234, observer_lat);
        assert_relative_eq!(h.altitude, observer_lat, epsilon = 1e-9);
    }

    #[test]
    fn gmst_at_j2000_known() {
        // GMST at J2000.0 (2000-01-01T12:00:00 UT1) is approximately
        // 18h 41m 50.5s = 280.46° = 4.8949 rad.
        // (This is the famous IAU starting value.)
        let g = gmst_rad(2_451_545.0);
        let g_deg = g.to_degrees();
        assert_relative_eq!(g_deg, 280.46, epsilon = 0.5);
    }

    #[test]
    fn azimuth_in_range() {
        // Sweep many configurations; azimuth must be in [0, 2π).
        for lat_deg in [-60.0_f64, -30.0, 0.0, 30.0, 60.0] {
            for ra_deg in [0.0_f64, 90.0, 180.0, 270.0] {
                for dec_deg in [-60.0_f64, -30.0, 0.0, 30.0, 60.0] {
                    let h = equatorial_to_horizontal(
                        Equatorial {
                            ra: ra_deg.to_radians(),
                            dec: dec_deg.to_radians(),
                        },
                        0.0,
                        lat_deg.to_radians(),
                    );
                    assert!(
                        h.azimuth >= 0.0 && h.azimuth < TAU,
                        "az={} out of range for lat={} ra={} dec={}",
                        h.azimuth,
                        lat_deg,
                        ra_deg,
                        dec_deg
                    );
                }
            }
        }
    }
}
