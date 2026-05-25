//! Wrappers exposing the existing optical horizon detectors as
//! [`bris_vision::HorizonProvider`] implementations.
//!
//! The detectors themselves are free functions in `bris-vision`;
//! these wrappers adapt them to the trait so the
//! `pipeline::horizon` dispatch can run all providers (optical
//! plus auto-horizon) uniformly. They carry the engine's
//! [`crate::EngineConfig`] by reference and ignore the
//! `body_candidates` / `position_prior` fields of the context
//! (optical detection doesn't need them).
//!
//! Each wrapper's `name()` and `OpticalKind` discriminant maps
//! 1:1 to a [`super::horizon::HorizonDetector`] variant. The
//! mapping is done by [`optical_kind_to_detector`] at the
//! pipeline dispatch site.

use crate::config::EngineConfig;
use bris_vision::{
    detect_horizon, detect_horizon_night, detect_horizon_night_textured,
    detect_horizon_via_sky_region, HorizonHypothesis, HorizonProvenance, HorizonProvider,
    HorizonProviderContext, OpticalKind, TemporalScope,
};
use tracing::trace;

use super::horizon::HorizonDetector;

/// Map a [`bris_vision::OpticalKind`] to the streaming engine's
/// [`HorizonDetector`] discriminant.
pub(crate) fn optical_kind_to_detector(kind: OpticalKind) -> HorizonDetector {
    match kind {
        OpticalKind::Gradient => HorizonDetector::Gradient,
        OpticalKind::SkyRegion => HorizonDetector::SkyRegion,
        OpticalKind::Night => HorizonDetector::Night,
        OpticalKind::NightTextured => HorizonDetector::NightTextured,
        OpticalKind::Segmentation => HorizonDetector::Segmentation,
    }
}

/// Wrapper over [`detect_horizon`] (gradient).
#[derive(Debug, Clone, Copy)]
pub(crate) struct GradientProvider<'a> {
    pub cfg: &'a EngineConfig,
}

impl HorizonProvider for GradientProvider<'_> {
    fn name(&self) -> &'static str {
        "gradient"
    }
    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }
    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        match detect_horizon(ctx.frame, self.cfg.horizon_cfg) {
            Ok(line) => Some(HorizonHypothesis {
                line,
                provenance: HorizonProvenance::Optical(OpticalKind::Gradient),
                direct_sight: None,
            }),
            Err(e) => {
                trace!(detector = "gradient", error = %e, "Stage C: detector failed");
                None
            }
        }
    }
}

/// Wrapper over [`detect_horizon_via_sky_region`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct SkyRegionProvider<'a> {
    pub cfg: &'a EngineConfig,
}

impl HorizonProvider for SkyRegionProvider<'_> {
    fn name(&self) -> &'static str {
        "sky_region"
    }
    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }
    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        match detect_horizon_via_sky_region(ctx.frame, self.cfg.horizon_cfg) {
            Ok(line) => Some(HorizonHypothesis {
                line,
                provenance: HorizonProvenance::Optical(OpticalKind::SkyRegion),
                direct_sight: None,
            }),
            Err(e) => {
                trace!(detector = "sky_region", error = %e, "Stage C: detector failed");
                None
            }
        }
    }
}

/// Wrapper over [`detect_horizon_night`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct NightProvider<'a> {
    pub cfg: &'a EngineConfig,
}

impl HorizonProvider for NightProvider<'_> {
    fn name(&self) -> &'static str {
        "night"
    }
    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }
    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        match detect_horizon_night(ctx.frame, self.cfg.night_horizon_cfg) {
            Ok(line) => Some(HorizonHypothesis {
                line,
                provenance: HorizonProvenance::Optical(OpticalKind::Night),
                direct_sight: None,
            }),
            Err(e) => {
                trace!(detector = "night", error = %e, "Stage C: detector failed");
                None
            }
        }
    }
}

/// Wrapper over [`detect_horizon_night_textured`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct NightTexturedProvider<'a> {
    pub cfg: &'a EngineConfig,
}

impl HorizonProvider for NightTexturedProvider<'_> {
    fn name(&self) -> &'static str {
        "night_textured"
    }
    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }
    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        match detect_horizon_night_textured(ctx.frame, self.cfg.textured_horizon_cfg) {
            Ok(line) => Some(HorizonHypothesis {
                line,
                provenance: HorizonProvenance::Optical(OpticalKind::NightTextured),
                direct_sight: None,
            }),
            Err(e) => {
                trace!(detector = "night_textured", error = %e, "Stage C: detector failed");
                None
            }
        }
    }
}

/// Wrapper over the feature-gated segmentation detector.
///
/// Only constructible from inside `pipeline::horizon` when the
/// `segmentation` feature is enabled by the streaming crate.
#[cfg(feature = "segmentation")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct SegmentationProvider<'a> {
    pub cfg: &'a EngineConfig,
}

#[cfg(feature = "segmentation")]
impl HorizonProvider for SegmentationProvider<'_> {
    fn name(&self) -> &'static str {
        "segmentation"
    }
    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }
    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        use bris_vision::{detect_horizon_via_segmentation, load_model};
        let model_path = self.cfg.segmentation_model_path.as_ref()?;
        if let Err(e) = load_model(model_path) {
            tracing::debug!(
                path = %model_path.display(),
                error = %e,
                "Stage C: segmentation model load failed; will not retry per-frame"
            );
            return None;
        }
        match detect_horizon_via_segmentation(ctx.frame, self.cfg.horizon_cfg) {
            Ok(line) => Some(HorizonHypothesis {
                line,
                provenance: HorizonProvenance::Optical(OpticalKind::Segmentation),
                direct_sight: None,
            }),
            Err(e) => {
                trace!(detector = "segmentation", error = %e, "Stage C: detector failed");
                None
            }
        }
    }
}
