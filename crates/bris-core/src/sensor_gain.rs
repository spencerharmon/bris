//! Sensor conversion gain (electrons per ADU).
//!
//! Carried as a value type alongside captured frames so the
//! photon-shot-noise weights used in sub-pixel centroid
//! refinement (and any other inverse-variance fit) reflect
//! the *measured* sensor gain rather than a constant.
//!
//! "Measured" here always refers to **analog** conversion
//! gain — what V4L2 reports as `V4L2_CID_ANALOGUE_GAIN` or
//! what `CameraCharacteristics.SENSOR_INFO_SENSITIVITY_RANGE`
//! reflects via ISO at the analog-gain plateau. Digital gain
//! is *post*-quantization and does not change the per-pixel
//! shot-noise variance in ADU²; folding it in here would
//! inflate σ artificially. Capture shells are responsible for
//! stripping any digital-gain contribution before constructing
//! [`SensorGain`].

/// Sensor conversion gain in electrons per ADU.
///
/// Larger values mean more electrons per output ADU step —
/// noisier per ADU. The photon-shot-noise weight in
/// `bris_vision::refine_centroid_subpixel` is
/// `1 / (I_adu/g + read_noise²)`, so doubling `g` doubles
/// the shot-noise variance and shrinks the weight on bright
/// pixels.
///
/// Construct via [`SensorGain::new`] (sanitizing) or use
/// [`SensorGain::UNITY`] when no measured value is available.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SensorGain {
    /// Electrons per ADU. Always finite and `> 0`.
    e_per_adu: f64,
}

impl SensorGain {
    /// Unity gain (1 e⁻/ADU). The back-compat / unknown-sensor
    /// fallback. Capture paths that cannot recover a measured
    /// gain ship this value and the centroid refinement degrades
    /// to its pre-plumbing behaviour.
    pub const UNITY: Self = Self { e_per_adu: 1.0 };

    /// Construct from electrons-per-ADU, falling back to
    /// [`Self::UNITY`] when the input is non-finite or
    /// non-positive. This is deliberately infallible: the
    /// downstream weight only cares that the value is sane,
    /// and a silently-degraded weight is preferable to
    /// erroring on an arguably-meaningful field.
    #[must_use]
    pub fn new(e_per_adu: f64) -> Self {
        if e_per_adu.is_finite() && e_per_adu > 0.0 {
            Self { e_per_adu }
        } else {
            Self::UNITY
        }
    }

    /// Electrons per ADU.
    #[must_use]
    pub fn e_per_adu(self) -> f64 {
        self.e_per_adu
    }
}

impl Default for SensorGain {
    fn default() -> Self {
        Self::UNITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_is_one() {
        assert!((SensorGain::UNITY.e_per_adu() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn new_rejects_non_finite() {
        assert_eq!(SensorGain::new(f64::NAN), SensorGain::UNITY);
        assert_eq!(SensorGain::new(f64::INFINITY), SensorGain::UNITY);
        assert_eq!(SensorGain::new(0.0), SensorGain::UNITY);
        assert_eq!(SensorGain::new(-3.0), SensorGain::UNITY);
    }

    #[test]
    fn new_accepts_typical_values() {
        assert!((SensorGain::new(4.0).e_per_adu() - 4.0).abs() < 1e-12);
    }
}
