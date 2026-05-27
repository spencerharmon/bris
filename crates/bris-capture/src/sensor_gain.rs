//! Sensor-specific mapping from V4L2 analog-gain driver
//! values to electrons-per-ADU.
//!
//! V4L2 reports `V4L2_CID_ANALOGUE_GAIN` (preferred) or
//! `V4L2_CID_GAIN` (fallback) as an integer in driver-specific
//! units \u2014 the kernel does not standardize a physical unit.
//! Each sensor driver picks its own scale (linear,
//! piecewise-linear, register-format). To recover the
//! conversion gain in electrons per ADU we therefore need a
//! per-sensor lookup, which the trait below abstracts.
//!
//! "Measured" gain here means **analog** \u2014 the per-pixel
//! amplification before the ADC. Digital gain (post-ADC
//! multiplication) does not change the per-pixel shot-noise
//! variance in ADU\u00b2 and must not be folded in. The driver
//! values returned by `V4L2_CID_ANALOGUE_GAIN` are by
//! definition the analog leg only.
//!
//! Map selection is by substring match on `VIDIOC_QUERYCAP`'s
//! card name; see [`map_for_card`].

use bris_core::SensorGain;

/// Sensor-specific conversion from a V4L2 driver-reported
/// analog-gain value to electrons-per-ADU.
pub trait SensorGainMap {
    /// Map a driver value (as reported via V4L2 control read)
    /// into electrons-per-ADU. Implementations sanitize bad
    /// inputs by clamping to a sensor-plausible range.
    fn driver_value_to_e_per_adu(&self, v: i32) -> f64;
}

/// Sony IMX219 (Raspberry Pi Camera v2).
///
/// The IMX219 driver reports analog gain as `register / 256`
/// where `register \u2208 [256, 2_560]` corresponds to physical
/// analog gain `1\u00d7 \u2026 10\u00d7`. The conversion gain at the ADC
/// is documented at ~0.31 e\u207b/ADU at unity gain on the
/// 10-bit sensor output; higher amplifier gain *reduces*
/// e\u207b/ADU proportionally (the same ADU step represents
/// fewer electrons).
///
/// So: `e_per_adu = unity_e_per_adu \u00d7 (256 / register)`. We
/// use the datasheet-implied 0.31 at unity. Clamping to the
/// physical register range guards against driver glitches.
#[derive(Debug, Clone, Copy, Default)]
pub struct Imx219LinearMap;

impl SensorGainMap for Imx219LinearMap {
    fn driver_value_to_e_per_adu(&self, v: i32) -> f64 {
        const UNITY_E_PER_ADU: f64 = 0.31;
        let r = v.clamp(256, 2_560);
        UNITY_E_PER_ADU * (256.0 / f64::from(r))
    }
}

/// `OmniVision` generic linear approximation.
///
/// OV-series drivers commonly expose gain as `register / 16`
/// in dB-ish steps. This impl uses a coarse linear
/// approximation: `gain_x = max(1.0, register / 16.0)`, and
/// `e_per_adu = unity / gain_x`, with `unity = 1.0` (the
/// per-sensor true value differs; this is a placeholder until
/// a per-OV-part map lands). The σ produced under this map
/// is "approximately right" rather than calibrated.
#[derive(Debug, Clone, Copy, Default)]
pub struct OvGenericMap;

impl SensorGainMap for OvGenericMap {
    fn driver_value_to_e_per_adu(&self, v: i32) -> f64 {
        const UNITY_E_PER_ADU: f64 = 1.0;
        let r = v.max(16);
        let gain_x = f64::from(r) / 16.0;
        UNITY_E_PER_ADU / gain_x.max(1.0)
    }
}

/// Fallback for any sensor we don't recognize: returns
/// `1.0 e\u207b/ADU` for every input. Capture sites that select
/// this should also `tracing::warn!` once per stream so the
/// operator sees that they're on an uncalibrated weight.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnknownMap;

impl SensorGainMap for UnknownMap {
    fn driver_value_to_e_per_adu(&self, _v: i32) -> f64 {
        1.0
    }
}

/// Select a [`SensorGainMap`] from a V4L2 card name (the
/// `card` field of `VIDIOC_QUERYCAP`).
///
/// Substring match is case-insensitive. Returns
/// [`SensorMapKind::Unknown`] when no rule matches; the
/// caller is responsible for the warn-once log.
#[must_use]
pub fn map_for_card(card: &str) -> SensorMapKind {
    let c = card.to_ascii_lowercase();
    if c.contains("imx219") {
        SensorMapKind::Imx219
    } else if c.contains("ov") && (c.contains("ov5") || c.contains("ov2") || c.contains("ov13")) {
        SensorMapKind::OvGeneric
    } else {
        SensorMapKind::Unknown
    }
}

/// Dispatch enum so the V4L2 capture path can hold a single
/// concrete value rather than a trait object.
#[derive(Debug, Clone, Copy)]
pub enum SensorMapKind {
    /// Sony IMX219 (Raspberry Pi Camera v2). See
    /// [`Imx219LinearMap`].
    Imx219,
    /// Generic `OmniVision` approximation. See [`OvGenericMap`].
    OvGeneric,
    /// Unrecognized; returns unity. See [`UnknownMap`].
    Unknown,
}

impl SensorMapKind {
    /// Convert a driver value to a [`SensorGain`] via the
    /// selected mapping. Sanitizing happens inside the
    /// per-impl mapper and in [`SensorGain::new`].
    #[must_use]
    pub fn to_sensor_gain(self, driver_value: i32) -> SensorGain {
        let e = match self {
            Self::Imx219 => Imx219LinearMap.driver_value_to_e_per_adu(driver_value),
            Self::OvGeneric => OvGenericMap.driver_value_to_e_per_adu(driver_value),
            Self::Unknown => UnknownMap.driver_value_to_e_per_adu(driver_value),
        };
        SensorGain::new(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imx219_unity_register_yields_datasheet_e_per_adu() {
        let g = Imx219LinearMap.driver_value_to_e_per_adu(256);
        assert!((g - 0.31).abs() < 1e-9, "got {g}");
    }

    #[test]
    fn imx219_high_gain_register_lowers_e_per_adu() {
        let g_unity = Imx219LinearMap.driver_value_to_e_per_adu(256);
        let g_8x = Imx219LinearMap.driver_value_to_e_per_adu(2_048);
        assert!(g_8x < g_unity);
        // 8x physical gain \u2192 e_per_adu shrinks by 8.
        assert!((g_8x - g_unity / 8.0).abs() < 1e-9);
    }

    #[test]
    fn imx219_clamps_below_unity_register() {
        // Driver glitch: a value below 256 must still produce
        // a sane (clamped) gain rather than infinity.
        let g = Imx219LinearMap.driver_value_to_e_per_adu(0);
        assert!(g.is_finite());
        assert!((g - 0.31).abs() < 1e-9);
    }

    #[test]
    fn ov_generic_unity_register_is_unity_e_per_adu() {
        let g = OvGenericMap.driver_value_to_e_per_adu(16);
        assert!((g - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ov_generic_high_register_lowers_e_per_adu() {
        let g = OvGenericMap.driver_value_to_e_per_adu(64);
        assert!((g - 0.25).abs() < 1e-9);
    }

    #[test]
    fn unknown_always_unity() {
        assert!((UnknownMap.driver_value_to_e_per_adu(0) - 1.0).abs() < 1e-12);
        assert!((UnknownMap.driver_value_to_e_per_adu(9_999) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn map_for_card_recognizes_imx219() {
        assert!(matches!(
            map_for_card("imx219 4-0010"),
            SensorMapKind::Imx219
        ));
        assert!(matches!(map_for_card("IMX219"), SensorMapKind::Imx219));
    }

    #[test]
    fn map_for_card_recognizes_ov_family() {
        assert!(matches!(
            map_for_card("ov5640 1-003c"),
            SensorMapKind::OvGeneric
        ));
        assert!(matches!(map_for_card("OV2740"), SensorMapKind::OvGeneric));
    }

    #[test]
    fn map_for_card_unknown_falls_through() {
        assert!(matches!(
            map_for_card("USB Generic Webcam"),
            SensorMapKind::Unknown
        ));
    }

    #[test]
    fn sensor_map_kind_to_sensor_gain_round_trips() {
        let g = SensorMapKind::Imx219.to_sensor_gain(256);
        assert!((g.e_per_adu() - 0.31).abs() < 1e-9);
        let g_unknown = SensorMapKind::Unknown.to_sensor_gain(42);
        assert!((g_unknown.e_per_adu() - 1.0).abs() < 1e-12);
    }
}
