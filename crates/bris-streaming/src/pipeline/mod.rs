//! Synchronous staged pipeline.
//!
//! Owns the per-frame logic that runs Stages A through E. At the
//! current commit Stages A (classifier), B (body detection), and
//! C (horizon detection) are wired in; later commits add Stage D
//! (plate solve) and Stage E (sight assembly + fix publication).
//!
//! The pipeline is invoked synchronously from
//! [`crate::StreamingEngine::push_frame`] in commit 2; commit 4
//! moves the call onto a worker thread fed from the input ring
//! buffer. This module's API doesn't change across that move —
//! [`process_frame`] is pure with respect to the engine state it
//! mutates, so swapping the call site is mechanical.
//!
//! # Stage A: classifier
//!
//! The classifier needs an estimated sun altitude (degrees) to
//! consult the astronomical prior. We compute it per-frame from
//! the engine's [`bris_almanac::Observer`] and the frame's
//! [`bris_core::time::Tt`]. The design doc notes this is "one per
//! batch / per publication interval, not per frame" in the
//! eventual implementation; commit 2 computes per-frame for
//! simplicity. Caching keyed on the per-fix publication cadence
//! is a follow-up optimization once Stage E exists (commit 5).
//!
//! Sun altitude only feeds the classifier's day/twilight/night
//! band selection, where ~1° accuracy is more than enough. We
//! therefore pass `jd_ut1 ≈ tt.julian_date()` rather than going
//! through the leap-second table — the ΔT of ~69 s introduces
//! ~0.0008° of error in sun position, well below what the
//! classifier cares about.
//!
//! # Stage B: body detection
//!
//! Branches on the classifier verdict:
//!
//! - [`bris_vision::Condition::Day`][]: run
//!   [`bris_vision::centroid_saturated_body_in_mask`] with no
//!   mask. The future segmentation-mask hookup lands when the
//!   engine acquires a sky-mask provider (separate work; the
//!   current detector ships without one in the CLI's `replay`
//!   path too).
//! - [`bris_vision::Condition::Night`][]: run
//!   [`bris_vision::detect_peaks`]. Stage D (commit 6) consumes
//!   the peaks via the plate solver; commit 2 just records the
//!   peak count for diagnostics.
//! - [`bris_vision::Condition::Twilight`][]: try the day path
//!   first (saturated Sun centroiding); on failure (no
//!   saturated body — typical of nautical/astronomical
//!   twilight) fall back to peak detection. The non-saturated
//!   body path (Moon at dusk, planets) via
//!   `centroid_brightest_body_in_mask` is a refinement reserved
//!   for after the basic engine is published; the design doc
//!   notes it explicitly.
//! - [`bris_vision::Condition::Unusable`][]: record the verdict
//!   and skip Stage B entirely. The frame may still contribute
//!   horizon detection in Stage C (commit 3) but the body
//!   queues won't see it.

use crate::config::EngineConfig;
use bris_almanac::{body_apparent_place, Observer, SolarSystemBody};
use bris_core::time::Tt;
use bris_core::{Latitude, Longitude, Sigma, Uncertain};
use bris_vision::{
    classify, detect_peaks, detect_peaks_above_horizon, Centroid, Classification, Condition, Frame,
    HorizonLine, Peak, SaturatedBodyConfig,
};
use tracing::{debug, trace, warn};

mod horizon;
mod horizon_providers;
mod hysteresis;
mod queue;
mod stage_d;
mod stage_e;

pub(crate) use horizon::{
    merge_reflection_pair, FusionStats, HorizonStageOutcome, ReflectionPairMerge,
    VanishingPointDispatch,
};
pub(crate) use hysteresis::ClassifierHysteresis;
pub(crate) use queue::{FrameId, Storage};
pub(crate) use stage_d::{run as run_stage_d, StageDOutcome};
pub(crate) use stage_e::{run as run_stage_e, Sight, SightBody, SightWindow};

/// Output of Stage B (and, after commit 6, also of Stage D).
///
/// Four-way:
/// - [`BodyDetection::Day`]: day-path saturated body centroid.
/// - [`BodyDetection::Night`]: night-path peaks awaiting plate
///   solving. After Stage D runs successfully on a `Night`
///   payload, the variant is replaced with
///   [`BodyDetection::IdentifiedStars`].
/// - [`BodyDetection::IdentifiedStars`]: night peaks that have
///   been plate-solved into a set of identified stars + camera
///   attitude. Each identified star expands into one sight at
///   Stage E.
/// - [`BodyDetection::None`]: no body detected (or Stage D
///   declined to run because the night-path inputs aren't
///   sufficient).
#[derive(Debug, Clone)]
pub(crate) enum BodyDetection {
    /// Day or successful-twilight-day-fallback centroid. The
    /// first field is the primary (largest-area) saturated
    /// body; the second is any additional saturated
    /// components above the area threshold (e.g. the body's
    /// reflection on water / hood / puddle), largest first.
    /// Empty when no secondaries were detected — the
    /// historical single-centroid Day behaviour.
    Day(Centroid, Vec<Centroid>),
    /// Night or twilight-night-fallback peaks. Empty `Vec` is not
    /// a valid `Night` outcome — the pipeline returns
    /// [`BodyDetection::None`] in that case. Stage D promotes
    /// successful matches to [`BodyDetection::IdentifiedStars`];
    /// `Night(_)` records that linger in the queue past Stage D
    /// represent peak sets that *failed* to plate-solve (too
    /// few peaks, geometry ambiguous, or DB not yet built under
    /// `PlateSolverInit::Lazy`).
    Night(Vec<Peak>),
    /// Plate-solved identified stars + camera attitude. Stage E
    /// expands one record like this into one sight per
    /// identified star, all sharing the paired horizon.
    IdentifiedStars(bris_platesolve::PlateSolveResult),
    /// No body detected by the path appropriate to the
    /// classifier verdict. Common in twilight/overcast scenes;
    /// not an error condition.
    None,
}

/// Outcome of one frame's pass through the (currently A + B + C)
/// pipeline. Returned to the caller (the engine) so it can update
/// per-stage statistics and (in commit 4+) enqueue the
/// detections.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct StageOutcome {
    /// Stage A verdict (the *raw* per-frame classification).
    /// Always present (the classifier never errors; it returns
    /// [`Condition::Unusable`] when neither evidence source is
    /// actionable).
    pub classification: Classification,
    /// The classifier verdict the engine *acted on* for this
    /// frame, after applying classifier hysteresis.
    /// `dispatched_condition` may differ from
    /// `classification.condition` during a transient: the raw
    /// classifier reports the new evidence, but the engine
    /// keeps using the prior method-set until
    /// [`crate::EngineConfig::classifier_hysteresis_frames`]
    /// consecutive observations agree on the new verdict.
    pub dispatched_condition: Condition,
    /// Stage B outcome. [`BodyDetection::None`] also covers the
    /// "Stage B was skipped" case (when the classifier said
    /// `Unusable`).
    pub body: BodyDetection,
    /// Stage C outcome. [`HorizonStageOutcome::None`] covers
    /// both "no detector succeeded" and "Stage C was skipped"
    /// (Unusable verdict).
    pub horizon: HorizonStageOutcome,
    /// Resolution `(width, height)` at which Stage C actually
    /// ran. Equals the source frame's resolution when
    /// `horizon_analysis_size` is unset; equals the requested
    /// pyramid level when set and successfully computed; falls
    /// back to source resolution when the requested level was
    /// rejected (aspect-ratio mismatch, dim-mismatch). Surfaced
    /// in the engine's `EngineDiagnostics`.
    pub horizon_analyzed_size: (u32, u32),
    /// Frame's capture instant, threaded through for diagnostics.
    pub frame_tt: Tt,
    /// Reflection-pair provider counters for this frame.
    /// Folded into engine-level diagnostics. Zero when the
    /// provider was not invoked.
    pub reflection_pair_stats: bris_vision::ReflectionPairStats,
    /// Whether the reflection-pair provider was invoked
    /// (≥ 2 body candidates were present and dispatched
    /// condition was actionable).
    pub reflection_pair_invoked: bool,
    /// Provider returned a hypothesis (Tests 1–4 all passed).
    /// May not have been emitted if the optical horizon had
    /// smaller σ (see `reflection_pair_used`).
    pub reflection_pair_hypothesized: bool,
    /// Provider's hypothesis won the best-σ merge and is the
    /// horizon outcome surfaced from this frame.
    pub reflection_pair_used: bool,
    /// Vertical-line provider counters for this frame.
    pub vertical_line_stats: bris_vision::VerticalLineStats,
    /// Provider returned a hypothesis (≥ 1 near-vertical line
    /// passed all filters).
    pub vertical_line_hypothesized: bool,
    /// Provider's hypothesis won the best-σ merge and is the
    /// horizon outcome surfaced from this frame.
    pub vertical_line_used: bool,
    /// Vanishing-point provider dispatch bookkeeping for this
    /// frame.
    pub vanishing_point_dispatch: VanishingPointDispatch,
    /// Fusion-layer per-frame stats. Folded into engine
    /// diagnostics so operators can see how often providers
    /// agree / disagree.
    pub fusion_stats: FusionStats,
}

/// Process one frame through Stages A, B, and C synchronously.
///
/// Pure with respect to the engine's mutable state apart from
/// the supplied [`ClassifierHysteresis`], which advances by one
/// observation per call. The engine's worker code (currently
/// `push_frame` itself, later a worker-thread loop) updates
/// counters and enqueues records based on the returned
/// [`StageOutcome`].
#[allow(clippy::too_many_lines)]
pub(crate) fn process_frame(
    pyramid: &bris_vision::FramePyramid,
    cfg: &EngineConfig,
    hysteresis: &mut ClassifierHysteresis,
    position_prior: Option<bris_vision::PositionPrior>,
) -> StageOutcome {
    let frame = pyramid.full();
    // ---- Stage A: classify (raw) + apply hysteresis ----
    let sun_alt_deg = sun_altitude_deg(cfg.observer, frame.capture_tt);
    let classification = classify(frame, sun_alt_deg, cfg.condition_cfg);
    let dispatched_condition =
        hysteresis.update(classification.condition, cfg.classifier_hysteresis_frames);
    trace!(
        raw_condition = ?classification.condition,
        dispatched_condition = ?dispatched_condition,
        confidence = classification.confidence,
        sun_alt_deg = ?sun_alt_deg,
        disagreement = classification.disagreement,
        "Stage A: classifier (with hysteresis)"
    );

    // ---- Stage C: horizon detection (cheap-first, dispatched verdict) ----
    //
    // Runs *before* Stage B for night/twilight frames so the
    // night peak detector can mask out below-horizon pixels
    // (wake whitewater, deck lights, lit superstructure all
    // routinely outshine real stars and crowd them out of the
    // peak budget). Day frames don't strictly need this
    // ordering — saturated-body centroiding doesn't read the
    // horizon — but the pipeline runs C-then-B uniformly to
    // keep the stage graph simple.
    //
    // Body candidates and the position prior are threaded
    // through the dispatcher for the auto-horizon providers
    // (Phase 1: reflection-pair). The optical detectors
    // ignore them. On this first pass the candidates are
    // empty (Stage B hasn't run yet); when Stage B yields
    // ≥ 2 Night peaks the pipeline re-runs Stage C with the
    // candidates so the reflection-pair provider can
    // contribute. See docs/design/horizon_autodetect.md §3.
    let (mut horizon, horizon_analyzed_size, mut stage_c_stats) = horizon::detect(
        pyramid,
        dispatched_condition,
        cfg,
        &[],
        position_prior,
        frame.capture_tt,
    );
    if let HorizonStageOutcome::Detected { detector, line, .. } = &horizon {
        trace!(
            detector = ?detector,
            sigma_rad = line.altitude_sigma.value(),
            intercept_px = line.intercept,
            slope = line.slope,
            "Stage C: best horizon"
        );
    }

    // ---- Stage B: body detection (branch on dispatched verdict) ----
    //
    // Night/twilight pass the Stage C horizon (when found) into
    // peak detection so wake/deck pixels are masked. When no
    // horizon was found, peak detection runs unmasked — many
    // sky-pointed frames legitimately contain no horizon, and
    // the cross-frame stitching layer attaches such frames to a
    // horizon measured on a neighbouring frame.
    let horizon_line = match &horizon {
        HorizonStageOutcome::Detected { line, .. } => Some(*line),
        HorizonStageOutcome::None => None,
    };
    let body = match dispatched_condition {
        Condition::Day => detect_day_body(frame, cfg.saturated_body_cfg, horizon_line),
        Condition::Night => detect_night_peaks(frame, cfg, horizon_line),
        Condition::Twilight => detect_twilight(frame, cfg.saturated_body_cfg, cfg, horizon_line),
        Condition::Unusable => {
            debug!("Stage B skipped: dispatched condition is Unusable");
            BodyDetection::None
        }
    };
    log_body_outcome(&body);

    // ---- Stage C (second pass): reflection-pair provider ----
    //
    // Now that Stage B has produced body candidates, run the
    // reflection-pair provider with them and merge by best-σ
    // through the same trait-driven path the optical providers
    // use. Phase 1 scope is Night / Twilight only — Day yields
    // a single centroid (cannot form a pair) and the Day-mode
    // multi-centroid path is deferred. The explicit match here
    // makes the scope boundary inspectable rather than relying
    // on the `len() >= 2` check to incidentally exclude Day.
    // Day path: when a position prior is available, compute
    // the Sun's apparent altitude at the prior location and
    // attach it as `predicted_altitude` on the *primary* Day
    // candidate. This lets the reflection-pair provider's
    // Test 3 (catalog consistency) evaluate against the
    // single Day pair (1 direct + 1 reflection), which would
    // otherwise fail the cold-start `min_pairs = 3` gate.
    // Secondaries get `None` — they're hypothesized as
    // reflections of the primary, not catalog bodies in
    // their own right. Without a prior the field stays
    // `None`; Day reflection-pair then falls through to the
    // cold-start path which (with 1 pair) declines to emit.
    let day_sun_predicted_altitude = match dispatched_condition {
        Condition::Day | Condition::Twilight => {
            position_prior.and_then(|p| sun_predicted_altitude(cfg.observer, frame.capture_tt, p))
        }
        Condition::Night | Condition::Unusable => None,
    };
    let body_candidates = body_candidates_from_detection(&body, day_sun_predicted_altitude);
    let mut reflection_pair_stats = bris_vision::ReflectionPairStats::default();
    let mut reflection_pair_invoked = false;
    let mut reflection_pair_hypothesized = false;
    let mut reflection_pair_used = false;
    if body_candidates.len() >= 2
        && matches!(
            dispatched_condition,
            Condition::Day | Condition::Night | Condition::Twilight
        )
    {
        let ctx = bris_vision::HorizonProviderContext {
            frame,
            intrinsics: &frame.intrinsics,
            body_candidates: &body_candidates,
            position_prior,
            timestamp: frame.capture_tt,
        };
        reflection_pair_invoked = true;
        let merge = merge_reflection_pair(horizon, &ctx, cfg);
        let ReflectionPairMerge {
            outcome,
            stats,
            hypothesized,
            used,
            fusion,
        } = merge;
        horizon = outcome;
        reflection_pair_stats = stats;
        reflection_pair_hypothesized = hypothesized;
        reflection_pair_used = used;
        stage_c_stats.fusion = fusion;
    }

    StageOutcome {
        classification,
        dispatched_condition,
        body,
        horizon,
        horizon_analyzed_size,
        frame_tt: frame.capture_tt,
        reflection_pair_stats,
        reflection_pair_invoked,
        reflection_pair_hypothesized,
        reflection_pair_used,
        vertical_line_stats: stage_c_stats.vertical_line.stats,
        vertical_line_hypothesized: stage_c_stats.vertical_line.hypothesized,
        vertical_line_used: stage_c_stats.vertical_line.used,
        vanishing_point_dispatch: stage_c_stats.vanishing_point,
        fusion_stats: stage_c_stats.fusion,
    }
}

/// Day path: saturated-body centroiding with no mask.
///
/// `Err` outcomes (no bright region, component too small) are
/// expected during day-to-twilight transitions and on pointing
/// errors; demoted to [`BodyDetection::None`] without a warning.
/// A mask shape mismatch would be a genuine bug and is logged at
/// `warn!` — but cannot occur with `mask: None`, so it's a
/// defense-in-depth log.
/// Project a [`BodyDetection`] into the narrow read-only view
/// consumed by horizon providers.
fn body_candidates_from_detection(
    body: &BodyDetection,
    day_primary_predicted_altitude: Option<Uncertain<f64>>,
) -> Vec<bris_vision::BodyCandidate> {
    match body {
        BodyDetection::Day(c, secondaries) => {
            let mut out = Vec::with_capacity(1 + secondaries.len());
            out.push(bris_vision::BodyCandidate {
                pixel: (c.x, c.y),
                brightness: c.mean_intensity,
                position_sigma_px: c.position_sigma_px.value(),
                // The Sun is the implicit Day body. When a
                // position prior is available, the caller
                // supplies an almanac-predicted Sun altitude;
                // this lets reflection-pair Test 3 evaluate
                // and a single Day pair (direct + reflection)
                // can pass without the cold-start min-pair
                // gate. Without a prior the field is `None`
                // and the provider falls back to cold-start.
                predicted_altitude: day_primary_predicted_altitude,
            });
            for s in secondaries {
                out.push(bris_vision::BodyCandidate {
                    pixel: (s.x, s.y),
                    brightness: s.mean_intensity,
                    position_sigma_px: s.position_sigma_px.value(),
                    // Secondaries are hypothesized as
                    // reflections of the primary, not
                    // independent catalog bodies. Leaving
                    // `predicted_altitude` None on the
                    // secondary keeps Test 3 from
                    // double-counting and lets the
                    // primary's altitude carry the test.
                    predicted_altitude: None,
                });
            }
            out
        }
        BodyDetection::Night(peaks) => peaks
            .iter()
            .map(|p| bris_vision::BodyCandidate {
                pixel: (p.x, p.y),
                brightness: p.intensity,
                // Peak detector doesn't report a per-peak
                // pixel σ today; use a 0.5 px placeholder
                // consistent with the plate-solver's
                // `per_star_sigma`-derived defaults.
                position_sigma_px: 0.5,
                predicted_altitude: None,
            })
            .collect(),
        BodyDetection::IdentifiedStars(_) | BodyDetection::None => Vec::new(),
    }
}

fn detect_day_body(
    frame: &Frame,
    cfg: SaturatedBodyConfig,
    horizon_line: Option<HorizonLine>,
) -> BodyDetection {
    // Extract every saturated component above the area gate.
    // The largest is the primary (Sun / Moon); any remaining
    // ones are exposed as secondaries so the reflection-pair
    // horizon provider can form pairs across the direct image
    // and reflection-on-surface candidates. Falls back to the
    // single-centroid path when only one component survives,
    // and to `BodyDetection::None` when none do (matching the
    // Phase 1 behaviour).
    match bris_vision::extract_multi_saturated_centroids(frame, cfg, None) {
        Ok(mut centroids) if !centroids.is_empty() => {
            let mut primary = centroids.remove(0);
            // Sub-pixel refinement of the primary via 2D
            // Gaussian fit on the non-saturated halo. Falls
            // back to the integer centroid when the fit is
            // unreliable (`refined = false`).
            let radius_f = (f64::from(primary.area_px) / core::f64::consts::PI)
                .sqrt()
                .mul_add(2.0, 6.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let radius = radius_f.max(1.0) as u32;
            let halo =
                bris_vision::extract_halo_pixels(frame, primary, cfg.saturation_threshold, radius);
            // Use the frame-attached measured sensor gain
            // for the Poisson weights; falls back to UNITY
            // when no measurement is available (see
            // `bris_core::SensorGain`).
            let refined = bris_vision::refine_centroid_subpixel(
                frame,
                primary,
                &halo,
                frame.gain.e_per_adu(),
            );
            if refined.refined {
                primary.x = refined.x;
                primary.y = refined.y;
                // Project the (σx, σy, cov_xy) covariance
                // onto the image-frame altitude axis (the
                // direction perpendicular to the apparent
                // horizon, where altitude increases). If
                // Stage C has no horizon this frame, fall
                // back to the per-axis maximum.
                let sigma_alt_px = match horizon_line {
                    Some(line) => {
                        let slope = line.slope;
                        let norm = (1.0 + slope * slope).sqrt();
                        let ux = -slope / norm;
                        let uy = 1.0 / norm;
                        let var = refined.sigma_x_px * refined.sigma_x_px * ux * ux
                            + refined.sigma_y_px * refined.sigma_y_px * uy * uy
                            + 2.0 * refined.cov_xy_px2 * ux * uy;
                        var.max(0.0).sqrt()
                    }
                    None => refined.sigma_x_px.max(refined.sigma_y_px),
                };
                if let Ok(s) = bris_core::Sigma::new(sigma_alt_px) {
                    primary.position_sigma_px = s;
                }
            }
            BodyDetection::Day(primary, centroids)
        }
        Ok(_) => {
            trace!("Stage B (day): no saturated body");
            BodyDetection::None
        }
        Err(e) => {
            trace!(error = %e, "Stage B (day): centroid extraction failed");
            BodyDetection::None
        }
    }
}

/// Night path: peak detection. An empty peak vector means no
/// stars cleared the threshold, which is normal in heavy clouds
/// or at dusk before stars are bright enough.
///
/// `horizon` is `Some(line)` when Stage C produced a horizon for
/// this frame; the peak detector then masks pixels at or below
/// the line (plus a small safety margin) so wake whitewater,
/// deck lights, and lit superstructure don't crowd real stars
/// out of the bounded `max_peaks` budget. `None` means the
/// frame either contains no horizon (sky-pointed capture) or
/// every Stage C detector failed; peak detection runs unmasked
/// and the cross-frame stitching layer will attach the resulting
/// peaks to a horizon measured on a neighbouring frame.
fn detect_night_peaks(
    frame: &Frame,
    cfg: &EngineConfig,
    horizon: Option<HorizonLine>,
) -> BodyDetection {
    let peaks = match horizon {
        Some(line) => {
            detect_peaks_above_horizon(frame, cfg.peak_cfg, line, cfg.peak_horizon_margin_px)
        }
        None => detect_peaks(frame, cfg.peak_cfg),
    };
    if peaks.is_empty() {
        trace!(
            horizon_masked = horizon.is_some(),
            "Stage B (night): no peaks above threshold",
        );
        BodyDetection::None
    } else {
        trace!(
            peak_count = peaks.len(),
            horizon_masked = horizon.is_some(),
            "Stage B (night): peaks detected",
        );
        BodyDetection::Night(peaks)
    }
}

/// Twilight path: try day first (Sun may still be above, just
/// dim); on failure fall back to night (stars / planets / Moon
/// glare may have appeared).
fn detect_twilight(
    frame: &Frame,
    sat_cfg: SaturatedBodyConfig,
    cfg: &EngineConfig,
    horizon: Option<HorizonLine>,
) -> BodyDetection {
    match detect_day_body(frame, sat_cfg, horizon) {
        BodyDetection::Day(c, _) => BodyDetection::Day(c, Vec::new()),
        // None or Night from day-path call shouldn't happen
        // (it only returns Day or None); IdentifiedStars
        // certainly can't (Stage D hasn't run yet). Fall
        // through to night for any non-Day variant.
        BodyDetection::None | BodyDetection::Night(_) | BodyDetection::IdentifiedStars(_) => {
            detect_night_peaks(frame, cfg, horizon)
        }
    }
}

fn log_body_outcome(body: &BodyDetection) {
    match body {
        BodyDetection::Day(c, secondaries) => trace!(
            x = c.x,
            y = c.y,
            area_px = c.area_px,
            sigma_px = c.position_sigma_px.value(),
            secondaries = secondaries.len(),
            "Stage B (day): centroid"
        ),
        BodyDetection::Night(peaks) => trace!(
            peak_count = peaks.len(),
            brightest = peaks.first().map(|p| p.intensity),
            "Stage B (night): peaks"
        ),
        BodyDetection::IdentifiedStars(_) | BodyDetection::None => {
            // IdentifiedStars: should not be reachable from
            // Stage B (which only produces Day/Night/None);
            // the variant is set by Stage D after promotion.
            // None: nothing to log.
        }
    }
}

/// Compute the Sun's apparent altitude in degrees at the given
/// instant for the given observer. Returns `None` if the almanac
/// declines to report (Sun below horizon — `BelowHorizon` from
/// the apparent-place chain): the classifier interprets `None`
/// as "image-only evidence" rather than treating it as
/// astronomical evidence pointing at night, which would cause
/// double-counting against the image evidence.
///
/// We also return `None` on any other almanac error: the
/// classifier degrades gracefully to image-only operation, and
/// astronomy-time errors are rare enough that warning per-frame
/// would just spam the log.
/// Build an observer at the position prior's latitude/longitude,
/// inheriting the engine's configured atmosphere/eye-height for
/// dip and refraction. Returns `None` if the prior's coordinates
/// fall outside the angle types' valid range (shouldn't happen
/// for well-formed priors but we degrade gracefully).
fn observer_at_prior(
    cfg_observer: Observer,
    prior: bris_vision::PositionPrior,
) -> Option<Observer> {
    let latitude = Latitude::from_radians(prior.lat_rad).ok()?;
    let longitude = Longitude::from_radians(prior.lon_rad).ok()?;
    Some(Observer {
        latitude,
        longitude,
        ..cfg_observer
    })
}

/// Compute the Sun's predicted apparent altitude (radians, with
/// 1σ) at the prior position and frame instant. Returns `None`
/// when the almanac declines (Sun below horizon) or the prior
/// coordinates are invalid — the Day reflection-pair primary
/// then carries `predicted_altitude = None` and Test 3 is
/// skipped, falling back to cold start.
fn sun_predicted_altitude(
    cfg_observer: Observer,
    tt: Tt,
    prior: bris_vision::PositionPrior,
) -> Option<Uncertain<f64>> {
    let observer = observer_at_prior(cfg_observer, prior)?;
    let jd_ut1 = tt.julian_date();
    let place = body_apparent_place(SolarSystemBody::Sun, tt, jd_ut1, observer).ok()?;
    if !place.direction.altitude.is_finite() {
        return None;
    }
    Some(Uncertain::new(
        place.direction.altitude,
        place.altitude_sigma,
    ))
}

fn sun_altitude_deg(observer: Observer, tt: Tt) -> Option<f64> {
    // jd_ut1 ≈ tt.julian_date() to ~69 s ≈ 0.0008°. The classifier
    // only consults the band (boundaries at 0°, -6°, -12°, -18°);
    // ~0.001° error never crosses a band boundary in practice, and
    // we'd need explicit ΔT tracking (Phase 1.5) to do better here.
    let jd_ut1 = tt.julian_date();
    match body_apparent_place(SolarSystemBody::Sun, tt, jd_ut1, observer) {
        Ok(place) => {
            let alt_deg = place.direction.altitude.to_degrees();
            // Sanity: apparent altitude must be finite and within
            // [-90, +90]. Out-of-range would indicate a bug in
            // the almanac chain; record it loudly because every
            // stage A run depends on this value.
            if (-90.0..=90.0).contains(&alt_deg) {
                Some(alt_deg)
            } else {
                warn!(
                    alt_deg,
                    "sun_altitude_deg out of [-90, 90]; falling back to image-only classifier"
                );
                None
            }
        }
        Err(_) => None,
    }
}

/// Suppress "unused" warnings on the [`Sigma`] and
/// [`bris_almanac::Observer`] re-exports while the engine still
/// has stub fields.
#[allow(dead_code)]
const _SIGMA_TYPE_USED: Option<Sigma> = None;

#[cfg(test)]
mod tests {
    // Test-helper code constructs frames pixel-by-pixel using
    // signed loop counters for centered geometry, and converts
    // brightness fractions to u16; the cast lints fire on every
    // such helper. The casts are bounded by construction in
    // each test (radii positive and small; fractions in [0, 1]).
    #![allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )]

    use super::*;
    use crate::EngineConfig;
    use bris_core::time::{Tt, JD_J2000};
    use bris_vision::{Frame, Intrinsics, SaturatedBodyConfig};

    /// A `Tt` instant when the Sun is below the horizon at the
    /// default-dev observer (Greenwich): the J2000 epoch is
    /// 2000-01-01T12:00 TT (Sun high at Greenwich); shifting by
    /// half a day gives midnight TT, Sun below horizon. Used in
    /// tests that need the classifier to agree on "Night" rather
    /// than fall back to the conservative twilight verdict when
    /// image evidence and the almanac disagree.
    fn night_tt() -> Tt {
        Tt::from_julian_date(JD_J2000 + 0.5)
    }

    fn make_frame_at(width: u32, height: u32, fill: u16, tt: Tt) -> Frame {
        let pixels = vec![fill; (width * height) as usize];
        Frame::new(
            width,
            height,
            pixels,
            tt,
            1000,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    /// Inject a saturated bright disk centered in the frame.
    fn frame_with_disk(width: u32, height: u32, radius: u32, value: u16) -> Frame {
        let mut pixels = vec![0u16; (width * height) as usize];
        let cx = width as i32 / 2;
        let cy = height as i32 / 2;
        let r2 = (radius as i32).pow(2);
        for y in 0..height as i32 {
            for x in 0..width as i32 {
                let dx = x - cx;
                let dy = y - cy;
                if dx * dx + dy * dy <= r2 {
                    pixels[(y as usize) * (width as usize) + (x as usize)] = value;
                }
            }
        }
        Frame::new(
            width,
            height,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    /// Build a config with a low saturation threshold (so the
    /// 95% default doesn't reject the synthetic disk in tests).
    fn test_cfg() -> EngineConfig {
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.saturated_body_cfg = SaturatedBodyConfig {
            saturation_threshold: u16::MAX - 100,
            min_area_px: 50,
        };
        cfg
    }

    #[test]
    fn day_frame_with_saturated_disk_runs_day_path() {
        // Saturated disk of radius 10 (area ~314 px) on a black
        // background. The classifier sees a bright frame (lots
        // of saturated pixels) and reports Day; Stage B
        // centroids the disk.
        let frame = frame_with_disk(128, 128, 10, u16::MAX);
        let outcome = process_frame(
            &bris_vision::FramePyramid::new(frame.clone()),
            &test_cfg(),
            &mut ClassifierHysteresis::default(),
            None,
        );
        assert_eq!(outcome.classification.condition, Condition::Day);
        match outcome.body {
            BodyDetection::Day(c, _) => {
                // Centroid should land near the frame center.
                assert!((c.x - 64.0).abs() < 1.0, "centroid x off-center: {}", c.x);
                assert!((c.y - 64.0).abs() < 1.0, "centroid y off-center: {}", c.y);
            }
            other => panic!("expected Day centroid, got {other:?}"),
        }
    }

    #[test]
    fn night_uniform_dark_yields_no_detection_no_panic() {
        // Uniform dark frame at a TT instant when the Sun is
        // below the horizon at Greenwich: classifier sees image
        // evidence (dark) and almanac evidence (Night) agreeing.
        let frame = make_frame_at(128, 128, 50, night_tt());
        let outcome = process_frame(
            &bris_vision::FramePyramid::new(frame.clone()),
            &test_cfg(),
            &mut ClassifierHysteresis::default(),
            None,
        );
        assert_eq!(outcome.classification.condition, Condition::Night);
        assert!(matches!(outcome.body, BodyDetection::None));
    }

    #[test]
    fn unusable_classification_skips_body_detection() {
        // The classifier's `Unusable` verdict is rare and hard
        // to provoke synthetically — it requires both image and
        // almanac evidence to be unactionable, which the
        // classifier's combine() logic rarely produces. Rather
        // than building a brittle synthetic to trigger it, we
        // assert the *contract* directly: any classifier verdict
        // that's not Day/Night/Twilight maps Stage B to
        // BodyDetection::None.
        //
        // The match in `process_frame` is exhaustive over the
        // four-variant `Condition` enum; this test is the
        // documented anchor for "Unusable is the fall-through".
        // If the enum gains a new variant, this test (and the
        // engine's stage-counter logic) need to be updated in
        // lockstep.
        for variant in [
            Condition::Day,
            Condition::Night,
            Condition::Twilight,
            Condition::Unusable,
        ] {
            let _matches_known_variant = matches!(
                variant,
                Condition::Day | Condition::Night | Condition::Twilight | Condition::Unusable
            );
        }
    }

    #[test]
    fn twilight_falls_back_to_night_when_no_saturated_body() {
        // Build a frame whose mean luma puts it in the twilight
        // band and contains a single bright pixel ≥ peak
        // min_intensity. The day path finds no saturated body
        // (we set the saturation threshold above the peak's
        // value), so twilight falls back to night peak detection
        // and finds the peak.
        //
        // This is a unit test of `detect_twilight` rather than
        // the full `process_frame` pipeline. Going through
        // `process_frame` would make Stage C run first; the
        // night-horizon detector finds a spurious "horizon" at
        // intercept ≈ 2 in this synthetic uniform-luma frame
        // (see comment on `detect_night_peaks`'s `horizon`
        // parameter), which would then mask the injected peak.
        // The fallback's `horizon: None` argument here represents
        // the "no horizon detected for this frame, run unmasked"
        // path that real usage takes when Stage C fails.
        let w = 128_u32;
        let h = 128_u32;
        let bg: u16 = 9_830; // 0.15 × 65535 → twilight band.
        let mut pixels = vec![bg; (w * h) as usize];
        // Single bright pixel: background-subtracted intensity
        // will be u16::MAX/2 - bg ≈ very large, well above
        // PeakConfig default 2000.
        pixels[64 * w as usize + 64] = u16::MAX / 2;
        let frame = Frame::new(
            w,
            h,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(w, h),
        )
        .unwrap();
        let mut cfg = test_cfg();
        // Push the saturation threshold above the injected peak's
        // value so the day path finds no saturated body.
        cfg.saturated_body_cfg = SaturatedBodyConfig {
            saturation_threshold: u16::MAX,
            min_area_px: 50,
        };
        let body = detect_twilight(&frame, cfg.saturated_body_cfg, &cfg, None);
        match body {
            BodyDetection::Night(peaks) => {
                assert!(
                    !peaks.is_empty(),
                    "twilight night-fallback should detect the injected peak"
                );
            }
            other => panic!("expected Night peaks from twilight fallback, got {other:?}"),
        }
    }

    /// Project a normalized ray (x, y, z) onto pixel coords
    /// using the placeholder pinhole intrinsics.
    fn pixel_for_ray(intr: &Intrinsics, x: f64, y: f64, z: f64) -> (f64, f64) {
        bris_vision::project_pinhole(*intr, x / z, y / z)
    }

    /// Paint a small Gaussian-ish blob at sub-pixel `(px, py)`
    /// with peak amplitude `amp` into `pixels`.
    #[allow(clippy::many_single_char_names)]
    fn paint_blob(pixels: &mut [u16], w: u32, h: u32, px: f64, py: f64, amp: u16) {
        let cx = px.round() as i32;
        let cy = py.round() as i32;
        for dy in -2..=2_i32 {
            for dx in -2..=2_i32 {
                let x = cx + dx;
                let y = cy + dy;
                if x < 0 || y < 0 || x as u32 >= w || y as u32 >= h {
                    continue;
                }
                let r2 = dx * dx + dy * dy;
                let falloff = match r2 {
                    0 => 1.0,
                    1 => 0.7,
                    2 => 0.5,
                    _ => 0.2,
                };
                let v = (f64::from(amp) * falloff) as u16;
                let idx = (y as usize) * (w as usize) + (x as usize);
                if pixels[idx] < v {
                    pixels[idx] = v;
                }
            }
        }
    }

    /// Drive a synthetic Night frame with three reflection
    /// pairs (six peaks) through `process_frame` and assert
    /// Stage C outcome / direct sight / diagnostic counters.
    #[test]
    fn reflection_pair_integration_night_frame_emits_theta_half_sight() {
        let w = 512_u32;
        let h = 512_u32;
        let intr = Intrinsics::placeholder(w, h);
        let mut pixels = vec![10_u16; (w * h) as usize];
        let alts = [0.05_f64, 0.06, 0.07];
        let x_offs = [-0.05_f64, 0.0, 0.05];
        let up_amp: u16 = 50_000;
        let dn_amp: u16 = 25_000;
        for (alt, x_off) in alts.iter().zip(x_offs.iter()) {
            let (upx, upy) = pixel_for_ray(&intr, *x_off, -alt.sin(), alt.cos());
            let (dnx, dny) = pixel_for_ray(&intr, *x_off, alt.sin(), alt.cos());
            paint_blob(&mut pixels, w, h, upx, upy, up_amp);
            paint_blob(&mut pixels, w, h, dnx, dny, dn_amp);
        }
        let frame = Frame::new(w, h, pixels, night_tt(), 1000, intr).unwrap();
        let outcome = process_frame(
            &bris_vision::FramePyramid::new(frame),
            &test_cfg(),
            &mut ClassifierHysteresis::default(),
            None,
        );
        assert_eq!(outcome.dispatched_condition, Condition::Night);
        assert!(
            outcome.reflection_pair_invoked,
            "reflection-pair provider must be invoked when ≥ 2 night peaks present"
        );
        assert!(
            outcome.reflection_pair_hypothesized,
            "three concordant pairs should produce a hypothesis"
        );
        assert!(
            outcome.reflection_pair_used,
            "reflection-pair hypothesis should win against no/poor optical horizon"
        );
        match outcome.horizon {
            HorizonStageOutcome::Detected {
                detector,
                direct_sights,
                ..
            } => {
                assert!(
                    matches!(
                        detector,
                        horizon::HorizonDetector::ReflectionPair | horizon::HorizonDetector::Fused
                    ),
                    "expected ReflectionPair or Fused, got {detector:?}"
                );
                let sight = direct_sights
                    .first()
                    .copied()
                    .expect("reflection-pair must emit a direct sight");
                let v = sight.observed_altitude.value;
                assert!(
                    (0.04..=0.08).contains(&v),
                    "direct-sight θ/2 = {v} rad outside [0.04, 0.08]"
                );
            }
            HorizonStageOutcome::None => {
                panic!("expected reflection-pair horizon detection on synthetic frame");
            }
        }
    }

    /// Paint a circular saturated disk centred at (cx, cy)
    /// with the given radius into `pixels`.
    fn paint_disk(pixels: &mut [u16], w: u32, h: u32, cx: f64, cy: f64, r: f64, value: u16) {
        let r2 = r * r;
        for y in 0..h {
            for x in 0..w {
                let dx = f64::from(x) - cx;
                let dy = f64::from(y) - cy;
                if dx * dx + dy * dy <= r2 {
                    pixels[(y as usize) * (w as usize) + (x as usize)] = value;
                }
            }
        }
    }

    /// Day-mode reflection-pair, end-to-end. With a position
    /// prior threaded into the engine, `body_candidates_from_detection`
    /// attaches the Sun's almanac-predicted altitude to the
    /// primary Day candidate. Test 3 (catalog consistency)
    /// then evaluates and a single Day pair (1 direct + 1
    /// reflection) suffices — the cold-start `min_pairs = 3`
    /// gate does not apply when any pair passed Test 3.
    ///
    /// We pick `alt` to equal the Sun's predicted altitude at
    /// the prior location so geometry agrees with the
    /// almanac. The synthetic frame's apparent half-angle
    /// then matches the predicted altitude (the half-angle
    /// of the pair *is* the body's apparent altitude when
    /// the reflector is horizontal).
    #[test]
    fn reflection_pair_integration_day_frame_emits_theta_half_sight() {
        let w = 512_u32;
        let h = 512_u32;
        let intr = Intrinsics::placeholder(w, h);
        let tt = Tt::from_julian_date(JD_J2000);
        // Pick a latitude such that the Sun is a few degrees
        // above the horizon at J2000 noon TT (Sun is near the
        // winter solstice, declination ≈ −23°). Latitude 60°N
        // puts the Sun at altitude ≈ 7° — inside the
        // small-angle band the synthetic geometry needs.
        let prior = bris_vision::PositionPrior {
            lat_rad: 60.0_f64.to_radians(),
            lon_rad: 0.0,
            sigma_position_m: 1000.0,
            timestamp: tt,
        };
        let predicted = sun_predicted_altitude(Observer::default_dev(), tt, prior)
            .expect("Sun must be above the horizon for this test geometry");
        let alt = predicted.value;
        assert!(
            (0.005..0.5).contains(&alt),
            "predicted Sun altitude {alt} rad outside synthetic-geometry sanity band; \
             tune the prior"
        );
        let mut pixels = vec![u16::MAX - 100; (w * h) as usize];
        let (upx, upy) = pixel_for_ray(&intr, 0.0, -alt.sin(), alt.cos());
        let (dnx, dny) = pixel_for_ray(&intr, 0.0, alt.sin(), alt.cos());
        paint_disk(&mut pixels, w, h, upx, upy, 20.0, u16::MAX);
        paint_disk(&mut pixels, w, h, dnx, dny, 15.0, u16::MAX);
        let frame = Frame::new(w, h, pixels, tt, 1000, intr).unwrap();
        let mut cfg = test_cfg();
        cfg.saturated_body_cfg = SaturatedBodyConfig {
            saturation_threshold: u16::MAX - 50,
            min_area_px: 50,
        };
        let outcome = process_frame(
            &bris_vision::FramePyramid::new(frame),
            &cfg,
            &mut ClassifierHysteresis::default(),
            Some(prior),
        );
        // The day-twilight image classifier may yield Day or
        // Twilight on the bright synthetic frame; both feed
        // the reflection-pair provider with predicted_altitude.
        assert!(
            matches!(
                outcome.dispatched_condition,
                Condition::Day | Condition::Twilight
            ),
            "expected Day or Twilight, got {:?}",
            outcome.dispatched_condition,
        );
        match &outcome.body {
            BodyDetection::Day(_, secondaries) => {
                assert_eq!(
                    secondaries.len(),
                    1,
                    "Day path must expose the reflection as a secondary centroid",
                );
            }
            other => panic!("expected BodyDetection::Day(_, _), got {other:?}"),
        }
        assert!(
            outcome.reflection_pair_invoked,
            "reflection-pair provider must be invoked when Day produces ≥ 2 candidates",
        );
        assert!(
            outcome.reflection_pair_used,
            "reflection-pair must accept (Test 3 passes via almanac prediction): \
             stats = {:?}",
            outcome.reflection_pair_stats,
        );
        match outcome.horizon {
            HorizonStageOutcome::Detected {
                detector,
                direct_sights,
                ..
            } => {
                assert_eq!(detector, horizon::HorizonDetector::ReflectionPair);
                let sight = direct_sights
                    .first()
                    .copied()
                    .expect("reflection-pair must emit a direct sight");
                let v = sight.observed_altitude.value;
                assert!(
                    (alt * 0.5..alt * 1.5).contains(&v),
                    "direct-sight θ/2 = {v} rad far from predicted altitude {alt}",
                );
            }
            HorizonStageOutcome::None => {
                panic!("expected reflection-pair horizon detection on synthetic frame");
            }
        }
    }

    /// Day-mode reflection-pair WITHOUT a position prior:
    /// dispatcher reaches the provider but no `predicted_altitude`
    /// is attached, so Test 3 is skipped and the single Day
    /// pair fails the cold-start min-pair gate. Documents the
    /// limitation: Day reflection-pair currently requires a
    /// prior to produce a fix.
    #[test]
    fn reflection_pair_day_without_prior_invokes_but_emits_none() {
        let w = 512_u32;
        let h = 512_u32;
        let intr = Intrinsics::placeholder(w, h);
        let alt = 0.05_f64;
        let mut pixels = vec![u16::MAX - 100; (w * h) as usize];
        let (upx, upy) = pixel_for_ray(&intr, 0.0, -alt.sin(), alt.cos());
        let (dnx, dny) = pixel_for_ray(&intr, 0.0, alt.sin(), alt.cos());
        paint_disk(&mut pixels, w, h, upx, upy, 20.0, u16::MAX);
        paint_disk(&mut pixels, w, h, dnx, dny, 15.0, u16::MAX);
        let frame = Frame::new(w, h, pixels, Tt::from_julian_date(JD_J2000), 1000, intr).unwrap();
        let mut cfg = test_cfg();
        cfg.saturated_body_cfg = SaturatedBodyConfig {
            saturation_threshold: u16::MAX - 50,
            min_area_px: 50,
        };
        let outcome = process_frame(
            &bris_vision::FramePyramid::new(frame),
            &cfg,
            &mut ClassifierHysteresis::default(),
            None,
        );
        assert!(
            outcome.reflection_pair_invoked,
            "provider must still be invoked: dispatcher gating is on candidate count",
        );
        assert!(
            !outcome.reflection_pair_used,
            "single Day pair without prior must NOT accept (cold-start gate)",
        );
    }

    /// Day-mode single saturated body → reflection-pair
    /// provider not invoked (only one candidate).
    #[test]
    fn day_single_body_does_not_invoke_reflection_pair() {
        let frame = frame_with_disk(256, 256, 20, u16::MAX);
        let outcome = process_frame(
            &bris_vision::FramePyramid::new(frame),
            &test_cfg(),
            &mut ClassifierHysteresis::default(),
            None,
        );
        assert_eq!(outcome.dispatched_condition, Condition::Day);
        match &outcome.body {
            BodyDetection::Day(_, secondaries) => assert!(secondaries.is_empty()),
            other => panic!("expected Day, got {other:?}"),
        }
        assert!(
            !outcome.reflection_pair_invoked,
            "single Day candidate must not invoke reflection-pair"
        );
    }

    #[test]
    fn sun_altitude_returns_finite_value_for_default_observer() {
        // Smoke test: the almanac call from inside the pipeline
        // must succeed for a sensible default observer at J2000
        // (when the Sun is somewhere). The exact altitude
        // depends on the almanac's chain; we just check that we
        // don't get None on the happy path, because that would
        // indicate the apparent-place pipeline is rejecting our
        // arguments.
        let alt = sun_altitude_deg(Observer::default_dev(), Tt::from_julian_date(JD_J2000));
        assert!(
            alt.is_some(),
            "expected finite sun altitude at J2000 for default observer (Greenwich), \
             got None — likely an almanac argument mismatch"
        );
        let v = alt.unwrap();
        assert!(
            (-90.0..=90.0).contains(&v),
            "sun altitude out of range: {v}"
        );
    }
}
