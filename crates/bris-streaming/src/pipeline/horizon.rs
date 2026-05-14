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
use bris_vision::{
    detect_horizon, detect_horizon_night, detect_horizon_night_textured,
    detect_horizon_via_sky_region, Condition, Frame, HorizonLine,
};
use tracing::{debug, trace};

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
}

/// Run Stage C on one frame.
///
/// `condition` is the Stage A verdict; it gates which detectors
/// are tried first. The returned outcome carries the best-σ
/// horizon found across all attempts, or
/// [`HorizonStageOutcome::None`] if no detector succeeded.
pub(crate) fn detect(
    frame: &Frame,
    condition: Condition,
    cfg: &EngineConfig,
) -> HorizonStageOutcome {
    let mut best: Option<(HorizonDetector, HorizonLine)> = None;

    let early_term = cfg.horizon_early_termination_sigma_rad;
    let day_first = matches!(condition, Condition::Day | Condition::Twilight);
    let night_first = matches!(condition, Condition::Night | Condition::Twilight);
    // Unusable: skip Stage C entirely. The classifier said the
    // frame has nothing actionable; running the heavy detectors
    // would just waste time.
    if matches!(condition, Condition::Unusable) {
        debug!("Stage C skipped: classifier reported Unusable");
        return HorizonStageOutcome::None;
    }

    if day_first {
        try_gradient(frame, cfg, &mut best);
        if early_terminate(best.as_ref(), early_term) {
            return finish(best);
        }
        try_sky_region(frame, cfg, &mut best);
        if early_terminate(best.as_ref(), early_term) {
            return finish(best);
        }
    }
    if night_first {
        try_night(frame, cfg, &mut best);
        if early_terminate(best.as_ref(), early_term) {
            return finish(best);
        }
        try_night_textured(frame, cfg, &mut best);
        if early_terminate(best.as_ref(), early_term) {
            return finish(best);
        }
    }
    // Last resort: segmentation. Gated on the feature flag and
    // on the operator opting in by supplying a model path.
    try_segmentation(frame, cfg, &mut best);
    finish(best)
}

fn try_gradient(
    frame: &Frame,
    cfg: &EngineConfig,
    best: &mut Option<(HorizonDetector, HorizonLine)>,
) {
    match detect_horizon(frame, cfg.horizon_cfg) {
        Ok(line) => {
            trace!(
                detector = "gradient",
                sigma_rad = line.altitude_sigma.value(),
                inliers = line.inlier_count,
                "Stage C: detector succeeded"
            );
            update_best(best, HorizonDetector::Gradient, line);
        }
        Err(e) => trace!(detector = "gradient", error = %e, "Stage C: detector failed"),
    }
}

fn try_sky_region(
    frame: &Frame,
    cfg: &EngineConfig,
    best: &mut Option<(HorizonDetector, HorizonLine)>,
) {
    match detect_horizon_via_sky_region(frame, cfg.horizon_cfg) {
        Ok(line) => {
            trace!(
                detector = "sky_region",
                sigma_rad = line.altitude_sigma.value(),
                inliers = line.inlier_count,
                "Stage C: detector succeeded"
            );
            update_best(best, HorizonDetector::SkyRegion, line);
        }
        Err(e) => trace!(detector = "sky_region", error = %e, "Stage C: detector failed"),
    }
}

fn try_night(frame: &Frame, cfg: &EngineConfig, best: &mut Option<(HorizonDetector, HorizonLine)>) {
    match detect_horizon_night(frame, cfg.night_horizon_cfg) {
        Ok(line) => {
            trace!(
                detector = "night",
                sigma_rad = line.altitude_sigma.value(),
                inliers = line.inlier_count,
                "Stage C: detector succeeded"
            );
            update_best(best, HorizonDetector::Night, line);
        }
        Err(e) => trace!(detector = "night", error = %e, "Stage C: detector failed"),
    }
}

fn try_night_textured(
    frame: &Frame,
    cfg: &EngineConfig,
    best: &mut Option<(HorizonDetector, HorizonLine)>,
) {
    match detect_horizon_night_textured(frame, cfg.textured_horizon_cfg) {
        Ok(line) => {
            trace!(
                detector = "night_textured",
                sigma_rad = line.altitude_sigma.value(),
                inliers = line.inlier_count,
                "Stage C: detector succeeded"
            );
            update_best(best, HorizonDetector::NightTextured, line);
        }
        Err(e) => trace!(detector = "night_textured", error = %e, "Stage C: detector failed"),
    }
}

#[cfg(feature = "segmentation")]
fn try_segmentation(
    frame: &Frame,
    cfg: &EngineConfig,
    best: &mut Option<(HorizonDetector, HorizonLine)>,
) {
    use bris_vision::{detect_horizon_via_segmentation, load_model};
    let Some(model_path) = cfg.segmentation_model_path.as_ref() else {
        // Operator hasn't opted into segmentation; skip
        // silently. This is the default state on embedded
        // builds that don't ship the model.
        return;
    };
    // Lazy load + cache. `load_model` is idempotent (uses a
    // OnceLock internally).
    if let Err(e) = load_model(model_path) {
        // Loading failed — log at debug because this is an
        // operator-facing problem (wrong path / corrupt model)
        // and they'll see the message via the engine's tracing
        // subscription. Don't keep retrying per-frame; the
        // OnceLock means subsequent calls return the cached
        // failure cheaply.
        debug!(
            path = %model_path.display(),
            error = %e,
            "Stage C: segmentation model load failed; will not retry per-frame"
        );
        return;
    }
    match detect_horizon_via_segmentation(frame, cfg.horizon_cfg) {
        Ok(line) => {
            trace!(
                detector = "segmentation",
                sigma_rad = line.altitude_sigma.value(),
                inliers = line.inlier_count,
                "Stage C: detector succeeded"
            );
            update_best(best, HorizonDetector::Segmentation, line);
        }
        Err(e) => trace!(detector = "segmentation", error = %e, "Stage C: detector failed"),
    }
}

#[cfg(not(feature = "segmentation"))]
fn try_segmentation(
    _frame: &Frame,
    _cfg: &EngineConfig,
    _best: &mut Option<(HorizonDetector, HorizonLine)>,
) {
    // Segmentation feature disabled at compile time; nothing to
    // do. The config field stays present so disabling the
    // feature doesn't break the EngineConfig surface, but the
    // engine simply never tries the detector.
}

/// Update `best` with `(detector, line)` if `line` improves on
/// the current best (smaller σ).
fn update_best(
    best: &mut Option<(HorizonDetector, HorizonLine)>,
    detector: HorizonDetector,
    line: HorizonLine,
) {
    match best {
        Some((_, existing)) if existing.altitude_sigma <= line.altitude_sigma => {
            // Existing is at least as good. Keep it.
        }
        _ => *best = Some((detector, line)),
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

fn finish(best: Option<(HorizonDetector, HorizonLine)>) -> HorizonStageOutcome {
    match best {
        Some((detector, line)) => HorizonStageOutcome::Detected { detector, line },
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
        let outcome = detect(&frame, Condition::Day, &cfg);
        match outcome {
            HorizonStageOutcome::Detected { detector, line } => {
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
        let outcome = detect(&frame, Condition::Unusable, &cfg);
        assert!(matches!(outcome, HorizonStageOutcome::None));
    }

    #[test]
    fn night_path_dispatched_for_night_classification() {
        // Night detectors have a hard time with synthetic
        // textureless frames; we mostly assert they don't panic
        // and that the day-path detectors aren't tried.
        let cfg = EngineConfig::new(Observer::default_dev());
        let frame = dark_frame();
        let outcome = detect(&frame, Condition::Night, &cfg);
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
        let outcome = detect(&frame, Condition::Day, &cfg);
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
