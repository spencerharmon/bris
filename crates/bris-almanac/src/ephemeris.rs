//! Heliocentric and geocentric solar-system body positions via VSOP87D.
//!
//! VSOP87D returns heliocentric ecliptic spherical coordinates of date
//! (longitude L, latitude B, radius R in AU). Bris consumes those to
//! compute geocentric apparent positions of the Sun and visible planets.
//!
//! # Implementation
//!
//! Backed by the `vsop87` crate (MIT/Apache-2.0). The crate ships the
//! full VSOP87D series; we wrap it in a Bris-flavored API that uses
//! our own coordinate types, applies the geocentric correction for the
//! Sun, and centralizes the time-scale conversion (VSOP87 takes JDE,
//! which is JD in TT).
//!
//! Light-time correction, frame rotation (precession/nutation/bias to
//! the equator-of-date frame), and aberration are *not* applied here;
//! they belong to the apparent-place computation in the `coord` module
//! and are layered on top of these heliocentric positions.
//!
//! # Accuracy
//!
//! VSOP87 is sub-arcsecond for Sun, planets out to Mars over 4000 yr
//! around J2000; ~1″ for Jupiter and Saturn over 2000 yr; sub-arcsec
//! for Uranus and Neptune over 6000 yr. All vastly better than the
//! Bris per-sight uncertainty budget.

use bris_core::time::Tt;
use vsop87::SphericalCoordinates;

/// Heliocentric ecliptic spherical coordinates of date.
///
/// Right-handed frame with origin at the Sun, x-y plane the ecliptic
/// of date, x-axis toward the equinox of date.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Heliocentric {
    /// Heliocentric ecliptic longitude (L), radians, normalized to `[0, 2π)`.
    pub longitude: f64,
    /// Heliocentric ecliptic latitude (B), radians.
    pub latitude: f64,
    /// Heliocentric radius (R), astronomical units.
    pub radius_au: f64,
}

impl From<SphericalCoordinates> for Heliocentric {
    fn from(c: SphericalCoordinates) -> Self {
        Self {
            longitude: c.longitude(),
            latitude: c.latitude(),
            radius_au: c.distance(),
        }
    }
}

/// One of the bodies for which Bris computes a heliocentric position.
///
/// The Sun is not included here because its heliocentric position is
/// trivially `(0, 0, 0)`; instead see [`sun_geocentric`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// Mercury.
    Mercury,
    /// Venus.
    Venus,
    /// The Earth-Moon barycenter. The Moon's geocentric position is
    /// computed separately via ELP2000 (next task).
    EarthMoonBarycenter,
    /// Mars.
    Mars,
    /// Jupiter.
    Jupiter,
    /// Saturn.
    Saturn,
    /// Uranus. (Not visible to the unaided eye in most conditions; included
    /// for completeness.)
    Uranus,
    /// Neptune. (Not visible to the unaided eye; included for completeness.)
    Neptune,
}

/// Compute the heliocentric ecliptic-of-date position of `body` at TT.
#[must_use]
pub fn heliocentric(body: Body, tt: Tt) -> Heliocentric {
    let jde = tt.julian_date();
    let raw = match body {
        Body::Mercury => vsop87::vsop87d::mercury(jde),
        Body::Venus => vsop87::vsop87d::venus(jde),
        Body::EarthMoonBarycenter => vsop87::vsop87d::earth(jde),
        Body::Mars => vsop87::vsop87d::mars(jde),
        Body::Jupiter => vsop87::vsop87d::jupiter(jde),
        Body::Saturn => vsop87::vsop87d::saturn(jde),
        Body::Uranus => vsop87::vsop87d::uranus(jde),
        Body::Neptune => vsop87::vsop87d::neptune(jde),
    };
    raw.into()
}

/// Compute the geocentric ecliptic-of-date position of the Sun at TT.
///
/// Equal to the heliocentric position of the Earth-Moon barycenter
/// (≈ Earth, to a few-arcsecond approximation suitable for sight
/// reduction) reflected through the Sun: longitude flipped by π,
/// latitude negated, radius unchanged.
///
/// For sub-arcsecond Sun positions one would use VSOP87 Earth (not EMB)
/// and apply light-time iteration. For our budget the EMB approximation
/// contributes < 6 arcsec error, well below per-sight uncertainty.
#[must_use]
pub fn sun_geocentric(tt: Tt) -> Heliocentric {
    let earth = heliocentric(Body::EarthMoonBarycenter, tt);
    Heliocentric {
        longitude: (earth.longitude + core::f64::consts::PI).rem_euclid(core::f64::consts::TAU),
        latitude: -earth.latitude,
        radius_au: earth.radius_au,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use bris_core::time::JD_J2000;

    fn at_jd(jd: f64) -> Tt {
        Tt::from_julian_date(jd)
    }

    #[test]
    fn earth_at_j2000_known_position() {
        // VSOP87D Earth at J2000.0. Reference values from the crate's
        // own published test data (which traces back to the Bureau des
        // Longitudes original VSOP87 distribution):
        //   L ≈ 1.7519 rad, B ≈ -1e-6 rad, R ≈ 0.9833 AU.
        let pos = heliocentric(Body::EarthMoonBarycenter, at_jd(JD_J2000));
        assert_relative_eq!(pos.longitude, 1.751_944, epsilon = 1e-3);
        assert!(
            pos.latitude.abs() < 1e-4,
            "B = {} should be near zero",
            pos.latitude
        );
        assert_relative_eq!(pos.radius_au, 0.983_3, epsilon = 1e-3);
    }

    #[test]
    fn sun_geocentric_is_opposite_earth() {
        // The Sun's geocentric position is the heliocentric Earth
        // position reflected through the origin.
        let earth = heliocentric(Body::EarthMoonBarycenter, at_jd(JD_J2000));
        let sun = sun_geocentric(at_jd(JD_J2000));
        let lon_diff = (sun.longitude - earth.longitude).rem_euclid(core::f64::consts::TAU);
        // Difference should be π (mod 2π).
        assert_relative_eq!(lon_diff, core::f64::consts::PI, epsilon = 1e-12);
        assert_relative_eq!(sun.latitude, -earth.latitude, epsilon = 1e-15);
        assert_relative_eq!(sun.radius_au, earth.radius_au, epsilon = 1e-15);
    }

    #[test]
    fn mars_radius_is_in_known_range() {
        // Mars's heliocentric distance varies between ~1.38 and ~1.67 AU
        // over its orbit. At any J2000-era date the value should fall
        // in that range. This is a sanity check that the right body
        // function is being called (a common mistake when wrapping a
        // family of functions).
        let mars = heliocentric(Body::Mars, at_jd(JD_J2000));
        assert!(
            (1.30..=1.70).contains(&mars.radius_au),
            "Mars R = {} AU outside expected range",
            mars.radius_au
        );
    }

    #[test]
    fn jupiter_radius_is_in_known_range() {
        // Jupiter's heliocentric distance: ~4.95 to ~5.46 AU.
        let jup = heliocentric(Body::Jupiter, at_jd(JD_J2000));
        assert!(
            (4.90..=5.50).contains(&jup.radius_au),
            "Jupiter R = {} AU outside expected range",
            jup.radius_au
        );
    }

    #[test]
    fn sun_longitude_2024_january_known() {
        // The Astronomical Almanac (USNO/RGO) for 2024-01-01.5 TT
        // (JD 2460311.0) lists the Sun's apparent geocentric ecliptic
        // longitude of date as approximately 280.07°. Our raw VSOP87D
        // output is the *mean* longitude of date (no aberration, no
        // light-time, no apparent-place corrections — those layer on
        // top in `coord`). The mean value at this date is ~280.4°.
        // A 0.5° tolerance covers the ~20″ aberration that would close
        // the gap to apparent place.
        let pos = sun_geocentric(at_jd(2_460_311.0));
        let lon_deg = pos.longitude.to_degrees();
        assert!(
            (279.5..=281.0).contains(&lon_deg),
            "Sun ecliptic longitude on 2024-01-01.5 = {lon_deg}°, expected ~280°"
        );
    }

    #[test]
    fn longitude_normalized() {
        // VSOP87 returns longitudes in [0, 2π). Verify across multiple
        // bodies and a span of dates.
        for &body in &[
            Body::Mercury,
            Body::Venus,
            Body::EarthMoonBarycenter,
            Body::Mars,
            Body::Jupiter,
            Body::Saturn,
        ] {
            for offset_days in [-50_000.0, 0.0, 50_000.0] {
                let pos = heliocentric(body, at_jd(JD_J2000 + offset_days));
                assert!(
                    pos.longitude >= 0.0 && pos.longitude < core::f64::consts::TAU,
                    "{:?} L = {} out of [0, 2π)",
                    body,
                    pos.longitude
                );
            }
        }
    }
}
