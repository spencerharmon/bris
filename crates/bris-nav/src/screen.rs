//! Per-sight blunder detection.
//!
//! Before LOPs are fed to the multi-sight fix, this module screens them
//! for obvious blunders: an intercept far larger than physically plausible,
//! or a per-sight residual several σ from the consensus of the others.
//! Rejecting blunders early prevents a single bad sight from corrupting
//! the fix and surfaces a clear diagnostic to the operator naming the
//! rejected sight and reason.
//!
//! The screening is conservative: it rejects only sights that are
//! statistically improbable given the others, never sights whose own σ
//! says they should be uncertain. The honest-uncertainty invariant
//! still holds — a high-σ sight is allowed in but down-weighted by
//! the LSQ fit.

use crate::sight::LineOfPosition;

/// Configuration for blunder detection.
#[derive(Debug, Clone, Copy)]
pub struct ScreeningConfig {
    /// Reject sights whose absolute intercept exceeds this many
    /// nautical miles. Default 60 nm — by then the assumed position
    /// is so wrong the linearization breaks down anyway.
    pub max_abs_intercept_nm: f64,
    /// When ≥ 3 sights are present, reject any sight whose intercept
    /// is more than `outlier_k_sigma × σ_consensus` from the
    /// median-of-others. Default 5 — Chauvenet-style aggressive only
    /// against truly egregious blunders.
    pub outlier_k_sigma: f64,
    /// Below this many sights, only the absolute-intercept screen
    /// applies; outlier rejection requires a meaningful consensus.
    /// Default 3.
    pub min_sights_for_outlier: usize,
}

impl Default for ScreeningConfig {
    fn default() -> Self {
        Self {
            max_abs_intercept_nm: 60.0,
            outlier_k_sigma: 5.0,
            min_sights_for_outlier: 3,
        }
    }
}

/// Reason a sight was rejected.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum RejectionReason {
    /// |intercept| exceeded `max_abs_intercept_nm`.
    #[error("intercept {0:.2} nm exceeds max plausible {1:.2} nm")]
    InterceptTooLarge(f64, f64),
    /// Sight's intercept was more than `k × σ` from the median of the
    /// remaining sights.
    #[error("intercept {0:.2} nm is {1:.1}σ from consensus {2:.2} nm")]
    OutlierIntercept(f64, f64, f64),
}

/// Result of screening one sight set.
#[derive(Debug, Clone)]
pub struct ScreeningResult {
    /// Sights that survived screening, in original order.
    pub kept: Vec<LineOfPosition>,
    /// Rejected sights with reason and original index.
    pub rejected: Vec<(usize, LineOfPosition, RejectionReason)>,
}

/// Screen a slice of LOPs for blunders.
///
/// Returns the kept and rejected sets. The caller (typically the fix
/// pipeline) feeds `kept` into `multi_sight_fix` and surfaces the
/// `rejected` set in the `$PBRIS,SIGHT` diagnostic stream.
#[must_use]
pub fn screen_sights(lops: &[LineOfPosition], cfg: ScreeningConfig) -> ScreeningResult {
    let mut kept_idx: Vec<usize> = (0..lops.len()).collect();
    let mut rejected: Vec<(usize, LineOfPosition, RejectionReason)> = Vec::new();

    // Pass 1: absolute-intercept screen.
    kept_idx.retain(|&i| {
        let lop = lops[i];
        if lop.intercept_nm.abs() > cfg.max_abs_intercept_nm {
            rejected.push((
                i,
                lop,
                RejectionReason::InterceptTooLarge(lop.intercept_nm, cfg.max_abs_intercept_nm),
            ));
            false
        } else {
            true
        }
    });

    // Pass 2: outlier-from-consensus screen, only with enough sights.
    if kept_idx.len() >= cfg.min_sights_for_outlier {
        // Take a leave-one-out median + MAD for robustness.
        let intercepts: Vec<f64> = kept_idx.iter().map(|&i| lops[i].intercept_nm).collect();
        let mut to_drop: Vec<usize> = Vec::new();
        for (j, &i) in kept_idx.iter().enumerate() {
            let others: Vec<f64> = intercepts
                .iter()
                .enumerate()
                .filter(|&(k, _)| k != j)
                .map(|(_, &v)| v)
                .collect();
            let median = robust_median(&others);
            let mad = mad_about(&others, median);
            // Robust σ from MAD: 1.4826 × MAD ≈ σ for Gaussian noise.
            let sigma = (1.4826 * mad).max(lops[i].intercept_sigma_nm.value().max(1e-3));
            let z = (lops[i].intercept_nm - median).abs() / sigma;
            if z > cfg.outlier_k_sigma {
                rejected.push((
                    i,
                    lops[i],
                    RejectionReason::OutlierIntercept(lops[i].intercept_nm, z, median),
                ));
                to_drop.push(j);
            }
        }
        // Remove in descending index order so we don't shift indices.
        for &j in to_drop.iter().rev() {
            kept_idx.remove(j);
        }
    }

    let kept: Vec<LineOfPosition> = kept_idx.into_iter().map(|i| lops[i]).collect();
    ScreeningResult { kept, rejected }
}

fn robust_median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        f64::midpoint(sorted[n / 2 - 1], sorted[n / 2])
    }
}

fn mad_about(values: &[f64], center: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let deviations: Vec<f64> = values.iter().map(|v| (v - center).abs()).collect();
    robust_median(&deviations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_core::{Latitude, Longitude, Sigma};

    fn lop(az_deg: f64, intercept_nm: f64, sigma_nm: f64) -> LineOfPosition {
        LineOfPosition {
            assumed_lat: Latitude::from_degrees(0.0).unwrap(),
            assumed_lon: Longitude::from_degrees(0.0).unwrap(),
            azimuth_rad: az_deg.to_radians(),
            intercept_nm,
            intercept_sigma_nm: Sigma::new(sigma_nm).unwrap(),
        }
    }

    #[test]
    fn keeps_all_within_thresholds() {
        let lops = [
            lop(0.0, 1.0, 0.5),
            lop(90.0, 2.0, 0.5),
            lop(180.0, -1.0, 0.5),
        ];
        let r = screen_sights(&lops, ScreeningConfig::default());
        assert_eq!(r.kept.len(), 3);
        assert!(r.rejected.is_empty());
    }

    #[test]
    fn rejects_implausibly_large_intercept() {
        let lops = [lop(0.0, 1.0, 0.5), lop(90.0, 100.0, 0.5)];
        let r = screen_sights(&lops, ScreeningConfig::default());
        assert_eq!(r.kept.len(), 1);
        assert_eq!(r.rejected.len(), 1);
        assert!(matches!(
            r.rejected[0].2,
            RejectionReason::InterceptTooLarge(_, _)
        ));
    }

    #[test]
    fn rejects_obvious_outlier() {
        // Three consistent sights near 0 nm + one wildly off.
        let lops = [
            lop(0.0, 0.5, 0.5),
            lop(90.0, 0.6, 0.5),
            lop(180.0, 0.4, 0.5),
            lop(270.0, 30.0, 0.5),
        ];
        let r = screen_sights(&lops, ScreeningConfig::default());
        assert_eq!(r.kept.len(), 3);
        assert_eq!(r.rejected.len(), 1);
        assert!(matches!(
            r.rejected[0].2,
            RejectionReason::OutlierIntercept(_, _, _)
        ));
    }

    #[test]
    fn does_not_reject_with_too_few_sights_for_consensus() {
        // With 2 sights, no consensus exists → don't apply outlier
        // rejection even if one sight is far from the other.
        let lops = [lop(0.0, 1.0, 0.5), lop(90.0, 50.0, 0.5)];
        let r = screen_sights(&lops, ScreeningConfig::default());
        // Both intercepts are below the 60 nm absolute threshold and
        // the outlier rule needs ≥ 3 sights, so both are kept.
        assert_eq!(r.kept.len(), 2);
        assert!(r.rejected.is_empty());
    }

    #[test]
    fn keeps_high_sigma_sight_within_consensus() {
        // A high-σ sight isn't itself a blunder; the LSQ will down-weight
        // it but it should be kept.
        let lops = [
            lop(0.0, 0.5, 0.5),
            lop(90.0, 0.6, 0.5),
            lop(180.0, 0.4, 5.0), // big σ but consistent value
        ];
        let r = screen_sights(&lops, ScreeningConfig::default());
        assert_eq!(r.kept.len(), 3);
    }
}
