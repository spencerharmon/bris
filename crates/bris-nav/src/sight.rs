//! Sight reduction: convert an observed altitude into a line of position.
//!
//! Given:
//! - The body's *apparent place* at the observation instant from
//!   `bris-almanac::apparent` (this gives the body's computed altitude
//!   `Hc` and azimuth `Zn` from an assumed observer position).
//! - The *observed* altitude `Ho` measured by the vision pipeline,
//!   from `bris-vision::measure_altitude`.
//!
//! Compute the *intercept* `a = Ho − Hc` and represent the line of
//! position as `(assumed_position, azimuth, intercept)`. The actual
//! observer position lies along this line within the combined
//! per-sight uncertainty.
//!
//! # Sign convention
//!
//! Following the standard navigator's "Marc Saint-Hilaire" method:
//! - Positive intercept: the true position is `a` nautical miles
//!   *toward* the body's geographic point (subpoint).
//! - Negative intercept: the true position is `|a|` nautical miles
//!   *away from* the body's GP.
//!
//! # Uncertainty
//!
//! Each LOP carries the per-sight altitude σ from the apparent-place
//! pipeline (refraction + dip + aberration placeholder + others) and
//! the observed-altitude σ from the vision pipeline (horizon fit +
//! centroid). Quadrature combination gives the LOP's positional σ
//! perpendicular to its direction.

// Variable names like ho_rad/hc_rad are standard navigation notation.
#![allow(clippy::similar_names)]

use bris_core::{Latitude, Longitude, Sigma, Uncertain};

/// One sight's line of position.
///
/// The LOP is a great circle on the Earth perpendicular to the body's
/// azimuth, passing `intercept` nautical miles from the `assumed` point
/// in the direction `azimuth_rad` (or opposite if `intercept` is
/// negative).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineOfPosition {
    /// Assumed (DR or trial) observer position used in the reduction.
    pub assumed_lat: Latitude,
    /// Assumed observer longitude.
    pub assumed_lon: Longitude,
    /// True azimuth of the body from the assumed position, radians,
    /// `[0, 2π)`. The LOP direction is perpendicular to this.
    pub azimuth_rad: f64,
    /// Intercept in nautical miles. Positive = true position is
    /// toward the body's GP.
    pub intercept_nm: f64,
    /// 1σ uncertainty in the intercept, nautical miles. Combines
    /// observed-altitude σ and computed-altitude σ.
    pub intercept_sigma_nm: Sigma,
}

/// Errors from sight reduction.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum LopError {
    /// One of the input altitudes was non-finite.
    #[error("non-finite altitude in sight reduction")]
    NonFinite,
}

/// Convert one arcminute of arc on the Earth to nautical miles.
///
/// The nautical mile is *defined* as 1852 m, and one minute of latitude
/// is approximately 1 nm by historical design. The exact value (1852 m
/// = 0.999340 arcmin × `R_earth`) makes 1 arcmin ≈ 1.0007 nm, but the
/// navigator's convention is exact: 1 arcmin = 1 nm. We follow it.
pub const NM_PER_ARCMIN: f64 = 1.0;

/// Compute the line of position from one sight.
///
/// `observed_altitude` is the apparent altitude measured by the vision
/// pipeline (Hs, the sextant-equivalent value, with refraction etc.
/// not yet subtracted — but in our pipeline the apparent-place chain
/// produces Hc with the same conventions, so the difference Ho − Hc
/// is meaningful directly).
///
/// `computed_altitude` and `azimuth_rad` come from the apparent-place
/// chain at the assumed observer position.
///
/// # Errors
///
/// Returns [`LopError::NonFinite`] if any input is NaN/infinite.
pub fn line_of_position(
    assumed_lat: Latitude,
    assumed_lon: Longitude,
    observed_altitude: Uncertain<f64>,
    computed_altitude: Uncertain<f64>,
    azimuth_rad: f64,
) -> Result<LineOfPosition, LopError> {
    if !observed_altitude.value.is_finite()
        || !computed_altitude.value.is_finite()
        || !azimuth_rad.is_finite()
    {
        return Err(LopError::NonFinite);
    }

    // Intercept in radians, then converted to arcminutes (≡ nm).
    let intercept_rad = observed_altitude.value - computed_altitude.value;
    let intercept_arcmin = intercept_rad.to_degrees() * 60.0;
    let intercept_nm = intercept_arcmin * NM_PER_ARCMIN;

    // Combined σ in radians → arcminutes → nm.
    let sigma_rad = observed_altitude.sigma.combine(computed_altitude.sigma);
    let sigma_arcmin = sigma_rad.value().to_degrees() * 60.0;
    let intercept_sigma_nm = Sigma::new(sigma_arcmin * NM_PER_ARCMIN).unwrap_or(Sigma::ZERO);

    Ok(LineOfPosition {
        assumed_lat,
        assumed_lon,
        azimuth_rad: azimuth_rad.rem_euclid(std::f64::consts::TAU),
        intercept_nm,
        intercept_sigma_nm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lat(d: f64) -> Latitude {
        Latitude::from_degrees(d).unwrap()
    }
    fn lon(d: f64) -> Longitude {
        Longitude::from_degrees(d).unwrap()
    }

    #[test]
    fn intercept_is_observed_minus_computed() {
        // Ho = 45°00.5', Hc = 45°00.0' → intercept = +0.5 nm toward body.
        let ho_rad = (45.0_f64 + 0.5 / 60.0).to_radians();
        let hc_rad = 45.0_f64.to_radians();
        let lop = line_of_position(
            lat(0.0),
            lon(0.0),
            Uncertain::new(ho_rad, Sigma::new(0.001).unwrap()),
            Uncertain::new(hc_rad, Sigma::new(0.001).unwrap()),
            0.0,
        )
        .unwrap();
        assert!(
            (lop.intercept_nm - 0.5).abs() < 1e-6,
            "intercept = {}, expected 0.5",
            lop.intercept_nm
        );
    }

    #[test]
    fn intercept_negative_means_away() {
        // Ho < Hc → negative intercept (body appears lower than expected;
        // observer is farther from the body's GP than assumed).
        let ho_rad = (30.0_f64 - 0.3 / 60.0).to_radians();
        let hc_rad = 30.0_f64.to_radians();
        let lop = line_of_position(
            lat(0.0),
            lon(0.0),
            Uncertain::new(ho_rad, Sigma::new(0.001).unwrap()),
            Uncertain::new(hc_rad, Sigma::new(0.001).unwrap()),
            std::f64::consts::PI,
        )
        .unwrap();
        assert!((lop.intercept_nm - (-0.3)).abs() < 1e-6);
    }

    #[test]
    fn sigma_combines_in_quadrature() {
        // Both inputs have σ = 1 arcmin. Combined: √2 arcmin ≈ 1.414 nm.
        let arcmin_in_rad = 1.0_f64.to_radians() / 60.0;
        let lop = line_of_position(
            lat(0.0),
            lon(0.0),
            Uncertain::new(0.5, Sigma::new(arcmin_in_rad).unwrap()),
            Uncertain::new(0.5, Sigma::new(arcmin_in_rad).unwrap()),
            0.0,
        )
        .unwrap();
        let expected_nm = (2.0_f64).sqrt();
        assert!(
            (lop.intercept_sigma_nm.value() - expected_nm).abs() < 1e-6,
            "sigma = {}, expected {}",
            lop.intercept_sigma_nm.value(),
            expected_nm
        );
    }

    #[test]
    fn azimuth_is_normalized() {
        let lop = line_of_position(
            lat(0.0),
            lon(0.0),
            Uncertain::new(0.5, Sigma::new(0.001).unwrap()),
            Uncertain::new(0.5, Sigma::new(0.001).unwrap()),
            -std::f64::consts::PI, // negative; should normalize to π
        )
        .unwrap();
        assert!((lop.azimuth_rad - std::f64::consts::PI).abs() < 1e-12);
    }

    #[test]
    fn rejects_non_finite_altitude() {
        let result = line_of_position(
            lat(0.0),
            lon(0.0),
            Uncertain::new(f64::NAN, Sigma::new(0.001).unwrap()),
            Uncertain::new(0.5, Sigma::new(0.001).unwrap()),
            0.0,
        );
        assert_eq!(result, Err(LopError::NonFinite));
    }
}
