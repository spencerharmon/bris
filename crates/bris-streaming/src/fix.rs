//! Per-fix publication payload.
//!
//! [`PublishedFix`] is what subscribers receive on the engine's
//! [`crate::StreamingEngine::fix_stream`]. It bundles the position
//! solution from [`bris_nav::Fix`] with the engine-level
//! diagnostics ($PBRIS-bound: number of sights, azimuth spread,
//! oldest sight age, dominant per-sight σ source).
//!
//! Producing the position alone is not enough — the operator
//! needs to know whether to trust it. The diagnostic fields are
//! the inputs that the `$PBRIS,UNC` and forthcoming `$PBRIS`
//! extensions surface to the chartplotter or mobile UI.

use bris_core::time::Tt;
use bris_nav::Fix;

/// One fix emitted by the engine, with engine-level diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct PublishedFix {
    /// Position solution (lat/lon + uncertainty ellipse + sight
    /// count) from [`bris_nav::multi_sight_fix`]. The ellipse is
    /// the geometry of the LSQ residual; the diagnostics below
    /// are about the *sights* that produced it.
    pub fix: Fix,

    /// Number of sights actively contributing to this fix. May be
    /// less than the engine's sight-window capacity if the window
    /// hasn't filled or some sights aged out.
    pub n_sights: usize,

    /// Spread, in radians, between the maximum and minimum
    /// azimuth across the contributing sights. A small spread
    /// (e.g. < 30°) means the LSQ geometry is poorly conditioned
    /// even when individual per-sight σ values look good — the
    /// fix is "good along one direction, weak across it." Render
    /// this in the operator UX so they can sweep to a different
    /// azimuth.
    pub azimuth_spread_rad: f64,

    /// Age of the oldest sight contributing to this fix, in
    /// seconds. With no course/speed input the engine cannot
    /// correct for observer motion between sights; a 10-minute-
    /// old sight from an underway vessel may have drifted
    /// significantly relative to a fresh sight. Surface in the
    /// operator UX as a "this fix uses sights from up to N
    /// minutes ago" advisory.
    pub oldest_sight_age_seconds: f64,

    /// Which per-sight σ source dominates the fix uncertainty.
    /// Mirrors [`bris_nmea::pbris::UncertaintyBudget::dominant_source`]
    /// but lives on the engine side so the NMEA crate stays a
    /// pure formatter.
    pub dominant_source: DominantSource,

    /// Capture-time timestamp of the most recent sight in the
    /// window. The fix itself is "current as of" this instant
    /// (modulo the publication-rate cap).
    pub timestamp: Tt,
}

/// Per-sight σ source attribution.
///
/// Each variant corresponds to one term in the engine's per-sight
/// σ budget. The budget itself lives in the engine; this enum
/// names the *dominant* term for one published fix. Operator
/// remediation guidance keys off this:
///
/// - [`DominantSource::Centroid`]: image is too dim or the body
///   is too small; longer exposure may help (but watch for
///   saturation / motion blur).
/// - [`DominantSource::Horizon`]: horizon is clutter-occluded
///   (boat structure) or noisy (rough sea); pan/tilt to a
///   cleaner stretch.
/// - [`DominantSource::Calibration`]: lens intrinsics fit poorly;
///   re-run the calibration workflow.
/// - [`DominantSource::Stitching`]: cross-frame pairs have large
///   alignment residuals; sweep more slowly so adjacent frames
///   overlap more.
/// - [`DominantSource::Refraction`]: anomalous atmospheric
///   conditions; nothing the operator can fix, but the fix
///   should be marked low-confidence.
/// - [`DominantSource::Dip`]: eye-height uncertainty dominates;
///   measure / record a more accurate eye height.
/// - [`DominantSource::Timing`]: clock is stale, drifting, or
///   was stepped; check NTP / GNSS time discipline.
/// - [`DominantSource::None`]: no contributing source identified
///   (typically because the budget hasn't been computed yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DominantSource {
    /// Body centroiding error (vision pipeline).
    Centroid,
    /// Horizon line-fit error (vision pipeline).
    Horizon,
    /// Lens calibration residual (per-device intrinsics quality).
    Calibration,
    /// Cross-frame stitching alignment residual.
    Stitching,
    /// Atmospheric refraction model error.
    Refraction,
    /// Horizon dip from eye-height uncertainty.
    Dip,
    /// Timing (NTP staleness, drift, or step events).
    Timing,
    /// Budget has not been computed; no dominant source identified.
    None,
}

impl DominantSource {
    /// Stable string label used by `$PBRIS` formatters.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Centroid => "centroid",
            Self::Horizon => "horizon",
            Self::Calibration => "calibration",
            Self::Stitching => "stitching",
            Self::Refraction => "refraction",
            Self::Dip => "dip",
            Self::Timing => "timing",
            Self::None => "none",
        }
    }
}

impl PublishedFix {
    /// Convert into the [`bris_nmea::FixSummary`] payload
    /// consumed by [`bris_nmea::pbris_fix`]. Lets a consumer
    /// (CLI, FFI shell, mobile) format `$PBRIS,FIX` with one
    /// call:
    ///
    /// ```ignore
    /// use bris_nmea::pbris_fix;
    /// let s = pbris_fix(utc, &published.to_pbris_fix_summary());
    /// ```
    ///
    /// Conversions:
    ///
    /// - `n_sights`: `usize` → `u32` saturating; ≤ 99 in
    ///   practice given the sight-window cap.
    /// - `oldest_sight_age_seconds`: `f64` → `u32` with
    ///   non-finite / negative inputs clamped to 0 and large
    ///   values saturating at `u32::MAX` (≈ 49 711 days, far
    ///   beyond any realistic sight-window age).
    /// - `dominant_source`: enum → `&'static str` via
    ///   [`DominantSource::label`].
    #[must_use]
    pub fn to_pbris_fix_summary(self) -> bris_nmea::FixSummary {
        let n_sights = u32::try_from(self.n_sights).unwrap_or(u32::MAX);
        let age = self.oldest_sight_age_seconds;
        let oldest_sight_age_s = if !age.is_finite() || age < 0.0 {
            0
        } else if age >= f64::from(u32::MAX) {
            u32::MAX
        } else {
            // Cast bounded by the comparison above.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = age as u32;
            v
        };
        bris_nmea::FixSummary {
            n_sights,
            azimuth_spread_rad: self.azimuth_spread_rad,
            oldest_sight_age_s,
            dominant_source: self.dominant_source.label(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_core::{Latitude, Longitude};
    use bris_nav::Fix;
    use bris_core::time::{Tt, JD_J2000};

    fn dummy_published(n_sights: usize, age: f64, dominant: DominantSource) -> PublishedFix {
        PublishedFix {
            fix: Fix {
                lat: Latitude::from_degrees(0.0).unwrap(),
                lon: Longitude::from_degrees(0.0).unwrap(),
                covariance_nm2: [[0.25, 0.0], [0.0, 0.25]],
                sigma_major_nm: 0.5,
                sigma_minor_nm: 0.5,
                orientation_rad: 0.0,
                #[allow(clippy::cast_possible_truncation)]
                sight_count: n_sights as u32,
            },
            n_sights,
            azimuth_spread_rad: 0.7,
            oldest_sight_age_seconds: age,
            dominant_source: dominant,
            timestamp: Tt::from_julian_date(JD_J2000),
        }
    }

    #[test]
    fn to_pbris_fix_summary_passes_through_typical_values() {
        let p = dummy_published(3, 120.5, DominantSource::Horizon);
        let s = p.to_pbris_fix_summary();
        assert_eq!(s.n_sights, 3);
        assert!((s.azimuth_spread_rad - 0.7).abs() < 1e-12);
        assert_eq!(s.oldest_sight_age_s, 120);
        assert_eq!(s.dominant_source, "horizon");
    }

    #[test]
    fn to_pbris_fix_summary_clamps_pathological_age_inputs() {
        // Negative age → 0.
        let p = dummy_published(2, -1.0, DominantSource::None);
        assert_eq!(p.to_pbris_fix_summary().oldest_sight_age_s, 0);
        // NaN → 0.
        let p = dummy_published(2, f64::NAN, DominantSource::None);
        assert_eq!(p.to_pbris_fix_summary().oldest_sight_age_s, 0);
        // Very large → saturating to u32::MAX (no overflow).
        let p = dummy_published(2, 1e20, DominantSource::None);
        assert_eq!(p.to_pbris_fix_summary().oldest_sight_age_s, u32::MAX);
    }
}
