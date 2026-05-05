//! Star catalog and stellar position computation.
//!
//! Embeds a vetted star list at compile time (see `data/stars.tsv` and
//! `build.rs`). Provides lookup by Yale BSC HR number, iteration, and
//! a single function to compute a star's position at a given epoch
//! including proper-motion advance from J2000.0.
//!
//! Frame transformation (precession, nutation, aberration) and the
//! geocentric→topocentric correction (parallax, refraction) are
//! applied uniformly in the `coord` module alongside the same
//! corrections for Solar System bodies.
//!
//! # Catalog completeness
//!
//! The current build embeds a small starter set including the brightest
//! stars + Polaris. The full ~9000-star Yale BSC import is tracked as a
//! Phase 0 task (see `plan.org`); when complete it will replace
//! `data/stars.tsv` and the same code paths will work unchanged.

use bris_core::time::{Tt, JD_J2000};

// Generated at build time from data/stars.tsv. Suppress style lints on
// the generated literals — they're machine-emitted values, not human-
// edited code, and contain pi/tau approximations from RA values near
// 24h (= 2π radians).
#[allow(
    clippy::unreadable_literal,
    clippy::approx_constant,
    clippy::excessive_precision
)]
mod catalog_data {
    use super::StarRecord;
    include!(concat!(env!("OUT_DIR"), "/catalog_data.rs"));
}
use catalog_data::{HR_INDEX, STARS};

/// Days in one Julian year. Used to convert proper motions to
/// time-since-epoch deltas.
const DAYS_PER_JULIAN_YEAR: f64 = 365.25;

/// Milliarcseconds per radian.
const MAS_TO_RAD: f64 = std::f64::consts::PI / (180.0 * 3600.0 * 1000.0);

/// One star's catalog record.
///
/// All angular quantities use the conventions documented in `stars.tsv`:
/// RA and Dec in radians (J2000.0 / ICRS), proper motions in mas/yr
/// (with the standard tangent-rate convention dα/dt × cos(δ) for
/// `pm_ra_mas_per_yr`), parallax in mas, magnitude on the Johnson V
/// scale.
#[derive(Debug, Clone, Copy)]
pub struct StarRecord {
    /// Yale BSC (HR) number, the primary key.
    pub hr: u32,
    /// Hipparcos catalog number, or 0 if unknown.
    pub hip: u32,
    /// Conventional name. Spaces in the source TSV are converted to `_`.
    pub name: &'static str,
    /// Right ascension at J2000.0 (ICRS), radians, in `[0, 2π)`.
    pub ra_rad: f64,
    /// Declination at J2000.0 (ICRS), radians, in `[-π/2, π/2]`.
    pub dec_rad: f64,
    /// Proper motion in RA, milliarcseconds per year. Tangent-rate:
    /// `dα/dt × cos(δ)`.
    pub pm_ra_mas_per_yr: f64,
    /// Proper motion in declination, milliarcseconds per year.
    pub pm_dec_mas_per_yr: f64,
    /// Trigonometric parallax, milliarcseconds. 0 if unknown.
    pub parallax_mas: f64,
    /// Apparent visual magnitude (Johnson V).
    pub vmag: f64,
    /// True if this star is one of the 57 standard navigational stars
    /// used by the Nautical Almanac.
    pub is_navigational: bool,
}

/// A star's RA/Dec at a given epoch, expressed in the J2000.0 frame.
///
/// Frame rotation to date (precession + nutation), light deflection,
/// stellar aberration, and the geocentric→topocentric correction are
/// applied later in the apparent-place pipeline. This output is the
/// catalog J2000 position with proper motion advanced to `epoch`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StarPosition {
    /// Right ascension in radians, J2000.0 frame, `[0, 2π)`.
    pub ra_rad: f64,
    /// Declination in radians, J2000.0 frame, `[-π/2, π/2]`.
    pub dec_rad: f64,
}

/// Iterate every star in the embedded catalog.
#[must_use]
pub fn all_stars() -> &'static [StarRecord] {
    STARS
}

/// Look up a star by its Yale BSC (HR) number.
///
/// Returns `None` if no star with that HR is in the embedded catalog.
/// Backed by binary search over a sorted index built at compile time.
#[must_use]
pub fn by_hr(hr: u32) -> Option<&'static StarRecord> {
    HR_INDEX
        .binary_search_by_key(&hr, |&(h, _)| h)
        .ok()
        .map(|i| &STARS[HR_INDEX[i].1])
}

/// Iterate just the 57 standard navigational stars.
pub fn navigational_stars() -> impl Iterator<Item = &'static StarRecord> {
    STARS.iter().filter(|s| s.is_navigational)
}

/// Compute a star's apparent position in the J2000.0 frame at `epoch`,
/// applying proper motion linearly.
///
/// Linear proper-motion advance is exact to first order in years and
/// negligibly inaccurate over the few-decade range of catalog
/// applicability. Bigger error sources (frame rotation, aberration,
/// refraction) are applied later by the apparent-place chain.
#[must_use]
pub fn position_at(star: &StarRecord, epoch: Tt) -> StarPosition {
    let years_since_j2000 = (epoch.julian_date() - JD_J2000) / DAYS_PER_JULIAN_YEAR;

    // pm_ra is the on-sky tangent rate (dα/dt × cos δ); divide by cos δ
    // to get the change in α itself.
    let cos_dec = star.dec_rad.cos();
    // Guard against the singularity at the celestial pole. cos(δ) is
    // ≤ ~0.013 for Polaris; the resulting amplification of proper-motion
    // uncertainty is real, not a code bug, and is properly accounted for
    // in the per-star uncertainty contribution. We just need to avoid
    // a literal division by zero.
    let cos_dec_safe = if cos_dec.abs() < 1e-12 {
        1e-12
    } else {
        cos_dec
    };

    let dra_rad = star.pm_ra_mas_per_yr * years_since_j2000 * MAS_TO_RAD / cos_dec_safe;
    let ddec_rad = star.pm_dec_mas_per_yr * years_since_j2000 * MAS_TO_RAD;

    let ra = (star.ra_rad + dra_rad).rem_euclid(std::f64::consts::TAU);
    let dec =
        (star.dec_rad + ddec_rad).clamp(-std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2);

    StarPosition {
        ra_rad: ra,
        dec_rad: dec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn catalog_is_non_empty() {
        assert!(!STARS.is_empty(), "embedded catalog must have entries");
    }

    #[test]
    fn known_stars_present() {
        // Spot-check a handful of well-known HR numbers. The BSC name
        // field carries Bayer/Flamsteed designations, not the
        // conventional "Sirius" / "Vega" names — those come from a
        // separate name cross-reference (not yet imported).
        assert_eq!(by_hr(2491).map(|s| s.name), Some("9Alp_CMa")); // Sirius
        assert_eq!(by_hr(7001).map(|s| s.name), Some("3Alp_Lyr")); // Vega
        assert_eq!(by_hr(424).map(|s| s.name), Some("1Alp_UMi")); // Polaris
    }

    #[test]
    fn unknown_hr_returns_none() {
        assert!(by_hr(0).is_none());
        assert!(by_hr(99_999).is_none());
    }

    #[test]
    fn navigational_subset_present() {
        // The 57 navigational stars are flagged. We don't yet have all
        // 57 embedded, but the ones we do should pass through.
        let count = navigational_stars().count();
        assert!(count > 0, "expected at least some navigational stars");
        // Sirius (HR 2491) is a standard navigational star.
        assert!(
            navigational_stars().any(|s| s.hr == 2491),
            "Sirius (HR 2491) should be in the navigational subset"
        );
        // Polaris (HR 424) is famously NOT one of the 57 navigational
        // stars (it's at the pole, so its azimuth is meaningless for
        // sight-reduction LOPs).
        assert!(
            !navigational_stars().any(|s| s.hr == 424),
            "Polaris (HR 424) is famously NOT one of the 57 navigational stars"
        );
    }

    #[test]
    fn position_at_j2000_equals_catalog_value() {
        let sirius = by_hr(2491).expect("Sirius in catalog");
        let pos = position_at(sirius, Tt::from_julian_date(JD_J2000));
        assert_relative_eq!(pos.ra_rad, sirius.ra_rad, epsilon = 1e-12);
        assert_relative_eq!(pos.dec_rad, sirius.dec_rad, epsilon = 1e-12);
    }

    #[test]
    fn proper_motion_advances_correctly() {
        // Sirius has very large proper motion. BSC values:
        //   pm_ra (tangent rate) = -553 mas/yr
        //   pm_dec               = -1205 mas/yr
        // Over 100 years that's:
        //   ΔRA × cos δ = -55_300 mas = -55.3 arcsec on-sky
        //   ΔDec        = -120_500 mas = -120.5 arcsec
        let sirius = by_hr(2491).unwrap();
        let pos = position_at(
            sirius,
            Tt::from_julian_date(JD_J2000 + 100.0 * DAYS_PER_JULIAN_YEAR),
        );
        let dra_arcsec = (pos.ra_rad - sirius.ra_rad).to_degrees() * 3600.0;
        let ddec_arcsec = (pos.dec_rad - sirius.dec_rad).to_degrees() * 3600.0;
        // Tangent-rate convention: dα = (pm_ra / cos δ) × dt.
        let cos_dec = sirius.dec_rad.cos();
        let expected_dra_arcsec = -553.0 * 100.0 / 1000.0 / cos_dec;
        let expected_ddec_arcsec = -1205.0 * 100.0 / 1000.0;
        assert_relative_eq!(dra_arcsec, expected_dra_arcsec, epsilon = 1e-3);
        assert_relative_eq!(ddec_arcsec, expected_ddec_arcsec, epsilon = 1e-3);
    }

    #[test]
    fn polaris_proper_motion_safe_near_pole() {
        // Polaris (HR 424) is at δ ≈ 89.26°, so cos δ ≈ 0.013. Make
        // sure the division-by-near-zero guard doesn't blow up.
        let polaris = by_hr(424).unwrap();
        let pos = position_at(
            polaris,
            Tt::from_julian_date(JD_J2000 + 100.0 * DAYS_PER_JULIAN_YEAR),
        );
        // Result should be in the valid Dec range; that's the assertion
        // the safety guard guarantees.
        assert!(pos.dec_rad.is_finite());
        assert!(pos.dec_rad.abs() <= std::f64::consts::FRAC_PI_2);
        assert!(pos.ra_rad >= 0.0 && pos.ra_rad < std::f64::consts::TAU);
    }

    #[test]
    fn all_records_in_valid_ranges() {
        for s in all_stars() {
            assert!(
                s.ra_rad >= 0.0 && s.ra_rad < std::f64::consts::TAU,
                "{}: RA out of range",
                s.name
            );
            assert!(
                s.dec_rad.abs() <= std::f64::consts::FRAC_PI_2,
                "{}: Dec out of range",
                s.name
            );
            // Parallax can be negative due to measurement noise (it
            // happens for distant stars whose true parallax is below
            // measurement uncertainty); BSC keeps these values rather
            // than zeroing them. Cap the magnitude as a sanity check.
            assert!(
                s.parallax_mas.abs() <= 1500.0,
                "{}: parallax magnitude implausible",
                s.name
            );
            assert!(
                s.vmag >= -2.0 && s.vmag <= 10.0,
                "{}: vmag implausible",
                s.name
            );
        }
    }

    #[test]
    fn hr_index_is_sorted() {
        for window in HR_INDEX.windows(2) {
            assert!(
                window[0].0 < window[1].0,
                "HR_INDEX must be strictly ascending"
            );
        }
    }
}
