//! Auto-detected horizon from reflection pairs.
//!
//! See `docs/design/horizon_autodetect.md` §3 for the
//! algorithm and §10 for the locked Phase 1 decisions.
//!
//! Real implementation lands in this file; this stub is the
//! commit-1 placeholder so the module path is reserved before
//! commit-3 fills in the algorithm.

use super::{HorizonHypothesis, HorizonProvider, HorizonProviderContext, TemporalScope};

/// Configuration for [`ReflectionPairProvider`].
#[derive(Debug, Clone, Copy)]
pub struct ReflectionPairConfig {
    /// k-sigma tolerance for Test 3 (catalog consistency).
    pub catalog_tolerance_sigma: f64,
    /// k-sigma tolerance for Test 4 (multi-pair agreement).
    pub multi_pair_tolerance_sigma: f64,
    /// Maximum angle (radians) from a strictly vertical pair
    /// plane before a pair is rejected by Test 1.
    pub max_bisector_horizontal_rad: f64,
    /// Cold-start: minimum concordant pairs required when no
    /// position prior is available (drops Test 3).
    pub cold_start_min_pairs: usize,
    /// Floor on the synthesized horizon altitude σ (radians).
    pub sigma_floor_rad: f64,
    /// Brightness-ratio tolerance for Test 2: a reflection
    /// passes if `brightness_dn ≤ brightness_up · (1 + tol)`.
    pub photometric_tolerance: f64,
    /// Maximum age (seconds) of a [`super::PositionPrior`]
    /// before it is treated as stale (i.e. cold start).
    pub max_prior_age_s: f64,
}

impl Default for ReflectionPairConfig {
    fn default() -> Self {
        Self {
            catalog_tolerance_sigma: 4.0,
            multi_pair_tolerance_sigma: 3.0,
            max_bisector_horizontal_rad: 0.05,
            cold_start_min_pairs: 3,
            sigma_floor_rad: 1e-4,
            photometric_tolerance: 0.10,
            max_prior_age_s: 30.0,
        }
    }
}

/// Reflection-pair horizon provider.
///
/// Commit-1 stub — real algorithm lands in commit 3.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReflectionPairProvider {
    /// Tunables for the five tests.
    pub config: ReflectionPairConfig,
}

impl HorizonProvider for ReflectionPairProvider {
    fn name(&self) -> &'static str {
        "reflection_pair"
    }

    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }

    fn detect(&self, _ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        // Real implementation lands in commit 3.
        None
    }
}
