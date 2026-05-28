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
//! Both same-frame and cross-frame pairs are reduced into
//! sights. Same-frame pairs go directly through
//! [`bris_vision::measure_altitude`]; cross-frame pairs run
//! [`bris_vision::panorama_altitude_for_pair`], which composes
//! the (already-detected) body centroid and horizon line via
//! [`bris_vision::track_rotation`] (Kabsch over camera-space
//! ray pairs) and the ray-space altitude composition. The
//! helper returns an `Uncertain<f64>` whose σ honestly
//! combines body-centroid σ, horizon σ, and the executed
//! stitch σ (Kabsch per-correspondence RMS angular residual) —
//! superseding the cheap time-gap-based estimate used during
//! pair selection. Stage E only further combines this with the
//! apparent-place altitude σ to get the per-sight altitude σ,
//! so the stitch contribution is counted exactly once. The
//! resulting [`Sight`] has `source_frame_id` (body) different
//! from `horizon_frame_id`; the engine surfaces a count of
//! such sights via
//! [`crate::EngineDiagnostics::cross_frame_sights_emitted`].
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
//!
//! # GP correctness
//!
//! Geographic-position derivation for cold-start circles is
//! regression-tested against Skyfield + JPL DE421; see
//! `moon_geographic_position_matches_skyfield_reference` in the
//! tests module.

use super::queue::{BodyRecord, FrameId, HorizonRecord, Storage};
use crate::config::EngineConfig;
use crate::fix::{DominantSource, FixProvenance, PublishedFix};
use crate::pipeline::BodyDetection;
use bris_almanac::{
    body_apparent_place, body_geocentric_apparent, coord::gmst_rad, frame::nutation,
    mean_obliquity, star_apparent_place, star_geocentric_apparent, ApparentPlace, Observer,
    SolarSystemBody,
};
use bris_core::time::Tt;
use bris_core::{Sigma, Uncertain};
use bris_nav::{
    cold_start_fix, line_of_position, multi_sight_fix, CircleOfPosition, ColdStartConfig,
    ColdStartError, ColdStartResult, FixError, LineOfPosition,
};
use bris_vision::{
    measure_altitude, panorama_altitude_for_pair, Centroid, HorizonLine, PanoramaError, TrackConfig,
};
use std::time::{Duration, Instant};
use tracing::{debug, info, trace};

/// Cheap stitching σ estimate, in radians per second of
/// inter-frame time gap. A 1-second gap yields ~0.5 arcmin of
/// uncertainty by this model. Calibrated by analogy with the
/// CLI's panorama-stitching residuals; the design doc notes
/// this is a placeholder until a frame-to-frame motion-aware
/// estimate lands.
///
/// Used only for *pair-selection* prioritization. At
/// sight-emission time the cross-frame path executes
/// [`bris_vision::panorama_altitude_for_pair`] which derives
/// the actual stitch σ from the Kabsch per-correspondence RMS
/// residual; that executed σ supersedes this estimate for the
/// reported sight σ.
const STITCH_SIGMA_PER_SECOND_RAD: f64 = 0.5 * std::f64::consts::PI / (60.0 * 180.0);

/// Outcome of one Stage E run.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub(crate) struct StageEOutcome {
    /// Number of new sights inserted into the window during
    /// this run. Zero is normal (no new same-frame pair, or
    /// all candidates worse than the worst sight already in
    /// the window).
    pub sights_inserted: usize,
    /// Sight values that were inserted into the window this
    /// run, in insertion order. The engine persists each of
    /// these via `SightStore::append_sight`.
    pub inserted_sights: Vec<Sight>,
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
    /// - The publication gate (geometric diversity, ellipse
    ///   axis ratio, absolute σ, motion-staleness) rejected
    ///   the fix.
    pub published: Option<PublishedFix>,
    /// True iff Stage E attempted publication this run (i.e.
    /// the window changed, the throttle was clear, and it
    /// called `try_publish`). Counts the LSQ + gate as a
    /// single attempt.
    pub publish_attempted: bool,
    /// True iff `multi_sight_fix` rejected for singular
    /// geometry (or any other reason) on this run.
    pub singular_geometry_rejection: bool,
    /// True iff a fix was produced (Saint-Hilaire or cold-
    /// start) but the publication gate (azimuth spread /
    /// axis ratio / absolute σ / motion staleness) rejected
    /// it.
    pub publication_gate_rejection: bool,
    /// Cold-start solver attempted (`multi_sight_fix` declined
    /// AND cold-start was enabled AND we have >= 2 sights).
    pub cold_start_attempted: bool,
    /// Cold-start published (Fix or hemisphere-resolved).
    pub cold_start_published: bool,
    /// Cold-start returned `TwoCandidates` with no hemisphere
    /// hint and was skipped.
    pub cold_start_ambiguous_skipped: bool,
    /// Cold-start returned `Inconsistent`.
    pub cold_start_inconsistent: bool,
    /// Cold-start returned `Disjoint`.
    pub cold_start_disjoint: bool,
    /// Cold-start beat a successful Saint-Hilaire fix whose
    /// max |intercept| exceeded the stale-prior threshold and
    /// was published in its stead.
    pub cold_start_preferred_over_stale_sh: bool,
    /// AP re-derivation was suppressed by `lock_ap_for_replay`
    /// during this Stage E pass. Diagnostic-only.
    pub ap_rederive_suppressed: bool,
    /// Number of cross-frame sights inserted into the window
    /// during this run (subset of `sights_inserted`). Mirrors
    /// the engine-level counter
    /// [`crate::EngineDiagnostics::cross_frame_sights_emitted`].
    pub cross_frame_sights_emitted: u64,
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
    /// Bulk-insert pre-existing sights (e.g. from on-disk
    /// hydration) without bumping `pending_inserts` or
    /// honouring capacity. Truncates to `capacity` keeping
    /// the lowest-σ entries.
    pub(crate) fn hydrate(&mut self, mut sights: Vec<Sight>, capacity: usize) {
        sights.sort_by(|a, b| {
            a.altitude_sigma_rad
                .partial_cmp(&b.altitude_sigma_rad)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        sights.truncate(capacity);
        self.sights = sights;
    }

    /// Snapshot the current sight list (copy). For external
    /// surfaces (FFI `pool_sights`).
    pub(crate) fn snapshot(&self) -> Vec<Sight> {
        self.sights.clone()
    }

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
    has_prior: bool,
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
        match reduce_to_sight(&cand, storage, cfg) {
            Ok(sights) => {
                let is_cross = !is_same_frame(&cand);
                for sight in sights {
                    if window.try_insert(sight, cfg.sight_window_capacity) {
                        inserted += 1;
                        if is_cross {
                            out.cross_frame_sights_emitted += 1;
                        }
                        out.inserted_sights.push(sight);
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
    try_publish(window, now_tt, cfg, has_prior, &mut out);
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

/// Pick the direct sight from `sights` whose `body_pixel` is
/// closest to `target_pixel`. Returns `None` if `sights` is
/// empty. Used by Stage E to attribute the right direct sight
/// (when several providers each emitted one) to the body
/// candidate being reduced.
fn pick_direct_sight_for(
    sights: &[bris_vision::DirectSight],
    target_pixel: (f64, f64),
) -> Option<bris_vision::DirectSight> {
    sights
        .iter()
        .min_by(|a, b| {
            let da = (a.body_pixel.0 - target_pixel.0).powi(2)
                + (a.body_pixel.1 - target_pixel.1).powi(2);
            let db = (b.body_pixel.0 - target_pixel.0).powi(2)
                + (b.body_pixel.1 - target_pixel.1).powi(2);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied()
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
        BodyDetection::Day(centroid, _) => {
            // Day path: Sun (commit 5 simplification — see
            // module docs for the deferred Moon/planet work).
            let body = SolarSystemBody::Sun;
            let apparent: ApparentPlace =
                body_apparent_place(body, c.body.frame_tt, jd_ut1, observer)
                    .map_err(ReduceError::Apparent)?;
            let observed = if c.body.frame_id == c.horizon.frame_id {
                // Same-frame: prefer the horizon record's
                // direct sight when one is present (Phase 1:
                // reflection-pair provider emits Ho = θ/2
                // directly). See module docs for the
                // double-counting discussion.
                if let Some(direct) =
                    pick_direct_sight_for(&c.horizon.direct_sights, (centroid.x, centroid.y))
                {
                    direct.observed_altitude
                } else {
                    measure_altitude(intrinsics, c.horizon.line, *centroid)
                        .map_err(ReduceError::Measure)?
                }
            } else {
                // Cross-frame: execute the panorama stitch +
                // ray-space altitude composition. The helper's
                // returned σ already combines body centroid σ,
                // horizon altitude σ, and the executed stitch
                // σ (Kabsch RMS residual). Do not also combine
                // with `c.combined_sigma_rad` (that's the
                // pair-selection estimate, superseded here).
                let horizon_frame = storage
                    .frame(c.horizon.frame_id)
                    .ok_or(ReduceError::FrameEvicted)?;
                // TODO: expose TrackConfig on EngineConfig
                // once we have empirical guidance for the
                // streaming engine's frame regime.
                let track_cfg = TrackConfig::default();
                panorama_altitude_for_pair(
                    ring_frame.frame(),
                    *centroid,
                    horizon_frame.frame(),
                    c.horizon.line,
                    track_cfg,
                )
                .map_err(ReduceError::Stitch)?
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
    /// Cross-frame panorama stitching failed. Wraps the
    /// underlying [`PanoramaError`]; the Stage E call site
    /// logs and continues so the body record can be retried
    /// when a better-paired horizon arrives.
    Stitch(PanoramaError),
}

/// Run `multi_sight_fix` over the current window; build a
/// [`PublishedFix`] from the result. On singular geometry,
/// fall back to [`bris_nav::cold_start_fix`] when enabled.
/// Either successful fix is then run through the publication
/// gate (geometric diversity, ellipse axis ratio, absolute σ,
/// motion staleness) before being assigned to `out.published`.
///
/// Writes the published fix (if any) and the LSQ / cold-start /
/// gate counters into `out`.
fn try_publish(
    window: &SightWindow,
    now_tt: Option<Tt>,
    cfg: &EngineConfig,
    has_prior: bool,
    out: &mut StageEOutcome,
) {
    out.publish_attempted = true;
    let lops: Vec<LineOfPosition> = window.iter().map(|s| s.lop).collect();
    let saint_hilaire_err = match multi_sight_fix(&lops) {
        Ok(sh_fix) => {
            // Stale-prior trigger: a successful SH fix whose max
            // |intercept| exceeds the configured threshold (default
            // 60 nm ≈ 1°) implies the assumed position is so far off
            // that the LSQ linearization is suspect. Run cold-start
            // as a comparison; prefer it iff it converges with a
            // tighter sigma_major_nm. See
            // `docs/design/circle_of_position.md` "Engine integration".
            let max_intercept_nm = window
                .iter()
                .map(|s| s.lop.intercept_nm.abs())
                .fold(0.0_f64, f64::max);
            // Track the stale-prior trigger condition
            // independently of whether the lock suppresses
            // cold-start, so the diagnostic counter reflects
            // every site we *would* have re-derived AP from.
            let stale_trigger_fired =
                max_intercept_nm > cfg.cold_start.stale_prior_intercept_threshold_nm;
            if cfg.lock_ap_for_replay && stale_trigger_fired {
                out.ap_rederive_suppressed = true;
            }
            if cfg.cold_start.enabled && !cfg.lock_ap_for_replay && stale_trigger_fired {
                let circles = circles_from_sights(window, cfg.observer);
                if circles.len() >= 2 {
                    if let Some((cs_fix, cs_provenance)) = solve_cold_start(&circles, cfg, out) {
                        if cs_fix.sigma_major_nm < sh_fix.sigma_major_nm {
                            out.cold_start_preferred_over_stale_sh = true;
                            if apply_gate(out, cs_fix, window, now_tt, cfg, cs_provenance) {
                                out.cold_start_published = true;
                            }
                            return;
                        }
                    }
                }
            }
            apply_gate(
                out,
                sh_fix,
                window,
                now_tt,
                cfg,
                FixProvenance::SaintHilaire,
            );
            return;
        }
        Err(e) => e,
    };
    trace!(error = %saint_hilaire_err, "Stage E: multi_sight_fix declined");
    if matches!(saint_hilaire_err, FixError::SingularGeometry) {
        out.singular_geometry_rejection = true;
    }

    if !cfg.cold_start.enabled || cfg.lock_ap_for_replay {
        // AP lock: cold-start is itself a form of AP re-derivation
        // (it produces a fresh fix without referencing the prior
        // AP). Suppressed under the replay lock so the engine's
        // behaviour stays referenced to the seeded AP only.
        if cfg.lock_ap_for_replay {
            out.ap_rederive_suppressed = true;
        }
        return;
    }
    // Cold-start triggers when either (a) LSQ is singular OR
    // (b) the engine has no PositionPrior at all and SH could
    // not produce a fix (cold-start is the only way to bootstrap
    // the very first fix without a prior). The third trigger
    // — SH succeeded but |intercept| exceeds a configurable
    // threshold — is handled in the `Ok` arm above.
    let trigger_singular = matches!(saint_hilaire_err, FixError::SingularGeometry);
    let trigger_no_prior = !has_prior
        && matches!(
            saint_hilaire_err,
            FixError::SingularGeometry | FixError::InsufficientSights(_)
        );
    if !(trigger_singular || trigger_no_prior) {
        return;
    }
    let circles = circles_from_sights(window, cfg.observer);
    if circles.len() < 2 {
        return;
    }
    if let Some((fix, provenance)) = solve_cold_start(&circles, cfg, out) {
        if apply_gate(out, fix, window, now_tt, cfg, provenance) {
            out.cold_start_published = true;
        }
    }
}

/// Run `cold_start_fix` and translate its result into a
/// `(Fix, FixProvenance)` pair, updating the cold-start
/// outcome counters. Returns `None` for non-publishable
/// outcomes (Inconsistent / Disjoint / two-candidate without
/// a resolvable hemisphere hint).
fn solve_cold_start(
    circles: &[CircleOfPosition],
    cfg: &EngineConfig,
    out: &mut StageEOutcome,
) -> Option<(bris_nav::Fix, FixProvenance)> {
    out.cold_start_attempted = true;
    match cold_start_fix(circles, &ColdStartConfig::default()) {
        Ok(ColdStartResult::Fix(cand)) => Some((
            bris_nav::Fix {
                lat: cand.lat,
                lon: cand.lon,
                covariance_nm2: cand.covariance_nm2,
                sigma_major_nm: cand.sigma_major_nm.value(),
                sigma_minor_nm: cand.sigma_minor_nm.value(),
                orientation_rad: cand.orientation_rad,
                sight_count: u32::try_from(cand.sight_count).unwrap_or(u32::MAX),
            },
            FixProvenance::ColdStart,
        )),
        Ok(ColdStartResult::TwoCandidates {
            primary, secondary, ..
        }) => {
            let Some(hemi) = cfg.cold_start.coarse_hemisphere else {
                trace!(
                    "cold-start fix has 2 candidates, no hemisphere hint configured; not publishing"
                );
                out.cold_start_ambiguous_skipped = true;
                return None;
            };
            let chosen = if hemi.contains(primary.lat) {
                primary
            } else if hemi.contains(secondary.lat) {
                secondary
            } else {
                trace!(
                    primary_lat_deg = primary.lat.degrees(),
                    secondary_lat_deg = secondary.lat.degrees(),
                    "cold-start two candidates: neither in configured hemisphere; not publishing"
                );
                out.cold_start_ambiguous_skipped = true;
                return None;
            };
            Some((
                bris_nav::Fix {
                    lat: chosen.lat,
                    lon: chosen.lon,
                    covariance_nm2: chosen.covariance_nm2,
                    sigma_major_nm: chosen.sigma_major_nm.value(),
                    sigma_minor_nm: chosen.sigma_minor_nm.value(),
                    orientation_rad: chosen.orientation_rad,
                    sight_count: u32::try_from(chosen.sight_count).unwrap_or(u32::MAX),
                },
                FixProvenance::ColdStartAmbiguous,
            ))
        }
        Ok(ColdStartResult::Inconsistent { .. }) => {
            trace!("cold-start fix returned Inconsistent; not publishing");
            out.cold_start_inconsistent = true;
            None
        }
        Err(ColdStartError::Disjoint) => {
            trace!("cold-start fix returned Disjoint");
            out.cold_start_disjoint = true;
            None
        }
        Err(e) => {
            trace!(error = ?e, "cold-start fix errored");
            None
        }
    }
}

/// Apply the publication gate to `fix`. On pass, builds the
/// [`PublishedFix`] (with the given provenance), assigns it to
/// `out.published`, and returns `true`. On fail, sets
/// `out.publication_gate_rejection` and returns `false`.
fn apply_gate(
    out: &mut StageEOutcome,
    fix: bris_nav::Fix,
    window: &SightWindow,
    now_tt: Option<Tt>,
    cfg: &EngineConfig,
    provenance: FixProvenance,
) -> bool {
    let azimuth_spread_rad = azimuth_spread(window);
    let anchor = now_tt.unwrap_or_else(|| Tt::from_julian_date(bris_core::time::JD_J2000));
    let oldest_age = window
        .iter()
        .map(|s| time_gap_seconds(anchor, s.anchor_tt))
        .fold(0.0_f64, f64::max);
    // Publication gate: geometric diversity + ellipse ratio +
    // absolute σ + motion-staleness inflation. See
    // `docs/design/observer_motion_staleness.md`.
    let gate = cfg.publication_gate;
    let motion_sigma_nm = gate.assumed_max_speed_kn * oldest_age / 3600.0;
    let effective_sigma_major_nm = (fix.sigma_major_nm.powi(2) + motion_sigma_nm.powi(2)).sqrt();
    let axis_ratio = if fix.sigma_minor_nm > 0.0 {
        fix.sigma_major_nm / fix.sigma_minor_nm
    } else {
        f64::INFINITY
    };
    if azimuth_spread_rad < gate.min_azimuth_spread_rad
        || axis_ratio > gate.max_ellipse_axis_ratio
        || effective_sigma_major_nm > gate.max_position_sigma_nm
    {
        out.publication_gate_rejection = true;
        info!(
            spread_deg = azimuth_spread_rad.to_degrees(),
            axis_ratio,
            sigma_major_nm = fix.sigma_major_nm,
            effective_sigma_major_nm,
            motion_sigma_nm,
            oldest_age_s = oldest_age,
            "fix gated",
        );
        return false;
    }
    out.published = Some(build_published_inner(
        fix,
        window,
        anchor,
        azimuth_spread_rad,
        oldest_age,
        provenance,
    ));
    true
}

fn build_published_inner(
    fix: bris_nav::Fix,
    window: &SightWindow,
    anchor: Tt,
    azimuth_spread_rad: f64,
    oldest_age: f64,
    provenance: FixProvenance,
) -> PublishedFix {
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
    PublishedFix {
        fix,
        n_sights: window.len(),
        azimuth_spread_rad,
        oldest_sight_age_seconds: oldest_age,
        dominant_source: DominantSource::None,
        timestamp: anchor,
        contributing_frame_ids,
        provenance,
    }
}

/// Build [`CircleOfPosition`] records from the current sight
/// window by recomputing each sight's body GP (declination,
/// −GHA) at the sight's anchor instant and combining with the
/// observed altitude implied by the LOP.
///
/// The observed altitude is recovered as `Hc + intercept`,
/// where `Hc` is the apparent-place altitude at the LOP's
/// assumed position. This is exact — it inverts the intercept
/// computation in [`line_of_position`].
fn circles_from_sights(window: &SightWindow, observer: Observer) -> Vec<CircleOfPosition> {
    let mut out = Vec::with_capacity(window.len());
    for s in window.iter() {
        let jd_ut1 = s.anchor_tt.julian_date();
        // Body GP from the *geocentric* apparent place: latitude =
        // declination, longitude = -GHA. Running the full
        // apparent-place chain (which applies refraction + diurnal
        // parallax at the engine observer) would bake observer-
        // dependent biases into the GP — systematically wrong by
        // tens of arcmin (refraction at low altitude) and up to ~1°
        // (lunar parallax). See PR #22 review.
        let Some((gp_lat_rad, gp_lon_rad)) = body_geographic_position(s.body, s.anchor_tt, jd_ut1)
        else {
            continue;
        };
        // Observed altitude: assumed-position Hc + intercept.
        // Inverts the intercept = Ho - Hc relationship in
        // line_of_position.
        let assumed_observer = Observer {
            latitude: s.lop.assumed_lat,
            longitude: s.lop.assumed_lon,
            ..observer
        };
        let apparent_at_ap = match &s.body {
            SightBody::SolarSystem(body) => {
                body_apparent_place(*body, s.anchor_tt, jd_ut1, assumed_observer)
            }
            SightBody::Star { hr } => {
                let Some(rec) = bris_almanac::by_hr(*hr) else {
                    continue;
                };
                star_apparent_place(rec, s.anchor_tt, jd_ut1, assumed_observer)
            }
        };
        let Ok(ApparentPlace {
            direction: hc_dir, ..
        }) = apparent_at_ap
        else {
            continue;
        };
        // intercept (nm) -> radians: 1 arcmin = 1 nm.
        let intercept_rad = s.lop.intercept_nm * std::f64::consts::PI / (180.0 * 60.0);
        let ho_rad = hc_dir.altitude + intercept_rad;
        let co_altitude_rad = std::f64::consts::FRAC_PI_2 - ho_rad;
        if co_altitude_rad <= 0.0 || co_altitude_rad >= std::f64::consts::FRAC_PI_2 {
            continue;
        }
        out.push(CircleOfPosition {
            gp_lat_rad,
            gp_lon_rad,
            co_altitude_rad,
            sigma_rad: s.altitude_sigma_rad,
        });
    }
    out
}

/// Compute the body's geographic (sub-point) position at the
/// given instant: latitude = declination, longitude = -GHA
/// wrapped to (-π, π].
///
/// Derived from the GEOCENTRIC apparent equatorial-of-date
/// (RA, Dec) via [`bris_almanac::body_geocentric_apparent`] (or
/// the stellar sibling) and Greenwich Apparent Sidereal Time:
/// GHA = GAST - RA, GP longitude = -GHA. No observer, no
/// refraction, no diurnal parallax — those are observer-
/// dependent shifts and must not enter the GP.
fn body_geographic_position(body: SightBody, tt: Tt, jd_ut1: f64) -> Option<(f64, f64)> {
    let eq = match body {
        SightBody::SolarSystem(b) => body_geocentric_apparent(b, tt),
        SightBody::Star { hr } => {
            let rec = bris_almanac::by_hr(hr)?;
            star_geocentric_apparent(rec, tt)
        }
    };
    if !eq.ra.is_finite() || !eq.dec.is_finite() {
        return None;
    }
    // GAST = GMST + Δψ cosε (equation of the equinoxes).
    let nu = nutation(tt);
    let eps = mean_obliquity(tt);
    let gast = (gmst_rad(jd_ut1) + nu.delta_psi * eps.cos()).rem_euclid(std::f64::consts::TAU);
    let gha = (gast - eq.ra).rem_euclid(std::f64::consts::TAU);
    let mut lon = -gha;
    if lon <= -std::f64::consts::PI {
        lon += std::f64::consts::TAU;
    } else if lon > std::f64::consts::PI {
        lon -= std::f64::consts::TAU;
    }
    Some((eq.dec, lon))
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
    fn moon_geographic_position_matches_skyfield_reference() {
        // Reference values generated by
        // scripts/generate_moon_gp_reference.py against Skyfield +
        // JPL DE421 (canonical 1900-2050 ephemeris). Tolerance: 1'
        // = 1/60 deg. Skyfield apparent-of-date includes light-time
        // + nutation + annual aberration; bris-almanac applies the
        // same chain in `body_geocentric_apparent`. This test guards
        // against re-introducing the topocentric / refraction bias
        // fixed by PR #28 (would manifest as tens of arcmin or up to
        // ~1° of lunar parallax in the GP).
        let cases: &[(f64, f64, f64, f64)] = &[
            // (TT_JD, UT1_JD, lat_deg, lon_deg)
            // 2026-02-26T00:00:00Z  high northern dec
            (
                2_461_097.500_801,
                2_461_097.500_001,
                28.427_328,
                -69.330_669,
            ),
            // 2026-07-06T06:00:00Z  near equator
            (
                2_461_227.750_801,
                2_461_227.750_001,
                -0.008_296,
                -19.618_815,
            ),
            // 2026-02-12T12:00:00Z  high southern dec
            (
                2_461_084.000_801,
                2_461_084.000_001,
                -28.406_482,
                -56.948_854,
            ),
        ];
        for (tt_jd, ut1_jd, lat_ref, lon_ref) in cases.iter().copied() {
            let tt = Tt::from_julian_date(tt_jd);
            let (lat_rad, lon_rad) =
                body_geographic_position(SightBody::SolarSystem(SolarSystemBody::Moon), tt, ut1_jd)
                    .expect("geocentric apparent place should succeed");
            let lat_deg = lat_rad.to_degrees();
            let lon_deg = lon_rad.to_degrees();
            let dlat_arcmin = (lat_deg - lat_ref).abs() * 60.0;
            let raw_dlon = (lon_deg - lon_ref).abs();
            let dlon_deg = raw_dlon.min(360.0 - raw_dlon);
            let dlon_arcmin = dlon_deg * 60.0;
            assert!(
                dlat_arcmin < 1.0,
                "Moon GP at TT_JD={tt_jd}: lat={lat_deg:.6}° vs ref={lat_ref:.6}°, Δ={dlat_arcmin:.3}'"
            );
            assert!(
                dlon_arcmin < 1.0,
                "Moon GP at TT_JD={tt_jd}: lon={lon_deg:.6}° vs ref={lon_ref:.6}°, Δ={dlon_arcmin:.3}'"
            );
        }
    }

    #[test]
    fn sun_geographic_position_matches_skyfield_reference() {
        // Sun GP sanity check (no parallax, simpler than Moon).
        // Generated by scripts/generate_moon_gp_reference.py.
        // 2026-03-21T12:00:00Z (near equinox).
        let tt_jd = 2_461_121.000_801;
        let ut1_jd = 2_461_121.000_001;
        let lat_ref = 0.349_810_f64;
        let lon_ref = 1.785_050_f64;
        let tt = Tt::from_julian_date(tt_jd);
        let (lat_rad, lon_rad) =
            body_geographic_position(SightBody::SolarSystem(SolarSystemBody::Sun), tt, ut1_jd)
                .expect("sun geocentric apparent place should succeed");
        let lat_deg = lat_rad.to_degrees();
        let lon_deg = lon_rad.to_degrees();
        let dlat_arcmin = (lat_deg - lat_ref).abs() * 60.0;
        let raw_dlon = (lon_deg - lon_ref).abs();
        let dlon_arcmin = raw_dlon.min(360.0 - raw_dlon) * 60.0;
        assert!(
            dlat_arcmin < 1.0,
            "Sun GP: lat={lat_deg:.6}° vs ref={lat_ref:.6}°, Δ={dlat_arcmin:.3}'"
        );
        assert!(
            dlon_arcmin < 1.0,
            "Sun GP: lon={lon_deg:.6}° vs ref={lon_ref:.6}°, Δ={dlon_arcmin:.3}'"
        );
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

    fn test_cfg_no_cold_start() -> EngineConfig {
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.cold_start.enabled = false;
        cfg
    }

    fn run_try_publish(window: &SightWindow, now: Option<Tt>) -> Option<PublishedFix> {
        let mut out = StageEOutcome::default();
        try_publish(window, now, &test_cfg_no_cold_start(), true, &mut out);
        out.published
    }

    #[test]
    fn try_publish_returns_none_for_singleton_window() {
        let mut w = SightWindow::default();
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        let now = Some(Tt::from_julian_date(JD_J2000));
        assert!(
            run_try_publish(&w, now).is_none(),
            "single-sight window cannot publish (LSQ needs >= 2 LOPs)"
        );
    }

    #[test]
    fn try_publish_succeeds_for_two_sights_with_diversity() {
        let mut w = SightWindow::default();
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        w.try_insert(dummy_sight(0.001, std::f64::consts::FRAC_PI_2, 0.0), 5);
        let now = Some(Tt::from_julian_date(JD_J2000));
        let published = run_try_publish(&w, now).expect("two diverse sights must yield a fix");
        assert_eq!(published.n_sights, 2);
        assert!(
            (published.azimuth_spread_rad - std::f64::consts::FRAC_PI_2).abs() < 1e-9,
            "expected 90° spread"
        );
        assert!(matches!(published.provenance, FixProvenance::SaintHilaire));
    }

    #[test]
    fn try_publish_collects_one_contributing_frame_id_per_same_frame_sight() {
        let mut w = SightWindow::default();
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        w.try_insert(dummy_sight(0.001, std::f64::consts::FRAC_PI_2, 0.0), 5);
        let published = run_try_publish(&w, Some(Tt::from_julian_date(JD_J2000)))
            .expect("two diverse sights must yield a fix");
        assert_eq!(published.contributing_frame_ids.len(), 2);
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        let republished = run_try_publish(&w, Some(Tt::from_julian_date(JD_J2000))).unwrap();
        assert_eq!(republished.contributing_frame_ids.len(), 2);
    }

    #[test]
    fn try_publish_collects_both_frames_for_cross_frame_sight() {
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
        let published = run_try_publish(&w, Some(Tt::from_julian_date(JD_J2000)))
            .expect("two diverse sights must yield a fix");
        assert_eq!(published.contributing_frame_ids.len(), 3);
        assert!(published.contributing_frame_ids.contains(&1000));
        assert!(published.contributing_frame_ids.contains(&1001));
    }

    /// Construct a sight from a Sun apparent-place at the
    /// given AP and time. Used by cold-start fallback tests
    /// where the sights need realistic GPs so
    /// `circles_from_sights` produces meaningful circles.
    fn realistic_sun_sight(
        ap_lat_deg: f64,
        ap_lon_deg: f64,
        tt_offset_s: f64,
        frame_id: u64,
    ) -> Sight {
        let tt = Tt::from_julian_date(JD_J2000 + tt_offset_s / 86_400.0);
        let jd_ut1 = tt.julian_date();
        let ap_lat = bris_core::Latitude::from_degrees(ap_lat_deg).unwrap();
        let ap_lon = bris_core::Longitude::from_degrees(ap_lon_deg).unwrap();
        let mut observer = Observer::default_dev();
        observer.latitude = ap_lat;
        observer.longitude = ap_lon;
        let apparent = body_apparent_place(SolarSystemBody::Sun, tt, jd_ut1, observer).unwrap();
        // Synthesize observed = computed (zero intercept) so
        // circles_from_sights recovers Ho = Hc and the
        // cold-start fix should land near the AP.
        let obs_alt = Uncertain::new(apparent.direction.altitude, Sigma::new(1e-4).unwrap());
        let computed = Uncertain::new(apparent.direction.altitude, apparent.altitude_sigma);
        let lop = line_of_position(
            ap_lat,
            ap_lon,
            obs_alt,
            computed,
            apparent.direction.azimuth,
        )
        .unwrap();
        Sight {
            lop,
            anchor_tt: tt,
            altitude_sigma_rad: 1e-4,
            body: SightBody::SolarSystem(SolarSystemBody::Sun),
            azimuth_rad: apparent.direction.azimuth,
            source_frame_id: FrameId(frame_id),
            horizon_frame_id: FrameId(frame_id),
        }
    }

    /// Build a Sun sight at the given anchor time with a fixed
    /// LOP azimuth and zero intercept anchored at the AP. The
    /// LOP azimuth is controlled independently of the Sun's
    /// actual azimuth so callers can force parallel LOPs (→
    /// singular LSQ) while still having two distinct body GPs
    /// for the cold-start circle solver to chew on.
    fn sun_sight_with_fixed_lop_azimuth(
        ap_lat_deg: f64,
        ap_lon_deg: f64,
        tt_offset_s: f64,
        lop_azimuth_rad: f64,
        frame_id: u64,
    ) -> Sight {
        let tt = Tt::from_julian_date(JD_J2000 + tt_offset_s / 86_400.0);
        let ap_lat = bris_core::Latitude::from_degrees(ap_lat_deg).unwrap();
        let ap_lon = bris_core::Longitude::from_degrees(ap_lon_deg).unwrap();
        let lop = LineOfPosition {
            assumed_lat: ap_lat,
            assumed_lon: ap_lon,
            azimuth_rad: lop_azimuth_rad,
            intercept_nm: 0.0,
            intercept_sigma_nm: Sigma::new(0.1).unwrap(),
        };
        Sight {
            lop,
            anchor_tt: tt,
            altitude_sigma_rad: 1e-4,
            body: SightBody::SolarSystem(SolarSystemBody::Sun),
            azimuth_rad: lop_azimuth_rad,
            source_frame_id: FrameId(frame_id),
            horizon_frame_id: FrameId(frame_id),
        }
    }

    #[test]
    fn cold_start_path_publishes_when_multi_sight_fix_singular() {
        // Two Sun sights with PARALLEL LOPs (same azimuth) →
        // rank-deficient design → multi_sight_fix returns
        // SingularGeometry. Anchor times an hour apart so the
        // Sun's geocentric GP moves ≈15° in longitude, giving
        // cold-start two distinct circles to intersect.
        let s1 = sun_sight_with_fixed_lop_azimuth(-23.0, 0.0, 0.0, 0.0, 1);
        let s2 = sun_sight_with_fixed_lop_azimuth(-23.0, 0.0, 3600.0, 0.0, 2);
        let mut w = SightWindow::default();
        w.try_insert(s1, 5);
        w.try_insert(s2, 5);
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.cold_start.enabled = true;
        cfg.cold_start.coarse_hemisphere = Some(bris_core::Hemisphere::South);
        // Cold-start from two same-body sights gives a wide
        // ellipse / low azimuth spread that the publication
        // gate would reject; the gate is orthogonal to this
        // test, so disable it.
        cfg.publication_gate.min_azimuth_spread_rad = 0.0;
        cfg.publication_gate.max_ellipse_axis_ratio = f64::INFINITY;
        cfg.publication_gate.max_position_sigma_nm = f64::INFINITY;
        let mut out = StageEOutcome::default();
        try_publish(&w, Some(s2.anchor_tt), &cfg, true, &mut out);
        // Either Saint-Hilaire succeeded (unlikely with same-
        // body 60 s apart) OR cold-start published.
        assert!(
            out.published.is_some(),
            "expected a published fix via cold-start fallback"
        );
        assert!(
            out.cold_start_attempted,
            "cold-start path must be attempted with two colocated-GP sights"
        );
        assert!(out.cold_start_published);
        let p = out.published.unwrap();
        assert!(
            matches!(
                p.provenance,
                FixProvenance::ColdStart | FixProvenance::ColdStartAmbiguous
            ),
            "published provenance must be ColdStart*, got {:?}",
            p.provenance
        );
    }

    #[test]
    fn cold_start_ambiguous_without_hint_skips_publication() {
        let s1 = sun_sight_with_fixed_lop_azimuth(-23.0, 0.0, 0.0, 0.0, 1);
        let s2 = sun_sight_with_fixed_lop_azimuth(-23.0, 0.0, 3600.0, 0.0, 2);
        let mut w = SightWindow::default();
        w.try_insert(s1, 5);
        w.try_insert(s2, 5);
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.cold_start.enabled = true;
        cfg.cold_start.coarse_hemisphere = None;
        let mut out = StageEOutcome::default();
        try_publish(&w, Some(s2.anchor_tt), &cfg, true, &mut out);
        assert!(
            out.cold_start_attempted,
            "cold-start path must be attempted with two colocated-GP sights"
        );
        assert!(
            out.cold_start_ambiguous_skipped,
            "ambiguous-no-hint path must mark cold_start_ambiguous_skipped"
        );
        assert!(
            out.published.is_none(),
            "no fix may publish without a hemisphere hint"
        );
    }

    #[test]
    fn try_publish_rejects_low_azimuth_spread() {
        let mut w = SightWindow::default();
        // Two sights only 10° apart — below the default 30°
        // gate. multi_sight_fix may still accept the geometry
        // (it's not strictly singular), but the gate must
        // reject for inadequate diversity.
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        w.try_insert(dummy_sight(0.001, 10.0_f64.to_radians(), 0.0), 5);
        let cfg = EngineConfig::new(Observer::default_dev());
        let mut out = StageEOutcome::default();
        try_publish(
            &w,
            Some(Tt::from_julian_date(JD_J2000)),
            &cfg,
            true,
            &mut out,
        );
        assert!(out.published.is_none(), "10°-spread fix must be gated");
        assert!(out.publish_attempted);
        // Either the LSQ rejected (singular) or the gate did;
        // both are valid — the gate counter should be set when
        // multi_sight_fix accepted.
        assert!(out.publication_gate_rejection || out.singular_geometry_rejection);
    }

    #[test]
    fn try_publish_rejects_huge_sigma() {
        let mut w = SightWindow::default();
        // Two diverse sights but with multi-NM intercept
        // sigmas — the resulting position ellipse exceeds
        // the absolute σ gate.
        let bad = |az: f64| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let id = FrameId(((az * 1000.0).abs() as u64).wrapping_add(1));
            Sight {
                lop: dummy_lop(0.5, 1000.0, az),
                anchor_tt: Tt::from_julian_date(JD_J2000),
                altitude_sigma_rad: 0.01,
                body: SightBody::SolarSystem(SolarSystemBody::Sun),
                azimuth_rad: az,
                source_frame_id: id,
                horizon_frame_id: id,
            }
        };
        w.try_insert(bad(0.0), 5);
        w.try_insert(bad(std::f64::consts::FRAC_PI_2), 5);
        let cfg = EngineConfig::new(Observer::default_dev());
        let mut out = StageEOutcome::default();
        try_publish(
            &w,
            Some(Tt::from_julian_date(JD_J2000)),
            &cfg,
            true,
            &mut out,
        );
        assert!(out.published.is_none(), "huge-σ fix must be gated");
        assert!(out.publication_gate_rejection);
    }

    #[test]
    fn try_publish_rejects_motion_stale() {
        let mut w = SightWindow::default();
        // Two diverse sights, modest σ, but the oldest is
        // 2 hours old and we assume 30 kn. Motion inflation:
        // 30 kn * 7200 s / 3600 = 60 nm > 50 nm gate.
        w.try_insert(dummy_sight(0.001, 0.0, 7200.0), 5);
        w.try_insert(dummy_sight(0.001, std::f64::consts::FRAC_PI_2, 0.0), 5);
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.publication_gate.assumed_max_speed_kn = 30.0;
        let mut out = StageEOutcome::default();
        // anchor = JD_J2000 (the "younger" sight's anchor);
        // the 7200-s-old sight is 7200s behind it.
        try_publish(
            &w,
            Some(Tt::from_julian_date(JD_J2000)),
            &cfg,
            true,
            &mut out,
        );
        assert!(out.published.is_none(), "motion-stale fix must be gated");
        assert!(out.publication_gate_rejection);
    }

    #[test]
    fn try_publish_accepts_when_all_gates_pass() {
        let mut w = SightWindow::default();
        w.try_insert(dummy_sight(0.001, 0.0, 0.0), 5);
        w.try_insert(dummy_sight(0.001, std::f64::consts::FRAC_PI_2, 0.0), 5);
        let cfg = EngineConfig::new(Observer::default_dev());
        let mut out = StageEOutcome::default();
        try_publish(
            &w,
            Some(Tt::from_julian_date(JD_J2000)),
            &cfg,
            true,
            &mut out,
        );
        assert!(out.published.is_some(), "clean fix must publish");
        assert!(out.publish_attempted);
        assert!(!out.publication_gate_rejection);
        assert!(!out.singular_geometry_rejection);
    }

    /// Build a Sun sight whose Ho is computed at
    /// `true_lat_deg`/`true_lon_deg` but whose LOP is anchored
    /// at the AP. The resulting LOP carries a large intercept
    /// proportional to the AP-to-true-position offset.
    #[allow(clippy::similar_names)]
    fn sun_sight_with_offset_ap(
        true_lat_deg: f64,
        true_lon_deg: f64,
        ap_lat_deg: f64,
        ap_lon_deg: f64,
        tt_offset_s: f64,
        frame_id: u64,
    ) -> Sight {
        let tt = Tt::from_julian_date(JD_J2000 + tt_offset_s / 86_400.0);
        let jd_ut1 = tt.julian_date();
        let true_lat = bris_core::Latitude::from_degrees(true_lat_deg).unwrap();
        let true_lon = bris_core::Longitude::from_degrees(true_lon_deg).unwrap();
        let ap_lat = bris_core::Latitude::from_degrees(ap_lat_deg).unwrap();
        let ap_lon = bris_core::Longitude::from_degrees(ap_lon_deg).unwrap();
        let mut true_obs = Observer::default_dev();
        true_obs.latitude = true_lat;
        true_obs.longitude = true_lon;
        let mut ap_obs = Observer::default_dev();
        ap_obs.latitude = ap_lat;
        ap_obs.longitude = ap_lon;
        let ap_true = body_apparent_place(SolarSystemBody::Sun, tt, jd_ut1, true_obs).unwrap();
        let ap_ap = body_apparent_place(SolarSystemBody::Sun, tt, jd_ut1, ap_obs).unwrap();
        let obs_alt = Uncertain::new(ap_true.direction.altitude, Sigma::new(1e-4).unwrap());
        let computed = Uncertain::new(ap_ap.direction.altitude, ap_ap.altitude_sigma);
        let lop =
            line_of_position(ap_lat, ap_lon, obs_alt, computed, ap_ap.direction.azimuth).unwrap();
        Sight {
            lop,
            anchor_tt: tt,
            altitude_sigma_rad: 1e-4,
            body: SightBody::SolarSystem(SolarSystemBody::Sun),
            azimuth_rad: ap_ap.direction.azimuth,
            source_frame_id: FrameId(frame_id),
            horizon_frame_id: FrameId(frame_id),
        }
    }

    fn distance_nm(
        lat1: bris_core::Latitude,
        lon1: bris_core::Longitude,
        lat2: bris_core::Latitude,
        lon2: bris_core::Longitude,
    ) -> f64 {
        let lat1r = lat1.radians();
        let lat2r = lat2.radians();
        let dlat = lat2r - lat1r;
        let dlon = lon2.radians() - lon1.radians();
        let a = (dlat / 2.0).sin().powi(2) + lat1r.cos() * lat2r.cos() * (dlon / 2.0).sin().powi(2);
        let c = 2.0 * a.sqrt().asin();
        c.to_degrees() * 60.0
    }

    #[test]
    fn ap_lock_for_replay_suppresses_cold_start() {
        // Same stale-prior scenario as
        // `cold_start_preferred_when_sh_intercept_exceeds_threshold`,
        // but with `lock_ap_for_replay = true`. Cold-start must
        // NOT run; the SH fix (offset from truth) publishes
        // instead and the suppression counter increments.
        let true_lat = -23.0;
        let true_lon = 0.0;
        let ap_lat = -10.0;
        let ap_lon = 10.0;
        let s1 = sun_sight_with_offset_ap(true_lat, true_lon, ap_lat, ap_lon, 0.0, 1);
        let s2 = sun_sight_with_offset_ap(true_lat, true_lon, ap_lat, ap_lon, 1.5 * 3600.0, 2);
        let s3 = sun_sight_with_offset_ap(true_lat, true_lon, ap_lat, ap_lon, 3.0 * 3600.0, 3);
        let mut w = SightWindow::default();
        w.try_insert(s1, 5);
        w.try_insert(s2, 5);
        w.try_insert(s3, 5);
        let mut observer = Observer::default_dev();
        observer.latitude = bris_core::Latitude::from_degrees(ap_lat).unwrap();
        observer.longitude = bris_core::Longitude::from_degrees(ap_lon).unwrap();
        let mut cfg = EngineConfig::new(observer);
        cfg.cold_start.enabled = true;
        cfg.cold_start.coarse_hemisphere = Some(bris_core::Hemisphere::South);
        cfg.cold_start.stale_prior_intercept_threshold_nm = 60.0;
        cfg.publication_gate.min_azimuth_spread_rad = 0.0;
        cfg.publication_gate.max_ellipse_axis_ratio = f64::INFINITY;
        cfg.publication_gate.max_position_sigma_nm = f64::INFINITY;
        cfg.lock_ap_for_replay = true;
        let mut out = StageEOutcome::default();
        try_publish(&w, Some(s3.anchor_tt), &cfg, true, &mut out);
        assert!(
            !out.cold_start_attempted,
            "cold-start must NOT run under lock_ap_for_replay"
        );
        assert!(out.ap_rederive_suppressed, "suppression flag must be set");
        let p = out.published.expect("SH fix must still publish");
        assert!(matches!(p.provenance, FixProvenance::SaintHilaire));
    }

    #[test]
    fn cold_start_preferred_when_sh_intercept_exceeds_threshold() {
        // J2000 noon TT: Sun declination ≈ -23° (winter
        // solstice). True position: -23°/0° (Sun near zenith
        // at t=0). AP: -21° / +2°, ≈ 170 nm off. Sights 1.5 hr
        // apart to keep Sun comfortably above horizon at all
        // three anchors.
        let true_lat = -23.0;
        let true_lon = 0.0;
        let ap_lat = -10.0;
        let ap_lon = 10.0;
        let s1 = sun_sight_with_offset_ap(true_lat, true_lon, ap_lat, ap_lon, 0.0, 1);
        let s2 = sun_sight_with_offset_ap(true_lat, true_lon, ap_lat, ap_lon, 1.5 * 3600.0, 2);
        let s3 = sun_sight_with_offset_ap(true_lat, true_lon, ap_lat, ap_lon, 3.0 * 3600.0, 3);
        let mut w = SightWindow::default();
        w.try_insert(s1, 5);
        w.try_insert(s2, 5);
        w.try_insert(s3, 5);
        let max_ic = w
            .iter()
            .map(|s| s.lop.intercept_nm.abs())
            .fold(0.0, f64::max);
        assert!(
            max_ic > 60.0,
            "test precondition: max |intercept| ({max_ic} nm) must exceed 60 nm"
        );
        let mut observer = Observer::default_dev();
        observer.latitude = bris_core::Latitude::from_degrees(ap_lat).unwrap();
        observer.longitude = bris_core::Longitude::from_degrees(ap_lon).unwrap();
        let mut cfg = EngineConfig::new(observer);
        cfg.cold_start.enabled = true;
        cfg.cold_start.coarse_hemisphere = Some(bris_core::Hemisphere::South);
        cfg.cold_start.stale_prior_intercept_threshold_nm = 60.0;
        cfg.publication_gate.min_azimuth_spread_rad = 0.0;
        cfg.publication_gate.max_ellipse_axis_ratio = f64::INFINITY;
        cfg.publication_gate.max_position_sigma_nm = f64::INFINITY;
        let mut out = StageEOutcome::default();
        try_publish(&w, Some(s3.anchor_tt), &cfg, true, &mut out);
        assert!(out.cold_start_attempted, "cold-start must be attempted");
        let p = out.published.expect("a fix must be published");
        assert!(
            matches!(
                p.provenance,
                FixProvenance::ColdStart | FixProvenance::ColdStartAmbiguous
            ),
            "provenance must be ColdStart*, got {:?}",
            p.provenance
        );
        assert!(out.cold_start_preferred_over_stale_sh);
        let true_lat_t = bris_core::Latitude::from_degrees(true_lat).unwrap();
        let true_lon_t = bris_core::Longitude::from_degrees(true_lon).unwrap();
        let ap_lat_t = bris_core::Latitude::from_degrees(ap_lat).unwrap();
        let ap_lon_t = bris_core::Longitude::from_degrees(ap_lon).unwrap();
        let d_true = distance_nm(p.fix.lat, p.fix.lon, true_lat_t, true_lon_t);
        let d_ap = distance_nm(p.fix.lat, p.fix.lon, ap_lat_t, ap_lon_t);
        assert!(
            d_true < d_ap,
            "cold-start fix should be closer to true ({d_true} nm) than AP ({d_ap} nm)"
        );
    }

    #[test]
    fn sh_kept_when_intercept_below_threshold() {
        // Sun-zenith position at J2000 for stable visibility.
        let lat = -23.0;
        let lon = 0.0;
        let s1 = sun_sight_with_offset_ap(lat, lon, lat, lon, 0.0, 1);
        let s2 = sun_sight_with_offset_ap(lat, lon, lat, lon, 1.5 * 3600.0, 2);
        let s3 = sun_sight_with_offset_ap(lat, lon, lat, lon, 3.0 * 3600.0, 3);
        let mut w = SightWindow::default();
        w.try_insert(s1, 5);
        w.try_insert(s2, 5);
        w.try_insert(s3, 5);
        let max_ic = w
            .iter()
            .map(|s| s.lop.intercept_nm.abs())
            .fold(0.0, f64::max);
        assert!(
            max_ic < 60.0,
            "test precondition: max |intercept| ({max_ic} nm) must be below 60 nm"
        );
        let mut observer = Observer::default_dev();
        observer.latitude = bris_core::Latitude::from_degrees(lat).unwrap();
        observer.longitude = bris_core::Longitude::from_degrees(lon).unwrap();
        let mut cfg = EngineConfig::new(observer);
        cfg.cold_start.enabled = true;
        cfg.cold_start.stale_prior_intercept_threshold_nm = 60.0;
        cfg.publication_gate.min_azimuth_spread_rad = 0.0;
        cfg.publication_gate.max_ellipse_axis_ratio = f64::INFINITY;
        cfg.publication_gate.max_position_sigma_nm = f64::INFINITY;
        let mut out = StageEOutcome::default();
        try_publish(&w, Some(s3.anchor_tt), &cfg, true, &mut out);
        let p = out
            .published
            .expect("SH fix must publish for small intercepts");
        assert!(matches!(p.provenance, FixProvenance::SaintHilaire));
        assert!(!out.cold_start_attempted);
        assert!(!out.cold_start_preferred_over_stale_sh);
    }

    #[test]
    fn observer_unused_warning_silencer() {
        // Touch Observer::default_dev to ensure the test
        // harness pulls it in and we don't accumulate unused
        // imports as future tests are added.
        let _ = Observer::default_dev();
    }
}
