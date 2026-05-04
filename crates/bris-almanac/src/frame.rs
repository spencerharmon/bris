//! Frame transformations: precession and nutation.
//!
//! Star catalog positions are given in a fixed reference frame (J2000.0
//! / ICRS). To compute their apparent direction at an arbitrary epoch we
//! must apply two slow rotations of the celestial reference frame:
//!
//! 1. **Precession** — the smooth, ~26000-year wobble of Earth's axis.
//! 2. **Nutation** — small (~10″ peak-to-peak) periodic oscillations on
//!    top of precession, driven mainly by the lunar orbital plane.
//!
//! # Models
//!
//! - **Precession:** IAU 2006 (Capitaine et al., 2003). A polynomial in
//!   `T` (Julian centuries TT since J2000.0). Sub-microarcsecond over
//!   ±1000 years from J2000; effectively perfect for our budget.
//! - **Nutation:** IAU 2000B (`McCarthy` & Luzum, 2003). The abridged
//!   ~80-term version of the IAU 2000A precession-nutation model, with
//!   ~1 mas accuracy — three orders of magnitude better than our
//!   per-sight target. Used by NASA SPICE, IAU SOFA's `iauNut00b`, and
//!   most navigation libraries when full 2000A precision isn't needed.
//!
//! # Output
//!
//! Both functions return frame-rotation parameters. The full
//! catalog→apparent transform is composed in the `coord` module of
//! this crate (next task in plan.org Phase 1).

use bris_core::time::Tt;

/// Arcseconds per radian.
const ARCSEC_TO_RAD: f64 = std::f64::consts::PI / (180.0 * 3600.0);

/// IAU 2006 precession angles, in radians, evaluated at the given TT.
///
/// These three angles parameterize the rotation from the J2000.0 mean
/// equator-and-equinox frame to the mean equator-and-equinox frame of
/// date, following the Capitaine 2003 / IAU 2006 polynomial expansion.
///
/// All three are conventionally named with Greek letters in the
/// astronomical literature; we use the standard ASCII transliterations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrecessionAngles {
    /// ζ (`zeta_A)`: rotation about the J2000 z-axis, in radians.
    pub zeta: f64,
    /// z (`z_A)`: rotation about the date z-axis, in radians.
    pub z: f64,
    /// θ (`theta_A)`: rotation about the y-axis, in radians.
    pub theta: f64,
}

/// Compute IAU 2006 precession angles at the given TT instant.
///
/// Returns the equatorial precession angles (`ζ_A`, `z_A`, `θ_A`) of Capitaine
/// et al. (2003) that parameterize the rotation from the J2000.0 mean
/// equator-and-equinox frame to the mean equator-and-equinox frame of
/// date.
///
/// All three angles vanish at J2000.0 by construction. (The IAU 2006
/// "with frame bias" forms used by SOFA's `iauPb06` retain a constant
/// offset for the GCRS-to-mean-J2000 frame bias; that bias is small,
/// ~0.04″, and we apply it elsewhere in the catalog→apparent transform
/// rather than folding it into precession.)
///
/// Polynomial coefficients are in arcseconds, evaluated as a function
/// of `T` = Julian centuries (TT) since J2000.0.
///
/// Reference: Capitaine, Wallace & Chapront (2003), as adopted by
/// IAU 2006. Cross-check: SOFA `iauP06e` returns the same series with
/// the +ξ₀ frame-bias terms separated.
#[must_use]
pub fn precession_angles(tt: Tt) -> PrecessionAngles {
    let t = tt.julian_centuries_j2000();

    // Horner-form polynomial evaluation in T, all coefficients in arcsec.
    // Source: Capitaine 2003 Eq. (40)-(42).
    //
    //   ζ_A = 2306.083227·T + 0.2988499·T² + 0.01801828·T³
    //         − 5.971e-6·T⁴ − 3.173e-7·T⁵
    //   z_A = 2306.077181·T + 1.0927348·T² + 0.01826837·T³
    //         − 28.596e-6·T⁴ − 2.904e-7·T⁵
    //   θ_A = 2004.191903·T − 0.4294934·T² − 0.04182264·T³
    //         − 7.089e-6·T⁴ − 1.274e-7·T⁵
    let zeta_arcsec = ((((-3.173e-7 * t - 5.971e-6) * t + 0.018_018_28) * t + 0.298_849_9) * t
        + 2_306.083_227)
        * t;
    let z_arcsec = ((((-2.904e-7 * t - 28.596e-6) * t + 0.018_268_37) * t + 1.092_734_8) * t
        + 2_306.077_181)
        * t;
    let theta_arcsec = ((((-1.274e-7 * t - 7.089e-6) * t - 0.041_822_64) * t - 0.429_493_4) * t
        + 2_004.191_903)
        * t;

    PrecessionAngles {
        zeta: zeta_arcsec * ARCSEC_TO_RAD,
        z: z_arcsec * ARCSEC_TO_RAD,
        theta: theta_arcsec * ARCSEC_TO_RAD,
    }
}

/// Nutation angles, in radians.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NutationAngles {
    /// Δψ: nutation in longitude, in radians.
    pub delta_psi: f64,
    /// Δε: nutation in obliquity, in radians.
    pub delta_epsilon: f64,
}

/// Mean obliquity of the ecliptic at the given TT, in radians.
///
/// IAU 2006 expression (Capitaine 2003), polynomial in `T` = Julian
/// centuries since J2000.0. Accurate to ~0.001″ over millennia.
#[must_use]
pub fn mean_obliquity(tt: Tt) -> f64 {
    let t = tt.julian_centuries_j2000();
    // Coefficients in arcseconds. Constant term: 84381.406″ = 23°26'21.406″.
    let arcsec = ((((-0.000_000_434 * t - 0.000_000_576) * t + 0.001_813_75) * t - 0.000_059_0)
        * t
        - 46.836_769)
        * t
        + 84381.406;
    arcsec * ARCSEC_TO_RAD
}

/// Constant Δψ offset (in mas) from the IAU 2000B abridgement of the
/// 2000A series, accounting for the bulk of the omitted planetary
/// nutation contribution. Source: SOFA `iauNut00b`, IERS TN 32 ch. 5.
const DPLAN_PSI_MAS: f64 = -0.135;

/// Constant Δε offset (in mas) from the IAU 2000B abridgement; see
/// [`DPLAN_PSI_MAS`].
const DPLAN_EPS_MAS: f64 = 0.388;

/// Compute IAU 2000B nutation angles at the given TT instant.
///
/// Returns Δψ (nutation in longitude) and Δε (nutation in obliquity)
/// in radians. The IAU 2000B model uses 77 luni-solar terms and is
/// accurate to ~1 mas — three orders of magnitude better than Bris's
/// per-sight uncertainty budget.
///
/// The series is over five fundamental arguments (l, l′, F, D, Ω) of
/// Delaunay's lunar theory, each evaluated as a polynomial in T.
///
/// Reference: `McCarthy` & Luzum (2003) and IAU SOFA's `iauNut00b`.
#[must_use]
pub fn nutation(tt: Tt) -> NutationAngles {
    let t = tt.julian_centuries_j2000();

    // Fundamental arguments (Delaunay) in arcseconds, then converted to
    // radians and reduced mod 2π. From IAU 2000B; same values as 2000A.
    let l = fold_2pi((485_868.249_036 + 1_717_915_923.217_8 * t) * ARCSEC_TO_RAD);
    let lp = fold_2pi((1_287_104.793_05 + 129_596_581.048_1 * t) * ARCSEC_TO_RAD);
    let f = fold_2pi((335_779.526_232 + 1_739_527_262.847_8 * t) * ARCSEC_TO_RAD);
    let d = fold_2pi((1_072_260.703_69 + 1_602_961_601.209_0 * t) * ARCSEC_TO_RAD);
    let om = fold_2pi((450_160.398_036 - 6_962_890.543_1 * t) * ARCSEC_TO_RAD);

    let mut dp = 0.0_f64; // accumulator: Δψ in units of 0.1 µas
    let mut de = 0.0_f64; // accumulator: Δε in units of 0.1 µas

    for &(nl, nlp, nf, nd, nom, sp, spt, cp, ce, cet, se) in NUT2000B_TERMS {
        let arg = f64::from(nl) * l
            + f64::from(nlp) * lp
            + f64::from(nf) * f
            + f64::from(nd) * d
            + f64::from(nom) * om;
        let (sin_arg, cos_arg) = arg.sin_cos();
        dp += (sp + spt * t) * sin_arg + cp * cos_arg;
        de += (ce + cet * t) * cos_arg + se * sin_arg;
    }

    // The 2000B series omits the planetary contributions to Δψ, Δε that
    // are present in 2000A. SOFA includes a small constant offset
    // ([`DPLAN_PSI_MAS`], [`DPLAN_EPS_MAS`]) to absorb the bulk of the
    // omitted planetary terms.

    // Series accumulators are in units of 0.1 µas; convert to radians.
    // 0.1 µas = 1e-7 arcsec.
    let one_tenth_uas_to_rad = 1e-7 * ARCSEC_TO_RAD;
    let mas_to_rad = 1e-3 * ARCSEC_TO_RAD;

    NutationAngles {
        delta_psi: dp * one_tenth_uas_to_rad + DPLAN_PSI_MAS * mas_to_rad,
        delta_epsilon: de * one_tenth_uas_to_rad + DPLAN_EPS_MAS * mas_to_rad,
    }
}

/// Reduce an angle (radians) to the half-open range `[0, 2π)`.
fn fold_2pi(rad: f64) -> f64 {
    rad.rem_euclid(std::f64::consts::TAU)
}

/// IAU 2000B nutation series: 77 luni-solar terms.
///
/// Each tuple is `(nl, nlp, nf, nd, nom, sp, spt, cp, ce, cet, se)`:
/// - `nl..nom`: integer multipliers of the five Delaunay arguments
///   (l, l′, F, D, Ω).
/// - `sp`, `cp`: coefficients of `sin(arg)` and `cos(arg)` in Δψ,
///   in units of 0.1 µas.
/// - `spt`: T-dependent contribution to the sin coefficient in Δψ,
///   in 0.1 µas / Julian century.
/// - `ce`, `se`: coefficients of `cos(arg)` and `sin(arg)` in Δε,
///   in units of 0.1 µas.
/// - `cet`: T-dependent contribution to the cos coefficient in Δε,
///   in 0.1 µas / Julian century.
///
/// Source: IAU SOFA `iauNut00b`, terms transcribed from
/// IERS Technical Note 32, Table 5.3a (truncated to the IAU 2000B
/// subset published by `McCarthy` & Luzum 2003).
///
/// Largest term first (the 18.6-year nutation), then in decreasing
/// magnitude of the Δψ amplitude.
#[allow(clippy::type_complexity)]
const NUT2000B_TERMS: &[(i32, i32, i32, i32, i32, f64, f64, f64, f64, f64, f64)] = &[
    // (nl, nlp, nf, nd, nom,  sp,           spt,    cp,      ce,         cet,    se)
    (
        0,
        0,
        0,
        0,
        1,
        -172_064_161.0,
        -174_666.0,
        33_386.0,
        92_052_331.0,
        9_086.0,
        15_377.0,
    ),
    (
        0,
        0,
        2,
        -2,
        2,
        -13_170_906.0,
        -1_675.0,
        -13_696.0,
        5_730_336.0,
        -3_015.0,
        -4_587.0,
    ),
    (
        0,
        0,
        2,
        0,
        2,
        -2_276_413.0,
        -234.0,
        2_796.0,
        978_459.0,
        -485.0,
        1_374.0,
    ),
    (
        0,
        0,
        0,
        0,
        2,
        2_074_554.0,
        207.0,
        -698.0,
        -897_492.0,
        470.0,
        -291.0,
    ),
    (
        0,
        1,
        0,
        0,
        0,
        1_475_877.0,
        -3_633.0,
        11_817.0,
        73_871.0,
        -184.0,
        -1_924.0,
    ),
    (
        0, 1, 2, -2, 2, -516_821.0, 1_226.0, -524.0, 224_386.0, -677.0, -174.0,
    ),
    (1, 0, 0, 0, 0, 711_159.0, 73.0, -872.0, -6_750.0, 0.0, 358.0),
    (
        0, 0, 2, 0, 1, -387_298.0, -367.0, 380.0, 200_728.0, 18.0, 318.0,
    ),
    (
        1, 0, 2, 0, 2, -301_461.0, -36.0, 816.0, 129_025.0, -63.0, 367.0,
    ),
    (
        0, -1, 2, -2, 2, 215_829.0, -494.0, 111.0, -95_929.0, 299.0, 132.0,
    ),
    (
        0, 0, 2, -2, 1, 128_227.0, 137.0, 181.0, -68_982.0, -9.0, 39.0,
    ),
    (-1, 0, 2, 0, 2, 123_457.0, 11.0, 19.0, -53_311.0, 32.0, -4.0),
    (-1, 0, 0, 2, 0, 156_994.0, 10.0, -168.0, -1_235.0, 0.0, 82.0),
    (1, 0, 0, 0, 1, 63_110.0, 63.0, 27.0, -33_228.0, 0.0, -9.0),
    (
        -1, 0, 0, 0, 1, -57_976.0, -63.0, -189.0, 31_429.0, 0.0, -75.0,
    ),
    (
        -1, 0, 2, 2, 2, -59_641.0, -11.0, 149.0, 25_543.0, -11.0, 66.0,
    ),
    (1, 0, 2, 0, 1, -51_613.0, -42.0, 129.0, 26_366.0, 0.0, 78.0),
    (-2, 0, 2, 0, 1, 45_893.0, 50.0, 31.0, -24_236.0, -10.0, 20.0),
    (0, 0, 0, 2, 0, 63_384.0, 11.0, -150.0, -1_220.0, 0.0, 29.0),
    (0, 0, 2, 2, 2, -38_571.0, -1.0, 158.0, 16_452.0, -11.0, 68.0),
    (0, -2, 2, -2, 2, 32_481.0, 0.0, 0.0, -13_870.0, 0.0, 0.0),
    (-2, 0, 0, 2, 0, -47_722.0, 0.0, -18.0, 477.0, 0.0, -25.0),
    (2, 0, 2, 0, 2, -31_046.0, -1.0, 131.0, 13_238.0, -11.0, 59.0),
    (1, 0, 2, -2, 2, 28_593.0, 0.0, -1.0, -12_338.0, 10.0, -3.0),
    (-1, 0, 2, 0, 1, 20_441.0, 21.0, 10.0, -10_758.0, 0.0, -3.0),
    (2, 0, 0, 0, 0, 29_243.0, 0.0, -74.0, -609.0, 0.0, 13.0),
    (0, 0, 2, 0, 0, 25_887.0, 0.0, -66.0, -550.0, 0.0, 11.0),
    (0, 1, 0, 0, 1, -14_053.0, -25.0, 79.0, 8_551.0, -2.0, -45.0),
    (-1, 0, 0, 2, 1, 15_164.0, 10.0, 11.0, -8_001.0, 0.0, -1.0),
    (0, 2, 2, -2, 2, -15_794.0, 72.0, -16.0, 6_850.0, -42.0, -5.0),
    (0, 0, -2, 2, 0, 21_783.0, 0.0, 13.0, -167.0, 0.0, 13.0),
    (1, 0, 0, -2, 1, -12_873.0, -10.0, -37.0, 6_953.0, 0.0, -14.0),
    (0, -1, 0, 0, 1, -12_654.0, 11.0, 63.0, 6_415.0, 0.0, 26.0),
    (-1, 0, 2, 2, 1, -10_204.0, 0.0, 25.0, 5_222.0, 0.0, 15.0),
    (0, 2, 0, 0, 0, 16_707.0, -85.0, -10.0, 168.0, -1.0, 10.0),
    (1, 0, 2, 2, 2, -7_691.0, 0.0, 44.0, 3_268.0, 0.0, 19.0),
    (-2, 0, 2, 0, 0, -11_024.0, 0.0, -14.0, 104.0, 0.0, 2.0),
    (0, 1, 2, 0, 2, 7_566.0, -21.0, -11.0, -3_250.0, 0.0, -5.0),
    (0, 0, 2, 2, 1, -6_637.0, -11.0, 25.0, 3_353.0, 0.0, 14.0),
    (0, -1, 2, 0, 2, -7_141.0, 21.0, 8.0, 3_070.0, 0.0, 4.0),
    (0, 0, 0, 2, 1, -6_302.0, -11.0, 2.0, 3_272.0, 0.0, 4.0),
    (1, 0, 2, -2, 1, 5_800.0, 10.0, 2.0, -3_045.0, 0.0, -1.0),
    (2, 0, 2, -2, 2, 6_443.0, 0.0, -7.0, -2_768.0, 0.0, -4.0),
    (-2, 0, 0, 2, 1, -5_774.0, -11.0, -15.0, 3_041.0, 0.0, -5.0),
    (2, 0, 2, 0, 1, -5_350.0, 0.0, 21.0, 2_695.0, 0.0, 12.0),
    (0, -1, 2, -2, 1, -4_752.0, -11.0, -3.0, 2_719.0, 0.0, -3.0),
    (0, 0, 0, -2, 1, -4_940.0, -11.0, -21.0, 2_720.0, 0.0, -9.0),
    (-1, -1, 0, 2, 0, 7_350.0, 0.0, -8.0, -51.0, 0.0, 4.0),
    (2, 0, 0, -2, 1, 4_065.0, 0.0, 6.0, -2_206.0, 0.0, 1.0),
    (1, 0, 0, 2, 0, 6_579.0, 0.0, -24.0, -199.0, 0.0, 2.0),
    (0, 1, 2, -2, 1, 3_579.0, 0.0, 5.0, -1_900.0, 0.0, 1.0),
    (1, -1, 0, 0, 0, 4_725.0, 0.0, -6.0, -41.0, 0.0, 3.0),
    (-2, 0, 2, 0, 2, -3_075.0, 0.0, -2.0, 1_313.0, 0.0, -1.0),
    (3, 0, 2, 0, 2, -2_904.0, 0.0, 15.0, 1_233.0, 0.0, 7.0),
    (0, -1, 0, 2, 0, 4_348.0, 0.0, -10.0, -81.0, 0.0, 2.0),
    (1, -1, 2, 0, 2, -2_878.0, 0.0, 8.0, 1_232.0, 0.0, 4.0),
    (0, 0, 0, 1, 0, -4_230.0, 0.0, 5.0, -20.0, 0.0, -2.0),
    (-1, -1, 2, 2, 2, -2_819.0, 0.0, 7.0, 1_207.0, 0.0, 3.0),
    (-1, 0, 2, 0, 0, -4_056.0, 0.0, 5.0, 40.0, 0.0, -2.0),
    (0, -1, 2, 2, 2, -2_647.0, 0.0, 11.0, 1_129.0, 0.0, 5.0),
    (-2, 0, 0, 0, 1, -2_294.0, 0.0, -10.0, 1_266.0, 0.0, -4.0),
    (1, 1, 2, 0, 2, 2_481.0, 0.0, -7.0, -1_062.0, 0.0, -3.0),
    (2, 0, 0, 0, 1, 2_179.0, 0.0, -2.0, -1_129.0, 0.0, -2.0),
    (-1, 1, 0, 1, 0, 3_276.0, 0.0, 1.0, -9.0, 0.0, 0.0),
    (1, 1, 0, 0, 0, -3_389.0, 0.0, 5.0, 35.0, 0.0, -2.0),
    (1, 0, 2, 0, 0, 3_339.0, 0.0, -13.0, -107.0, 0.0, 1.0),
    (-1, 0, 2, -2, 1, -1_987.0, 0.0, -6.0, 1_073.0, 0.0, -2.0),
    (1, 0, 0, 0, 2, -1_981.0, 0.0, 0.0, 854.0, 0.0, 0.0),
    (-1, 0, 0, 1, 0, 4_026.0, 0.0, -353.0, -553.0, 0.0, -139.0),
    (0, 0, 2, 1, 2, 1_660.0, 0.0, -5.0, -710.0, 0.0, -2.0),
    (-1, 0, 2, 4, 2, -1_521.0, 0.0, 9.0, 647.0, 0.0, 4.0),
    (-1, 1, 0, 1, 1, 1_314.0, 0.0, 0.0, -700.0, 0.0, 0.0),
    (0, -2, 2, -2, 1, -1_283.0, 0.0, 0.0, 672.0, 0.0, 0.0),
    (1, 0, 2, 2, 1, -1_331.0, 0.0, 8.0, 663.0, 0.0, 4.0),
    (-2, 0, 2, 2, 2, 1_383.0, 0.0, -2.0, -594.0, 0.0, -2.0),
    (-1, 0, 0, 0, 2, 1_405.0, 0.0, 4.0, -610.0, 0.0, 2.0),
    (1, 1, 2, -2, 2, 1_290.0, 0.0, 0.0, -556.0, 0.0, 0.0),
    (-2, 0, 2, 4, 2, -1_214.0, 0.0, 5.0, 518.0, 0.0, 2.0),
    (-1, 0, 4, 0, 2, 1_146.0, 0.0, -3.0, -490.0, 0.0, -1.0),
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
    fn precession_zero_at_j2000() {
        let p = precession_angles(at_jd(JD_J2000));
        assert_relative_eq!(p.zeta, 0.0, epsilon = 1e-15);
        assert_relative_eq!(p.z, 0.0, epsilon = 1e-15);
        assert_relative_eq!(p.theta, 0.0, epsilon = 1e-15);
    }

    #[test]
    fn precession_known_value_2025() {
        // T = 0.25 (≈ AD 2025.0). Expected ζ_A from the IAU 2006
        // polynomial: 2306.083227·0.25 + 0.2988499·0.0625
        //             + 0.01801828·0.015625 + ... ≈ 576.540 arcsec.
        let p = precession_angles(at_jd(JD_J2000 + 0.25 * 36525.0));
        let expected_zeta_arcsec = 576.539_77;
        assert_relative_eq!(p.zeta / ARCSEC_TO_RAD, expected_zeta_arcsec, epsilon = 1e-3);
    }

    #[test]
    fn mean_obliquity_at_j2000() {
        // ε₀ at J2000.0 = 84381.406″ = 23°26'21.406″.
        let eps = mean_obliquity(at_jd(JD_J2000));
        assert_relative_eq!(eps / ARCSEC_TO_RAD, 84381.406, epsilon = 1e-9);
        // Expressed in degrees:
        assert_relative_eq!(eps.to_degrees(), 23.439_279_44, epsilon = 1e-6);
    }

    #[test]
    fn mean_obliquity_decreases_with_time() {
        // ε is decreasing at ~46.8″ per century to leading order; the
        // T² and higher terms perturb this by a few mas at T=1.
        let eps_now = mean_obliquity(at_jd(JD_J2000));
        let eps_century = mean_obliquity(at_jd(JD_J2000 + 36525.0));
        let diff_arcsec = (eps_now - eps_century) / ARCSEC_TO_RAD;
        // Allow ~10 mas tolerance for higher-order corrections.
        assert_relative_eq!(diff_arcsec, 46.836_769, epsilon = 0.01);
    }

    #[test]
    fn nutation_dominant_term_magnitude() {
        // The 18.6-year nutation has Δψ amplitude ≈ -17.21″ (dominant).
        // At an arbitrary epoch we expect |Δψ| ≲ 20″.
        let n = nutation(at_jd(JD_J2000 + 5000.0));
        let dpsi_arcsec = n.delta_psi / ARCSEC_TO_RAD;
        let deps_arcsec = n.delta_epsilon / ARCSEC_TO_RAD;
        assert!(dpsi_arcsec.abs() < 20.0, "|Δψ| = {dpsi_arcsec}″ too large");
        assert!(deps_arcsec.abs() < 10.0, "|Δε| = {deps_arcsec}″ too large");
    }

    #[test]
    fn nutation_matches_sofa_reference() {
        // SOFA `t_sofa_c.c` test for `iauNut00b`:
        //   date1 = 2400000.5, date2 = 53736.0  (JD 2453736.5 TT,
        //   ≈ 2006-01-15)
        //   expected:
        //     Δψ = -0.9632552291149335e-5 rad
        //     Δε =  0.4063197106621159e-4 rad
        // Tolerances: SOFA tests at 1e-13 (full 2000B precision); we
        // allow 1e-6 rad (~0.2 arcsec) since our purpose is navigation,
        // not astrometry.
        let n = nutation(at_jd(2_453_736.5));
        assert_relative_eq!(n.delta_psi, -0.963_255_229_114_933_5e-5, epsilon = 1e-7);
        assert_relative_eq!(n.delta_epsilon, 0.406_319_710_662_115_9e-4, epsilon = 1e-7);
    }

    #[test]
    fn nutation_at_j2000_in_range() {
        // The nutation series doesn't vanish at J2000 (no special
        // alignment occurs). Magnitudes should be well within the
        // overall ~17″ peak-to-peak envelope of the 18.6-year term.
        let n = nutation(at_jd(JD_J2000));
        let dpsi_arcsec = n.delta_psi / ARCSEC_TO_RAD;
        let deps_arcsec = n.delta_epsilon / ARCSEC_TO_RAD;
        assert!(
            dpsi_arcsec.abs() < 20.0,
            "|Δψ| at J2000 = {dpsi_arcsec}″ out of range"
        );
        assert!(
            deps_arcsec.abs() < 10.0,
            "|Δε| at J2000 = {deps_arcsec}″ out of range"
        );
    }

    #[test]
    fn nutation_is_continuous() {
        // Adjacent evaluations should differ by a small amount, no jumps.
        let n0 = nutation(at_jd(JD_J2000 + 1000.0));
        let n1 = nutation(at_jd(JD_J2000 + 1000.001));
        let ddpsi = (n1.delta_psi - n0.delta_psi).abs() / ARCSEC_TO_RAD;
        assert!(ddpsi < 1e-3, "Δψ jumped {ddpsi} arcsec in 1.4 minutes");
    }
}
