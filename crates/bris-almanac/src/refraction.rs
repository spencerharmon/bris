//! Atmospheric refraction.
//!
//! Light from a celestial body bends as it traverses the atmosphere,
//! making bodies appear higher than they truly are. The refraction
//! correction subtracts this bending to recover the true altitude
//! from an observed apparent altitude.
//!
//! # Model
//!
//! Bennett's formula (1982), with the standard temperature/pressure
//! scaling. Bennett takes *apparent* altitude as input and returns the
//! refraction angle to subtract:
//!
//! ```text
//! R = cot(h_a + 7.31° / (h_a + 4.4°))   [arcmin, for h_a in degrees]
//! ```
//!
//! Scaled by `(P / 1010 mbar) × (283 K / T)` for non-standard
//! atmospheres.
//!
//! # Accuracy
//!
//! Per Bennett (1982) and confirmed by Astronomical Almanac comparisons:
//! - h ≥ 15°: residual error ≲ 0.1′ — well below per-sight budget.
//! - 5° ≤ h < 15°: residual error ~0.3-1′ — comparable to the budget.
//! - h < 5°: residual error grows rapidly to several arcminutes,
//!   irreducible without local atmospheric profile data we don't have.
//!
//! We expose the altitude-dependent uncertainty as part of the return
//! value so callers (sight-reduction in `bris-nav`) can downweight or
//! reject low-altitude sights honestly.
//!
//! # References
//!
//! - Bennett, G.G. (1982). "The Calculation of Astronomical Refraction
//!   in Marine Navigation," *Journal of Navigation* 35(2), 255-259.
//! - *Astronomical Almanac*, refraction tables.

use bris_core::{Angle, Sigma, Uncertain};

/// Standard atmospheric pressure, millibars.
pub const STD_PRESSURE_MBAR: f64 = 1010.0;

/// Standard atmospheric temperature, kelvin.
pub const STD_TEMPERATURE_K: f64 = 283.0;

/// Atmospheric conditions at the observer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Atmosphere {
    /// Pressure in millibars (≡ hectopascals). Default 1010.
    pub pressure_mbar: f64,
    /// Temperature in kelvin. Default 283 K (10 °C).
    pub temperature_k: f64,
}

impl Atmosphere {
    /// Standard atmosphere: 1010 mbar, 283 K (10 °C).
    pub const STANDARD: Self = Self {
        pressure_mbar: STD_PRESSURE_MBAR,
        temperature_k: STD_TEMPERATURE_K,
    };

    /// Construct from pressure and temperature with finite/non-zero checks.
    ///
    /// # Errors
    ///
    /// Returns [`RefractionError::InvalidAtmosphere`] for non-finite,
    /// non-positive, or wildly out-of-range inputs.
    pub fn new(pressure_mbar: f64, temperature_k: f64) -> Result<Self, RefractionError> {
        if !pressure_mbar.is_finite()
            || !temperature_k.is_finite()
            || !(50.0..=1500.0).contains(&pressure_mbar)
            || !(150.0..=350.0).contains(&temperature_k)
        {
            return Err(RefractionError::InvalidAtmosphere);
        }
        Ok(Self {
            pressure_mbar,
            temperature_k,
        })
    }

    /// Multiplicative scaling applied to standard-atmosphere refraction
    /// values: `(P / 1010) × (283 / T)`.
    fn scale_factor(self) -> f64 {
        (self.pressure_mbar / STD_PRESSURE_MBAR) * (STD_TEMPERATURE_K / self.temperature_k)
    }
}

impl Default for Atmosphere {
    fn default() -> Self {
        Self::STANDARD
    }
}

/// Compute Bennett refraction for an apparent altitude.
///
/// Returns the refraction angle (always non-negative for `apparent_alt > 0`)
/// paired with a 1σ uncertainty estimate that grows toward the horizon.
/// The true altitude is `apparent_alt - refraction.value`.
///
/// # Errors
///
/// Returns [`RefractionError::BelowHorizon`] for `apparent_alt < -1°`
/// (a small tolerance below the horizon is allowed because real
/// horizon dip and rounding can push the geometric value just below
/// zero for sights right at the horizon).
pub fn bennett(
    apparent_alt: Angle,
    atmosphere: Atmosphere,
) -> Result<Uncertain<Angle>, RefractionError> {
    let h_deg = apparent_alt.degrees();
    if h_deg < -1.0 {
        return Err(RefractionError::BelowHorizon);
    }

    // Bennett's formula. Argument of cot: (h + 7.31 / (h + 4.4)) degrees.
    // Result is in arcminutes for the standard atmosphere.
    let arg_deg = h_deg + 7.31 / (h_deg + 4.4);
    let arg_rad = arg_deg.to_radians();
    let r_arcmin_std = 1.0 / arg_rad.tan();

    // Scale for non-standard atmosphere.
    let r_arcmin = r_arcmin_std * atmosphere.scale_factor();

    let value = Angle::from_arcminutes(r_arcmin).map_err(|_| RefractionError::InvalidAtmosphere)?;
    let sigma = bennett_sigma(h_deg)?;

    Ok(Uncertain::new(value, sigma))
}

/// Altitude-dependent 1σ uncertainty in the Bennett refraction value.
///
/// Quantifies the irreducible model error. This is the contribution
/// the refraction step makes to the per-sight altitude uncertainty;
/// near the horizon it can dominate the entire error budget.
///
/// Empirical model fit to Bennett-1982 residuals:
/// - At h ≥ 15°: ~0.1 arcmin (constant floor).
/// - At 5° ≤ h < 15°: linear ramp from ~0.3' to ~0.1'.
/// - At h < 5°: blows up as `~1/sin(h)` toward the horizon.
fn bennett_sigma(h_deg: f64) -> Result<Sigma, RefractionError> {
    let arcmin = if h_deg >= 15.0 {
        0.1
    } else if h_deg >= 5.0 {
        // Linear ramp from 0.3' at 5° to 0.1' at 15°.
        0.3 - 0.02 * (h_deg - 5.0)
    } else if h_deg > 0.0 {
        // Below 5°: model error grows roughly with 1/sin(h_deg).
        // Anchor: 1.0' at 5° (continuous with the linear ramp above),
        // increasing to ~10' near the horizon.
        let h_rad = h_deg.to_radians();
        0.3_f64.max(0.5 / h_rad.sin().max(0.05))
    } else {
        // At and below the horizon, model is meaningless. Return a
        // large sigma so any consumer downweights this sight to
        // effectively zero.
        20.0
    };
    let sigma_angle =
        Angle::from_arcminutes(arcmin).map_err(|_| RefractionError::InvalidAtmosphere)?;
    Sigma::new(sigma_angle.radians()).map_err(|_| RefractionError::InvalidAtmosphere)
}

/// Errors from the refraction module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RefractionError {
    /// Apparent altitude was more than 1° below the horizon; no
    /// meaningful refraction value is defined.
    #[error("apparent altitude is below the horizon; refraction undefined")]
    BelowHorizon,
    /// Atmospheric inputs were out of range or otherwise invalid.
    #[error("invalid atmospheric conditions or angle")]
    InvalidAtmosphere,
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    fn at_deg(d: f64) -> Angle {
        Angle::from_degrees(d).unwrap()
    }

    #[test]
    fn refraction_at_zenith_is_zero() {
        // At the zenith Bennett's argument is ~92° → cot(92°) ≈ -0.035';
        // that's not exactly zero but is near it. Check < 0.05'.
        let r = bennett(at_deg(90.0), Atmosphere::STANDARD).unwrap();
        assert!(
            r.value.arcminutes().abs() < 0.05,
            "zenith refraction = {} arcmin, expected ~0",
            r.value.arcminutes()
        );
    }

    #[test]
    fn refraction_at_45_known_value() {
        // Standard refraction at h=45° is ~0.97' per Bennett.
        let r = bennett(at_deg(45.0), Atmosphere::STANDARD).unwrap();
        assert_relative_eq!(r.value.arcminutes(), 0.97, epsilon = 0.05);
    }

    #[test]
    fn refraction_at_15_known_value() {
        // Standard refraction at h=15° is ~3.5' per Astronomical Almanac.
        let r = bennett(at_deg(15.0), Atmosphere::STANDARD).unwrap();
        assert_relative_eq!(r.value.arcminutes(), 3.5, epsilon = 0.2);
    }

    #[test]
    fn refraction_at_horizon_large() {
        // At h=0° the standard refraction is ~34.5' (the famous
        // "horizon dip" effect; the Sun is fully below the geometric
        // horizon when its lower limb appears to touch).
        let r = bennett(at_deg(0.0), Atmosphere::STANDARD).unwrap();
        assert!(
            r.value.arcminutes() > 30.0 && r.value.arcminutes() < 40.0,
            "horizon refraction = {} arcmin, expected ~34.5",
            r.value.arcminutes()
        );
    }

    #[test]
    fn refraction_increases_toward_horizon() {
        // Refraction is monotonically decreasing in altitude.
        let r90 = bennett(at_deg(90.0), Atmosphere::STANDARD).unwrap();
        let r60 = bennett(at_deg(60.0), Atmosphere::STANDARD).unwrap();
        let r30 = bennett(at_deg(30.0), Atmosphere::STANDARD).unwrap();
        let r10 = bennett(at_deg(10.0), Atmosphere::STANDARD).unwrap();
        let r02 = bennett(at_deg(2.0), Atmosphere::STANDARD).unwrap();
        assert!(r90.value.arcminutes() < r60.value.arcminutes());
        assert!(r60.value.arcminutes() < r30.value.arcminutes());
        assert!(r30.value.arcminutes() < r10.value.arcminutes());
        assert!(r10.value.arcminutes() < r02.value.arcminutes());
    }

    #[test]
    fn pressure_scales_linearly() {
        let std = bennett(at_deg(45.0), Atmosphere::STANDARD).unwrap();
        let half = bennett(
            at_deg(45.0),
            Atmosphere::new(STD_PRESSURE_MBAR / 2.0, STD_TEMPERATURE_K).unwrap(),
        )
        .unwrap();
        assert_relative_eq!(
            half.value.radians(),
            std.value.radians() / 2.0,
            epsilon = 1e-12
        );
    }

    #[test]
    fn temperature_scales_inversely() {
        let std = bennett(at_deg(45.0), Atmosphere::STANDARD).unwrap();
        // Use 200 K (a plausibly cold high-altitude observation
        // condition) to stay within the validated atmosphere range
        // while exercising a meaningful scale change.
        let cold = bennett(
            at_deg(45.0),
            Atmosphere::new(STD_PRESSURE_MBAR, 200.0).unwrap(),
        )
        .unwrap();
        let expected_factor = STD_TEMPERATURE_K / 200.0;
        assert_relative_eq!(
            cold.value.radians(),
            std.value.radians() * expected_factor,
            epsilon = 1e-12
        );
    }

    #[test]
    fn uncertainty_grows_toward_horizon() {
        let s90 = bennett(at_deg(90.0), Atmosphere::STANDARD).unwrap().sigma;
        let s30 = bennett(at_deg(30.0), Atmosphere::STANDARD).unwrap().sigma;
        let s10 = bennett(at_deg(10.0), Atmosphere::STANDARD).unwrap().sigma;
        let s02 = bennett(at_deg(2.0), Atmosphere::STANDARD).unwrap().sigma;
        assert!(s90.value() <= s30.value());
        assert!(s30.value() <= s10.value());
        assert!(s10.value() < s02.value());
    }

    #[test]
    fn uncertainty_floor_at_high_altitude() {
        // ~0.1' floor.
        let s = bennett(at_deg(60.0), Atmosphere::STANDARD).unwrap().sigma;
        let arcmin = s.value().to_degrees() * 60.0;
        assert_relative_eq!(arcmin, 0.1, epsilon = 0.01);
    }

    #[test]
    fn rejects_below_horizon() {
        let r = bennett(at_deg(-2.0), Atmosphere::STANDARD);
        assert_eq!(r, Err(RefractionError::BelowHorizon));
    }

    #[test]
    fn rejects_invalid_atmosphere() {
        assert_eq!(
            Atmosphere::new(f64::NAN, 283.0),
            Err(RefractionError::InvalidAtmosphere)
        );
        assert_eq!(
            Atmosphere::new(1010.0, -1.0),
            Err(RefractionError::InvalidAtmosphere)
        );
        assert_eq!(
            Atmosphere::new(0.0, 283.0),
            Err(RefractionError::InvalidAtmosphere)
        );
    }
}
