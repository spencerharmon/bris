//! Multi-frame fusion of altitude measurements.
//!
//! The plan calls for ORB-feature stitching of overlapping frames into
//! a panorama, but for the streaming-fix engine the *useful* output is
//! a fused altitude estimate from N frames, not a literal panorama
//! image. This module implements the angle-averaging path: each frame
//! produces an [`crate::measure::measure_altitude`] result; we combine
//! those weighted by their per-frame uncertainties and report a fused
//! altitude with a tighter σ.
//!
//! # Why this works
//!
//! Per-frame altitude measurements are independent (different camera
//! orientations, different horizon-line samples, different centroid
//! noise realizations). Inverse-variance weighting is the optimal
//! combination for independent Gaussian estimates:
//!
//! ```text
//! μ = Σ(xᵢ / σᵢ²) / Σ(1 / σᵢ²)
//! σ_fused² = 1 / Σ(1 / σᵢ²)
//! ```
//!
//! The fused σ shrinks as 1/√N for equal-quality measurements, which
//! is the rationale for the operator's "sweep more for better
//! accuracy" UX.
//!
//! # Time correction
//!
//! Bodies move across the sky at sidereal rate (~15″/sec for a body
//! at the equator). When N frames span more than a few seconds we
//! must NOT just average their altitudes — we'd be averaging
//! different sky positions. Two valid approaches:
//!
//! 1. Reduce each frame's measurement to a common reference time by
//!    advancing the body's expected position; combine the residuals.
//! 2. Restrict the fusion window to a short interval (e.g. 5 seconds)
//!    over which the body's apparent motion is smaller than the
//!    per-frame σ.
//!
//! For the MVP we adopt approach (2): the fusion window is bounded
//! by a configurable max interval. Approach (1) requires the full
//! apparent-place pipeline at each frame and is the right thing to
//! do when we're confident in the time-correction code; we'll
//! upgrade after the streaming engine is in place.

use bris_core::{Sigma, Uncertain};

/// One frame's contribution to the fused estimate.
#[derive(Debug, Clone, Copy)]
pub struct FrameMeasurement {
    /// Apparent altitude in radians.
    pub altitude_rad: f64,
    /// 1σ uncertainty in radians.
    pub sigma_rad: f64,
    /// Frame capture time in seconds since some epoch (only the
    /// differences matter for the fusion window check).
    pub time_seconds: f64,
}

/// Fusion configuration.
#[derive(Debug, Clone, Copy)]
pub struct FusionConfig {
    /// Maximum time span (seconds) the fusion window may cover.
    /// Frames spanning more than this are split into multiple
    /// windows. Default 5 s — at sidereal rate (~15″/s for an
    /// equatorial body) this corresponds to ≈ 75″ of body motion,
    /// which is comparable to a single-frame σ in good conditions.
    pub max_window_seconds: f64,
    /// Minimum number of frames required to produce a fused result.
    /// Default 2.
    pub min_frames: usize,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            max_window_seconds: 5.0,
            min_frames: 2,
        }
    }
}

/// Errors from the fusion step.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum FusionError {
    /// Fewer than `min_frames` measurements were supplied.
    #[error("not enough frames ({0}) for fusion (need ≥ {1})")]
    InsufficientFrames(usize, usize),
    /// Frames spanned more than `max_window_seconds`.
    #[error("frames span {0:.2} s, exceeds max window {1:.2} s")]
    WindowExceeded(f64, f64),
    /// All input σ values were zero or non-finite, so weights cannot
    /// be computed.
    #[error("input uncertainties are degenerate (all zero or non-finite)")]
    DegenerateWeights,
}

/// Fuse a set of single-frame altitude measurements.
///
/// Inputs are inverse-variance combined; the result's σ is
/// `1/√Σ(1/σᵢ²)`.
///
/// # Errors
///
/// See [`FusionError`].
pub fn fuse_altitudes(
    measurements: &[FrameMeasurement],
    cfg: FusionConfig,
) -> Result<Uncertain<f64>, FusionError> {
    if measurements.len() < cfg.min_frames {
        return Err(FusionError::InsufficientFrames(
            measurements.len(),
            cfg.min_frames,
        ));
    }

    // Verify all measurements fit in one fusion window.
    let t_min = measurements
        .iter()
        .map(|m| m.time_seconds)
        .fold(f64::INFINITY, f64::min);
    let t_max = measurements
        .iter()
        .map(|m| m.time_seconds)
        .fold(f64::NEG_INFINITY, f64::max);
    let span = t_max - t_min;
    if span > cfg.max_window_seconds {
        return Err(FusionError::WindowExceeded(span, cfg.max_window_seconds));
    }

    // Inverse-variance weighted mean.
    let mut weighted_sum = 0.0;
    let mut weight_sum = 0.0;
    for m in measurements {
        if !m.sigma_rad.is_finite() || m.sigma_rad <= 0.0 {
            continue;
        }
        let w = 1.0 / (m.sigma_rad * m.sigma_rad);
        weighted_sum += m.altitude_rad * w;
        weight_sum += w;
    }
    if weight_sum <= 0.0 {
        return Err(FusionError::DegenerateWeights);
    }
    let fused_alt = weighted_sum / weight_sum;
    let fused_sigma = (1.0 / weight_sum).sqrt();

    Ok(Uncertain::new(
        fused_alt,
        Sigma::new(fused_sigma).unwrap_or(Sigma::ZERO),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn fusion_of_equal_measurements_shrinks_sigma_by_sqrt_n() {
        // 4 measurements all at altitude 0.5 rad with σ = 0.001 rad.
        // Fused σ should be 0.001 / √4 = 0.0005.
        let m = FrameMeasurement {
            altitude_rad: 0.5,
            sigma_rad: 0.001,
            time_seconds: 1.0,
        };
        let measurements = vec![m, m, m, m];
        let fused = fuse_altitudes(&measurements, FusionConfig::default()).unwrap();
        assert_relative_eq!(fused.value, 0.5, epsilon = 1e-12);
        assert_relative_eq!(fused.sigma.value(), 0.0005, epsilon = 1e-12);
    }

    #[test]
    fn fusion_pulls_toward_more_certain_measurement() {
        // One precise measurement and one noisy one. The fused mean
        // should be much closer to the precise one.
        let precise = FrameMeasurement {
            altitude_rad: 0.500,
            sigma_rad: 0.001,
            time_seconds: 0.0,
        };
        let noisy = FrameMeasurement {
            altitude_rad: 0.520,
            sigma_rad: 0.020,
            time_seconds: 1.0,
        };
        let fused = fuse_altitudes(&[precise, noisy], FusionConfig::default()).unwrap();
        // Precise weight = 1/0.001² = 10⁶. Noisy weight = 1/0.020² = 2500.
        // Mean = (0.500 × 10⁶ + 0.520 × 2500) / (10⁶ + 2500)
        //      ≈ 0.50005.
        assert_relative_eq!(fused.value, 0.50005, epsilon = 1e-4);
    }

    #[test]
    fn fusion_rejects_too_few_frames() {
        let m = FrameMeasurement {
            altitude_rad: 0.5,
            sigma_rad: 0.001,
            time_seconds: 0.0,
        };
        let result = fuse_altitudes(&[m], FusionConfig::default());
        assert_eq!(result, Err(FusionError::InsufficientFrames(1, 2)));
    }

    #[test]
    fn fusion_rejects_long_window() {
        let cfg = FusionConfig {
            max_window_seconds: 5.0,
            ..FusionConfig::default()
        };
        let m1 = FrameMeasurement {
            altitude_rad: 0.5,
            sigma_rad: 0.001,
            time_seconds: 0.0,
        };
        let m2 = FrameMeasurement {
            altitude_rad: 0.5,
            sigma_rad: 0.001,
            time_seconds: 10.0, // 10 s > 5 s window
        };
        let result = fuse_altitudes(&[m1, m2], cfg);
        assert!(matches!(result, Err(FusionError::WindowExceeded(_, _))));
    }

    #[test]
    fn fusion_handles_degenerate_sigmas() {
        let m = FrameMeasurement {
            altitude_rad: 0.5,
            sigma_rad: 0.0,
            time_seconds: 0.0,
        };
        let result = fuse_altitudes(&[m, m], FusionConfig::default());
        assert_eq!(result, Err(FusionError::DegenerateWeights));
    }
}
