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
        use bris_vision::detect_horizon_via_segmentation_with_mask;
        // The pipeline pre-computes the segmentation mask once
        // per frame (`process_frame::precompute_seg_mask`) and
        // threads it through the provider context. When the
        // pre-compute didn't produce a mask (feature disabled,
        // model failed to load, frame lacks a `source_path`,
        // inference failed), this provider declines for this
        // frame rather than running inference in-line — doing
        // so would defeat the once-per-frame cache.
        let mask = ctx.seg_mask?;
        match detect_horizon_via_segmentation_with_mask(ctx.frame, self.cfg.horizon_cfg, mask, None)
        {
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

#[cfg(all(test, feature = "segmentation"))]
mod seg_cache_tests {
    //! Structural tests proving the segmentation provider
    //! reads from the pipeline-supplied cache rather than
    //! running its own inference pass. The "seg runs once
    //! per frame" guarantee falls out of this contract:
    //! since the provider declines when `ctx.seg_mask` is
    //! `None`, the only inference call site is the
    //! pipeline's `precompute_seg_mask`, which runs at
    //! most once per `process_frame`.

    #![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

    use super::*;
    use bris_almanac::Observer;
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::horizon_providers::{HorizonProvider, HorizonProviderContext};
    use bris_vision::segment::{CLASS_SEA, CLASS_SKY, INFERENCE_SIZE};
    use bris_vision::{Frame, Intrinsics, SegmentationMask};

    fn frame(w: u32, h: u32) -> Frame {
        let pixels = vec![0u16; (w * h) as usize];
        Frame::new(
            w,
            h,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(w, h),
        )
        .unwrap()
    }

    /// Build a synthetic segmentation mask whose top half is
    /// `CLASS_SKY` and bottom half is `CLASS_SEA` — enough for
    /// the finalize step to fit a horizon line. Inference-resolution.
    fn synthetic_sky_sea_mask() -> SegmentationMask {
        let n = INFERENCE_SIZE;
        let mut labels = vec![0u32; n * n];
        for y in 0..n {
            let cls = if y < n / 2 { CLASS_SKY } else { CLASS_SEA };
            for x in 0..n {
                labels[y * n + x] = cls;
            }
        }
        SegmentationMask {
            width: n as u32,
            height: n as u32,
            labels,
        }
    }

    fn ctx_with<'a>(
        f: &'a Frame,
        mask: Option<&'a SegmentationMask>,
    ) -> HorizonProviderContext<'a> {
        HorizonProviderContext {
            frame: f,
            intrinsics: &f.intrinsics,
            body_candidates: &[],
            position_prior: None,
            timestamp: f.capture_tt,
            seg_mask: mask,
        }
    }

    #[test]
    fn segmentation_provider_declines_when_seg_mask_is_none() {
        // No cached mask -> provider must NOT run inference
        // in-line. Returns None without touching the model.
        let cfg = EngineConfig::new(Observer::default_dev());
        let f = frame(256, 256);
        let ctx = ctx_with(&f, None);
        let provider = SegmentationProvider { cfg: &cfg };
        assert!(
            provider.detect(&ctx).is_none(),
            "SegmentationProvider must decline without a cached seg mask",
        );
    }

    #[test]
    fn segmentation_provider_uses_cached_mask_without_loading_model() {
        // load_model has not been called. The synthetic mask
        // makes the finalize step produce a horizon. If the
        // provider tried to run its own inference it would
        // fail with "model not loaded".
        let cfg = EngineConfig::new(Observer::default_dev());
        let f = frame(256, 256);
        let mask = synthetic_sky_sea_mask();
        let ctx = ctx_with(&f, Some(&mask));
        let provider = SegmentationProvider { cfg: &cfg };
        let hyp = provider
            .detect(&ctx)
            .expect("cached mask should drive a horizon hypothesis");
        // Sky/sea boundary at mid-frame; check intercept is
        // roughly halfway down (256 / 2 = 128).
        assert!(
            (hyp.line.intercept - 128.0).abs() < 16.0,
            "expected horizon near y=128, got {}",
            hyp.line.intercept,
        );
    }

    #[test]
    fn multiple_provider_invocations_share_one_cached_mask() {
        // Two back-to-back detect() calls with the SAME
        // cached mask must each succeed without invoking
        // inference (model is not loaded in this test).
        // This is the structural guarantee that the
        // segmentation pass runs at most once per frame
        // regardless of how many horizon providers consult
        // the mask.
        let cfg = EngineConfig::new(Observer::default_dev());
        let f = frame(256, 256);
        let mask = synthetic_sky_sea_mask();
        let ctx = ctx_with(&f, Some(&mask));
        let provider = SegmentationProvider { cfg: &cfg };
        assert!(provider.detect(&ctx).is_some());
        assert!(provider.detect(&ctx).is_some());
    }
}
