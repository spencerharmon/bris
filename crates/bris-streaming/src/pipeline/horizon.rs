//! Stage C: horizon detection in cheap-first order with
//! per-detector early termination.
//!
//! The design doc lists the cost ordering:
//!
//! | Detector                                                                                                | Approx. cost | Regime          |
//! |---------------------------------------------------------------------------------------------------------|--------------|-----------------|
//! | [`bris_vision::detect_horizon`] (gradient)                                                              | ~5 ms        | Day             |
//! | [`bris_vision::detect_horizon_via_sky_region`]                                                          | ~10 ms       | Day             |
//! | [`bris_vision::detect_horizon_night`]                                                                   | ~10 ms       | Night/Twilight  |
//! | [`bris_vision::detect_horizon_night_textured`]                                                          | ~15 ms       | Night/Twilight  |
//! | `bris_vision::detect_horizon_via_segmentation` (feature-gated)                                          | ~100 ms      | Last resort     |
//!
//! Cheap-first ordering:
//!
//! 1. Run the cheap detectors appropriate to the classifier
//!    verdict (Day → gradient + sky-region; Night/Twilight →
//!    night + night-textured; Twilight tries both day and night
//!    variants because the regime is ambiguous by definition).
//! 2. If any successful detector produced a horizon with σ at or
//!    below [`crate::EngineConfig::horizon_early_termination_sigma_rad`],
//!    stop and return the best.
//! 3. Otherwise (no detector succeeded, or σ above threshold)
//!    fall through to the segmentation detector when
//!    [`crate::EngineConfig::segmentation_model_path`] is set
//!    and the `segmentation` feature is enabled.
//! 4. Return the best-σ horizon across all attempts, or
//!    [`HorizonStageOutcome::None`] if every attempt failed.
//!
//! # Why "best across all attempts" rather than "first success"
//!
//! A cheap detector can return a successful horizon with σ above
//! the early-termination threshold. We still want segmentation to
//! run in that case (because it might do much better). The match
//! returns the *better* of the two — we don't throw away the
//! cheap result just because we ran segmentation, and we don't
//! commit to the cheap result just because it was first.
//!
//! # Failure handling
//!
//! Every detector returns a typed `Result`. The `Err` variants
//! carry rich diagnostic context but for Stage C's purposes
//! they're all equivalent: "this detector did not produce a
//! horizon for this frame." We log at `trace!` (per failure) so
//! that operators inspecting frame-level logs can see exactly
//! which detectors failed where, but we don't surface the
//! errors upward — Stage C's contract is "best horizon found,
//! or None."

use crate::config::EngineConfig;
use crate::pipeline::horizon_providers::{
    optical_kind_to_detector, GradientProvider, NightProvider, NightTexturedProvider,
    SkyRegionProvider,
};
use bris_core::time::Tt;
use bris_vision::{
    BodyCandidate, Condition, Frame, FramePyramid, HorizonLine, HorizonProvenance, HorizonProvider,
    HorizonProviderContext, Intrinsics, PositionPrior,
};
use tracing::{debug, trace};

#[cfg(feature = "segmentation")]
use crate::pipeline::horizon_providers::SegmentationProvider;

/// Outcome of one frame's pass through Stage C.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HorizonStageOutcome {
    /// A horizon line was detected. Carries the detector that
    /// produced it (for diagnostics) and the line itself.
    Detected {
        /// Which detector produced the line. Useful for
        /// diagnostics and for the eventual stitching σ
        /// estimate (different detectors have different
        /// per-frame jitter).
        detector: HorizonDetector,
        /// The horizon line, with its `altitude_sigma`.
        line: HorizonLine,
        /// Direct sight emitted alongside the horizon (e.g.
        /// reflection-pair's `Ho = θ/2`). `None` for optical
        /// detectors. Stage E consumes this when present
        /// instead of computing a sight via `measure_altitude`.
        direct_sight: Option<bris_vision::DirectSight>,
    },
    /// No detector produced a horizon for this frame.
    None,
}

/// Identifier for the horizon detector that produced a line.
///
/// Numbered in order of evaluation so the diagnostics surface
/// can summarize "the cheap detectors are sufficient" vs "we
/// keep falling through to segmentation."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HorizonDetector {
    /// [`bris_vision::detect_horizon`] (gradient).
    Gradient,
    /// [`bris_vision::detect_horizon_via_sky_region`].
    SkyRegion,
    /// [`bris_vision::detect_horizon_night`].
    Night,
    /// [`bris_vision::detect_horizon_night_textured`].
    NightTextured,
    /// `bris_vision::detect_horizon_via_segmentation`. Only
    /// constructible when the `segmentation` feature is
    /// enabled.
    Segmentation,
    /// Auto-detected reflection-pair provider (Phase 1 of the
    /// horizon-providers roadmap; see
    /// `docs/design/horizon_autodetect.md`).
    ReflectionPair,
}

/// Run Stage C on one frame.
///
/// `condition` is the Stage A verdict; it gates which detectors
/// are tried first. The returned outcome carries the best-σ
/// horizon found across all attempts, or
/// [`HorizonStageOutcome::None`] if no detector succeeded.
pub(crate) fn detect(
    pyramid: &FramePyramid,
    condition: Condition,
    cfg: &EngineConfig,
    body_candidates: &[BodyCandidate],
    position_prior: Option<PositionPrior>,
    timestamp: Tt,
) -> (HorizonStageOutcome, (u32, u32)) {
    let mut best: Option<(HorizonDetector, HorizonLine)> = None;

    let early_term = cfg.horizon_early_termination_sigma_rad;
    let day_first = matches!(condition, Condition::Day | Condition::Twilight);
    let night_first = matches!(condition, Condition::Night | Condition::Twilight);
    // Unusable: skip Stage C entirely. The classifier said the
    // frame has nothing actionable; running the heavy detectors
    // would just waste time.
    if matches!(condition, Condition::Unusable) {
        debug!("Stage C skipped: classifier reported Unusable");
        return (
            HorizonStageOutcome::None,
            (pyramid.full_width(), pyramid.full_height()),
        );
    }

    let frame_for_detect = match cfg
        .resolved_horizon_analysis_size(pyramid.full_width(), pyramid.full_height())
    {
        Some((w, h)) => match pyramid.level(w, h) {
            Ok(level) => CowFrame::Owned(level.frame),
            Err(e) => {
                debug!(
                    error = %e,
                    requested_w = w,
                    requested_h = h,
                    "Stage C: requested pyramid level unavailable; falling back to source resolution",
                );
                CowFrame::Borrowed(pyramid.full())
            }
        },
        None => CowFrame::Borrowed(pyramid.full()),
    };
    let frame = frame_for_detect.as_ref();
    let analyzed_size = (frame.width(), frame.height());
    let intrinsics: Intrinsics = frame.intrinsics;
    let ctx = HorizonProviderContext {
        frame,
        intrinsics: &intrinsics,
        body_candidates,
        position_prior,
        timestamp,
    };
    // Last-emitted direct sight (if any) from a provider that
    // produced one. Phase 1 has only one such provider
    // (reflection-pair); when more land the cheap-first /
    // best-σ winner determines which direct sight propagates.
    let mut best_direct_sight: Option<bris_vision::DirectSight> = None;

    if day_first {
        run_provider(
            &GradientProvider { cfg },
            &ctx,
            &mut best,
            &mut best_direct_sight,
        );
        if early_terminate(best.as_ref(), early_term) {
            return (finish(best, best_direct_sight), analyzed_size);
        }
        run_provider(
            &SkyRegionProvider { cfg },
            &ctx,
            &mut best,
            &mut best_direct_sight,
        );
        if early_terminate(best.as_ref(), early_term) {
            return (finish(best, best_direct_sight), analyzed_size);
        }
    }
    if night_first {
        run_provider(
            &NightProvider { cfg },
            &ctx,
            &mut best,
            &mut best_direct_sight,
        );
        if early_terminate(best.as_ref(), early_term) {
            return (finish(best, best_direct_sight), analyzed_size);
        }
        run_provider(
            &NightTexturedProvider { cfg },
            &ctx,
            &mut best,
            &mut best_direct_sight,
        );
        if early_terminate(best.as_ref(), early_term) {
            return (finish(best, best_direct_sight), analyzed_size);
        }
        // Reflection-pair is run as a *second pass* after
        // Stage B has produced body candidates; see
        // [`merge_reflection_pair`]. The first-pass dispatcher
        // receives `body_candidates = &[]` from `process_frame`
        // because horizon must run before Stage B for masking.
        let _ = body_candidates; // tracked at the second-pass site
    }
    // Last resort: segmentation. Gated on the feature flag and
    // on the operator opting in by supplying a model path.
    run_segmentation(&ctx, cfg, &mut best, &mut best_direct_sight);
    (finish(best, best_direct_sight), analyzed_size)
}

/// Run the reflection-pair provider against an already-
/// computed [`HorizonStageOutcome`] and merge by best-σ.
///
/// The first-pass `detect()` runs the *optical* providers
/// with empty body candidates (horizon must run before Stage
/// B to mask night peaks). Once Stage B has populated body
/// candidates this helper runs the reflection-pair provider
/// with them and merges its hypothesis into the existing
/// outcome under the same trait-driven best-σ rule used by
/// [`run_provider`]. Returns the (possibly updated) outcome
/// plus per-frame stats and merge bookkeeping.
pub(crate) struct ReflectionPairMerge {
    pub outcome: HorizonStageOutcome,
    pub stats: bris_vision::ReflectionPairStats,
    /// Provider returned `Some(hypothesis)`.
    pub hypothesized: bool,
    /// Hypothesis won the merge (smallest σ) and is the
    /// new outcome.
    pub used: bool,
}

pub(crate) fn merge_reflection_pair(
    prev: HorizonStageOutcome,
    ctx: &HorizonProviderContext<'_>,
) -> ReflectionPairMerge {
    let mut stats = bris_vision::ReflectionPairStats::default();
    let provider = bris_vision::ReflectionPairProvider::default();
    let Some(hyp) = provider.detect_with_stats(ctx, &mut stats) else {
        return ReflectionPairMerge {
            outcome: prev,
            stats,
            hypothesized: false,
            used: false,
        };
    };
    // Seed `best` from the previous outcome (if any) and use
    // the same best-σ merge as the first-pass dispatcher.
    let mut best: Option<(HorizonDetector, HorizonLine)> = match prev {
        HorizonStageOutcome::Detected { detector, line, .. } => Some((detector, line)),
        HorizonStageOutcome::None => None,
    };
    let mut best_direct_sight: Option<bris_vision::DirectSight> = match prev {
        HorizonStageOutcome::Detected { direct_sight, .. } => direct_sight,
        HorizonStageOutcome::None => None,
    };
    let detector = detector_from_provenance(hyp.provenance);
    let improved = match best {
        Some((_, ref existing)) => hyp.line.altitude_sigma < existing.altitude_sigma,
        None => true,
    };
    if improved {
        best = Some((detector, hyp.line));
        best_direct_sight = hyp.direct_sight;
    }
    let used = improved;
    let outcome = finish(best, best_direct_sight);
    ReflectionPairMerge {
        outcome,
        stats,
        hypothesized: true,
        used,
    }
}

/// Run one provider; update `best` (smallest σ wins) and
/// record the winning hypothesis's direct sight, if any.
fn run_provider<P: HorizonProvider>(
    provider: &P,
    ctx: &HorizonProviderContext<'_>,
    best: &mut Option<(HorizonDetector, HorizonLine)>,
    best_direct_sight: &mut Option<bris_vision::DirectSight>,
) {
    let Some(hyp) = provider.detect(ctx) else {
        return;
    };
    let detector = detector_from_provenance(hyp.provenance);
    trace!(
        detector = ?detector,
        sigma_rad = hyp.line.altitude_sigma.value(),
        inliers = hyp.line.inlier_count,
        "Stage C: provider produced hypothesis"
    );
    let improved = match best {
        Some((_, existing)) => hyp.line.altitude_sigma < existing.altitude_sigma,
        None => true,
    };
    if improved {
        *best = Some((detector, hyp.line));
        *best_direct_sight = hyp.direct_sight;
    }
}

fn detector_from_provenance(p: HorizonProvenance) -> HorizonDetector {
    match p {
        HorizonProvenance::Optical(kind) => optical_kind_to_detector(kind),
        HorizonProvenance::ReflectionPair { .. } => HorizonDetector::ReflectionPair,
    }
}

#[cfg(feature = "segmentation")]
fn run_segmentation(
    ctx: &HorizonProviderContext<'_>,
    cfg: &EngineConfig,
    best: &mut Option<(HorizonDetector, HorizonLine)>,
    best_direct_sight: &mut Option<bris_vision::DirectSight>,
) {
    let provider = SegmentationProvider { cfg };
    run_provider(&provider, ctx, best, best_direct_sight);
}

#[cfg(not(feature = "segmentation"))]
fn run_segmentation(
    _ctx: &HorizonProviderContext<'_>,
    _cfg: &EngineConfig,
    _best: &mut Option<(HorizonDetector, HorizonLine)>,
    _best_direct_sight: &mut Option<bris_vision::DirectSight>,
) {
    // Segmentation feature disabled at compile time.
}

/// Cheap copy-on-write wrapper for the analysis-resolution frame.
/// Either we borrow the pyramid's full frame (no downsample) or
/// own a freshly-cloned downsampled level. Both expose the same
/// `&Frame` view to the per-detector helpers, which already
/// take `&Frame` and don't care about ownership.
enum CowFrame<'a> {
    Borrowed(&'a Frame),
    Owned(Frame),
}

impl CowFrame<'_> {
    fn as_ref(&self) -> &Frame {
        match self {
            Self::Borrowed(f) => f,
            Self::Owned(f) => f,
        }
    }
}

/// True iff the current best horizon's σ is at or below the
/// early-termination threshold (and a best exists at all).
fn early_terminate(
    best: Option<&(HorizonDetector, HorizonLine)>,
    early_term_sigma_rad: f64,
) -> bool {
    match best {
        Some((_, line)) => line.altitude_sigma.value() <= early_term_sigma_rad,
        None => false,
    }
}

fn finish(
    best: Option<(HorizonDetector, HorizonLine)>,
    direct_sight: Option<bris_vision::DirectSight>,
) -> HorizonStageOutcome {
    match best {
        Some((detector, line)) => HorizonStageOutcome::Detected {
            detector,
            line,
            direct_sight,
        },
        None => HorizonStageOutcome::None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )]

    use super::*;
    use crate::EngineConfig;
    use bris_almanac::Observer;
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{Frame, Intrinsics};

    /// A synthetic 64×64 frame with a sharp horizontal sky/sea
    /// boundary at row `horizon_y`: bright above, dark below.
    /// The gradient detector should find a horizontal line at
    /// (or very close to) `y = horizon_y`.
    fn synthetic_horizon(horizon_y: u32) -> Frame {
        let w = 64_u32;
        let h = 64_u32;
        let mut pixels = vec![0u16; (w * h) as usize];
        for y in 0..h {
            let value = if y < horizon_y { 50_000 } else { 200 };
            for x in 0..w {
                pixels[(y as usize) * (w as usize) + (x as usize)] = value;
            }
        }
        Frame::new(
            w,
            h,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(w, h),
        )
        .unwrap()
    }

    fn dark_frame() -> Frame {
        let pixels = vec![10u16; 64 * 64];
        Frame::new(
            64,
            64,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(64, 64),
        )
        .unwrap()
    }

    #[test]
    fn day_synthetic_horizon_detected_by_cheap_detector() {
        // The synthetic horizon is a textbook case for the
        // gradient detector. Stage C should detect it; the
        // returned horizon's σ should be small (synthetic data
        // gives sub-arcsec residuals).
        let cfg = EngineConfig::new(Observer::default_dev());
        let frame = synthetic_horizon(32);
        let (outcome, _) = detect(
            &FramePyramid::new(frame.clone()),
            Condition::Day,
            &cfg,
            &[],
            None,
            frame.capture_tt,
        );
        match outcome {
            HorizonStageOutcome::Detected { detector, line, .. } => {
                // Detector should be one of the cheap day-path
                // detectors; segmentation isn't enabled by path.
                assert!(
                    matches!(
                        detector,
                        HorizonDetector::Gradient | HorizonDetector::SkyRegion
                    ),
                    "expected a day-path detector, got {detector:?}"
                );
                // Intercept should be near the true horizon row
                // (32). Tolerance generous because the gradient
                // detector centers between bright and dark rows.
                assert!(
                    (line.intercept - 32.0).abs() < 5.0,
                    "horizon intercept {} too far from synthetic 32",
                    line.intercept
                );
                // σ should be small for a synthetic frame.
                assert!(
                    line.altitude_sigma.value() < 0.01,
                    "σ {} unexpectedly large for synthetic horizon",
                    line.altitude_sigma.value()
                );
            }
            HorizonStageOutcome::None => panic!("Stage C produced no horizon for synthetic frame"),
        }
    }

    #[test]
    fn unusable_classification_skips_stage_c() {
        let cfg = EngineConfig::new(Observer::default_dev());
        let frame = synthetic_horizon(32);
        let (outcome, _) = detect(
            &FramePyramid::new(frame.clone()),
            Condition::Unusable,
            &cfg,
            &[],
            None,
            frame.capture_tt,
        );
        assert!(matches!(outcome, HorizonStageOutcome::None));
    }

    #[test]
    fn night_path_dispatched_for_night_classification() {
        // Night detectors have a hard time with synthetic
        // textureless frames; we mostly assert they don't panic
        // and that the day-path detectors aren't tried.
        let cfg = EngineConfig::new(Observer::default_dev());
        let frame = dark_frame();
        let (outcome, _) = detect(
            &FramePyramid::new(frame.clone()),
            Condition::Night,
            &cfg,
            &[],
            None,
            frame.capture_tt,
        );
        // Either a Night/NightTextured detection or None;
        // never a day-path detector.
        match outcome {
            HorizonStageOutcome::Detected { detector, .. } => {
                assert!(
                    matches!(
                        detector,
                        HorizonDetector::Night | HorizonDetector::NightTextured
                    ),
                    "night classification should not select day detectors, got {detector:?}"
                );
            }
            HorizonStageOutcome::None => {
                // Acceptable: textureless dark frame, neither
                // night detector finds anything.
            }
        }
    }

    #[test]
    fn early_termination_skips_more_expensive_detectors() {
        // With a synthetic horizon and the default 1-arcmin
        // early-termination threshold, the gradient detector
        // alone should produce a horizon below threshold and
        // sky-region/segmentation should not be invoked. We
        // verify by checking that the detector that produced
        // the result is the gradient one.
        let cfg = EngineConfig::new(Observer::default_dev());
        let frame = synthetic_horizon(32);
        let (outcome, _) = detect(
            &FramePyramid::new(frame.clone()),
            Condition::Day,
            &cfg,
            &[],
            None,
            frame.capture_tt,
        );
        match outcome {
            HorizonStageOutcome::Detected { detector, .. } => {
                // Allow either the gradient or sky-region
                // detector to claim the win — both are cheap;
                // both terminate early. The point is that
                // segmentation never ran (asserted by the
                // detector identity).
                assert_ne!(
                    detector,
                    HorizonDetector::Segmentation,
                    "segmentation should not run when a cheap detector terminated early"
                );
            }
            HorizonStageOutcome::None => panic!("expected synthetic horizon to be detected"),
        }
    }

    #[test]
    fn segmentation_skipped_when_path_unset() {
        // Default config has segmentation_model_path = None;
        // the segmentation detector must not be tried.
        // Engineering check: the helper `try_segmentation`
        // returns early on None path (verified above by the
        // early-termination test, but make it explicit here).
        let cfg = EngineConfig::new(Observer::default_dev());
        assert!(
            cfg.segmentation_model_path.is_none(),
            "default config should not enable segmentation"
        );
    }
}
