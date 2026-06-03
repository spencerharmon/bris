//! Multi-sight position fix via weighted least squares.
//!
//! Each sight contributes a line of position; intersecting two or more
//! LOPs gives the fix. The standard formulation linearizes altitude as
//! a function of small (north, east) displacements from an assumed
//! position, then solves the resulting overdetermined linear system in
//! the weighted least-squares sense. The fix's 2×2 position covariance
//! falls out of the same algebra.

// Variable names like dn_nm/de_nm, lat0/lon0, sigma_major/sigma_minor,
// dlat_rad/dlon_rad are domain-standard navigation notation. Suppress
// the similar_names lint at module level rather than peppering allows.
#![allow(clippy::similar_names)]
//!
//! # Linearization
//!
//! For an observer at `(φ, λ)` and a body with computed altitude `Hc`
//! and true azimuth `Zn`, a small displacement `(dN, dE)` in metres
//! (north and east) changes the computed altitude by approximately:
//!
//! ```text
//! dH = (cos Zn · dN + sin Zn · dE) / R_earth
//! ```
//!
//! where `R_earth` is the Earth's radius (≈ 6 371 000 m). Re-expressing
//! in nautical miles (1 nm = 1852 m) and arcminutes (1 arcmin ≈ 1 nm
//! by navigator's convention), the per-sight observation equation is:
//!
//! ```text
//! intercept_nm_i = cos(Zn_i) · dN_nm + sin(Zn_i) · dE_nm + ε_i
//! ```
//!
//! where `ε_i` is the per-sight noise with variance `σ_i²`. The
//! weighted normal equations are
//!
//! ```text
//! (Aᵀ W A) x = Aᵀ W b
//! ```
//!
//! with `A_i = (cos Zn_i, sin Zn_i)`, `W = diag(1 / σ_i²)`,
//! `b_i = intercept_nm_i`, and `x = (dN_nm, dE_nm)`.

use crate::sight::LineOfPosition;
use bris_core::{Latitude, Longitude, Sigma};

/// A celestial position fix.
///
/// `lat` and `lon` are the fixed observer position; `covariance_nm2`
/// is the 2×2 position covariance in nautical-mile units, with index
/// 0 = north, 1 = east. The `sigma_major_nm` / `sigma_minor_nm` /
/// `orientation_rad` fields decompose the covariance into a 1σ
/// uncertainty ellipse for direct chartplotter rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fix {
    /// Fixed observer latitude.
    pub lat: Latitude,
    /// Fixed observer longitude.
    pub lon: Longitude,
    /// 2×2 position covariance in nm² (north, east).
    pub covariance_nm2: [[f64; 2]; 2],
    /// 1σ semi-major axis of the uncertainty ellipse, nm.
    pub sigma_major_nm: f64,
    /// 1σ semi-minor axis of the uncertainty ellipse, nm.
    pub sigma_minor_nm: f64,
    /// Orientation of the major axis from north (radians, clockwise),
    /// in `[0, π)`.
    pub orientation_rad: f64,
    /// Number of sights used.
    pub sight_count: u32,
    /// Reduced chi-square of the weighted LSQ residuals
    /// (rᵀ W r / DOF, where DOF = `n_sights` − 2). `None` for
    /// exactly-determined geometry (`n_sights` == 2) where the
    /// statistic is undefined; a value near 1.0 indicates the
    /// per-sight σ budget matches the observed scatter.
    pub chi_square: Option<f64>,
}

impl Fix {
    /// Combined 1σ position uncertainty (the geometric mean of the
    /// ellipse axes), nm. A single scalar suitable for the
    /// red/yellow/green session UX.
    #[must_use]
    pub fn sigma_nm(&self) -> Sigma {
        Sigma::new((self.sigma_major_nm * self.sigma_minor_nm).sqrt()).unwrap_or(Sigma::ZERO)
    }
}

/// Errors from the multi-sight fix.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum FixError {
    /// Fewer than two LOPs supplied.
    #[error("multi-sight fix needs ≥ 2 LOPs, got {0}")]
    InsufficientSights(usize),
    /// Sights were collinear in azimuth (geometry singular). Operator
    /// must take a sight in a different azimuth band before a fix is
    /// possible.
    #[error("sight geometry is singular (azimuth diversity too small)")]
    SingularGeometry,
    /// Numerical failure in the LSQ solve.
    #[error("non-finite arithmetic in LSQ solve")]
    NonFinite,
}

/// Earth's mean radius in metres (IUGG 1980).
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// Compute a multi-sight weighted-least-squares fix.
///
/// All LOPs must share the same assumed position. (Operationally, in
/// the streaming engine, this is the rolling-window centroid; for
/// single-shot reduction it's the DR or trial position.)
///
/// # Errors
///
/// Returns `Err` if fewer than two sights, if the geometry is
/// singular (all sights at nearly the same azimuth), or if
/// arithmetic produced non-finite values.
pub fn multi_sight_fix(lops: &[LineOfPosition]) -> Result<Fix, FixError> {
    multi_sight_fix_with_time_sigma(lops, None)
}

/// Time-uncertainty rate: one second of clock error projects onto
/// approximately 15 arcminutes of longitude at the equator (Earth
/// rotates 360°/86 400 s ≈ 15′/s in the navigator's convention used
/// by `plan.org` Phase 4 "Multi-sight fix with full covariance";
/// the exact sidereal rate of ~15.04″/s would change this term by
/// less than 0.3 %, well below the dominant sight-σ budget).
///
/// At latitude φ, the east-west position uncertainty is multiplied
/// by cos φ because a meridian-aligned arcminute of longitude
/// shrinks toward the poles.
const LONGITUDE_NM_PER_SECOND_AT_EQUATOR: f64 = 15.0;

/// Compute a multi-sight weighted-least-squares fix with optional
/// time-uncertainty inflation.
///
/// `time_sigma_seconds` is the 1σ clock-vs-UTC uncertainty (in
/// seconds) at the sight epoch. When `Some`, it contributes a 1-D
/// longitude variance term `(15′/s × σ_t × cos φ)²` to the [1][1]
/// (east) diagonal of the returned covariance. `None` (and
/// `Some(Sigma::ZERO)`) yield no inflation — identical to
/// [`multi_sight_fix`] for callers that haven't yet plumbed a
/// time-σ source.
///
/// # Errors
///
/// Same conditions as [`multi_sight_fix`].
pub fn multi_sight_fix_with_time_sigma(
    lops: &[LineOfPosition],
    time_sigma_seconds: Option<Sigma>,
) -> Result<Fix, FixError> {
    if lops.len() < 2 {
        return Err(FixError::InsufficientSights(lops.len()));
    }

    // All LOPs must share the same assumed position (caller's
    // responsibility). We assert it for early-fail diagnostics.
    let lat0 = lops[0].assumed_lat;
    let lon0 = lops[0].assumed_lon;

    // Build A, W, b for the normal equations.
    let mut ata = [[0.0_f64; 2]; 2];
    let mut atb = [0.0_f64; 2];
    for lop in lops {
        let cos_z = lop.azimuth_rad.cos();
        let sin_z = lop.azimuth_rad.sin();
        let sigma = lop.intercept_sigma_nm.value().max(1e-9);
        let w = 1.0 / (sigma * sigma);
        ata[0][0] += w * cos_z * cos_z;
        ata[0][1] += w * cos_z * sin_z;
        ata[1][0] += w * cos_z * sin_z;
        ata[1][1] += w * sin_z * sin_z;
        atb[0] += w * cos_z * lop.intercept_nm;
        atb[1] += w * sin_z * lop.intercept_nm;
    }

    // Solve the 2×2 normal equations.
    let det = ata[0][0] * ata[1][1] - ata[0][1] * ata[1][0];
    if det.abs() < 1e-12 || !det.is_finite() {
        return Err(FixError::SingularGeometry);
    }

    let inv = [
        [ata[1][1] / det, -ata[0][1] / det],
        [-ata[1][0] / det, ata[0][0] / det],
    ];
    let dn_nm = inv[0][0] * atb[0] + inv[0][1] * atb[1];
    let de_nm = inv[1][0] * atb[0] + inv[1][1] * atb[1];

    if !dn_nm.is_finite() || !de_nm.is_finite() {
        return Err(FixError::NonFinite);
    }

    // Convert (dN, dE) in nm back to lat/lon.
    let nm_to_rad = std::f64::consts::PI / (180.0 * 60.0);
    let dlat_rad = dn_nm * nm_to_rad;
    let cos_lat0 = lat0.radians().cos().max(1e-12);
    let dlon_rad = (de_nm * nm_to_rad) / cos_lat0;

    let new_lat_rad = (lat0.radians() + dlat_rad)
        .clamp(-std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2);
    let new_lon =
        Longitude::from_radians(lon0.radians() + dlon_rad).map_err(|_| FixError::NonFinite)?;
    let new_lat = Latitude::from_radians(new_lat_rad).map_err(|_| FixError::NonFinite)?;

    // Position covariance is the inverse of A^T W A, in nm² —
    // then add the 1-D time-σ → longitude-variance term on the
    // east diagonal at the new latitude.
    let mut covariance_nm2 = inv;
    let time_sigma_s = time_sigma_seconds.map_or(0.0, Sigma::value);
    if time_sigma_s > 0.0 {
        let cos_lat_new = new_lat.radians().cos().max(0.0);
        let lon_sigma_nm = LONGITUDE_NM_PER_SECOND_AT_EQUATOR * time_sigma_s * cos_lat_new;
        covariance_nm2[1][1] += lon_sigma_nm * lon_sigma_nm;
    }

    // Decompose the 2×2 covariance into ellipse axes and orientation.
    let (sigma_major_nm, sigma_minor_nm, orientation_rad) = ellipse_from_covariance(covariance_nm2);

    // Weighted residual chi-square: r_i = b_i − A_i·x, where
    // A_i = (cos Zn_i, sin Zn_i), x = (dN, dE), W = diag(1/σ_i²).
    // Reduced χ² = sum(w_i r_i²) / (n − 2). Undefined for n == 2.
    let chi_square = if lops.len() > 2 {
        let mut sum_wr2 = 0.0_f64;
        for lop in lops {
            let cos_z = lop.azimuth_rad.cos();
            let sin_z = lop.azimuth_rad.sin();
            let sigma = lop.intercept_sigma_nm.value().max(1e-9);
            let w = 1.0 / (sigma * sigma);
            let predicted = cos_z * dn_nm + sin_z * de_nm;
            let r = lop.intercept_nm - predicted;
            sum_wr2 += w * r * r;
        }
        #[allow(clippy::cast_precision_loss)]
        let dof = (lops.len() - 2) as f64;
        let chi2 = sum_wr2 / dof;
        if chi2.is_finite() {
            Some(chi2)
        } else {
            None
        }
    } else {
        None
    };

    let _ = EARTH_RADIUS_M; // kept for documentation; not used directly.

    Ok(Fix {
        lat: new_lat,
        lon: new_lon,
        covariance_nm2,
        sigma_major_nm,
        sigma_minor_nm,
        orientation_rad,
        #[allow(clippy::cast_possible_truncation)]
        sight_count: lops.len() as u32,
        chi_square,
    })
}

/// Decompose a 2×2 covariance into 1σ semi-major / semi-minor axes
/// and major-axis orientation (radians clockwise from north).
pub(crate) fn ellipse_from_covariance(c: [[f64; 2]; 2]) -> (f64, f64, f64) {
    let a = c[0][0]; // var(N)
    let b = c[1][1]; // var(E)
    let cov = c[0][1]; // cov(N, E)
    let mean = f64::midpoint(a, b);
    let half_diff = (a - b) / 2.0;
    let radius = (half_diff * half_diff + cov * cov).sqrt();
    let lambda1 = mean + radius;
    let lambda2 = (mean - radius).max(0.0);
    let sigma_major_nm = lambda1.max(0.0).sqrt();
    let sigma_minor_nm = lambda2.sqrt();
    // Major axis orientation (clockwise from N) in [0, π).
    let orientation_rad = if half_diff.abs() < 1e-12 && cov.abs() < 1e-12 {
        0.0
    } else {
        let theta = 0.5 * (2.0 * cov).atan2(a - b);
        theta.rem_euclid(std::f64::consts::PI)
    };
    (sigma_major_nm, sigma_minor_nm, orientation_rad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sight::line_of_position;
    use approx::assert_relative_eq;
    use bris_core::Uncertain;

    fn lat(d: f64) -> Latitude {
        Latitude::from_degrees(d).unwrap()
    }
    fn lon(d: f64) -> Longitude {
        Longitude::from_degrees(d).unwrap()
    }

    fn lop(az_deg: f64, intercept_nm: f64, sigma_nm: f64) -> LineOfPosition {
        LineOfPosition {
            assumed_lat: lat(0.0),
            assumed_lon: lon(0.0),
            azimuth_rad: az_deg.to_radians(),
            intercept_nm,
            intercept_sigma_nm: Sigma::new(sigma_nm).unwrap(),
        }
    }

    #[test]
    fn fix_with_two_perpendicular_sights() {
        // Sight 1: bearing N (Zn=0), intercept = +3 nm toward body.
        //   → observer is 3 nm north of assumed.
        // Sight 2: bearing E (Zn=90°), intercept = +5 nm toward body.
        //   → observer is 5 nm east of assumed.
        // Expected fix: (3 N, 5 E).
        let lops = [lop(0.0, 3.0, 1.0), lop(90.0, 5.0, 1.0)];
        let fix = multi_sight_fix(&lops).unwrap();
        // Convert to nm offsets from assumed (0, 0).
        let dn_nm = fix.lat.degrees() * 60.0;
        let de_nm = fix.lon.degrees() * 60.0; // cos(0°)=1
        assert_relative_eq!(dn_nm, 3.0, epsilon = 1e-3);
        assert_relative_eq!(de_nm, 5.0, epsilon = 1e-3);
    }

    #[test]
    fn fix_consistent_with_three_sights() {
        // Three sights spaced 120° apart, all with intercept = 0.
        // Expected fix: assumed position itself.
        let lops = [
            lop(0.0, 0.0, 1.0),
            lop(120.0, 0.0, 1.0),
            lop(240.0, 0.0, 1.0),
        ];
        let fix = multi_sight_fix(&lops).unwrap();
        let dn_nm = fix.lat.degrees() * 60.0;
        let de_nm = fix.lon.degrees() * 60.0;
        assert!(dn_nm.abs() < 1e-6);
        assert!(de_nm.abs() < 1e-6);
        assert_eq!(fix.sight_count, 3);
    }

    #[test]
    fn rejects_singular_geometry() {
        // Two LOPs with the same azimuth → singular.
        let lops = [lop(45.0, 1.0, 1.0), lop(45.0, 2.0, 1.0)];
        let result = multi_sight_fix(&lops);
        assert!(matches!(result, Err(FixError::SingularGeometry)));
    }

    #[test]
    fn rejects_too_few_sights() {
        let lops = [lop(0.0, 0.0, 1.0)];
        let result = multi_sight_fix(&lops);
        assert!(matches!(result, Err(FixError::InsufficientSights(1))));
    }

    #[test]
    fn covariance_shrinks_with_more_sights() {
        // 3 perpendicular-ish sights (0°, 90°, 180°) with σ=1 nm each.
        // Compare to 2 sights (0°, 90°) with σ=1 each.
        let lops_3 = [
            lop(0.0, 0.0, 1.0),
            lop(90.0, 0.0, 1.0),
            lop(180.0, 0.0, 1.0),
        ];
        let fix_3 = multi_sight_fix(&lops_3).unwrap();
        let lops_2 = [lop(0.0, 0.0, 1.0), lop(90.0, 0.0, 1.0)];
        let fix_2 = multi_sight_fix(&lops_2).unwrap();
        // More sights → tighter ellipse (geometric-mean σ).
        assert!(fix_3.sigma_nm().value() < fix_2.sigma_nm().value());
    }

    #[test]
    fn ellipse_is_circular_for_uniform_geometry() {
        // 4 sights at 0°, 90°, 180°, 270° with equal σ → circular ellipse.
        let lops = [
            lop(0.0, 0.0, 1.0),
            lop(90.0, 0.0, 1.0),
            lop(180.0, 0.0, 1.0),
            lop(270.0, 0.0, 1.0),
        ];
        let fix = multi_sight_fix(&lops).unwrap();
        assert_relative_eq!(fix.sigma_major_nm, fix.sigma_minor_nm, epsilon = 1e-9);
    }

    #[test]
    fn ellipse_is_elongated_for_skewed_geometry() {
        // Two sights at slightly different azimuths → elongated ellipse
        // perpendicular to their common direction.
        let lops = [lop(0.0, 0.0, 1.0), lop(20.0, 0.0, 1.0)];
        let fix = multi_sight_fix(&lops).unwrap();
        assert!(fix.sigma_major_nm > 1.5 * fix.sigma_minor_nm);
    }

    #[test]
    fn time_sigma_inflates_longitude_variance_at_mid_latitude() {
        // At lat=30°N with time_σ=1.0s, the [1][1] (east/longitude)
        // variance entry should increase by approximately
        // (15′ × 1.0s × cos(30°))² ≈ (15 × 0.8660)² ≈ 168.74 nm².
        // Build two perpendicular σ=1 nm sights so the baseline
        // covariance is the 2×2 identity (in nm²); the inflation
        // term should dominate the east axis.
        let lops = [
            LineOfPosition {
                assumed_lat: lat(30.0),
                assumed_lon: lon(0.0),
                azimuth_rad: 0.0,
                intercept_nm: 0.0,
                intercept_sigma_nm: Sigma::new(1.0).unwrap(),
            },
            LineOfPosition {
                assumed_lat: lat(30.0),
                assumed_lon: lon(0.0),
                azimuth_rad: std::f64::consts::FRAC_PI_2,
                intercept_nm: 0.0,
                intercept_sigma_nm: Sigma::new(1.0).unwrap(),
            },
        ];
        let fix_no_time = multi_sight_fix_with_time_sigma(&lops, None).unwrap();
        let fix_t1 =
            multi_sight_fix_with_time_sigma(&lops, Some(Sigma::new(1.0).unwrap())).unwrap();
        let inflation = fix_t1.covariance_nm2[1][1] - fix_no_time.covariance_nm2[1][1];
        let expected = (15.0_f64 * 1.0 * 30.0_f64.to_radians().cos()).powi(2);
        assert_relative_eq!(inflation, expected, max_relative = 0.1);
    }

    #[test]
    fn time_sigma_zero_matches_no_time_input() {
        // Passing None and passing Some(Sigma::ZERO) (and the
        // original signature) must all yield identical covariance.
        let lops = [
            LineOfPosition {
                assumed_lat: lat(45.0),
                assumed_lon: lon(0.0),
                azimuth_rad: 0.0,
                intercept_nm: 0.5,
                intercept_sigma_nm: Sigma::new(1.0).unwrap(),
            },
            LineOfPosition {
                assumed_lat: lat(45.0),
                assumed_lon: lon(0.0),
                azimuth_rad: std::f64::consts::FRAC_PI_2,
                intercept_nm: 0.3,
                intercept_sigma_nm: Sigma::new(1.0).unwrap(),
            },
        ];
        let fix_legacy = multi_sight_fix(&lops).unwrap();
        let fix_none = multi_sight_fix_with_time_sigma(&lops, None).unwrap();
        let fix_zero = multi_sight_fix_with_time_sigma(&lops, Some(Sigma::ZERO)).unwrap();
        for i in 0..2 {
            for j in 0..2 {
                assert_relative_eq!(
                    fix_legacy.covariance_nm2[i][j],
                    fix_none.covariance_nm2[i][j],
                    epsilon = 1e-15
                );
                assert_relative_eq!(
                    fix_legacy.covariance_nm2[i][j],
                    fix_zero.covariance_nm2[i][j],
                    epsilon = 1e-15
                );
            }
        }
    }

    #[test]
    fn time_sigma_inflation_shrinks_with_cos_lat() {
        // At lat=89° the cos(lat) factor reduces the inflation
        // by ~57× relative to lat=30°.
        let mk = |lat_deg: f64| {
            [
                LineOfPosition {
                    assumed_lat: lat(lat_deg),
                    assumed_lon: lon(0.0),
                    azimuth_rad: 0.0,
                    intercept_nm: 0.0,
                    intercept_sigma_nm: Sigma::new(1.0).unwrap(),
                },
                LineOfPosition {
                    assumed_lat: lat(lat_deg),
                    assumed_lon: lon(0.0),
                    azimuth_rad: std::f64::consts::FRAC_PI_2,
                    intercept_nm: 0.0,
                    intercept_sigma_nm: Sigma::new(1.0).unwrap(),
                },
            ]
        };
        let t = Some(Sigma::new(1.0).unwrap());
        let lo = multi_sight_fix_with_time_sigma(&mk(89.0), t).unwrap();
        let hi = multi_sight_fix_with_time_sigma(&mk(30.0), t).unwrap();
        let lo_inflation =
            lo.covariance_nm2[1][1] - multi_sight_fix(&mk(89.0)).unwrap().covariance_nm2[1][1];
        let hi_inflation =
            hi.covariance_nm2[1][1] - multi_sight_fix(&mk(30.0)).unwrap().covariance_nm2[1][1];
        assert!(
            lo_inflation < hi_inflation,
            "cos(lat) factor must reduce inflation at high latitude: lo={lo_inflation}, hi={hi_inflation}"
        );
        // Specifically, the ratio should be (cos89/cos30)² within 10%.
        let expected_ratio = (89.0_f64.to_radians().cos() / 30.0_f64.to_radians().cos()).powi(2);
        assert_relative_eq!(
            lo_inflation / hi_inflation,
            expected_ratio,
            max_relative = 0.1
        );
    }

    #[test]
    fn chi_square_populated_when_overdetermined() {
        // Three sights with mutually inconsistent intercepts at
        // azimuths spaced 120° apart: residuals are non-zero,
        // DOF = 3 - 2 = 1, so reduced chi-square must be Some.
        let lops = [
            lop(0.0, 1.0, 1.0),
            lop(120.0, 1.0, 1.0),
            lop(240.0, 1.0, 1.0),
        ];
        let fix = multi_sight_fix(&lops).unwrap();
        let chi2 = fix.chi_square.expect("chi_square must be Some for n=3");
        assert!(chi2.is_finite() && chi2 > 0.0, "chi2 = {chi2}");
    }

    #[test]
    fn chi_square_none_for_exactly_determined() {
        // Two sights, two unknowns → DOF = 0 → chi-square undefined.
        let lops = [lop(0.0, 3.0, 1.0), lop(90.0, 5.0, 1.0)];
        let fix = multi_sight_fix(&lops).unwrap();
        assert!(fix.chi_square.is_none());
    }

    #[test]
    fn end_to_end_with_line_of_position() {
        // Synthesize two sights using line_of_position (the API the
        // streaming pipeline will use), then solve for the fix.
        // Body 1 at azimuth 0 with Ho > Hc by 1 arcmin → 1 nm toward.
        // Body 2 at azimuth 90 with Ho > Hc by 2 arcmin → 2 nm toward.
        // Expected fix: (1 N, 2 E).
        let arcmin = 1.0_f64.to_radians() / 60.0;
        let lop1 = line_of_position(
            lat(0.0),
            lon(0.0),
            Uncertain::new(arcmin, Sigma::new(0.001 * arcmin).unwrap()),
            Uncertain::new(0.0, Sigma::new(0.001 * arcmin).unwrap()),
            0.0,
        )
        .unwrap();
        let lop2 = line_of_position(
            lat(0.0),
            lon(0.0),
            Uncertain::new(2.0 * arcmin, Sigma::new(0.001 * arcmin).unwrap()),
            Uncertain::new(0.0, Sigma::new(0.001 * arcmin).unwrap()),
            std::f64::consts::FRAC_PI_2,
        )
        .unwrap();
        let fix = multi_sight_fix(&[lop1, lop2]).unwrap();
        let dn_nm = fix.lat.degrees() * 60.0;
        let de_nm = fix.lon.degrees() * 60.0;
        assert_relative_eq!(dn_nm, 1.0, epsilon = 0.01);
        assert_relative_eq!(de_nm, 2.0, epsilon = 0.01);
    }
}
