//! Stage E: pair selection, sight emission, sight window, fix
//! publication.
//!
//! After Stages A/B/C have populated the body and horizon
//! priority queues, Stage E runs whenever the engine wants to
//! decide "should I emit a sight from any of these records, and
//! if so, should I publish a fresh fix?"
//!
//! # Pair selection
//!
//! For each body record (in ascending-σ order), find the
//! horizon record that minimizes:
//!
//! ```text
//! combined_σ(body, horizon) =
//!     sqrt(body_σ² + horizon_σ² + stitch_σ²)
//! ```
//!
//! where `stitch_σ = 0` for same-frame pairs (no stitching
//! needed) and `stitch_σ ≈ time_gap × stitch_sigma_per_second`
//! for cross-frame pairs (cheap estimate; the actual stitch is
//! deferred until pair selection commits to running it).
//!
//! Greedy bipartite matching: the best body picks its best
//! horizon first; subsequent bodies pick from the remaining
//! horizons. This isn't optimal min-cost matching but is O(N²)
//! cheap and makes the right decision for the regimes Bris
//! actually runs in (≤ ~10 of each record type at any time).
//!
//! Horizons are *not* consumed by being picked — see "Multi-body
//! per frame" below — only book-kept so multiple body records
//! from the *same frame* pair with the same horizon. A horizon
//! from frame F is reusable across all body records from frame
//! F (the σ correlation is exactly the horizon's `altitude_σ`,
//! which would be double-counted in `multi_sight_fix`'s LSQ if
//! we let it; the design doc accepts this approximation for the
//! commit-5 milestone with the rest of the per-fix covariance
//! work tracked under Phase 4).
//!
//! # Same-frame vs cross-frame pairs
//!
//! Commit 5 emits **only same-frame sights**. Cross-frame pairs
//! are *selected* (the `stitch_σ` math is in place) but the
//! actual cross-frame altitude measurement via
//! [`bris_vision::panorama_altitude`] is deferred to a follow-up
//! commit. A cross-frame "best pair" with no same-frame
//! alternative therefore yields no sight; the body record waits
//! for a same-frame horizon partner or for the eventual
//! cross-frame execution. Documented as a known limitation.
//!
//! # Body identification
//!
//! Per the design doc's Stage E mapping: same-frame Day/Twilight
//! body records are treated as the Sun (the overwhelmingly
//! common case during daylight). Night peaks are not yet
//! handled — Stage D's plate-solver lands in commit 6 and
//! identifies stars individually. Twilight Moon/planets via the
//! non-saturated-body centroider are deferred (the segmentation
//! mask hookup needed to disambiguate them from sun glare lives
//! in a separate work item per the design doc).
//!
//! # Sight window
//!
//! Per the design doc:
//!
//! - Cap at [`crate::EngineConfig::sight_window_capacity`] (default 10).
//! - Replace-worst-on-insertion when full (drop the highest-σ
//!   sight to make room).
//! - Age-eviction: drop sights older than
//!   [`crate::EngineConfig::sight_window_seconds`] (default 600 s).
//! - Linear age-weighting: a sight of age `t` contributes with
//!   weight `max(0, 1 - t / time_constant)`.
//!
//! For commit 5 the LSQ in [`bris_nav::multi_sight_fix`] is
//! unweighted across sights (it weights per-sight σ already).
//! Adding the age weight is one line at the call site, deferred
//! to a follow-up because it requires a small extension to the
//! `multi_sight_fix` API to accept per-sight weights.
//!
//! # Fix publication
//!
//! After pair selection and sight-window update, attempt
//! [`bris_nav::multi_sight_fix`] over the window. Publish the
//! resulting fix if:
//!
//! 1. The window changed since last attempt (new sight added or
//!    old sight evicted), AND
//! 2. At least
//!    [`crate::EngineConfig::min_fix_publication_interval_ms`] ms
//!    have elapsed since the last successful publish, AND
//! 3. `multi_sight_fix` returns Ok (≥ 2 sights with non-singular
//!    azimuth diversity).

use super::queue::{BodyRecord, FrameId, HorizonRecord, Storage};
use crate::config::EngineConfig;
use crate::fix::{DominantSource, PublishedFix};
use crate::pipeline::BodyDetection;
use bris_almanac::{body_apparent_place, ApparentPlace, Observer, SolarSystemBody};
use bris_core::time::Tt;
use bris_core::{Sigma, Uncertain};
use bris_nav::{line_of_position, multi_sight_fix, LineOfPosition};
use bris_vision::{measure_altitude, Centroid, HorizonLine};
use std::time::{Duration, Instant};
use tracing::{debug, trace};

/// Cheap stitching σ estimate, in radians per second of
/// inter-frame time gap. A 1-second gap yields ~0.5 arcmin of
/// uncertainty by this model. Calibrated by analogy with the
/// CLI's panorama-stitching residuals; the design doc notes
/// this is a placeholder until a frame-to-frame motion-aware
/// estimate lands.
///
/// Used only for *pair-selection* prioritization. The actual
/// stitch (when commit 5+follow-up wires in `panorama_altitude`)
/// reports its own σ from cross-correlation residuals, which
/// supersedes this estimate.
const STITCH_SIGMA_PER_SECOND_RAD: f64 = 0.5 * std::f64::consts::PI / (60.0 * 180.0);

/// Outcome of one Stage E run.
#[derive(Debug, Default)]
pub(crate) struct StageEOutcome {
    /// Number of new sights inserted into the window during
    /// this run. Zero is normal (no new same-frame pair, or
    /// all candidates worse than the worst sight already in
    /// the window).
    pub sights_inserted: usize,
    /// Number of sights age-evicted from the window during
    /// this run.
    pub sights_evicted: usize,
    /// Fix that was emitted, if any. None when:
    ///
    /// - The window has < 2 sights (LSQ needs ≥ 2).
    /// - The geometry is singular (all sights at nearly the
    ///   same azimuth).
    /// - The publication-interval throttle is in effect.
    /// - The window didn't change meaningfully since the last
    ///   publication attempt.
    pub published: Option<PublishedFix>,
}

/// Identifier for the body that produced a sight.
///
/// Bris produces sights from two distinct body classes:
/// Solar-System bodies (Sun, Moon, planets — the "day" path)
/// and catalog stars identified via plate solving (the "night"
/// path). The two have different almanac entry points
/// ([`bris_almanac::body_apparent_place`] vs
/// [`bris_almanac::star_apparent_place`]), and the operator-
/// facing display wants to label them differently. This enum
/// captures both as a single type so a [`Sight`] can name
/// either uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SightBody {
    /// A Solar System body. For commit 5+ this is always
    /// [`SolarSystemBody::Sun`] from the day path; commit 6's
    /// non-saturated-body work (separate from this commit's
    /// plate-solve work) extends to Moon/planets.
    SolarSystem(SolarSystemBody),
    /// A catalog star, by Yale BSC HR id. Produced from
    /// plate-solved peaks at Stage D.
    Star { hr: u32 },
}

/// One sight retained in the active window.
///
/// Sourced from a (body, horizon) pair; contains the reduced
/// LOP plus enough context to age out, replace, and report.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Sight {
    /// LOP from sight reduction.
    pub lop: LineOfPosition,
    /// Capture time of the body record (or, for cross-frame
    /// pairs, the body's frame; the horizon's frame may be
    /// older or newer). Used as the age anchor for window
    /// eviction.
    pub anchor_tt: Tt,
    /// Per-sight altitude σ in radians. Stored separately
    /// from `lop.intercept_sigma_nm` because age-weighting
    /// (deferred) operates on the altitude σ.
    pub altitude_sigma_rad: f64,
    /// Identifier of the body that produced this sight.
    pub body: SightBody,
    /// Body azimuth at the assumed observer (radians, [0, 2π)).
    /// Cached for the diagnostic azimuth-spread computation.
    pub azimuth_rad: f64,
    /// Source body record's `FrameId`. Used to dedup: a body
    /// record produces at most one sight ever, regardless of
    /// how many Stage E runs see it. Without this, every
    /// `push_frame` would re-emit sights for every record still
    /// in the storage.
    ///
    /// For night-path records that expand into N per-star
    /// sights, all N share the same `source_frame_id` —
    /// dedup operates at the frame-record level so the engine
    /// emits all stars from a frame on a single Stage E run
    /// and never re-emits them.
    pub source_frame_id: FrameId,
    /// `FrameId` of the horizon record this sight was paired
    /// with. Equal to `source_frame_id` for same-frame fixes
    /// (the common case); different for cross-frame stitched
    /// pairs. For night plate-solve sights without an
    /// explicitly-paired horizon record (the horizon line is
    /// passed in as a value), equal to `source_frame_id`.
    ///
    /// Surfaced via [`crate::PublishedFix::contributing_frame_ids`]
    /// so foreign callers can retrieve the exact frames that
    /// produced a fix.
    pub horizon_frame_id: FrameId,
}

/// Active sight window with cap + age eviction + change
/// detection.
#[derive(Debug, Default)]
pub(crate) struct SightWindow {
    sights: Vec<Sight>,
    /// Number of sights inserted since the window was last
    /// "consumed" by a fix publication. Reset to zero each
    /// time [`Self::take_change_count`] is called.
    pending_inserts: usize,
    /// Number of sights age-evicted since last publication.
    pending_evictions: usize,
}

impl SightWindow {
    /// Insert a sight; honour the cap with replace-worst-on-
    /// insertion. Returns `true` if the insert took (either
    /// because the window had room or the new sight was
    /// better than the existing worst).
    pub(crate) fn try_insert(&mut self, sight: Sight, capacity: usize) -> bool {
        if self.sights.len() < capacity {
            self.sights.push(sight);
            self.pending_inserts += 1;
            return true;
        }
        // Window is full. Find the worst (largest σ) sight.
        let (worst_idx, worst_sigma) = self
            .sights
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.altitude_sigma_rad))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .expect("window is non-empty (sights.len() == capacity > 0)");
        if sight.altitude_sigma_rad < worst_sigma {
            self.sights[worst_idx] = sight;
            self.pending_inserts += 1;
            true
        } else {
            false
        }
    }

    /// Drop sights older than `max_age_seconds` relative to
    /// `now_tt`. Returns the number evicted.
    pub(crate) fn evict_aged(&mut self, now_tt: Tt, max_age_seconds: f64) -> usize {
        let before = self.sights.len();
        self.sights.retain(|s| {
            let age = time_gap_seconds(now_tt, s.anchor_tt);
            age <= max_age_seconds
        });
        let evicted = before - self.sights.len();
        self.pending_evictions += evicted;
        evicted
    }

    /// Number of sights currently held.
    pub(crate) fn len(&self) -> usize {
        self.sights.len()
    }

    /// Iterate over sights in insertion order (no σ ordering).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Sight> {
        self.sights.iter()
    }

    /// True iff a sight in the window has the given
    /// `source_frame_id`. Used by Stage E to dedup: a body
    /// record produces at most one sight in the window.
    pub(crate) fn contains_source(&self, source_frame_id: FrameId) -> bool {
        self.sights
            .iter()
            .any(|s| s.source_frame_id == source_frame_id)
    }

    /// Snapshot the per-publication change counters and
    /// reset them to zero. The engine calls this exactly
    /// when it publishes; the counters then accumulate
    /// future changes until the next publication.
    pub(crate) fn take_change_count(&mut self) -> (usize, usize) {
        let inserts = self.pending_inserts;
        let evictions = self.pending_evictions;
        self.pending_inserts = 0;
        self.pending_evictions = 0;
        (inserts, evictions)
    }
}

/// Run Stage E.
///
/// `last_publication` is the [`Instant`] of the last successful
/// fix publication, used to throttle publication rate; pass
/// `None` if no publication has occurred yet (in which case the
/// throttle is bypassed for the first emission).
///
/// # Sight-window vs storage lifecycle
///
/// The storage (ring buffer + body/horizon queues) holds raw
/// frames and detection records for at most
/// [`crate::EngineConfig::stitching_window_seconds`] (default
/// 2 s); records drop out of the queues when their source
/// frames age out of the ring buffer.
///
/// The sight window holds *reduced* sights (LOPs) for up to
/// [`crate::EngineConfig::sight_window_seconds`] (default
/// 600 s). Sights therefore outlive their source records: a
/// sight reduced from a body+horizon detection at t=0 stays in
/// the window until t=600s, even though the storage has
/// forgotten about that frame by t=2s.
///
/// To respect this lifecycle, sights are inserted into the
/// window *once* per source body record (deduped by the body
/// record's `frame_id`). Stage E iterates the body queue every
/// push but only emits sights for `frame_ids` it hasn't already
/// emitted.
///
/// Mutates `window` in place; returns a [`StageEOutcome`]
/// describing what changed and any [`PublishedFix`] to emit.
pub(crate) fn run(
    storage: &Storage,
    window: &mut SightWindow,
    cfg: &EngineConfig,
    last_publication: Option<Instant>,
) -> StageEOutcome {
    let mut out = StageEOutcome::default();

    // 1. Pair selection.
    let candidates = select_pairs(storage, cfg);

    // 2. Reduce candidates → Sights, deduped by source
    //    frame_id (one sight per body record).
    let now_tt = candidates
        .iter()
        .map(|c| c.body_tt)
        .reduce(|a, b| {
            if a.julian_date() >= b.julian_date() {
                a
            } else {
                b
            }
        })
        .or_else(|| window.iter().map(|s| s.anchor_tt).next());
    let mut inserted = 0_usize;
    for cand in candidates {
        if window.contains_source(cand.body_frame_id_repr) {
            // Sight from this body record already in the
            // window; don't re-emit.
            continue;
        }
        if !is_same_frame(&cand) {
            trace!(
                body_frame = %cand.body_frame_id_repr,
                horizon_frame = %cand.horizon_frame_id_repr,
                "Stage E: skipping cross-frame pair (panorama_altitude wiring deferred)",
            );
            continue;
        }
        match reduce_to_sight(&cand, storage, cfg) {
            Ok(sights) => {
                for sight in sights {
                    if window.try_insert(sight, cfg.sight_window_capacity) {
                        inserted += 1;
                    }
                }
            }
            Err(e) => trace!(error = ?e, "Stage E: sight reduction failed"),
        }
    }
    out.sights_inserted = inserted;

    // 3. Age-evict sights older than the window's age limit
    //    relative to the most recent processing instant.
    if let Some(now) = now_tt {
        out.sights_evicted = window.evict_aged(now, cfg.sight_window_seconds);
    }

    // 4. Throttled publication, only on meaningful change.
    if out.sights_inserted == 0 && out.sights_evicted == 0 {
        return out;
    }
    let throttle_elapsed = match last_publication {
        None => true,
        Some(t) => t.elapsed() >= Duration::from_millis(cfg.min_fix_publication_interval_ms),
    };
    if !throttle_elapsed {
        debug!("Stage E: skipping publication (throttle window not yet elapsed)",);
        return out;
    }
    out.published = try_publish(window, now_tt);
    if out.published.is_some() {
        let _ = window.take_change_count();
    }
    out
}

/// One candidate pair from the body × horizon cross-product,
/// pre-σ-evaluation. We keep the underlying records by reference
/// so `reduce_to_sight` can read out the geometry.
struct PairCandidate<'a> {
    body: &'a BodyRecord,
    horizon: &'a HorizonRecord,
    /// Estimated combined σ (radians) used for ranking. Lower
    /// is better.
    combined_sigma_rad: f64,
    /// Body capture time (used as the sight's age anchor).
    body_tt: Tt,
    /// Convenience copies for diagnostic logging.
    body_frame_id_repr: FrameId,
    horizon_frame_id_repr: FrameId,
}

/// Greedy O(N²) pair selection.
///
/// Iterate body records in σ-ascending order; for each, scan all
/// horizon records and compute the combined σ; pick the lowest;
/// emit a candidate. Each body produces at most one candidate;
/// horizons may be reused across multiple bodies (see module
/// docs for the multi-body-per-frame correlation discussion).
fn select_pairs<'a>(storage: &'a Storage, _cfg: &EngineConfig) -> Vec<PairCandidate<'a>> {
    let body_records: Vec<&BodyRecord> = storage.body_records().collect();
    let horizon_records: Vec<&HorizonRecord> = storage.horizon_records().collect();
    let mut out = Vec::with_capacity(body_records.len());
    for body in body_records {
        // For ranking, we use the body's σ_key value as a
        // coarse stand-in for "body angular σ". For Day this
        // is the centroid pixel σ; for Night it's the
        // 1/sqrt(N_peaks) placeholder. Both are positive and
        // the ranking is monotone in body σ either way.
        let body_sigma_proxy = body.sigma_key.value();
        let mut best: Option<(&HorizonRecord, f64)> = None;
        for horizon in &horizon_records {
            let stitch_sigma = if body.frame_id == horizon.frame_id {
                0.0
            } else {
                let gap = time_gap_seconds(body.frame_tt, horizon.frame_tt);
                gap * STITCH_SIGMA_PER_SECOND_RAD
            };
            let h_sigma = horizon.line.altitude_sigma.value();
            let combined =
                (body_sigma_proxy.powi(2) + h_sigma.powi(2) + stitch_sigma.powi(2)).sqrt();
            match best {
                Some((_, current)) if current <= combined => {}
                _ => best = Some((horizon, combined)),
            }
        }
        if let Some((horizon, combined)) = best {
            out.push(PairCandidate {
                body,
                horizon,
                combined_sigma_rad: combined,
                body_tt: body.frame_tt,
                body_frame_id_repr: body.frame_id,
                horizon_frame_id_repr: horizon.frame_id,
            });
        }
    }
    out
}

fn is_same_frame(c: &PairCandidate<'_>) -> bool {
    c.body.frame_id == c.horizon.frame_id
}

/// Reduce one same-frame candidate into a [`Sight`].
///
/// Returns `Err` for non-actionable candidates: night-path
/// peaks (deferred to commit 6's plate solver), measurement
/// failure (centroid-below-horizon, etc.), or almanac error
/// (body below horizon at the observer position).
/// Reduce one same-frame candidate into one or more
/// [`Sight`] values.
///
/// - [`BodyDetection::Day`]: yields exactly one sight (Sun).
/// - [`BodyDetection::IdentifiedStars`]: yields one sight per
///   identified star, all sharing the paired horizon.
/// - [`BodyDetection::Night`]: yields zero sights — peaks
///   that haven't been plate-solved aren't actionable. (Stage
///   D normally promotes these to `IdentifiedStars`; what
///   reaches Stage E as `Night(_)` is a Night payload that
///   plate-solving declined or hadn't yet seen.)
/// - [`BodyDetection::None`]: unreachable (queue admission
///   filters these out).
///
/// Returns `Err` for actionable candidates that nonetheless
/// failed reduction (almanac error, measurement error, etc.).
/// Returns `Ok(empty vec)` for the not-actionable cases above
/// — the caller treats empty the same as "nothing to insert"
/// and doesn't log it as an error.
#[allow(
    // `observer` (the input arg) and `observed` (the measured
    // altitude) are both domain-standard names; renaming
    // either is worse than the lint.
    clippy::similar_names,
)]
fn reduce_to_sight(
    c: &PairCandidate<'_>,
    storage: &Storage,
    cfg: &EngineConfig,
) -> Result<Vec<Sight>, ReduceError> {
    let observer = cfg.observer;
    // Look up intrinsics once; both same-frame paths need
    // them.
    let ring_frame = storage
        .frame(c.body.frame_id)
        .ok_or(ReduceError::FrameEvicted)?;
    let intrinsics = ring_frame.frame().intrinsics;
    let jd_ut1 = c.body.frame_tt.julian_date();

    match &c.body.detection {
        BodyDetection::Day(centroid) => {
            // Day path: Sun (commit 5 simplification — see
            // module docs for the deferred Moon/planet work).
            let body = SolarSystemBody::Sun;
            let apparent: ApparentPlace =
                body_apparent_place(body, c.body.frame_tt, jd_ut1, observer)
                    .map_err(ReduceError::Apparent)?;
            // Prefer the horizon record's direct sight when
            // one is present (Phase 1: reflection-pair
            // provider emits `Ho = θ/2` directly). The sight-
            // combination stage in `bris-nav` de-duplicates
            // per-body sights in a window so the same body's
            // direct sight and a separately-derived horizon-
            // based sight cannot both contribute. Today only
            // one provider wins per frame so the
            // double-counting risk is hypothetical; documented
            // here so it stays visible as more providers land.
            let observed = if let Some(direct) = c.horizon.direct_sight {
                direct.observed_altitude
            } else {
                measure_altitude(intrinsics, c.horizon.line, *centroid)
                    .map_err(ReduceError::Measure)?
            };
            let computed = Uncertain::new(apparent.direction.altitude, apparent.altitude_sigma);
            let lop = line_of_position(
                observer.latitude,
                observer.longitude,
                observed,
                computed,
                apparent.direction.azimuth,
            )
            .map_err(ReduceError::Lop)?;
            Ok(vec![Sight {
                lop,
                anchor_tt: c.body_tt,
                altitude_sigma_rad: observed.sigma.combine(apparent.altitude_sigma).value(),
                body: SightBody::SolarSystem(body),
                azimuth_rad: apparent.direction.azimuth,
                source_frame_id: c.body.frame_id,
                horizon_frame_id: c.horizon.frame_id,
            }])
        }
        BodyDetection::IdentifiedStars(result) => {
            // Night path: one sight per identified star.
            let mut out = Vec::with_capacity(result.identified.len());
            for ident in &result.identified {
                match expand_identified_star(
                    ident,
                    &result.attitude.matrix,
                    intrinsics,
                    c.horizon.line,
                    c.body.frame_tt,
                    jd_ut1,
                    observer,
                    cfg.per_star_sigma,
                    c.body.frame_id,
                    c.horizon.frame_id,
                    c.body_tt,
                ) {
                    Ok(sight) => out.push(sight),
                    Err(e) => trace!(
                        hr = ident.hr,
                        error = ?e,
                        "Stage E: per-star reduction failed"
                    ),
                }
            }
            Ok(out)
        }
        BodyDetection::Night(_) => {
            // Plate solve hadn't run or declined. Not an
            // error; just nothing to emit.
            Ok(Vec::new())
        }
        BodyDetection::None => {
            unreachable!("BodyDetection::None records are filtered out at queue admission",)
        }
    }
}

/// Reduce one identified star into a [`Sight`].
#[allow(clippy::too_many_arguments, clippy::similar_names)]
fn expand_identified_star(
    ident: &bris_platesolve::IdentifiedStar,
    attitude: &[f64; 9],
    intrinsics: bris_vision::Intrinsics,
    horizon: HorizonLine,
    frame_tt: Tt,
    jd_ut1: f64,
    observer: Observer,
    per_star_sigma: Sigma,
    source_frame_id: FrameId,
    horizon_frame_id: FrameId,
    anchor_tt: Tt,
) -> Result<Sight, ReduceError> {
    // Look up the catalog record for the apparent place.
    let star_record = bris_almanac::by_hr(ident.hr).ok_or(ReduceError::UnknownStarHr(ident.hr))?;
    let apparent = bris_almanac::star_apparent_place(star_record, frame_tt, jd_ut1, observer)
        .map_err(ReduceError::Apparent)?;

    // Observed altitude via the platesolve crate's helper:
    // takes catalog vec → attitude-rotated camera ray →
    // horizon-relative altitude.
    let observed_altitude =
        bris_platesolve::star_altitude(ident, attitude, intrinsics, horizon, per_star_sigma)
            .map_err(ReduceError::Measure)?
            .altitude;

    let computed = Uncertain::new(apparent.direction.altitude, apparent.altitude_sigma);
    let lop = line_of_position(
        observer.latitude,
        observer.longitude,
        observed_altitude,
        computed,
        apparent.direction.azimuth,
    )
    .map_err(ReduceError::Lop)?;
    Ok(Sight {
        lop,
        anchor_tt,
        altitude_sigma_rad: observed_altitude
            .sigma
            .combine(apparent.altitude_sigma)
            .value(),
        body: SightBody::Star { hr: ident.hr },
        azimuth_rad: apparent.direction.azimuth,
        source_frame_id,
        horizon_frame_id,
    })
}

/// Errors during sight reduction. Internal-only; logged at
/// `trace` and otherwise discarded.
#[derive(Debug)]
enum ReduceError {
    /// Source frame has been evicted from the ring buffer
    /// since the queue record was admitted. Rare; caused by
    /// the capacity hard cap.
    FrameEvicted,
    /// Plate-solver returned an HR id that's not in the
    /// almanac catalog. Should be impossible (the solver
    /// matches against the same catalog the almanac uses)
    /// but guarded for diagnostics.
    UnknownStarHr(u32),
    Apparent(bris_almanac::ApparentPlaceError),
    Measure(bris_vision::MeasurementError),
    Lop(bris_nav::LopError),
}

/// Run `multi_sight_fix` over the current window; build a
/// [`PublishedFix`] from the result.
///
/// Returns `None` if the LSQ refuses (fewer than 2 sights or
/// singular geometry).
fn try_publish(window: &SightWindow, now_tt: Option<Tt>) -> Option<PublishedFix> {
    let lops: Vec<LineOfPosition> = window.iter().map(|s| s.lop).collect();
    let fix = match multi_sight_fix(&lops) {
        Ok(f) => f,
        Err(e) => {
            trace!(error = %e, "Stage E: multi_sight_fix declined");
            return None;
        }
    };
    let azimuth_spread_rad = azimuth_spread(window);
    let anchor = now_tt.unwrap_or_else(|| {
        // Fallback only reachable if the window is empty,
        // which can't happen given multi_sight_fix succeeded.
        Tt::from_julian_date(bris_core::time::JD_J2000)
    });
    let oldest_age = window
        .iter()
        .map(|s| time_gap_seconds(anchor, s.anchor_tt))
        .fold(0.0_f64, f64::max);
    // Collect every frame ID referenced by a sight in the
    // window. Each sight contributes its body frame and (when
    // different) its horizon frame; same-frame sights only
    // contribute one. Order is preserved (body-first, horizon-
    // second per sight); duplicates are removed while keeping
    // the first occurrence so the foreign caller sees a stable
    // frame ordering across publications.
    let mut seen = std::collections::BTreeSet::new();
    let mut contributing_frame_ids: Vec<u64> = Vec::with_capacity(window.len() * 2);
    for s in window.iter() {
        if seen.insert(s.source_frame_id) {
            contributing_frame_ids.push(s.source_frame_id.0);
        }
        if s.horizon_frame_id != s.source_frame_id && seen.insert(s.horizon_frame_id) {
            contributing_frame_ids.push(s.horizon_frame_id.0);
        }
    }
    Some(PublishedFix {
        fix,
        n_sights: window.len(),
        azimuth_spread_rad,
        oldest_sight_age_seconds: oldest_age,
        // Commit 5: the per-source budget breakdown isn't yet
        // computed (TODO 8 wires it through to $PBRIS).
        dominant_source: DominantSource::None,
        timestamp: anchor,
        contributing_frame_ids,
    })
}

/// Max minus min azimuth across the window's sights, accounting
/// for the [0, 2π) wrap. Returns 0 for windows with < 2 sights.
fn azimuth_spread(window: &SightWindow) -> f64 {
    let azimuths: Vec<f64> = window.iter().map(|s| s.azimuth_rad).collect();
    if azimuths.len() < 2 {
        return 0.0;
    }
    let mut sorted = azimuths.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // The minimum gap when "wrapping around" 2π: insert the
    // [0, 2π] boundary as a virtual point and find the largest
    // arc segment not containing any azimuth; the spread is
    // 2π minus that gap. This handles the case where azimuths
    // straddle north (e.g. 350° and 10° are 20° apart, not
    // 340°).
    let mut max_gap = sorted[0] + std::f64::consts::TAU - sorted[sorted.len() - 1];
    for i in 1..sorted.len() {
        let gap = sorted[i] - sorted[i - 1];
        if gap > max_gap {
            max_gap = gap;
        }
    }
    std::f64::consts::TAU - max_gap
}

fn time_gap_seconds(a: Tt, b: Tt) -> f64 {
    const SECS_PER_DAY: f64 = 86_400.0;
    (a.julian_date() - b.julian_date()).abs() * SECS_PER_DAY
}

// Suppress unused warnings on internal fields/types that Stage
// E will populate fully in follow-up commits.
#[allow(dead_code)]
const _: Option<Centroid> = None;
#[allow(dead_code)]
const _: Option<HorizonLine> = None;
#[allow(dead_code)]
const _: Option<Sigma> = None;
#[allow(dead_code)]
fn _unused_combined(c: &PairCandidate<'_>) -> f64 {
    c.combined_sigma_rad
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_almanac::Observer;
    use bris_core::time::JD_J2000;
    use bris_core::{Latitude, Longitude};

    fn dummy_lop(intercept_nm: f64, sigma_nm: f64, azimuth_rad: f64) -> LineOfPosition {
        LineOfPosition {
            assumed_lat: Latitude::from_degrees(0.0).unwrap(),
            assumed_lon: Longitude::from_degrees(0.0).unwrap(),
            azimuth_rad,
            intercept_nm,
            intercept_sigma_nm: Sigma::new(sigma_nm).unwrap(),
        }
    }

    fn dummy_sight(altitude_sigma_rad: f64, azimuth_rad: f64, age_offset_s: f64) -> Sight {
        // Use a unique-ish frame_id derived from the input;
        // for tests where dedup matters we pass distinct
        // azimuths so frame_id derivation doesn't collide.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let frame_id = FrameId(((azimuth_rad * 1000.0).abs() as u64).wrapping_add(1));
        Sight {
            lop: dummy_lop(0.5, 0.1, azimuth_rad),
            anchor_tt: Tt::from_julian_date(JD_J2000 + age_offset_s / 86_400.0),
            altitude_sigma_rad,
            body: SightBody::SolarSystem(SolarSystemBody::Sun),
            azimuth_rad,
            source_frame_id: frame_id,
            horizon_frame_id: frame_id,
        }
    }

    #[test]
    fn sight_window_inserts_until_capacity() {
        let mut w = SightWindow::default();
        for i in 0..5 {
            assert!(w.try_insert(dummy_sight(0.001, f64::from(i), 0.0), 5));
        }
        assert_eq!(w.len(), 5);
        assert_eq!(w.take_change_count(), (5, 0));
    }

    #[test]
    fn sight_window_replaces_worst_when_full() {
        let mut w = SightWindow::default();
        // Fill with sights of σ = 0.01.
        for i in 0..3 {
            w.try_insert(dummy_sight(0.01, f64::from(i), 0.0), 3);
        }
        // A worse sight should be rejected.
        let rejected = !w.try_insert(dummy_sight(0.02, 4.0, 0.0), 3);
        assert!(rejected, "worse-σ sight should not displace anyone");
        // A better sight should replace one.
        let accepted = w.try_insert(dummy_sight(0.001, 5.0, 0.0), 3);
        assert!(accepted, "better-σ sight should be inserted");
        assert_eq!(w.len(), 3);
        // The replacement should have removed one of the
        // σ=0.01 sights, not added a fourth.
        assert!(
            w.iter()
                .any(|s| (s.altitude_sigma_rad - 0.001).abs() < 1e-12),
            "the new better-σ sight should be present"
        );
    }

    #[test]
    fn sight_window_age_evicts_old_sights() {
        let mut w = SightWindow::default();
        // Two sights, ages 0s and 700s.
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        w.try_insert(dummy_sight(0.001, 1.0, 700.0), 5);
        assert_eq!(w.len(), 2);
        // Now is t=0; max_age 600s. The 700-second sight is
        // newer than now, so its age is +700s ≥ 600s? No —
        // the 700s sight has anchor_tt at t=+700s; now is
        // t=0 (anchor_tt = JD_J2000); time_gap is |+700s| =
        // 700s > 600s → evict.
        let now = Tt::from_julian_date(JD_J2000);
        let evicted = w.evict_aged(now, 600.0);
        assert_eq!(evicted, 1);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn azimuth_spread_handles_wrap() {
        let mut w = SightWindow::default();
        // 350° and 10° are 20° apart, not 340°.
        w.try_insert(dummy_sight(0.001, 350.0_f64.to_radians(), 0.0), 5);
        w.try_insert(dummy_sight(0.001, 10.0_f64.to_radians(), 0.0), 5);
        let spread_rad = azimuth_spread(&w);
        let spread_deg = spread_rad.to_degrees();
        assert!(
            (spread_deg - 20.0).abs() < 1e-9,
            "azimuth spread {spread_deg}°, expected ~20° (350° to 10° wrapping past 0°)"
        );
    }

    #[test]
    fn azimuth_spread_widely_distributed() {
        let mut w = SightWindow::default();
        for deg in [0.0_f64, 90.0, 180.0, 270.0] {
            w.try_insert(dummy_sight(0.001, deg.to_radians(), 0.0), 10);
        }
        // Four equally-spaced azimuths: max gap is 90° → spread is 270°.
        let spread_deg = azimuth_spread(&w).to_degrees();
        assert!(
            (spread_deg - 270.0).abs() < 1e-6,
            "expected 270° spread, got {spread_deg}°"
        );
    }

    #[test]
    fn try_publish_returns_none_for_singleton_window() {
        let mut w = SightWindow::default();
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        let now = Some(Tt::from_julian_date(JD_J2000));
        assert!(
            try_publish(&w, now).is_none(),
            "single-sight window cannot publish (LSQ needs >= 2 LOPs)"
        );
    }

    #[test]
    fn try_publish_succeeds_for_two_sights_with_diversity() {
        let mut w = SightWindow::default();
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        w.try_insert(dummy_sight(0.001, std::f64::consts::FRAC_PI_2, 0.0), 5);
        let now = Some(Tt::from_julian_date(JD_J2000));
        let published = try_publish(&w, now).expect("two diverse sights must yield a fix");
        assert_eq!(published.n_sights, 2);
        assert!(
            (published.azimuth_spread_rad - std::f64::consts::FRAC_PI_2).abs() < 1e-9,
            "expected 90° spread"
        );
    }

    #[test]
    fn try_publish_collects_one_contributing_frame_id_per_same_frame_sight() {
        // Each dummy_sight has source_frame_id == horizon_frame_id
        // (constructed by the test helper); a window of two
        // distinct dummy sights should produce exactly two
        // contributing frame IDs.
        let mut w = SightWindow::default();
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        w.try_insert(dummy_sight(0.001, std::f64::consts::FRAC_PI_2, 0.0), 5);
        let published = try_publish(&w, Some(Tt::from_julian_date(JD_J2000)))
            .expect("two diverse sights must yield a fix");
        assert_eq!(
            published.contributing_frame_ids.len(),
            2,
            "two same-frame sights should contribute exactly two frame IDs, got {:?}",
            published.contributing_frame_ids,
        );
        // Ensure de-duplication: re-insert one of the sights
        // (same frame_id derivation) and the count must not
        // grow.
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        let republished = try_publish(&w, Some(Tt::from_julian_date(JD_J2000))).unwrap();
        assert_eq!(
            republished.contributing_frame_ids.len(),
            2,
            "re-inserting a sight from the same frame should not duplicate its frame_id"
        );
    }

    #[test]
    fn try_publish_collects_both_frames_for_cross_frame_sight() {
        // Synthesize a cross-frame sight by hand-constructing
        // a Sight with distinct source_frame_id and
        // horizon_frame_id, then publishing alongside a
        // distinct same-frame sight. Expected: 3 unique frame
        // IDs (cross-frame contributes 2; same-frame
        // contributes 1).
        let cross_sight = Sight {
            lop: dummy_lop(0.5, 0.1, 0.0),
            anchor_tt: Tt::from_julian_date(JD_J2000),
            altitude_sigma_rad: 0.001,
            body: SightBody::SolarSystem(SolarSystemBody::Sun),
            azimuth_rad: 0.0,
            source_frame_id: FrameId(1000),
            horizon_frame_id: FrameId(1001),
        };
        let mut w = SightWindow::default();
        w.try_insert(cross_sight, 5);
        w.try_insert(dummy_sight(0.001, std::f64::consts::FRAC_PI_2, 0.0), 5);
        let published = try_publish(&w, Some(Tt::from_julian_date(JD_J2000)))
            .expect("two diverse sights must yield a fix");
        assert_eq!(
            published.contributing_frame_ids.len(),
            3,
            "cross-frame + same-frame should give three frame IDs, got {:?}",
            published.contributing_frame_ids,
        );
        assert!(published.contributing_frame_ids.contains(&1000));
        assert!(published.contributing_frame_ids.contains(&1001));
    }

    #[test]
    fn observer_unused_warning_silencer() {
        // Touch Observer::default_dev to ensure the test
        // harness pulls it in and we don't accumulate unused
        // imports as future tests are added.
        let _ = Observer::default_dev();
    }
}
