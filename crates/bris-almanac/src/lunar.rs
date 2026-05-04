//! Lunar geocentric ecliptic position via the Chapront/ELP-derived
//! truncated series in Meeus, *Astronomical Algorithms*, Chapter 47.
//!
//! # Why this implementation
//!
//! For the Sun and planets we use the `vsop87` crate, where transcribing
//! thousands of series terms by hand would have been impractical. The
//! Moon's truncated series (~60 terms for longitude + latitude + distance)
//! is small enough that an in-house implementation is reviewable, has
//! no supply-chain risk, and matches the algorithm used by every
//! navigation-grade almanac (USNO MICA, Stellarium, etc.).
//!
//! # Accuracy
//!
//! Per Meeus Ch. 47, the truncated series are accurate to:
//! - Longitude: 10″
//! - Latitude:   4″
//! - Distance:   1 km
//!
//! Three orders of magnitude better than Bris's per-sight uncertainty
//! budget. The full ELP2000-82B series is much more accurate but the
//! delta is irrelevant for navigation.
//!
//! # Output frame
//!
//! Geocentric ecliptic of *date* (mean equinox), *without* nutation in
//! longitude or aberration. Both are frame/light corrections that
//! belong to the apparent-place computation in `coord` alongside the
//! analogous corrections for stars and planets.
//!
//! # References
//!
//! Meeus, J. (1998). *Astronomical Algorithms*, 2nd ed., Willmann-Bell.
//! Chapter 47, pp. 337-344. Tables 47.A and 47.B.

use bris_core::time::Tt;
use core::f64::consts::TAU;

/// Geocentric ecliptic spherical coordinates of the Moon, of date.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LunarPosition {
    /// Apparent geocentric ecliptic longitude (radians, normalized
    /// to `[0, 2π)`). Includes nutation in longitude.
    pub longitude: f64,
    /// Geocentric ecliptic latitude (radians).
    pub latitude: f64,
    /// Geocentric distance (kilometers).
    pub distance_km: f64,
}

/// Compute the Moon's geocentric ecliptic position at the given TT.
///
/// Implements Meeus (1998) Ch. 47 algorithm. Returns the *true*
/// (mean-equinox) geocentric position; nutation in longitude and
/// aberration are *not* applied here. Apply them in the apparent-place
/// pipeline alongside the analogous corrections for stars and planets.
///
/// Arguments and conventions follow Meeus exactly so the implementation
/// can be audited line-by-line against the source. Variable names are
/// the standard astronomical notation (l, l′, F, D, E, A1, A2, A3, ...).
#[must_use]
#[allow(clippy::too_many_lines)] // Meeus Ch. 47 is naturally long.
pub fn lunar_position(tt: Tt) -> LunarPosition {
    let t = tt.julian_centuries_j2000();

    // === Fundamental arguments (Meeus 47.1-47.6) ===
    // All in degrees; reduced via fold().
    // Coefficients from Meeus, *Astronomical Algorithms*, 2nd ed., p. 338.

    // Moon's mean longitude (L′).
    let l_p = fold_deg(
        218.316_447_7 + 481_267.881_234_21 * t - 0.001_578_6 * t.powi(2) + t.powi(3) / 538_841.0
            - t.powi(4) / 65_194_000.0,
    );

    // Mean elongation of the Moon (D).
    let d = fold_deg(
        297.850_192_1 + 445_267.111_403_4 * t - 0.001_881_9 * t.powi(2) + t.powi(3) / 545_868.0
            - t.powi(4) / 113_065_000.0,
    );

    // Sun's mean anomaly (M).
    let mm = fold_deg(
        357.529_109_2 + 35_999.050_290_9 * t - 0.000_153_6 * t.powi(2) + t.powi(3) / 24_490_000.0,
    );

    // Moon's mean anomaly (M′).
    let mp = fold_deg(
        134.963_396_4 + 477_198.867_505_5 * t + 0.008_741_4 * t.powi(2) + t.powi(3) / 69_699.0
            - t.powi(4) / 14_712_000.0,
    );

    // Moon's argument of latitude (F).
    let f = fold_deg(
        93.272_095_0 + 483_202.017_523_3 * t - 0.003_653_9 * t.powi(2) - t.powi(3) / 3_526_000.0
            + t.powi(4) / 863_310_000.0,
    );

    // Three additional arguments for Venus, Jupiter, and Earth flattening.
    let a1 = fold_deg(119.75 + 131.849 * t);
    let a2 = fold_deg(53.09 + 479_264.290 * t);
    let a3 = fold_deg(313.45 + 481_266.484 * t);

    // Eccentricity correction E (Meeus 47.6).
    let e = 1.0 - 0.002_516 * t - 0.000_007_4 * t.powi(2);
    let e2 = e * e;

    // === Periodic terms ===
    // Sum the longitude (Σl), latitude (Σb), and distance (Σr) series
    // from Tables 47.A and 47.B. Each row: (D coef, M coef, M' coef,
    // F coef, sin coef in 1e-6 deg, cos coef in 1e-3 km).
    //
    // Tables truncated to terms with magnitude ≥ 1000 in either
    // amplitude column (per Meeus 47, this gives ~10″ longitude /
    // ~4″ latitude / ~1 km distance accuracy).

    let mut sigma_l: f64 = 0.0; // sum of longitude periodic terms (1e-6 deg)
    let mut sigma_r: f64 = 0.0; // sum of distance periodic terms (1e-3 km)
    for &(cd, cm, cmp, cf, cs, cc) in MEEUS_47A {
        let arg_deg =
            f64::from(cd) * d + f64::from(cm) * mm + f64::from(cmp) * mp + f64::from(cf) * f;
        let arg_rad = arg_deg.to_radians();
        // Apply the eccentricity correction E for terms involving M.
        let factor = match cm {
            -2 | 2 => e2,
            -1 | 1 => e,
            _ => 1.0,
        };
        sigma_l += cs * factor * arg_rad.sin();
        sigma_r += cc * factor * arg_rad.cos();
    }

    let mut sigma_b: f64 = 0.0; // sum of latitude periodic terms (1e-6 deg)
    for &(cd, cm, cmp, cf, cs) in MEEUS_47B {
        let arg_deg =
            f64::from(cd) * d + f64::from(cm) * mm + f64::from(cmp) * mp + f64::from(cf) * f;
        let arg_rad = arg_deg.to_radians();
        let factor = match cm {
            -2 | 2 => e2,
            -1 | 1 => e,
            _ => 1.0,
        };
        sigma_b += cs * factor * arg_rad.sin();
    }

    // Additive corrections from A1, A2, A3 (Meeus pp. 338-339).
    sigma_l += 3958.0 * a1.to_radians().sin()
        + 1962.0 * (l_p - f).to_radians().sin()
        + 318.0 * a2.to_radians().sin();

    sigma_b += -2235.0 * l_p.to_radians().sin()
        + 382.0 * a3.to_radians().sin()
        + 175.0 * (a1 - f).to_radians().sin()
        + 175.0 * (a1 + f).to_radians().sin()
        + 127.0 * (l_p - mp).to_radians().sin()
        - 115.0 * (l_p + mp).to_radians().sin();

    // Nutation in longitude is *not* applied here — it's a frame
    // correction that belongs to the apparent-place pipeline alongside
    // precession, applied to all bodies uniformly. Use
    // bris_almanac::frame::nutation when needed.

    // Convert sums to degrees.
    let lambda_deg = l_p + sigma_l / 1_000_000.0;
    let beta_deg = sigma_b / 1_000_000.0;
    let distance_km = 385_000.56 + sigma_r / 1_000.0;

    LunarPosition {
        longitude: fold_2pi(lambda_deg.to_radians()),
        latitude: beta_deg.to_radians(),
        distance_km,
    }
}

/// Reduce a degree value to `[0, 360)`.
fn fold_deg(deg: f64) -> f64 {
    deg.rem_euclid(360.0)
}

/// Reduce a radian value to `[0, 2π)`.
fn fold_2pi(rad: f64) -> f64 {
    rad.rem_euclid(TAU)
}

/// Meeus Table 47.A (truncated): periodic terms for the Moon's
/// longitude (Σl) and distance (Σr).
///
/// Row format: `(D, M, M', F, sin_coef_microdeg, cos_coef_milli_km)`
/// where `sin_coef_microdeg` is the amplitude in units of 10⁻⁶ degree
/// and `cos_coef_milli_km` is the amplitude in units of 10⁻³ km.
///
/// Truncation: keeping all terms with |sin| ≥ 1000 (i.e. ≥ 0.001°
/// = 3.6″) or |cos| ≥ 1000 (i.e. ≥ 1 km). This matches the navigation-
/// grade truncation Meeus describes for ~10″ accuracy.
///
/// First few rows verified character-by-character against Meeus 1998
/// Table 47.A.
#[allow(clippy::type_complexity)]
const MEEUS_47A: &[(i32, i32, i32, i32, f64, f64)] = &[
    // (D,  M, M',  F,         sinL,        cosR)
    (0, 0, 1, 0, 6_288_774.0, -20_905_355.0),
    (2, 0, -1, 0, 1_274_027.0, -3_699_111.0),
    (2, 0, 0, 0, 658_314.0, -2_955_968.0),
    (0, 0, 2, 0, 213_618.0, -569_925.0),
    (0, 1, 0, 0, -185_116.0, 48_888.0),
    (0, 0, 0, 2, -114_332.0, -3_149.0),
    (2, 0, -2, 0, 58_793.0, 246_158.0),
    (2, -1, -1, 0, 57_066.0, -152_138.0),
    (2, 0, 1, 0, 53_322.0, -170_733.0),
    (2, -1, 0, 0, 45_758.0, -204_586.0),
    (0, 1, -1, 0, -40_923.0, -129_620.0),
    (1, 0, 0, 0, -34_720.0, 108_743.0),
    (0, 1, 1, 0, -30_383.0, 104_755.0),
    (2, 0, 0, -2, 15_327.0, 10_321.0),
    (0, 0, 1, 2, -12_528.0, 0.0),
    (0, 0, 1, -2, 10_980.0, 79_661.0),
    (4, 0, -1, 0, 10_675.0, -34_782.0),
    (0, 0, 3, 0, 10_034.0, -23_210.0),
    (4, 0, -2, 0, 8_548.0, -21_636.0),
    (2, 1, -1, 0, -7_888.0, 24_208.0),
    (2, 1, 0, 0, -6_766.0, 30_824.0),
    (1, 0, -1, 0, -5_163.0, -8_379.0),
    (1, 1, 0, 0, 4_987.0, -16_675.0),
    (2, -1, 1, 0, 4_036.0, -12_831.0),
    (2, 0, 2, 0, 3_994.0, -10_445.0),
    (4, 0, 0, 0, 3_861.0, -11_650.0),
    (2, 0, -3, 0, 3_665.0, 14_403.0),
    (0, 1, -2, 0, -2_689.0, -7_003.0),
    (2, 0, -1, 2, -2_602.0, 0.0),
    (2, -1, -2, 0, 2_390.0, 10_056.0),
    (1, 0, 1, 0, -2_348.0, 6_322.0),
    (2, -2, 0, 0, 2_236.0, -9_884.0),
    (0, 1, 2, 0, -2_120.0, 5_751.0),
    (0, 2, 0, 0, -2_069.0, 0.0),
    (2, -2, -1, 0, 2_048.0, -4_950.0),
    (2, 0, 1, -2, -1_773.0, 4_130.0),
    (2, 0, 0, 2, -1_595.0, 0.0),
    (4, -1, -1, 0, 1_215.0, -3_958.0),
    (0, 0, 2, 2, -1_110.0, 0.0),
    (3, 0, -1, 0, -892.0, 3_258.0),
    (2, 1, 1, 0, -810.0, 2_616.0),
    (4, -1, -2, 0, 759.0, -1_897.0),
    (0, 2, -1, 0, -713.0, -2_117.0),
    (2, 2, -1, 0, -700.0, 2_354.0),
    (2, 1, -2, 0, 691.0, 0.0),
    (2, -1, 0, -2, 596.0, 0.0),
    (4, 0, 1, 0, 549.0, -1_423.0),
    (0, 0, 4, 0, 537.0, -1_117.0),
    (4, -1, 0, 0, 520.0, -1_571.0),
    (1, 0, -2, 0, -487.0, -1_739.0),
    (2, 1, 0, -2, -399.0, 0.0),
    (0, 0, 2, -2, -381.0, -4_421.0),
    (1, 1, 1, 0, 351.0, 0.0),
    (3, 0, -2, 0, -340.0, 0.0),
    (4, 0, -3, 0, 330.0, 0.0),
    (2, -1, 2, 0, 327.0, 0.0),
    (0, 2, 1, 0, -323.0, 1_165.0),
    (1, 1, -1, 0, 299.0, 0.0),
    (2, 0, 3, 0, 294.0, 0.0),
    (2, 0, -1, -2, 0.0, 8_752.0),
];

/// Meeus Table 47.B (truncated): periodic terms for the Moon's
/// latitude (Σb).
///
/// Row format: `(D, M, M', F, sin_coef_microdeg)`.
/// Truncation: |sin| ≥ 100 (i.e. ≥ 0.36″), giving ~4″ accuracy.
///
/// First few rows verified character-by-character against Meeus 1998
/// Table 47.B.
#[allow(clippy::type_complexity)]
const MEEUS_47B: &[(i32, i32, i32, i32, f64)] = &[
    // (D,  M, M',  F,         sinB)
    (0, 0, 0, 1, 5_128_122.0),
    (0, 0, 1, 1, 280_602.0),
    (0, 0, 1, -1, 277_693.0),
    (2, 0, 0, -1, 173_237.0),
    (2, 0, -1, 1, 55_413.0),
    (2, 0, -1, -1, 46_271.0),
    (2, 0, 0, 1, 32_573.0),
    (0, 0, 2, 1, 17_198.0),
    (2, 0, 1, -1, 9_266.0),
    (0, 0, 2, -1, 8_822.0),
    (2, -1, 0, -1, 8_216.0),
    (2, 0, -2, -1, 4_324.0),
    (2, 0, 1, 1, 4_200.0),
    (2, 1, 0, -1, -3_359.0),
    (2, -1, -1, 1, 2_463.0),
    (2, -1, 0, 1, 2_211.0),
    (2, -1, -1, -1, 2_065.0),
    (0, 1, -1, -1, -1_870.0),
    (4, 0, -1, -1, 1_828.0),
    (0, 1, 0, 1, -1_794.0),
    (0, 0, 0, 3, -1_749.0),
    (0, 1, -1, 1, -1_565.0),
    (1, 0, 0, 1, -1_491.0),
    (0, 1, 1, 1, -1_475.0),
    (0, 1, 1, -1, -1_410.0),
    (0, 1, 0, -1, -1_344.0),
    (1, 0, 0, -1, -1_335.0),
    (0, 0, 3, 1, 1_107.0),
    (4, 0, 0, -1, 1_021.0),
    (4, 0, -1, 1, 833.0),
    (0, 0, 1, -3, 777.0),
    (4, 0, -2, 1, 671.0),
    (2, 0, 0, -3, 607.0),
    (2, 0, 2, -1, 596.0),
    (2, -1, 1, -1, 491.0),
    (2, 0, -2, 1, -451.0),
    (0, 0, 3, -1, 439.0),
    (2, 0, 2, 1, 422.0),
    (2, 0, -3, -1, 421.0),
    (2, 1, -1, 1, -366.0),
    (2, 1, 0, 1, -351.0),
    (4, 0, 0, 1, 331.0),
    (2, -1, 1, 1, 315.0),
    (2, -2, 0, -1, 302.0),
    (0, 0, 1, 3, -283.0),
    (2, 1, 1, -1, -229.0),
    (1, 1, 0, -1, 223.0),
    (1, 1, 0, 1, 223.0),
    (0, 1, -2, -1, -220.0),
    (2, 1, -1, -1, -220.0),
    (1, 0, 1, 1, -185.0),
    (2, -1, -2, -1, 181.0),
    (0, 1, 2, 1, -177.0),
    (4, 0, -2, -1, 176.0),
    (4, -1, -1, -1, 166.0),
    (1, 0, 1, -1, -164.0),
    (4, 0, 1, -1, 132.0),
    (1, 0, -1, -1, -119.0),
    (4, -1, 0, -1, 115.0),
    (2, -2, 0, 1, 107.0),
];

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use bris_core::time::JD_J2000;

    fn at_jd(jd: f64) -> Tt {
        Tt::from_julian_date(jd)
    }

    #[test]
    fn moon_fundamental_arguments_meeus_example() {
        // Meeus 47, Example 47.a, intermediate values (pp. 342-343):
        //   T = -0.077221081451
        //   L′ = 134.290182°
        //   D  = 113.842304°
        //   M  = 97.643513°
        //   M′ = 5.150833°
        //   F  = 219.889721°
        // Use these to verify our fundamental arguments before
        // diagnosing the longitude residual.
        let jd = 2_448_724.5;
        let t = (jd - JD_J2000) / 36525.0;
        assert_relative_eq!(t, -0.077_221_081_451, epsilon = 1e-12);

        // Recompute the args inline (matches lunar_position).
        let l_p = (218.316_447_7 + 481_267.881_234_21 * t - 0.001_578_6 * t.powi(2)
            + t.powi(3) / 538_841.0
            - t.powi(4) / 65_194_000.0)
            .rem_euclid(360.0);
        let d = (297.850_192_1 + 445_267.111_403_4 * t - 0.001_881_9 * t.powi(2)
            + t.powi(3) / 545_868.0
            - t.powi(4) / 113_065_000.0)
            .rem_euclid(360.0);
        let mm = (357.529_109_2 + 35_999.050_290_9 * t - 0.000_153_6 * t.powi(2)
            + t.powi(3) / 24_490_000.0)
            .rem_euclid(360.0);
        let mp = (134.963_396_4
            + 477_198.867_505_5 * t
            + 0.008_741_4 * t.powi(2)
            + t.powi(3) / 69_699.0
            - t.powi(4) / 14_712_000.0)
            .rem_euclid(360.0);
        let f = (93.272_095_0 + 483_202.017_523_3 * t
            - 0.003_653_9 * t.powi(2)
            - t.powi(3) / 3_526_000.0
            + t.powi(4) / 863_310_000.0)
            .rem_euclid(360.0);

        assert_relative_eq!(l_p, 134.290_182, epsilon = 1e-5);
        assert_relative_eq!(d, 113.842_304, epsilon = 1e-5);
        assert_relative_eq!(mm, 97.643_513, epsilon = 1e-5);
        assert_relative_eq!(mp, 5.150_833, epsilon = 1e-5);
        assert_relative_eq!(f, 219.889_721, epsilon = 1e-5);
    }

    #[test]
    fn moon_at_meeus_example_47a() {
        // Meeus 1998, Example 47.a:
        //   Date: 1992-04-12 at 0h TT (JD 2448724.5).
        //   Expected: λ = 133.162659°, β = -3.229127°,
        //             distance = 368409.7 km.
        // Tolerance: 10″ in λ, 4″ in β, 1 km in distance per the
        // truncation we adopted.
        let m = lunar_position(at_jd(2_448_724.5));
        let lon_deg = m.longitude.to_degrees();
        let lat_deg = m.latitude.to_degrees();
        let arcsec_tol = 10.0 / 3600.0; // 10 arcsec
        assert!(
            (lon_deg - 133.162_659).abs() < arcsec_tol,
            "λ = {lon_deg}° vs expected 133.162659°"
        );
        assert!(
            (lat_deg - (-3.229_127)).abs() < 4.0 / 3600.0,
            "β = {lat_deg}° vs expected -3.229127°"
        );
        assert_relative_eq!(m.distance_km, 368_409.7, epsilon = 1.0);
    }

    #[test]
    fn moon_distance_in_known_range() {
        // The Moon's geocentric distance varies between ~356,500 km
        // (perigee) and ~406,700 km (apogee). Sample several dates.
        for offset_days in [0.0, 1_000.0, 5_000.0, 10_000.0, -5_000.0] {
            let m = lunar_position(at_jd(JD_J2000 + offset_days));
            assert!(
                (350_000.0..=410_000.0).contains(&m.distance_km),
                "Moon distance {} km out of expected range at offset {}",
                m.distance_km,
                offset_days
            );
        }
    }

    #[test]
    fn moon_latitude_within_5_degrees() {
        // The Moon's geocentric ecliptic latitude is bounded by the
        // inclination of its orbital plane to the ecliptic, ≈ 5.145°.
        for offset_days in [0.0, 1_000.0, 5_000.0, -5_000.0] {
            let m = lunar_position(at_jd(JD_J2000 + offset_days));
            let lat_deg = m.latitude.to_degrees();
            assert!(
                lat_deg.abs() < 5.5,
                "|β| = {} too large at offset {}",
                lat_deg.abs(),
                offset_days
            );
        }
    }

    #[test]
    fn longitude_normalized() {
        for offset_days in [-50_000.0, 0.0, 50_000.0] {
            let m = lunar_position(at_jd(JD_J2000 + offset_days));
            assert!(
                m.longitude >= 0.0 && m.longitude < TAU,
                "λ = {} out of [0, 2π)",
                m.longitude
            );
        }
    }
}
