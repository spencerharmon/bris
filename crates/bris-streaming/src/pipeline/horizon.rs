//! Stage C: horizon detection, multi-source fusion.
//!
//! All providers compatible with the dispatched condition run
//! in cheap-first order; each contributes (or declines) a
//! [`bris_vision::HorizonHypothesis`]. The collected vector
//! is then handed to
//! [`bris_vision::fuse_horizon_hypotheses`], which either
//! produces a fused [`Fused`][bris_vision::HorizonProvenance::Fused]
//! estimate (when ≥ 2 hypotheses are concordant) or falls back
//! to the lowest-σ singleton.
//!
//! Cheap-first ordering and early-termination are preserved:
//! once a cheap provider produces a hypothesis below
//! [`crate::EngineConfig::horizon_early_termination_sigma_rad`],
//! more expensive providers are *not* invoked. This keeps the
//! Pi Zero 2W budget intact. Early-termination operates on the
//! best-σ-so-far, the same condition as before; fusion runs over
//! whatever hypotheses were collected at that point.
//!
//! The reflection-pair provider still runs as a second pass —
//! it depends on Stage B body candidates that are not available
//! when Stage C first runs. The second-pass entry point
//! [`merge_reflection_pair`] re-runs the fuser over the prior
//! hypotheses plus the new one.
//!
//! ## Vertical-line provider: disabled by default
//!
//! The [`bris_vision::VerticalLineProvider`] is **off** in
//! Stage C unless
//! [`crate::EngineConfig::enable_vertical_line_provider`] is
//! flipped to `true`. The provider's gravity inference
//! (`gravity ≈ r_bot - r_top`) is only valid for short lines
//! centered on the principal point; full-height lines on
//! tilted cameras (typical capture geometry) yield
//! confidently-wrong horizons off by 20–40°. See
//! `docs/design/ml_gravity.md` for the planned replacement.

use crate::config::EngineConfig;
use crate::pipeline::horizon_providers::{
    optical_kind_to_detector, GradientProvider, NightProvider, NightTexturedProvider,
    SkyRegionProvider,
};
use bris_core::time::Tt;
use bris_vision::{
    fuse_horizon_hypotheses, BodyCandidate, Condition, DirectSight, Frame, FramePyramid,
    FusionMode, HorizonHypothesis, HorizonLine, HorizonProvenance, HorizonProvider,
    HorizonProviderContext, Intrinsics, PositionPrior,
};
use tracing::{debug, trace};

#[cfg(feature = "segmentation")]
use crate::pipeline::horizon_providers::SegmentationProvider;

/// Outcome of one frame's pass through Stage C.
#[derive(Debug, Clone)]
pub(crate) enum HorizonStageOutcome {
    /// A horizon line was detected. Carries the detector that
    /// produced it (for diagnostics) and the line itself.
    Detected {
        /// Which detector produced the line.
        detector: HorizonDetector,
        /// Provenance of the winning hypothesis, preserved
        /// for diagnostics surfaces (HUD / submissions) that
        /// want the provider-specific payload rather than just
        /// the detector discriminant.
        provenance: HorizonProvenance,
        /// The horizon line, with its `altitude_sigma`.
        line: HorizonLine,
        /// Direct sights from any providers that contributed
        /// to this outcome. Empty for purely-optical outcomes;
        /// non-empty when the reflection-pair provider (or any
        /// future direct-sight-emitting provider) participated.
        direct_sights: Vec<DirectSight>,
    },
    /// No detector produced a horizon for this frame.
    None,
}

/// Identifier for the horizon detector that produced a line.
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
    /// `bris_vision::detect_horizon_via_segmentation`.
    Segmentation,
    /// Auto-detected reflection-pair provider.
    ReflectionPair,
    /// Auto-detected single near-vertical line (plumb / edge).
    VerticalLine,
    /// Auto-detected vanishing-point provider (Manhattan-world).
    VanishingPoint,
    /// Multi-source fusion of ≥ 2 concordant hypotheses.
    Fused,
    /// ML-estimated camera-frame gravity provider.
    MlGravity,
}

/// Per-frame fusion bookkeeping. Returned to the engine for
/// folding into [`crate::EngineDiagnostics`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FusionStats {
    /// Cluster size on this frame (≥ 2 means a fused result).
    /// Zero if no hypothesis was produced.
    pub cluster_size: usize,
    /// True iff ≥ 2 providers produced concordant hypotheses
    /// and were fused.
    pub clustered: bool,
    /// True iff ≥ 2 providers produced hypotheses but none
    /// were concordant; outcome is the lowest-σ singleton.
    pub discordant: bool,
    /// True iff exactly one provider produced a hypothesis.
    pub singleton: bool,
}

/// Per-frame bookkeeping for the vanishing-point provider's
/// participation in the dispatch.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VanishingPointDispatch {
    pub stats: bris_vision::VanishingPointStats,
    pub invoked: bool,
    pub used: bool,
}

/// Per-frame bookkeeping for the vertical-line provider.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VerticalLineDispatch {
    pub stats: bris_vision::VerticalLineStats,
    /// Whether Stage C invoked the provider on this frame.
    /// `false` when [`crate::EngineConfig::enable_vertical_line_provider`]
    /// is `false` (the default — see that field's docs and
    /// `docs/design/ml_gravity.md` for the gravity-math bug
    /// motivating the disable).
    pub invoked: bool,
    pub hypothesized: bool,
    pub used: bool,
}

/// Per-frame bookkeeping for the ML-gravity provider.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MlGravityDispatch {
    #[cfg(feature = "ml-gravity")]
    pub stats: bris_vision::MlGravityStats,
    pub invoked: bool,
    pub hypothesized: bool,
    pub used: bool,
}

/// Aggregate per-frame Stage C diagnostics that are not the
/// horizon outcome itself.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StageCStats {
    pub fusion: FusionStats,
    pub vertical_line: VerticalLineDispatch,
    pub vanishing_point: VanishingPointDispatch,
    pub ml_gravity: MlGravityDispatch,
}

/// Run Stage C on one frame.
#[allow(clippy::too_many_lines)] // dispatch fan-out is inherently long; refactoring fragments the per-condition flow
pub(crate) fn detect(
    pyramid: &FramePyramid,
    condition: Condition,
    cfg: &EngineConfig,
    body_candidates: &[BodyCandidate],
    position_prior: Option<PositionPrior>,
    timestamp: Tt,
) -> (HorizonStageOutcome, (u32, u32), StageCStats) {
    let analyzed_size_full = (pyramid.full_width(), pyramid.full_height());
    if matches!(condition, Condition::Unusable) {
        debug!("Stage C skipped: classifier reported Unusable");
        return (
            HorizonStageOutcome::None,
            analyzed_size_full,
            StageCStats::default(),
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

    let early_term = cfg.horizon_early_termination_sigma_rad;
    let day_first = matches!(condition, Condition::Day | Condition::Twilight);
    let night_first = matches!(condition, Condition::Night | Condition::Twilight);

    let mut hypotheses: Vec<HorizonHypothesis> = Vec::with_capacity(6);
    let mut stats = StageCStats::default();

    if day_first {
        run_provider(&GradientProvider { cfg }, &ctx, &mut hypotheses);
        if best_below(&hypotheses, early_term) {
            return finish(
                &hypotheses,
                &intrinsics,
                frame.width(),
                cfg,
                analyzed_size,
                stats,
            );
        }
        run_provider(&SkyRegionProvider { cfg }, &ctx, &mut hypotheses);
        if best_below(&hypotheses, early_term) {
            return finish(
                &hypotheses,
                &intrinsics,
                frame.width(),
                cfg,
                analyzed_size,
                stats,
            );
        }
    }
    if night_first {
        run_provider(&NightProvider { cfg }, &ctx, &mut hypotheses);
        if best_below(&hypotheses, early_term) {
            return finish(
                &hypotheses,
                &intrinsics,
                frame.width(),
                cfg,
                analyzed_size,
                stats,
            );
        }
        run_provider(&NightTexturedProvider { cfg }, &ctx, &mut hypotheses);
        if best_below(&hypotheses, early_term) {
            return finish(
                &hypotheses,
                &intrinsics,
                frame.width(),
                cfg,
                analyzed_size,
                stats,
            );
        }
    }
    run_segmentation(&ctx, cfg, &mut hypotheses);
    if best_below(&hypotheses, early_term) {
        return finish(
            &hypotheses,
            &intrinsics,
            frame.width(),
            cfg,
            analyzed_size,
            stats,
        );
    }

    // Vertical-line provider: independent of body candidates.
    // Disabled by default; see EngineConfig::enable_vertical_line_provider
    // and docs/design/ml_gravity.md for the gravity-math bug
    // (`gravity ≈ r_bot - r_top` is only valid for short,
    // principal-point-centered lines; the streaming engine sees
    // full-height lines on tilted cameras and the inferred
    // gravity is wrong by 20–40°).
    if cfg.enable_vertical_line_provider {
        stats.vertical_line.invoked = true;
        let provider = bris_vision::VerticalLineProvider {
            config: cfg.vertical_line_provider_config,
        };
        if let Some(hyp) = provider.detect_with_stats(&ctx, &mut stats.vertical_line.stats) {
            trace!(
                provider = "vertical-line",
                sigma_rad = hyp.line.altitude_sigma.value(),
                "Stage C: provider produced hypothesis"
            );
            stats.vertical_line.hypothesized = true;
            hypotheses.push(hyp);
        }
        if best_below(&hypotheses, early_term) {
            return finish(
                &hypotheses,
                &intrinsics,
                frame.width(),
                cfg,
                analyzed_size,
                stats,
            );
        }
    }

    // Vanishing-point provider (most expensive).
    {
        stats.vanishing_point.invoked = true;
        let provider = bris_vision::VanishingPointProvider {
            config: cfg.vanishing_point_provider_config,
        };
        if let Some(hyp) = provider.detect_with_stats(&ctx, &mut stats.vanishing_point.stats) {
            trace!(
                provider = "vanishing-point",
                sigma_rad = hyp.line.altitude_sigma.value(),
                "Stage C: provider produced hypothesis"
            );
            hypotheses.push(hyp);
        }
    }

    run_ml_gravity(&ctx, cfg, &mut hypotheses, &mut stats);

    finish(
        &hypotheses,
        &intrinsics,
        frame.width(),
        cfg,
        analyzed_size,
        stats,
    )
}

/// Returns true iff the smallest-σ hypothesis collected so far
/// is at or below the early-termination threshold.
fn best_below(hypotheses: &[HorizonHypothesis], early_term_sigma_rad: f64) -> bool {
    hypotheses
        .iter()
        .map(|h| h.line.altitude_sigma.value())
        .fold(f64::INFINITY, f64::min)
        <= early_term_sigma_rad
}

fn run_provider<P: HorizonProvider>(
    provider: &P,
    ctx: &HorizonProviderContext<'_>,
    out: &mut Vec<HorizonHypothesis>,
) {
    if let Some(hyp) = provider.detect(ctx) {
        trace!(
            provider = provider.name(),
            sigma_rad = hyp.line.altitude_sigma.value(),
            inliers = hyp.line.inlier_count,
            "Stage C: provider produced hypothesis"
        );
        out.push(hyp);
    }
}

#[cfg(feature = "segmentation")]
fn run_segmentation(
    ctx: &HorizonProviderContext<'_>,
    cfg: &EngineConfig,
    out: &mut Vec<HorizonHypothesis>,
) {
    run_provider(&SegmentationProvider { cfg }, ctx, out);
}

#[cfg(not(feature = "segmentation"))]
fn run_segmentation(
    _ctx: &HorizonProviderContext<'_>,
    _cfg: &EngineConfig,
    _out: &mut Vec<HorizonHypothesis>,
) {
}

#[cfg(feature = "ml-gravity")]
fn run_ml_gravity(
    ctx: &HorizonProviderContext<'_>,
    cfg: &EngineConfig,
    out: &mut Vec<HorizonHypothesis>,
    stats: &mut StageCStats,
) {
    if !cfg.enable_ml_gravity {
        return;
    }
    if !bris_vision::horizon_providers::ml_gravity::is_loaded() {
        return;
    }
    stats.ml_gravity.invoked = true;
    let provider = bris_vision::MlGravityProvider::new(cfg.ml_gravity_provider_config.clone());
    if let Some(hyp) = provider.detect_with_stats(ctx, &mut stats.ml_gravity.stats) {
        trace!(
            provider = "ml-gravity",
            sigma_rad = hyp.line.altitude_sigma.value(),
            inference_ms = stats.ml_gravity.stats.inference_ms,
            "Stage C: provider produced hypothesis"
        );
        stats.ml_gravity.hypothesized = true;
        out.push(hyp);
    }
}

#[cfg(not(feature = "ml-gravity"))]
fn run_ml_gravity(
    _ctx: &HorizonProviderContext<'_>,
    _cfg: &EngineConfig,
    _out: &mut Vec<HorizonHypothesis>,
    _stats: &mut StageCStats,
) {
}

fn detector_from_provenance(p: HorizonProvenance) -> HorizonDetector {
    match p {
        HorizonProvenance::Optical(kind) => optical_kind_to_detector(kind),
        HorizonProvenance::ReflectionPair { .. } => HorizonDetector::ReflectionPair,
        HorizonProvenance::VerticalLine { .. } => HorizonDetector::VerticalLine,
        HorizonProvenance::VanishingPoint { .. } => HorizonDetector::VanishingPoint,
        HorizonProvenance::Fused { .. } => HorizonDetector::Fused,
        HorizonProvenance::MlGravity { .. } => HorizonDetector::MlGravity,
    }
}

fn finish(
    hypotheses: &[HorizonHypothesis],
    intrinsics: &Intrinsics,
    image_width: u32,
    cfg: &EngineConfig,
    analyzed_size: (u32, u32),
    mut stats: StageCStats,
) -> (HorizonStageOutcome, (u32, u32), StageCStats) {
    let (outcome, fusion) = fuse_to_outcome(hypotheses, intrinsics, image_width, cfg);
    // Mark which optional providers actually won.
    if let HorizonStageOutcome::Detected { detector, .. } = &outcome {
        if *detector == HorizonDetector::VerticalLine {
            stats.vertical_line.used = true;
        }
        if *detector == HorizonDetector::VanishingPoint {
            stats.vanishing_point.used = true;
        }
        if *detector == HorizonDetector::MlGravity {
            stats.ml_gravity.used = true;
        }
    }
    stats.fusion = fusion;
    (outcome, analyzed_size, stats)
}

/// Run the fuser and translate its result into the engine's
/// [`HorizonStageOutcome`] + [`FusionStats`].
fn fuse_to_outcome(
    hypotheses: &[HorizonHypothesis],
    intrinsics: &Intrinsics,
    image_width: u32,
    cfg: &EngineConfig,
) -> (HorizonStageOutcome, FusionStats) {
    if hypotheses.is_empty() {
        return (HorizonStageOutcome::None, FusionStats::default());
    }
    let fused = fuse_horizon_hypotheses(hypotheses, intrinsics, image_width, &cfg.horizon_fusion);
    let Some(hyp) = fused.hypothesis else {
        return (HorizonStageOutcome::None, FusionStats::default());
    };
    let detector = detector_from_provenance(hyp.provenance);
    let mut direct_sights = fused.direct_sights;
    if direct_sights.is_empty() {
        if let Some(ds) = hyp.direct_sight {
            direct_sights.push(ds);
        }
    }
    let stats = FusionStats {
        cluster_size: fused.cluster_size,
        clustered: fused.mode == FusionMode::Clustered,
        discordant: fused.mode == FusionMode::Discordant,
        singleton: matches!(fused.mode, FusionMode::Singleton | FusionMode::Disabled)
            && hypotheses.len() == 1,
    };
    (
        HorizonStageOutcome::Detected {
            detector,
            provenance: hyp.provenance,
            line: hyp.line,
            direct_sights,
        },
        stats,
    )
}

/// Merge the reflection-pair provider's hypothesis into a
/// previously-computed Stage C outcome.
pub(crate) struct ReflectionPairMerge {
    pub outcome: HorizonStageOutcome,
    pub stats: bris_vision::ReflectionPairStats,
    pub hypothesized: bool,
    pub used: bool,
    pub fusion: FusionStats,
}

pub(crate) fn merge_reflection_pair(
    prev: HorizonStageOutcome,
    ctx: &HorizonProviderContext<'_>,
    cfg: &EngineConfig,
) -> ReflectionPairMerge {
    let mut stats = bris_vision::ReflectionPairStats::default();
    let provider = bris_vision::ReflectionPairProvider::default();
    let Some(hyp) = provider.detect_with_stats(ctx, &mut stats) else {
        let fusion = FusionStats {
            singleton: matches!(prev, HorizonStageOutcome::Detected { .. }),
            ..FusionStats::default()
        };
        return ReflectionPairMerge {
            outcome: prev,
            stats,
            hypothesized: false,
            used: false,
            fusion,
        };
    };
    // Build a hypotheses list from prev (if any) + the new
    // reflection-pair hypothesis. `prev` may already be a fused
    // result; we treat it as a single "best-evidence" hypothesis
    // with its own σ and provenance.
    let mut hypotheses: Vec<HorizonHypothesis> = Vec::with_capacity(2);
    if let HorizonStageOutcome::Detected {
        line,
        direct_sights,
        provenance,
        ..
    } = &prev
    {
        hypotheses.push(HorizonHypothesis {
            line: *line,
            provenance: *provenance,
            direct_sight: direct_sights.first().copied(),
        });
    }
    let rp_sigma = hyp.line.altitude_sigma.value();
    hypotheses.push(hyp);
    let (outcome, fusion) = fuse_to_outcome(&hypotheses, ctx.intrinsics, ctx.frame.width(), cfg);
    let used = match &outcome {
        HorizonStageOutcome::Detected { detector, line, .. } => {
            *detector == HorizonDetector::ReflectionPair
                || *detector == HorizonDetector::Fused
                || line.altitude_sigma.value() <= rp_sigma
        }
        HorizonStageOutcome::None => false,
    };
    ReflectionPairMerge {
        outcome,
        stats,
        hypothesized: true,
        used,
        fusion,
    }
}

/// Cheap copy-on-write wrapper for the analysis-resolution frame.
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
        let cfg = EngineConfig::new(Observer::default_dev());
        let frame = synthetic_horizon(32);
        let (outcome, _, _) = detect(
            &FramePyramid::new(frame.clone()),
            Condition::Day,
            &cfg,
            &[],
            None,
            frame.capture_tt,
        );
        match outcome {
            HorizonStageOutcome::Detected { detector, line, .. } => {
                assert!(
                    matches!(
                        detector,
                        HorizonDetector::Gradient
                            | HorizonDetector::SkyRegion
                            | HorizonDetector::Fused
                    ),
                    "expected a day-path detector or fused, got {detector:?}"
                );
                assert!(
                    (line.intercept - 32.0).abs() < 5.0,
                    "horizon intercept {} too far from synthetic 32",
                    line.intercept
                );
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
        let (outcome, _, _) = detect(
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
        let cfg = EngineConfig::new(Observer::default_dev());
        let frame = dark_frame();
        let (outcome, _, _) = detect(
            &FramePyramid::new(frame.clone()),
            Condition::Night,
            &cfg,
            &[],
            None,
            frame.capture_tt,
        );
        match outcome {
            HorizonStageOutcome::Detected { detector, .. } => {
                assert!(
                    matches!(
                        detector,
                        HorizonDetector::Night
                            | HorizonDetector::NightTextured
                            | HorizonDetector::VerticalLine
                            | HorizonDetector::VanishingPoint
                            | HorizonDetector::Fused
                    ),
                    "night classification should not select day detectors, got {detector:?}"
                );
            }
            HorizonStageOutcome::None => {
                // Acceptable: textureless dark frame.
            }
        }
    }

    #[test]
    fn segmentation_skipped_when_path_unset() {
        let cfg = EngineConfig::new(Observer::default_dev());
        assert!(
            cfg.segmentation_model_path.is_none(),
            "default config should not enable segmentation"
        );
    }

    /// Build a textureless dark frame with a single bright
    /// near-vertical stripe. Gradient / sky-region / night /
    /// night-textured / vanishing-point providers all decline
    /// on this geometry; only the vertical-line provider has
    /// historically returned a hypothesis. Used to prove that
    /// disabling the flag actually removes that silent-winner
    /// path.
    fn vertical_stripe_frame() -> Frame {
        let w = 128_u32;
        let h = 96_u32;
        let mut pixels = vec![0_u16; (w * h) as usize];
        let cx = (w / 2) as i32;
        for y in 0..h {
            for dx in -1_i32..=1_i32 {
                let x = cx + dx;
                if x >= 0 && (x as u32) < w {
                    pixels[(y as usize) * (w as usize) + (x as usize)] = 60_000;
                }
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

    #[test]
    fn vertical_line_provider_off_by_default() {
        let cfg = EngineConfig::new(Observer::default_dev());
        assert!(
            !cfg.enable_vertical_line_provider,
            "default EngineConfig must leave the vertical-line provider disabled"
        );
        let frame = vertical_stripe_frame();
        let (_outcome, _, stats) = detect(
            &FramePyramid::new(frame.clone()),
            Condition::Night,
            &cfg,
            &[],
            None,
            frame.capture_tt,
        );
        assert!(
            !stats.vertical_line.invoked,
            "Stage C must not invoke VerticalLineProvider when the flag is false"
        );
        assert!(
            !stats.vertical_line.hypothesized,
            "vertical_line.hypothesized must be false when provider not invoked"
        );
        assert!(
            !stats.vertical_line.used,
            "vertical_line.used must be false when provider not invoked"
        );
        assert_eq!(
            stats.vertical_line.stats.hypothesized, 0,
            "per-call hypothesized counter must stay zero when provider not invoked"
        );
    }

    #[test]
    fn vertical_line_provider_on_when_flag_set() {
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.enable_vertical_line_provider = true;
        let frame = vertical_stripe_frame();
        let (_outcome, _, stats) = detect(
            &FramePyramid::new(frame.clone()),
            Condition::Night,
            &cfg,
            &[],
            None,
            frame.capture_tt,
        );
        assert!(
            stats.vertical_line.invoked,
            "Stage C must invoke VerticalLineProvider when the flag is true"
        );
    }

    /// Load-bearing test: with only a vertical stripe in the
    /// scene, the disabled engine returns *no* horizon (rather
    /// than the historically-wrong gravity-from-line one).
    /// Flipping the flag back on restores the prior behavior:
    /// the vertical-line provider produces a hypothesis.
    #[test]
    fn vertical_stripe_only_yields_no_horizon_when_disabled() {
        let cfg_off = EngineConfig::new(Observer::default_dev());
        let frame = vertical_stripe_frame();
        let (outcome_off, _, _) = detect(
            &FramePyramid::new(frame.clone()),
            Condition::Night,
            &cfg_off,
            &[],
            None,
            frame.capture_tt,
        );
        assert!(
            matches!(outcome_off, HorizonStageOutcome::None),
            "with only a vertical stripe in frame, disabled Stage C must \
             emit no horizon (got {outcome_off:?})"
        );

        let mut cfg_on = EngineConfig::new(Observer::default_dev());
        cfg_on.enable_vertical_line_provider = true;
        let (outcome_on, _, stats_on) = detect(
            &FramePyramid::new(frame.clone()),
            Condition::Night,
            &cfg_on,
            &[],
            None,
            frame.capture_tt,
        );
        assert!(
            stats_on.vertical_line.hypothesized,
            "vertical-stripe frame must yield a hypothesis when the provider is enabled"
        );
        assert!(
            matches!(
                outcome_on,
                HorizonStageOutcome::Detected {
                    detector: HorizonDetector::VerticalLine,
                    ..
                }
            ),
            "enabled Stage C on a vertical-stripe-only frame must select \
             the VerticalLine detector (got {outcome_on:?})"
        );
    }
}
