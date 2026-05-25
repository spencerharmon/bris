//! Body & horizon priority queues, raw-frame ring buffer, and
//! eviction policy.
//!
//! See `docs/design/frame_scheduling.md` "Ring buffer and frame
//! eviction" and "Multi-body fix combination math" sections for
//! the rationale behind these structures. This module is pure
//! data-structure code; no detectors run here, no fixes are
//! computed. The engine's worker calls into [`Storage`] after
//! each pipeline pass to:
//!
//! 1. Insert the raw frame into the ring buffer (via
//!    [`Storage::admit_frame`]).
//! 2. Insert any body/horizon records produced by Stage B/C
//!    into the corresponding priority queues (via
//!    [`Storage::admit_records`]).
//! 3. Garbage-collect ring entries no longer needed (via
//!    [`Storage::evict`]).
//!
//! Priority is keyed on σ (smaller is better). Same-σ records
//! tie-break on `frame_id` for determinism.
//!
//! # Eviction policy
//!
//! Per design-doc §"Ring buffer and frame eviction", a frame is
//! evictable when **both**:
//!
//! 1. No body or horizon record from this frame is in either
//!    queue, AND
//! 2. No queue record has this frame as its closest viable
//!    stitching partner.
//!
//! For commit 4 we implement the slightly conservative "within
//! the stitching window" version of (2): a frame is protected
//! iff some queue record's `frame_tt` is within
//! [`crate::EngineConfig::stitching_window_seconds`] of this
//! frame's capture time. The strict "closest viable partner"
//! reading is an optimization for Phase 4 follow-up; until
//! memory pressure is measured, the over-conservative rule
//! keeps more candidate intermediaries alive at low extra
//! cost.

use crate::pipeline::{BodyDetection, HorizonStageOutcome};
use bris_core::time::Tt;
use bris_core::Sigma;
use bris_vision::{Frame, HorizonLine};
use std::collections::HashSet;
use std::fmt;

/// Monotonic per-engine identifier for a captured frame.
///
/// Assigned by the engine when the frame is pushed; threaded
/// through the pipeline so that body/horizon records can refer
/// back to the raw frame they came from. Wraps a `u64` so that
/// at the engine's design throughput (60 fps × 24 h × 365 d) the
/// counter has > 100 years of headroom before wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct FrameId(pub u64);

impl fmt::Display for FrameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "frame#{}", self.0)
    }
}

/// One frame retained in the ring buffer for later use as a
/// stitching intermediary.
///
/// We hold the full [`Frame`] (pixels + intrinsics + timestamps)
/// because Stage E's `panorama_altitude` re-reads pixel data to
/// compute the cross-frame alignment. An optimization for Phase
/// 4 is to retain a downscaled copy or pre-computed feature
/// pyramid alongside the original; for commit 4 we keep things
/// simple and store the full frame.
///
/// As of step 3b of the per-stage-resolution overhaul, the
/// underlying frame is wrapped in a [`bris_vision::FramePyramid`]
/// so downstream stages that prefer a downsampled view (e.g.
/// horizon detection, segmentation) can request a cached
/// pyramid level rather than re-downsampling on every read.
/// The full-resolution frame remains available via
/// [`RingFrame::frame`]; the pyramid is reachable via
/// [`RingFrame::pyramid`].
#[derive(Debug)]
pub(crate) struct RingFrame {
    /// Engine-assigned id.
    pub(crate) frame_id: FrameId,
    /// Capture instant (mid-exposure, TT). Mirrors
    /// [`Frame::capture_tt`] so eviction checks don't need to
    /// dig into the frame to compute time gaps.
    pub(crate) frame_tt: Tt,
    /// The raw frame, wrapped in a per-stage downsample cache.
    /// Direct field access remains crate-private; consumers
    /// should reach the source frame through [`Self::frame`]
    /// (zero-cost) or a downsampled level through
    /// [`Self::pyramid`].
    pub(crate) pyramid: bris_vision::FramePyramid,
}

impl RingFrame {
    /// Borrow the source (full-resolution) frame. Equivalent
    /// to `self.pyramid.full()` and zero cost.
    pub(crate) fn frame(&self) -> &Frame {
        self.pyramid.full()
    }

    /// Borrow the pyramid for downsampled-level access.
    pub(crate) fn pyramid(&self) -> &bris_vision::FramePyramid {
        &self.pyramid
    }
}

/// One body detection retained for Stage E pairing.
///
/// `sigma_key` is the value used for priority-queue ordering. We
/// derive it from the underlying detection's reported σ:
///
/// - [`BodyDetection::Day`]: pixel-level position σ from the
///   centroider. Smaller is better.
/// - [`BodyDetection::Night`]: peak count drives an effective σ
///   for the *frame-as-a-whole* (more peaks = better plate-solve
///   prospects = lower σ at Stage E). For commit 4 we use a
///   simple monotone transform `1 / sqrt(peak_count)` with a
///   floor; commit 6 (plate solving) refines this to actual
///   per-star σ post-solve.
///
/// The σ stored here is the *priority key*, not the final
/// per-sight angular σ. Stage E does the pixel→angular
/// conversion and combines with horizon σ + stitching σ to
/// get the per-sight altitude σ.
#[derive(Debug)]
pub(crate) struct BodyRecord {
    /// Source frame in the ring buffer.
    pub(crate) frame_id: FrameId,
    /// Mirror of the source frame's capture time (cheap copy
    /// avoids ring-lookup in the hot path of pair selection).
    pub(crate) frame_tt: Tt,
    /// The detection payload.
    pub(crate) detection: BodyDetection,
    /// Priority key. Smaller = better. Always finite,
    /// non-negative.
    pub(crate) sigma_key: SigmaKey,
}

/// One horizon detection retained for Stage E pairing.
#[derive(Debug)]
pub(crate) struct HorizonRecord {
    pub(crate) frame_id: FrameId,
    pub(crate) frame_tt: Tt,
    /// The detected horizon line.
    pub(crate) line: HorizonLine,
    /// Priority key. Smaller = better. Always finite,
    /// non-negative.
    pub(crate) sigma_key: SigmaKey,
    /// Optional direct sight emitted alongside this horizon
    /// (Phase 1 of the horizon-providers roadmap: the
    /// reflection-pair provider). When present, Stage E uses
    /// it directly instead of computing an altitude via
    /// `measure_altitude` for the participating body.
    pub(crate) direct_sight: Option<bris_vision::DirectSight>,
}

/// Total-ordered, NaN-free σ used as priority-queue key.
///
/// Stored as `f64` and constructed via [`SigmaKey::from_sigma`]
/// (which handles the [`Sigma`] → key conversion) or
/// [`SigmaKey::from_f64`] (for the night-path approximation).
/// `Ord` is implemented by total comparison; NaN is rejected at
/// construction time so the impl never has to handle it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SigmaKey(f64);

impl SigmaKey {
    /// Construct from a [`Sigma`]. [`Sigma::value`] is always
    /// finite and non-negative by construction, so this is
    /// infallible.
    pub(crate) fn from_sigma(s: Sigma) -> Self {
        Self(s.value())
    }

    /// Construct from a raw `f64`. NaN is mapped to
    /// `f64::INFINITY` so that NaN-σ records sort last and
    /// don't poison the queue ordering. Negative values are
    /// clamped to zero (defensive; should not occur for σ).
    pub(crate) fn from_f64(v: f64) -> Self {
        if v.is_nan() {
            Self(f64::INFINITY)
        } else if v < 0.0 {
            Self(0.0)
        } else {
            Self(v)
        }
    }

    /// The underlying scalar.
    pub(crate) fn value(self) -> f64 {
        self.0
    }
}

impl Eq for SigmaKey {}

impl PartialOrd for SigmaKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SigmaKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // total_cmp handles ±0, ±inf, and (post-construction)
        // NaN-free f64 deterministically.
        self.0.total_cmp(&other.0)
    }
}

/// Sorted-by-σ collection of body or horizon records.
///
/// Insertion is O(N) (binary-search-find-position +
/// `Vec::insert`). Pop-best is O(N) (`Vec::remove(0)`). Removal
/// by frame id is O(N) (linear scan). All N values in scope
/// are bounded by the ring-buffer capacity (default 120
/// records), so the constant factors dominate over asymptotic
/// concerns. Using a [`std::collections::BinaryHeap`] would
/// give O(log N) push but O(N) arbitrary-removal (frame
/// eviction needs to drop records by `frame_id`, not by σ);
/// `BTreeSet` would give O(log N) for both but at the cost of
/// requiring a `Ord` impl on the record type (the record's
/// natural ordering is by σ but Rust's `BTreeSet` treats equal
/// keys as duplicates, which a body or horizon queue can have).
///
/// The sorted-`Vec` choice keeps the implementation
/// straightforward and fast for the bounded-N regime; revisit
/// if profiling ever shows it as a hot spot.
#[derive(Debug, Default)]
pub(crate) struct PriorityQueue<R: HasFrameId + HasSigmaKey> {
    items: Vec<R>,
}

impl<R: HasFrameId + HasSigmaKey> PriorityQueue<R> {
    pub(crate) fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Insert a record. Maintains ascending-σ order.
    pub(crate) fn insert(&mut self, record: R) {
        let key = record.sigma_key();
        // partition_point is binary search for the first
        // index whose key > our key; insert there to maintain
        // stable ordering (records with equal σ keep
        // insertion order).
        let pos = self.items.partition_point(|r| r.sigma_key() <= key);
        self.items.insert(pos, record);
    }

    /// Number of records currently held.
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// Remove every record whose `frame_id` matches the
    /// predicate. Returns the number of records removed.
    pub(crate) fn retain<F: FnMut(&R) -> bool>(&mut self, mut pred: F) -> usize {
        let before = self.items.len();
        self.items.retain(|r| pred(r));
        before - self.items.len()
    }

    /// True iff any record in the queue has the given
    /// `frame_id`. O(N).
    pub(crate) fn contains_frame(&self, frame_id: FrameId) -> bool {
        self.items.iter().any(|r| r.frame_id() == frame_id)
    }

    /// Iterate over records in ascending σ order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &R> {
        self.items.iter()
    }
}

/// Trait so [`PriorityQueue`] can ask records for their σ
/// without committing to a concrete record type.
pub(crate) trait HasSigmaKey {
    fn sigma_key(&self) -> SigmaKey;
}

/// Trait so [`PriorityQueue`] can ask records for their source
/// frame.
pub(crate) trait HasFrameId {
    fn frame_id(&self) -> FrameId;
}

impl HasFrameId for BodyRecord {
    fn frame_id(&self) -> FrameId {
        self.frame_id
    }
}
impl HasSigmaKey for BodyRecord {
    fn sigma_key(&self) -> SigmaKey {
        self.sigma_key
    }
}
impl HasFrameId for HorizonRecord {
    fn frame_id(&self) -> FrameId {
        self.frame_id
    }
}
impl HasSigmaKey for HorizonRecord {
    fn sigma_key(&self) -> SigmaKey {
        self.sigma_key
    }
}

/// The umbrella structure: ring buffer + body queue + horizon
/// queue + eviction logic.
///
/// One instance lives inside [`crate::engine::EngineState`].
/// All operations are O(N) at most (with N bounded by ring
/// capacity, default 120); none allocate beyond what's needed
/// to grow `Vec` capacity.
#[derive(Debug)]
pub(crate) struct Storage {
    /// Raw frames retained as stitching intermediaries.
    /// Insertion order matches capture order (the engine
    /// guarantees monotonic `frame_tt` because pushes are
    /// serialized through `push_frame`).
    ring: Vec<RingFrame>,
    /// Body detection records keyed on σ.
    body_q: PriorityQueue<BodyRecord>,
    /// Horizon detection records keyed on σ.
    horizon_q: PriorityQueue<HorizonRecord>,
    /// Maximum number of frames to keep in the ring before
    /// forcing eviction of the oldest, even if the eviction
    /// rule would normally protect them. A safety valve: if
    /// the eviction logic is ever wrong (bug) or genuinely
    /// every frame in the ring is protected (capture rate >
    /// processing rate), this prevents unbounded growth.
    /// Mirrors [`crate::EngineConfig::input_ring_capacity`].
    capacity: usize,
}

impl Storage {
    /// Construct an empty storage with the given ring
    /// capacity.
    pub(crate) fn new(capacity: usize) -> Self {
        // Capacity 0 is meaningless (would force eviction of
        // every frame the moment it's admitted). Clamp to at
        // least 1 to keep the invariants simple.
        let capacity = capacity.max(1);
        Self {
            ring: Vec::with_capacity(capacity),
            body_q: PriorityQueue::new(),
            horizon_q: PriorityQueue::new(),
            capacity,
        }
    }

    /// Insert one raw frame into the ring buffer.
    ///
    /// Capacity is enforced as a hard ceiling: if the ring is
    /// already at capacity, the *oldest* frame is evicted
    /// regardless of whether the eviction rule would have
    /// protected it. This keeps memory bounded under any
    /// pathological capture-vs-processing-rate ratio. In
    /// normal operation the time-window-based [`Self::evict`]
    /// call (typically right after `admit_records`) keeps
    /// the ring well below capacity.
    /// Admit a raw frame into the ring buffer. Wraps the
    /// frame in a [`bris_vision::FramePyramid`] (with an
    /// empty downsample cache) so downstream stages can
    /// request pyramid levels via [`RingFrame::pyramid`].
    ///
    /// Use [`Self::admit_pyramid`] when the engine has
    /// already constructed a pyramid for this frame (e.g.
    /// because `process_frame` populated cached levels during
    /// stage C); admitting via that path preserves the cache.
    pub(crate) fn admit_frame(&mut self, frame_id: FrameId, frame_tt: Tt, frame: Frame) {
        self.admit_pyramid(frame_id, frame_tt, bris_vision::FramePyramid::new(frame));
    }

    /// Admit a frame whose pyramid was constructed upstream
    /// (e.g. by [`crate::pipeline::process_frame`] so its
    /// horizon-stage downsample cache survives).
    pub(crate) fn admit_pyramid(
        &mut self,
        frame_id: FrameId,
        frame_tt: Tt,
        pyramid: bris_vision::FramePyramid,
    ) {
        if self.ring.len() >= self.capacity {
            // Evict the oldest. Also drop any queue records
            // that referenced it, because their stitching
            // intermediary is gone.
            let oldest = self.ring.remove(0);
            self.drop_records_for(oldest.frame_id);
        }
        self.ring.push(RingFrame {
            frame_id,
            frame_tt,
            pyramid,
        });
    }

    /// Insert body and horizon records (as appropriate) for a
    /// frame whose pipeline pass produced them.
    ///
    /// Either or both arguments may be `None`/`HorizonStageOutcome::None`
    /// for a frame that produced no detections; the call is
    /// then a no-op for the corresponding queue.
    pub(crate) fn admit_records(
        &mut self,
        frame_id: FrameId,
        frame_tt: Tt,
        body: BodyDetection,
        horizon: HorizonStageOutcome,
    ) {
        if let Some(record) = body_record_from_detection(frame_id, frame_tt, body) {
            self.body_q.insert(record);
        }
        if let Some(record) = horizon_record_from_outcome(frame_id, frame_tt, horizon) {
            self.horizon_q.insert(record);
        }
    }

    /// Apply the eviction rule. Removes from the ring buffer
    /// every frame that:
    ///
    /// 1. Is referenced by no record in either queue, AND
    /// 2. Is more than `stitching_window_seconds` from every
    ///    queue record, AND
    /// 3. Is more than `stitching_window_seconds` older than
    ///    the newest frame in the ring (i.e. it's outside the
    ///    rolling stitching-window-of-most-recent-capture).
    ///
    /// Condition (3) implements the design-doc intent: "even
    /// body-less, horizon-less frames remain available as
    /// stitching intermediaries." Without it, a session that
    /// hasn't yet produced any detection (early startup,
    /// occluded view) would evict every captured frame
    /// immediately because conditions (1) and (2) are
    /// vacuously satisfied with empty queues.
    ///
    /// Returns the number of frames evicted.
    pub(crate) fn evict(&mut self, stitching_window_seconds: f64) -> usize {
        // Pre-compute the set of frame ids currently
        // referenced by any queue record. O(N_queue).
        let mut referenced: HashSet<FrameId> = HashSet::new();
        for r in self.body_q.iter() {
            referenced.insert(r.frame_id);
        }
        for r in self.horizon_q.iter() {
            referenced.insert(r.frame_id);
        }
        // Pre-compute the set of `frame_tt` of all queue
        // records (they may originate from frames that were
        // already evicted, but their `frame_tt` is still the
        // anchor for the stitching-window check).
        let queue_tts: Vec<Tt> = self
            .body_q
            .iter()
            .map(|r| r.frame_tt)
            .chain(self.horizon_q.iter().map(|r| r.frame_tt))
            .collect();
        // Newest capture time in the ring (the rolling
        // stitching-window anchor for condition 3).
        let newest_tt: Option<Tt> = self.ring.iter().map(|f| f.frame_tt).reduce(|a, b| {
            if a.julian_date() >= b.julian_date() {
                a
            } else {
                b
            }
        });

        let before = self.ring.len();
        let mut keep = Vec::with_capacity(self.ring.len());
        for f in self.ring.drain(..) {
            let protected_by_record = referenced.contains(&f.frame_id);
            let protected_by_queue_window = queue_tts
                .iter()
                .any(|&qtt| time_gap_seconds(qtt, f.frame_tt) <= stitching_window_seconds);
            let protected_by_recency = match newest_tt {
                Some(n) => time_gap_seconds(n, f.frame_tt) <= stitching_window_seconds,
                None => false,
            };
            if protected_by_record || protected_by_queue_window || protected_by_recency {
                keep.push(f);
            }
        }
        self.ring = keep;
        before - self.ring.len()
    }

    /// Number of frames currently in the ring.
    pub(crate) fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// Number of records in the body queue.
    pub(crate) fn body_queue_len(&self) -> usize {
        self.body_q.len()
    }

    /// Number of records in the horizon queue.
    pub(crate) fn horizon_queue_len(&self) -> usize {
        self.horizon_q.len()
    }

    /// Iterate over body records in ascending σ order.
    pub(crate) fn body_records(&self) -> impl Iterator<Item = &BodyRecord> {
        self.body_q.iter()
    }

    /// Iterate over horizon records in ascending σ order.
    pub(crate) fn horizon_records(&self) -> impl Iterator<Item = &HorizonRecord> {
        self.horizon_q.iter()
    }

    /// Look up a frame in the ring by id. Returns `None` if
    /// the frame has been evicted.
    pub(crate) fn frame(&self, frame_id: FrameId) -> Option<&RingFrame> {
        self.ring.iter().find(|f| f.frame_id == frame_id)
    }

    /// Drop every body/horizon record sourced from the given
    /// frame. Used when a frame is evicted from the ring
    /// (capacity hard cap).
    fn drop_records_for(&mut self, frame_id: FrameId) {
        self.body_q.retain(|r| r.frame_id != frame_id);
        self.horizon_q.retain(|r| r.frame_id != frame_id);
    }
}

/// Convert a Stage B [`BodyDetection`] into a [`BodyRecord`].
/// Returns `None` for [`BodyDetection::None`] (no record to
/// insert).
fn body_record_from_detection(
    frame_id: FrameId,
    frame_tt: Tt,
    detection: BodyDetection,
) -> Option<BodyRecord> {
    let sigma_key = match &detection {
        BodyDetection::Day(c) => SigmaKey::from_sigma(c.position_sigma_px),
        BodyDetection::Night(peaks) => {
            // Night-path priority (peaks not yet plate-solved):
            // more peaks → smaller key. The `1 / sqrt(N)` form
            // matches the way per-star altitude σ falls with N
            // visible stars (random-walk convergence). The
            // 1.0 numerator is arbitrary because Stage E does
            // the actual pixel→angular conversion; only
            // relative order matters at queue level.
            //
            // Cast: peak counts are ≤ PeakConfig::max_peaks
            // (default 200) so usize→f64 is exact in practice;
            // we allow the lint locally rather than reach for
            // an integer-square-root dance.
            #[allow(clippy::cast_precision_loss)]
            let n = peaks.len().max(1) as f64;
            SigmaKey::from_f64(1.0 / n.sqrt())
        }
        BodyDetection::IdentifiedStars(result) => {
            // Plate-solved priority: more identified stars →
            // smaller key. Same `1 / sqrt(N)` form as Night
            // peaks, but offset slightly downward (multiply by
            // 0.5) so an IdentifiedStars record always
            // outranks a Night record with the same N. The
            // engine should prefer plate-solved records — they
            // produce N independent altitude observations
            // versus N peaks-of-unknown-identity producing
            // zero usable observations.
            #[allow(clippy::cast_precision_loss)]
            let n = result.identified.len().max(1) as f64;
            SigmaKey::from_f64(0.5 / n.sqrt())
        }
        BodyDetection::None => return None,
    };
    Some(BodyRecord {
        frame_id,
        frame_tt,
        detection,
        sigma_key,
    })
}

/// Convert a Stage C [`HorizonStageOutcome`] into a
/// [`HorizonRecord`]. Returns `None` for
/// [`HorizonStageOutcome::None`].
fn horizon_record_from_outcome(
    frame_id: FrameId,
    frame_tt: Tt,
    outcome: HorizonStageOutcome,
) -> Option<HorizonRecord> {
    match outcome {
        HorizonStageOutcome::Detected {
            line, direct_sight, ..
        } => {
            let sigma_key = SigmaKey::from_sigma(line.altitude_sigma);
            Some(HorizonRecord {
                frame_id,
                frame_tt,
                line,
                sigma_key,
                direct_sight,
            })
        }
        HorizonStageOutcome::None => None,
    }
}

/// Absolute time gap between two TT instants, in seconds.
fn time_gap_seconds(a: Tt, b: Tt) -> f64 {
    const SECS_PER_DAY: f64 = 86_400.0;
    (a.julian_date() - b.julian_date()).abs() * SECS_PER_DAY
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]

    use super::*;
    use bris_core::time::JD_J2000;
    use bris_vision::{Centroid, Frame, HorizonLine, Intrinsics, Peak};

    fn dummy_frame(tt_offset_seconds: f64) -> Frame {
        Frame::new(
            8,
            8,
            vec![0u16; 64],
            Tt::from_julian_date(JD_J2000 + tt_offset_seconds / 86_400.0),
            1000,
            Intrinsics::placeholder(8, 8),
        )
        .unwrap()
    }

    fn day_centroid(sigma_px: f64) -> BodyDetection {
        BodyDetection::Day(Centroid {
            x: 4.0,
            y: 4.0,
            area_px: 100,
            mean_intensity: 50_000.0,
            position_sigma_px: Sigma::new(sigma_px).unwrap(),
        })
    }

    fn horizon_outcome(sigma_rad: f64) -> HorizonStageOutcome {
        HorizonStageOutcome::Detected {
            detector: super::super::horizon::HorizonDetector::Gradient,
            provenance: bris_vision::HorizonProvenance::Optical(bris_vision::OpticalKind::Gradient),
            line: HorizonLine {
                slope: 0.0,
                intercept: 4.0,
                inlier_count: 50,
                candidate_count: 100,
                residual_rms_px: 0.5,
                altitude_sigma: Sigma::new(sigma_rad).unwrap(),
            },
            direct_sight: None,
        }
    }

    #[test]
    fn priority_queue_orders_by_ascending_sigma() {
        let mut q: PriorityQueue<BodyRecord> = PriorityQueue::new();
        for (id, sig) in [(1u64, 0.5), (2, 0.1), (3, 0.3)] {
            let frame = dummy_frame(f64::from(id as u32));
            q.insert(BodyRecord {
                frame_id: FrameId(id),
                frame_tt: frame.capture_tt,
                detection: day_centroid(sig),
                sigma_key: SigmaKey::from_f64(sig),
            });
        }
        let order: Vec<u64> = q.iter().map(|r| r.frame_id.0).collect();
        assert_eq!(order, vec![2, 3, 1], "expected ascending-σ order");
    }

    #[test]
    fn priority_queue_handles_equal_sigma_stably() {
        // Two records with identical σ should retain
        // insertion order (record A inserted first comes
        // first when iterating).
        let mut q: PriorityQueue<BodyRecord> = PriorityQueue::new();
        for id in [10u64, 20, 30] {
            let frame = dummy_frame(f64::from(id as u32));
            q.insert(BodyRecord {
                frame_id: FrameId(id),
                frame_tt: frame.capture_tt,
                detection: day_centroid(0.42),
                sigma_key: SigmaKey::from_f64(0.42),
            });
        }
        let order: Vec<u64> = q.iter().map(|r| r.frame_id.0).collect();
        assert_eq!(order, vec![10, 20, 30]);
    }

    #[test]
    fn admit_then_evict_removes_unreferenced_old_frames() {
        // Three frames: f0 (t=0, no record), f1 (t=20s, has
        // record), f2 (t=30s, no record, is the newest).
        // Stitching window 2s.
        //
        // - f0: no record (cond 1 fails); 20s from f1's
        //   record_tt > 2s (cond 2 fails); 30s from newest > 2s
        //   (cond 3 fails) → EVICT.
        // - f1: record-protected → keep.
        // - f2: 30s from queue record, but 0s from newest
        //   (recency-protected) → keep.
        let mut storage = Storage::new(10);
        let f0 = dummy_frame(0.0);
        let f1 = dummy_frame(20.0);
        let f2 = dummy_frame(30.0);
        let f1_tt = f1.capture_tt;
        storage.admit_frame(FrameId(0), f0.capture_tt, f0);
        storage.admit_frame(FrameId(1), f1.capture_tt, f1);
        storage.admit_frame(FrameId(2), f2.capture_tt, f2);
        storage.admit_records(
            FrameId(1),
            f1_tt,
            day_centroid(0.5),
            HorizonStageOutcome::None,
        );
        assert_eq!(storage.ring_len(), 3);
        let evicted = storage.evict(2.0);
        assert_eq!(evicted, 1, "only f0 should be evictable");
        assert!(storage.frame(FrameId(0)).is_none());
        assert!(storage.frame(FrameId(1)).is_some(), "f1 has record");
        assert!(
            storage.frame(FrameId(2)).is_some(),
            "f2 is newest, recency-protected"
        );
    }

    #[test]
    fn evict_protects_intermediaries_inside_window() {
        // Three frames at t = 0, 1, 2 seconds. Only frame 0
        // has a record. Stitching window 2.5s. Frames 1 and
        // 2 have no records but are within 2.5s of frame 0's
        // record's frame_tt; they must be protected.
        let mut storage = Storage::new(10);
        let f0 = dummy_frame(0.0);
        let f1 = dummy_frame(1.0);
        let f2 = dummy_frame(2.0);
        let f0_tt = f0.capture_tt;
        storage.admit_frame(FrameId(0), f0.capture_tt, f0);
        storage.admit_frame(FrameId(1), f1.capture_tt, f1);
        storage.admit_frame(FrameId(2), f2.capture_tt, f2);
        storage.admit_records(
            FrameId(0),
            f0_tt,
            day_centroid(0.5),
            HorizonStageOutcome::None,
        );
        let evicted = storage.evict(2.5);
        assert_eq!(evicted, 0, "all three frames must be retained");
    }

    #[test]
    fn evict_drops_intermediaries_outside_window() {
        // Same shape as the protect-inside-window test but
        // with a 0.5s window. Frame 0 (t=0, has record) is
        // record-protected; frame 1 (t=1, no record) is too
        // far from both the record (gap 1s > 0.5s) and the
        // newest frame (gap 1s > 0.5s); frame 2 (t=2, no
        // record, newest) is recency-protected. So only
        // frame 1 evicts.
        let mut storage = Storage::new(10);
        let f0 = dummy_frame(0.0);
        let f1 = dummy_frame(1.0);
        let f2 = dummy_frame(2.0);
        let f0_tt = f0.capture_tt;
        storage.admit_frame(FrameId(0), f0.capture_tt, f0);
        storage.admit_frame(FrameId(1), f1.capture_tt, f1);
        storage.admit_frame(FrameId(2), f2.capture_tt, f2);
        storage.admit_records(
            FrameId(0),
            f0_tt,
            day_centroid(0.5),
            HorizonStageOutcome::None,
        );
        let evicted = storage.evict(0.5);
        assert_eq!(evicted, 1);
        assert!(storage.frame(FrameId(0)).is_some(), "f0 record-protected");
        assert!(storage.frame(FrameId(1)).is_none(), "f1 unprotected");
        assert!(
            storage.frame(FrameId(2)).is_some(),
            "f2 newest, recency-protected"
        );
    }

    #[test]
    fn capacity_hard_cap_evicts_oldest_and_drops_its_records() {
        // Capacity 2. Push 3 frames; the first should be
        // evicted along with its body record. The body queue
        // ends up holding only the records of the surviving
        // frames.
        let mut storage = Storage::new(2);
        for id in 0..3 {
            let f = dummy_frame(f64::from(id));
            let tt = f.capture_tt;
            storage.admit_frame(FrameId(id as u64), tt, f);
            storage.admit_records(
                FrameId(id as u64),
                tt,
                day_centroid(0.1 + 0.1 * f64::from(id)),
                HorizonStageOutcome::None,
            );
        }
        assert_eq!(storage.ring_len(), 2);
        assert!(
            storage.frame(FrameId(0)).is_none(),
            "oldest evicted by capacity"
        );
        assert!(storage.frame(FrameId(1)).is_some());
        assert!(storage.frame(FrameId(2)).is_some());
        // Body queue should have dropped the record sourced
        // from frame 0 along with the frame.
        assert_eq!(
            storage.body_queue_len(),
            2,
            "frame 0's body record must be dropped when frame 0 is evicted"
        );
        let frame_ids: Vec<u64> = storage.body_records().map(|r| r.frame_id.0).collect();
        assert!(!frame_ids.contains(&0));
    }

    #[test]
    fn body_detection_none_produces_no_record() {
        let mut storage = Storage::new(10);
        let f = dummy_frame(0.0);
        let tt = f.capture_tt;
        storage.admit_frame(FrameId(0), tt, f);
        storage.admit_records(
            FrameId(0),
            tt,
            BodyDetection::None,
            HorizonStageOutcome::None,
        );
        assert_eq!(storage.body_queue_len(), 0);
        assert_eq!(storage.horizon_queue_len(), 0);
    }

    #[test]
    fn night_peaks_priority_decreases_with_more_peaks() {
        // More peaks → smaller σ key → earlier in queue.
        let mut storage = Storage::new(10);
        let many_peaks: Vec<Peak> = (0..100)
            .map(|i| Peak {
                x: f64::from(i),
                y: 0.0,
                intensity: 10_000.0,
            })
            .collect();
        let few_peaks: Vec<Peak> = (0..4)
            .map(|i| Peak {
                x: f64::from(i),
                y: 0.0,
                intensity: 10_000.0,
            })
            .collect();
        let f0 = dummy_frame(0.0);
        let tt0 = f0.capture_tt;
        let f1 = dummy_frame(1.0);
        let tt1 = f1.capture_tt;
        storage.admit_frame(FrameId(0), tt0, f0);
        storage.admit_records(
            FrameId(0),
            tt0,
            BodyDetection::Night(few_peaks),
            HorizonStageOutcome::None,
        );
        storage.admit_frame(FrameId(1), tt1, f1);
        storage.admit_records(
            FrameId(1),
            tt1,
            BodyDetection::Night(many_peaks),
            HorizonStageOutcome::None,
        );
        let order: Vec<u64> = storage.body_records().map(|r| r.frame_id.0).collect();
        assert_eq!(
            order,
            vec![1, 0],
            "frame with more peaks (100) should outrank frame with fewer (4)"
        );
    }

    #[test]
    fn horizon_record_admitted_with_correct_sigma_key() {
        let mut storage = Storage::new(10);
        let f = dummy_frame(0.0);
        let tt = f.capture_tt;
        storage.admit_frame(FrameId(0), tt, f);
        storage.admit_records(
            FrameId(0),
            tt,
            BodyDetection::None,
            horizon_outcome(2.5e-4), // 2.5e-4 rad ≈ 0.86 arcmin
        );
        assert_eq!(storage.horizon_queue_len(), 1);
        let r = storage.horizon_records().next().unwrap();
        assert!((r.sigma_key.value() - 2.5e-4).abs() < 1e-12);
    }

    #[test]
    fn sigma_key_handles_nan_and_negative_defensively() {
        // NaN → +∞ (sorts last); negative → 0.
        let nan = SigmaKey::from_f64(f64::NAN);
        assert!(nan.value().is_infinite());
        let neg = SigmaKey::from_f64(-1.0);
        assert!((neg.value() - 0.0).abs() < f64::EPSILON);
        let normal = SigmaKey::from_f64(0.5);
        assert!(
            normal < nan,
            "normal σ should sort before NaN-promoted-to-inf"
        );
    }
}
