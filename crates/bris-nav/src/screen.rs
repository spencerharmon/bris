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
    /// Two sights whose azimuths are within this many radians of
    /// each other are considered to look in the "same direction"
    /// for the purposes of the azimuth-disagreement gate. When
    /// such a pair disagrees in intercept sign (one says "toward
    /// the body", the other says "away"), one of them is a
    /// blunder. Default 5° in radians. The spec
    /// (`plan.org` L1776) does not pin the value; 5° is the
    /// smallest threshold that still tolerates honest azimuth
    /// noise (~1° per-sight) without false-firing on near-
    /// orthogonal sights.
    pub same_look_direction_delta_rad: f64,
}

impl Default for ScreeningConfig {
    fn default() -> Self {
        Self {
            max_abs_intercept_nm: 60.0,
            outlier_k_sigma: 5.0,
            min_sights_for_outlier: 3,
            same_look_direction_delta_rad: 5.0_f64 * std::f64::consts::PI / 180.0,
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
    /// This sight shares a look-direction (azimuth within δ) with
    /// another sight in the set, but their intercepts disagree in
    /// sign. Two sights pointing the same way cannot honestly
    /// disagree about which side of the assumed position the body
    /// lies on; one of them is a blunder. Args: this sight's
    /// intercept (nm), partner sight's intercept (nm), azimuth
    /// separation (degrees).
    #[error(
        "intercept {0:.2} nm disagrees in sign with partner {1:.2} nm at {2:.2}° azimuth separation"
    )]
    AzimuthDisagreement(f64, f64, f64),
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

    // Pass 2: azimuth-disagreement screen. Pairwise: if two
    // sights look in nearly the same direction (azimuth within
    // cfg.same_look_direction_delta_rad on the circle) but their
    // intercepts have opposite signs, one of them is a blunder.
    // Reject the one with the larger |intercept| (further from
    // the assumed position is more likely the bad measurement).
    // This fires *before* the consensus-outlier pass so the
    // surviving sight can participate honestly in the median.
    {
        let mut to_drop_set: std::collections::BTreeSet<usize> =
            std::collections::BTreeSet::new();
        for a_pos in 0..kept_idx.len() {
            for b_pos in (a_pos + 1)..kept_idx.len() {
                let ia = kept_idx[a_pos];
                let ib = kept_idx[b_pos];
                if to_drop_set.contains(&a_pos) || to_drop_set.contains(&b_pos) {
                    continue;
                }
                let a = lops[ia];
                let b = lops[ib];
                let az_delta = circular_delta_rad(a.azimuth_rad, b.azimuth_rad);
                if az_delta > cfg.same_look_direction_delta_rad {
                    continue;
                }
                if a.intercept_nm.signum() == b.intercept_nm.signum() {
                    continue;
                }
                // Same look-direction, opposite-sign intercepts.
                // Drop the larger-magnitude offender.
                let (drop_pos, drop_idx, partner_intercept) =
                    if a.intercept_nm.abs() >= b.intercept_nm.abs() {
                        (a_pos, ia, b.intercept_nm)
                    } else {
                        (b_pos, ib, a.intercept_nm)
                    };
                let dropped = lops[drop_idx];
                rejected.push((
                    drop_idx,
                    dropped,
                    RejectionReason::AzimuthDisagreement(
                        dropped.intercept_nm,
                        partner_intercept,
                        az_delta.to_degrees(),
                    ),
                ));
                to_drop_set.insert(drop_pos);
            }
        }
        if !to_drop_set.is_empty() {
            let mut to_drop: Vec<usize> = to_drop_set.into_iter().collect();
            for j in to_drop.drain(..).rev() {
                kept_idx.remove(j);
            }
        }
    }

    // Pass 3: outlier-from-consensus screen, only with enough sights.
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

/// Smallest absolute difference between two angles on the
/// unit circle, in radians, in [0, π].
fn circular_delta_rad(a: f64, b: f64) -> f64 {
    let two_pi = std::f64::consts::TAU;
    let mut d = (a - b).rem_euclid(two_pi);
    if d > std::f64::consts::PI {
        d = two_pi - d;
    }
    d
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
    fn azimuth_agreeing_same_sign_intercepts_keeps_both() {
        // Two sights at nearly the same azimuth (within 5°) with
        // intercepts of the same sign: no disagreement, both
        // kept. Third sight at 90° to keep min_sights_for_outlier
        // from kicking in unexpectedly.
        let lops = [lop(0.0, 1.0, 0.5), lop(2.0, 1.2, 0.5), lop(90.0, 0.5, 0.5)];
        let r = screen_sights(&lops, ScreeningConfig::default());
        assert_eq!(r.kept.len(), 3, "same-sign within-δ sights must stay");
        assert!(r.rejected.is_empty());
    }

    #[test]
    fn azimuth_agreeing_opposite_sign_intercepts_rejects_one() {
        // Two sights at nearly the same azimuth (within 5°) with
        // opposite-sign intercepts: a blunder. Larger |intercept|
        // gets dropped.
        let lops = [lop(0.0, 1.0, 0.5), lop(3.0, -2.5, 0.5)];
        let r = screen_sights(&lops, ScreeningConfig::default());
        assert_eq!(r.kept.len(), 1);
        assert_eq!(r.rejected.len(), 1);
        assert!(matches!(
            r.rejected[0].2,
            RejectionReason::AzimuthDisagreement(_, _, _)
        ));
        // The larger-magnitude offender (-2.5 nm) is the one dropped.
        assert!((r.rejected[0].1.intercept_nm - (-2.5)).abs() < 1e-9);
    }

    #[test]
    fn azimuth_disagreeing_opposite_sign_intercepts_keeps_both() {
        // Spec: the gate only fires when azimuths AGREE. Two
        // sights at 0° and 90° pointing in very different
        // directions can legitimately have opposite-sign
        // intercepts; nothing to reject from this gate.
        let lops = [lop(0.0, 1.0, 0.5), lop(90.0, -1.0, 0.5)];
        let r = screen_sights(&lops, ScreeningConfig::default());
        assert_eq!(r.kept.len(), 2);
        assert!(r.rejected.is_empty());
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
