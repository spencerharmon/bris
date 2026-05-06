//! Observer geometry: location, atmosphere, eye height.
//!
//! An [`Observer`] bundles everything needed to convert a geocentric
//! celestial direction into what the observer actually sees: where on
//! Earth they are, the atmosphere they're looking through, and how
//! high above the sea their eye is. Eye height feeds the horizon-dip
//! correction.

use crate::refraction::Atmosphere;
use bris_core::{Latitude, Longitude, Sigma};

/// Observer position and conditions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observer {
    /// Geodetic latitude of the observer.
    pub latitude: Latitude,
    /// Geodetic longitude of the observer (east positive).
    pub longitude: Longitude,
    /// Eye height above the sea surface, in meters.
    pub eye_height_m: f64,
    /// 1σ uncertainty in `eye_height_m`, in meters. Used by the
    /// horizon-dip uncertainty contribution.
    pub eye_height_sigma_m: f64,
    /// Atmospheric conditions at the observer.
    pub atmosphere: Atmosphere,
}

impl Observer {
    /// Default observer used for tests and dev: equator/Greenwich,
    /// 2 m eye height with ±0.5 m uncertainty, standard atmosphere.
    #[must_use]
    pub fn default_dev() -> Self {
        Self {
            latitude: Latitude::EQUATOR,
            longitude: Longitude::PRIME_MERIDIAN,
            eye_height_m: 2.0,
            eye_height_sigma_m: 0.5,
            atmosphere: Atmosphere::STANDARD,
        }
    }

    /// Horizon dip in radians: the angle by which the visible sea
    /// horizon is below the geometric (true) horizon, due to the
    /// observer's eye height above sea level.
    ///
    /// Formula: `dip ≈ 1.76′ × √h_meters` (a standard approximation
    /// folding terrestrial refraction; see Bowditch §16). Returns
    /// the value in radians for direct subtraction from apparent
    /// altitudes.
    #[must_use]
    pub fn horizon_dip_rad(self) -> f64 {
        let dip_arcmin = 1.76 * self.eye_height_m.max(0.0).sqrt();
        // arcmin → radians: divide by (60 arcmin/deg × 180 deg/π rad).
        dip_arcmin * std::f64::consts::PI / (60.0 * 180.0)
    }

    /// 1σ uncertainty contribution to the horizon dip from eye-height
    /// uncertainty, in radians.
    ///
    /// `∂dip/∂h = 0.88′ / √h`, so `σ_dip = (0.88 / √h) × σ_h` in
    /// arcminutes per meter of eye-height uncertainty.
    #[must_use]
    pub fn horizon_dip_sigma(self) -> Sigma {
        let h = self.eye_height_m.max(0.01);
        let sigma_arcmin = 0.88 / h.sqrt() * self.eye_height_sigma_m;
        let sigma_rad = sigma_arcmin * std::f64::consts::PI / (60.0 * 180.0);
        Sigma::new(sigma_rad).unwrap_or(Sigma::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    fn rad_to_arcmin(rad: f64) -> f64 {
        rad * 60.0 * 180.0 / std::f64::consts::PI
    }

    #[test]
    fn dip_at_2m_about_2_5_arcmin() {
        // 1.76′ × √2 = 2.49′
        let obs = Observer::default_dev();
        let dip_arcmin = rad_to_arcmin(obs.horizon_dip_rad());
        assert_relative_eq!(dip_arcmin, 2.49, epsilon = 0.05);
    }

    #[test]
    fn dip_at_20m_about_7_9_arcmin() {
        // 1.76′ × √20 = 7.87′
        let mut obs = Observer::default_dev();
        obs.eye_height_m = 20.0;
        let dip_arcmin = rad_to_arcmin(obs.horizon_dip_rad());
        assert_relative_eq!(dip_arcmin, 7.87, epsilon = 0.1);
    }

    #[test]
    fn higher_eye_height_reduces_dip_uncertainty() {
        // Demonstrates the "higher is better" intuition we agreed on:
        // for the same eye-height σ, dip σ shrinks as 1/√h.
        let mut low = Observer::default_dev();
        low.eye_height_m = 4.0;
        low.eye_height_sigma_m = 0.5;
        let mut high = Observer::default_dev();
        high.eye_height_m = 20.0;
        high.eye_height_sigma_m = 0.5;
        let s_low = low.horizon_dip_sigma().value();
        let s_high = high.horizon_dip_sigma().value();
        // Ratio should be √(20/4) = √5 ≈ 2.24.
        assert_relative_eq!(s_low / s_high, (20.0_f64 / 4.0).sqrt(), epsilon = 0.01);
    }

    #[test]
    fn dip_handles_zero_eye_height() {
        // Surface-level observation: dip is 0, sigma is finite (uses
        // a small floor to avoid div-by-zero).
        let mut obs = Observer::default_dev();
        obs.eye_height_m = 0.0;
        assert!(obs.horizon_dip_rad().abs() < 1e-15);
        assert!(obs.horizon_dip_sigma().value().is_finite());
    }

    #[test]
    fn negative_eye_height_clamps_dip_to_zero() {
        // Defensive: a misconfigured negative height must not blow up
        // the sqrt() — it should clamp to zero, yielding zero dip and
        // a finite sigma (using the same low-h floor).
        let mut obs = Observer::default_dev();
        obs.eye_height_m = -3.0;
        assert!(obs.horizon_dip_rad().abs() < 1e-15);
        assert!(obs.horizon_dip_sigma().value().is_finite());
    }

    #[test]
    fn dip_sigma_matches_analytic_formula() {
        // ∂dip/∂h = 0.88′/√h, so σ_dip = (0.88 / √h) × σ_h arcminutes.
        // At h=4 m, σ_h=0.5 m → σ_dip = 0.88/2 × 0.5 = 0.22′.
        let mut obs = Observer::default_dev();
        obs.eye_height_m = 4.0;
        obs.eye_height_sigma_m = 0.5;
        let sigma_arcmin = rad_to_arcmin(obs.horizon_dip_sigma().value());
        assert_relative_eq!(sigma_arcmin, 0.22, epsilon = 1e-3);
    }

    #[test]
    fn zero_eye_height_sigma_yields_zero_dip_sigma() {
        // No eye-height uncertainty → no dip uncertainty.
        let mut obs = Observer::default_dev();
        obs.eye_height_sigma_m = 0.0;
        assert_eq!(obs.horizon_dip_sigma().value(), 0.0);
    }

    #[test]
    fn default_dev_has_documented_constants() {
        // Lock in the documented defaults so that future edits to the
        // helper produce a visible test failure rather than silently
        // shifting baselines used across the test suite.
        let obs = Observer::default_dev();
        assert_eq!(obs.latitude, Latitude::EQUATOR);
        assert_eq!(obs.longitude, Longitude::PRIME_MERIDIAN);
        assert_relative_eq!(obs.eye_height_m, 2.0);
        assert_relative_eq!(obs.eye_height_sigma_m, 0.5);
        // Atmosphere::STANDARD has finite, non-zero pressure & temperature.
        assert!(obs.atmosphere.pressure_mbar.is_finite());
        assert!(obs.atmosphere.temperature_k.is_finite());
    }
}
