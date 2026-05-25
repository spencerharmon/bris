//! Weighted multi-source fusion of [`HorizonHypothesis`] values.
//!
//! When multiple horizon providers produce hypotheses for the
//! same frame and the hypotheses agree (their horizon-plane
//! normals lie within `k_concordance · sqrt(σ_i² + σ_j²)` of
//! each other), the fused estimate is tighter than any single
//! source. When the providers disagree, we honestly report the
//! lowest-σ singleton — silent averaging across discordant
//! sources would violate the "honest uncertainty everywhere"
//! invariant from `AGENTS.md`.
//!
//! See `docs/design/horizon_autodetect.md` §10 (Phase 1 stub
//! fusion deliverable; this is the Phase 2 upgrade) and the
//! handoff for the algorithm rationale.
//!
//! # σ math (inverse-variance combination)
//!
//! For N independent estimates of a scalar with variances
//! `σ_i²`, the minimum-variance unbiased linear combination
//! has weights `w_i = 1/σ_i²` (normalized to sum to 1) and
//! variance `σ_fused² = 1 / Σ(1/σ_i²)`.
//!
//! Units throughout: σ is an *angular* 1σ in **radians** (the
//! altitude-σ contribution carried on [`HorizonLine`]). The
//! horizon normals (`CameraRay`) are unit vectors; pairwise
//! angles between them are in radians, which is the same unit
//! as the σ values — the concordance test
//! `angle(n_i, n_j) ≤ k · sqrt(σ_i² + σ_j²)` is therefore
//! dimensionally consistent.
//!
//! Independence assumption: the providers are treated as
//! independent. For the current set (gradient / sky-region /
//! reflection-pair) this is approximately true — they use
//! disjoint pixel evidence. If a future provider shares the
//! same input as another (e.g. two ML segmenters fed the same
//! frame), the inverse-variance form will *under-state* the
//! fused σ; the design doc §11 calls out tracking provider
//! correlation as a future-phase concern.
//!
//! # Discordance handling
//!
//! When no cluster of size ≥ 2 forms, the fuser falls back to
//! the lowest-σ singleton (current pre-fusion behavior) and
//! reports `FusionOutcome::Discordant`. Operators learn about
//! this via the engine's `horizon_fusion_discordant_frames`
//! counter; a non-zero value is a signal that something is
//! wrong (mis-calibrated provider, false-positive detection,
//! or a multi-modal scene that the cluster heuristic can't
//! resolve).

use super::{HorizonHypothesis, HorizonProvenance};
use crate::frame::Intrinsics;
use crate::ray::{horizon_line_from_normal, CameraRay, HorizonRay};
use bris_core::Sigma;

/// Tunable fusion parameters.
#[derive(Debug, Clone, Copy)]
pub struct HorizonFusionConfig {
    /// Concordance threshold: hypotheses `i` and `j` are
    /// concordant iff `angle(n_i, n_j) ≤ k · sqrt(σ_i² + σ_j²)`.
    /// Default 3.0.
    pub concordance_k: f64,
    /// Floor on the fused σ in radians. Default `1e-4` rad
    /// (~20 arcsec) — well below the noise floor of any
    /// real-world horizon source, prevents pathological
    /// over-tightening when several providers report
    /// implausibly small σ on a synthetic frame.
    pub sigma_floor_rad: f64,
    /// Master switch. When false, the fuser unconditionally
    /// returns the lowest-σ singleton (pre-fusion behavior).
    /// Lets operators A/B fusion against the baseline.
    pub enabled: bool,
}

impl Default for HorizonFusionConfig {
    fn default() -> Self {
        Self {
            concordance_k: 3.0,
            sigma_floor_rad: 1e-4,
            enabled: true,
        }
    }
}

/// How the fuser chose its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FusionMode {
    /// Only one hypothesis was available; passed through.
    Singleton,
    /// Multiple hypotheses available and ≥ 2 were concordant;
    /// returned a weighted-mean fused estimate.
    Clustered,
    /// Multiple hypotheses available but no pair was
    /// concordant; fell back to the lowest-σ singleton.
    Discordant,
    /// Fusion disabled by config; returned the lowest-σ
    /// singleton.
    Disabled,
}

/// Outcome of one fuser invocation.
#[derive(Debug, Clone)]
pub struct FusionOutcome {
    /// The chosen hypothesis (fused or singleton). `None` when
    /// the input was empty.
    pub hypothesis: Option<HorizonHypothesis>,
    /// All direct sights from the cluster (or just the chosen
    /// singleton's direct sight, if any). Stage E consumes
    /// the whole vector; `bris-nav` de-duplicates per-body.
    pub direct_sights: Vec<crate::horizon_providers::DirectSight>,
    /// How the choice was made.
    pub mode: FusionMode,
    /// Cluster size when `mode == Clustered`; otherwise 1 for
    /// `Singleton` / `Discordant` / `Disabled`, or 0 for an
    /// empty input.
    pub cluster_size: usize,
}

/// Fuse a slice of [`HorizonHypothesis`] into a single output.
///
/// `intrinsics` and `image_width` are needed to lift each
/// hypothesis's pixel-coordinate horizon line into a
/// camera-space normal for the concordance test, and to project
/// the fused normal back to a pixel line on the way out.
#[must_use]
#[allow(clippy::similar_names, clippy::cast_precision_loss)]
pub fn fuse_horizon_hypotheses(
    hypotheses: &[HorizonHypothesis],
    intrinsics: &Intrinsics,
    image_width: u32,
    cfg: &HorizonFusionConfig,
) -> FusionOutcome {
    if hypotheses.is_empty() {
        return FusionOutcome {
            hypothesis: None,
            direct_sights: Vec::new(),
            mode: FusionMode::Singleton,
            cluster_size: 0,
        };
    }
    let lowest_idx = lowest_sigma_index(hypotheses);
    let lowest = &hypotheses[lowest_idx];
    let singleton_outcome = |mode: FusionMode| FusionOutcome {
        hypothesis: Some(*lowest),
        direct_sights: lowest.direct_sight.into_iter().collect(),
        mode,
        cluster_size: 1,
    };
    if !cfg.enabled {
        return singleton_outcome(FusionMode::Disabled);
    }
    if hypotheses.len() == 1 {
        return singleton_outcome(FusionMode::Singleton);
    }
    // Lift each hypothesis to a camera-space normal +
    // altitude σ. Discard any that fail to lift (degenerate
    // horizon line through the principal point) — those
    // can't participate in the angular concordance test.
    let mut entries: Vec<Entry> = Vec::with_capacity(hypotheses.len());
    for (idx, hyp) in hypotheses.iter().enumerate() {
        if let Some(ray) = HorizonRay::from_line(&hyp.line, intrinsics, image_width) {
            entries.push(Entry {
                idx,
                normal: ray.normal,
                sigma: hyp.line.altitude_sigma.value().max(cfg.sigma_floor_rad),
            });
        }
    }
    if entries.len() < 2 {
        return singleton_outcome(FusionMode::Singleton);
    }
    // Greedy cluster: sort by σ ascending, seed with the
    // lowest-σ entry, accept any entry whose normal is
    // concordant with the *current cluster mean*.
    entries.sort_by(|a, b| a.sigma.total_cmp(&b.sigma));
    let mut cluster_idxs: Vec<usize> = vec![0];
    let mut cluster_mean = entries[0].normal;
    for (i, entry) in entries.iter().enumerate().skip(1) {
        let mean_sigma_sq: f64 = cluster_idxs
            .iter()
            .map(|&ci| entries[ci].sigma.powi(2))
            .sum::<f64>()
            / (cluster_idxs.len().max(1) as f64).powi(2);
        let mean_sigma = mean_sigma_sq.sqrt();
        let combined = (mean_sigma.powi(2) + entry.sigma.powi(2)).sqrt();
        let ang = angle_between(&cluster_mean, &entry.normal);
        if ang <= cfg.concordance_k * combined {
            cluster_idxs.push(i);
            // Update running cluster mean (weighted by 1/σ²).
            let mut sx = 0.0_f64;
            let mut sy = 0.0_f64;
            let mut sz = 0.0_f64;
            for &ci in &cluster_idxs {
                let w = 1.0 / entries[ci].sigma.powi(2);
                sx += w * entries[ci].normal.x;
                sy += w * entries[ci].normal.y;
                sz += w * entries[ci].normal.z;
            }
            if let Some(n) = (CameraRay {
                x: sx,
                y: sy,
                z: sz,
            })
            .normalize()
            {
                cluster_mean = n;
            }
        }
    }
    if cluster_idxs.len() < 2 {
        return singleton_outcome(FusionMode::Discordant);
    }
    // Inverse-variance fused σ.
    let inv_var_sum: f64 = cluster_idxs
        .iter()
        .map(|&ci| 1.0 / entries[ci].sigma.powi(2))
        .sum();
    let fused_sigma_val = (1.0 / inv_var_sum).sqrt().max(cfg.sigma_floor_rad);
    let fused_sigma = Sigma::new(fused_sigma_val).unwrap_or(lowest.line.altitude_sigma);

    let Some(fused_line) = horizon_line_from_normal(&cluster_mean, intrinsics, fused_sigma) else {
        // Numerically degenerate fused normal — fall back to
        // the lowest-σ singleton honestly.
        return singleton_outcome(FusionMode::Discordant);
    };
    // Collect direct sights from cluster members in source
    // order so reproducibility is independent of cluster
    // discovery order.
    let mut original_idxs: Vec<usize> = cluster_idxs.iter().map(|&ci| entries[ci].idx).collect();
    original_idxs.sort_unstable();
    let direct_sights: Vec<_> = original_idxs
        .iter()
        .filter_map(|&i| hypotheses[i].direct_sight)
        .collect();
    let hypothesis = HorizonHypothesis {
        line: fused_line,
        provenance: HorizonProvenance::Fused {
            cluster_size: cluster_idxs.len(),
        },
        // The fused hypothesis itself doesn't carry a single
        // direct sight; the full list is on `direct_sights`
        // beside it. Keep the inner field `None` to avoid
        // accidental double-counting if a downstream consumer
        // reads `hypothesis.direct_sight` directly.
        direct_sight: None,
    };
    FusionOutcome {
        hypothesis: Some(hypothesis),
        direct_sights,
        mode: FusionMode::Clustered,
        cluster_size: cluster_idxs.len(),
    }
}

struct Entry {
    /// Index into the original `hypotheses` slice.
    idx: usize,
    normal: CameraRay,
    sigma: f64,
}

fn lowest_sigma_index(hypotheses: &[HorizonHypothesis]) -> usize {
    let mut best = 0;
    let mut best_sigma = hypotheses[0].line.altitude_sigma.value();
    for (i, h) in hypotheses.iter().enumerate().skip(1) {
        let s = h.line.altitude_sigma.value();
        if s < best_sigma {
            best = i;
            best_sigma = s;
        }
    }
    best
}

fn angle_between(a: &CameraRay, b: &CameraRay) -> f64 {
    let d = a.dot(b).clamp(-1.0, 1.0);
    d.acos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::horizon::HorizonLine;
    use crate::horizon_providers::{
        DirectSight, HorizonHypothesis, HorizonProvenance, OpticalKind,
    };
    use bris_core::Uncertain;

    fn intr() -> Intrinsics {
        Intrinsics::placeholder(1280, 720)
    }

    fn hyp_at(intercept: f64, sigma: f64, with_sight: bool) -> HorizonHypothesis {
        let line = HorizonLine {
            slope: 0.0,
            intercept,
            inlier_count: 100,
            candidate_count: 200,
            residual_rms_px: 0.3,
            altitude_sigma: Sigma::new(sigma).unwrap(),
        };
        HorizonHypothesis {
            line,
            provenance: HorizonProvenance::Optical(OpticalKind::Gradient),
            direct_sight: if with_sight {
                Some(DirectSight {
                    body_pixel: (100.0, 100.0),
                    observed_altitude: Uncertain::new(0.05, Sigma::new(0.001).unwrap()),
                })
            } else {
                None
            },
        }
    }

    #[test]
    fn two_identical_concordant_yields_sigma_over_sqrt2() {
        let sigma = 1e-3;
        let h = vec![hyp_at(400.0, sigma, false), hyp_at(400.0, sigma, false)];
        let out = fuse_horizon_hypotheses(&h, &intr(), 1280, &HorizonFusionConfig::default());
        assert_eq!(out.mode, FusionMode::Clustered);
        assert_eq!(out.cluster_size, 2);
        let s = out.hypothesis.unwrap().line.altitude_sigma.value();
        let expected = sigma / 2.0_f64.sqrt();
        assert!(
            (s - expected).abs() / expected < 0.05,
            "fused σ {s} not ≈ σ/√2 = {expected}"
        );
    }

    #[test]
    fn three_identical_concordant_yields_sigma_over_sqrt3() {
        let sigma = 1e-3;
        let h = vec![
            hyp_at(400.0, sigma, false),
            hyp_at(400.0, sigma, false),
            hyp_at(400.0, sigma, false),
        ];
        let out = fuse_horizon_hypotheses(&h, &intr(), 1280, &HorizonFusionConfig::default());
        assert_eq!(out.cluster_size, 3);
        let s = out.hypothesis.unwrap().line.altitude_sigma.value();
        let expected = sigma / 3.0_f64.sqrt();
        assert!(
            (s - expected).abs() / expected < 0.05,
            "fused σ {s} not ≈ σ/√3 = {expected}"
        );
    }

    #[test]
    fn outlier_rejected_from_cluster() {
        let sigma = 1e-3;
        // Three concordant at intercept 400 + one wildly off
        // at intercept 100 with the same σ. The outlier's
        // normal differs by tens of σ from the cluster mean.
        let h = vec![
            hyp_at(400.0, sigma, false),
            hyp_at(400.1, sigma, false),
            hyp_at(399.9, sigma, false),
            hyp_at(100.0, sigma, false),
        ];
        let out = fuse_horizon_hypotheses(&h, &intr(), 1280, &HorizonFusionConfig::default());
        assert_eq!(out.mode, FusionMode::Clustered);
        assert_eq!(out.cluster_size, 3, "outlier should not join the cluster");
    }

    #[test]
    fn all_discordant_falls_back_to_lowest_sigma_singleton() {
        // Widely separated intercepts; each pair's angular
        // distance dwarfs k·σ.
        let h = vec![
            hyp_at(100.0, 1e-4, false),
            hyp_at(400.0, 2e-4, false),
            hyp_at(700.0, 3e-4, false),
        ];
        let out = fuse_horizon_hypotheses(&h, &intr(), 1280, &HorizonFusionConfig::default());
        assert_eq!(out.mode, FusionMode::Discordant);
        assert_eq!(out.cluster_size, 1);
        let chosen = out.hypothesis.unwrap();
        // Lowest σ was h[0].
        assert!((chosen.line.intercept - 100.0).abs() < 1e-6);
    }

    #[test]
    fn cluster_with_direct_sights_propagates_all() {
        let sigma = 1e-3;
        let h = vec![
            hyp_at(400.0, sigma, true),
            hyp_at(400.0, sigma, true),
            hyp_at(400.0, sigma, false),
        ];
        let out = fuse_horizon_hypotheses(&h, &intr(), 1280, &HorizonFusionConfig::default());
        assert_eq!(out.mode, FusionMode::Clustered);
        assert_eq!(out.direct_sights.len(), 2);
    }

    #[test]
    fn disabled_returns_singleton_with_lowest_sigma() {
        let h = vec![hyp_at(400.0, 2e-3, false), hyp_at(400.0, 1e-3, false)];
        let cfg = HorizonFusionConfig {
            enabled: false,
            ..HorizonFusionConfig::default()
        };
        let out = fuse_horizon_hypotheses(&h, &intr(), 1280, &cfg);
        assert_eq!(out.mode, FusionMode::Disabled);
        assert_eq!(out.cluster_size, 1);
        let chosen = out.hypothesis.unwrap();
        assert!(matches!(chosen.provenance, HorizonProvenance::Optical(_)));
        assert!((chosen.line.altitude_sigma.value() - 1e-3).abs() < 1e-12);
    }

    #[test]
    fn single_hypothesis_passes_through_unchanged() {
        let h = vec![hyp_at(400.0, 1e-3, false)];
        let out = fuse_horizon_hypotheses(&h, &intr(), 1280, &HorizonFusionConfig::default());
        assert_eq!(out.mode, FusionMode::Singleton);
        assert_eq!(out.cluster_size, 1);
        let chosen = out.hypothesis.unwrap();
        assert!(matches!(chosen.provenance, HorizonProvenance::Optical(_)));
    }
}
