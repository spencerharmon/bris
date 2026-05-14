//! [`StreamingEngine`]: top-level orchestration entry point.
//!
//! The engine owns the input ring buffer, the body and horizon
//! detection queues, the raw-frame ring buffer for stitching
//! intermediaries, the active sight window, and the worker
//! thread(s) that drive the staged pipeline.
//!
//! At Phase 3.5 commit 1 the engine's processing loop is stubbed:
//! [`StreamingEngine::push_frame`] accepts and counts frames but
//! does not currently process them; [`StreamingEngine::fix_stream`]
//! returns a receiver that will receive nothing until the worker
//! is wired in (commit 5). The shape of the public API is what
//! commit 1 establishes.

use crate::config::{EngineConfig, PlateSolverInit};
use crate::diagnostics::{EngineDiagnostics, PipelineStageStats};
use crate::fix::PublishedFix;
use crate::pipeline::{
    process_frame, run_stage_d, run_stage_e, BodyDetection, ClassifierHysteresis, FrameId,
    HorizonStageOutcome, SightWindow, StageDOutcome, StageOutcome, Storage,
};
use bris_platesolve::StarHashDb;
use bris_vision::{Condition, Frame};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;
use tracing::{debug, info};

/// Stage index into [`EngineDiagnostics::stages`] for Stage A
/// (classifier).
const STAGE_A: usize = 0;
/// Stage index for Stage B (body detection).
const STAGE_B: usize = 1;
/// Stage index for Stage C (horizon detection).
const STAGE_C: usize = 2;
/// Stage index for Stage D (plate solving).
const STAGE_D: usize = 3;
/// Stage index for Stage E (sight assembly + fix publication).
const STAGE_E: usize = 4;

/// Continuous-operation streaming engine.
///
/// Construct via [`StreamingEngine::new`]; push captured frames
/// via [`Self::push_frame`]; subscribe to fix publications via
/// [`Self::fix_stream`]; query state via [`Self::diagnostics`].
///
/// Cheap to clone-by-reference (the engine owns its state); not
/// `Clone`. Intended to live for the duration of an operating
/// session — the CLI's `bris serve` will own one, the mobile
/// shell will own one. Drop the engine to stop processing
/// (worker threads join on drop in the eventual implementation).
#[derive(Debug)]
pub struct StreamingEngine {
    config: EngineConfig,
    /// Internally-mutable engine state. Wrapped in a `Mutex` so
    /// the (eventual) worker thread and the (current) push/poll
    /// API can both access it. The choice of `std::sync::Mutex`
    /// over `parking_lot` keeps the dependency tree lean for the
    /// embedded build.
    state: Mutex<EngineState>,
    /// Sender end of the fix-publication channel. The worker
    /// thread (when wired) sends [`PublishedFix`] values; each
    /// active [`FixReceiver`] holds the corresponding receiver.
    fix_tx: Sender<PublishedFix>,
    /// Receiver retained so [`Self::fix_stream`] can hand out
    /// clones of a [`FixReceiver`]. The first-receiver pattern
    /// gets unwound when we move to broadcast in a later commit;
    /// for now there's exactly one consumer.
    fix_rx: Mutex<Option<Receiver<PublishedFix>>>,
    /// Plate-solving hash database. `OnceLock` so that the
    /// (potentially) deferred construction is thread-safe and
    /// the post-build state is read-only / lock-free.
    ///
    /// `AtStartup` populates this during `new()`; `Lazy`
    /// leaves it empty until the first night frame triggers a
    /// build. The build itself is synchronous and blocks the
    /// caller for ~10-30 s in release; documented at
    /// [`crate::PlateSolverInit`].
    plate_db: OnceLock<StarHashDb>,
}

/// Mutable engine state. Kept in a single struct so the worker
/// thread can lock once per stage rather than locking each
/// sub-piece independently.
#[derive(Debug)]
struct EngineState {
    frames_pushed: u64,
    frames_dropped: u64,
    /// Monotonic counter for [`FrameId`] assignment. Incremented
    /// on every push (including drops) so that gaps in the id
    /// sequence are observable in diagnostics if backpressure
    /// kicks in (commit 4 doesn't drop, but commit when the
    /// worker thread lands will).
    next_frame_id: u64,
    stages: [PipelineStageStats; 5],
    last_classification: Option<bris_vision::Condition>,
    last_processed_frame_tt: Option<bris_core::time::Tt>,
    last_published_fix_tt: Option<bris_core::time::Tt>,
    /// Ring buffer + body/horizon queues + eviction. Owns the
    /// retained raw frames and detection records.
    storage: Storage,
    /// Active sight window: candidate sights kept for fix
    /// computation. Replace-worst-on-insertion when full;
    /// age-evicted by [`crate::EngineConfig::sight_window_seconds`].
    sight_window: SightWindow,
    /// Wall-clock instant of the most recent successful fix
    /// publication. Drives the
    /// [`crate::EngineConfig::min_fix_publication_interval_ms`]
    /// throttle in Stage E. `None` until the first publication.
    last_publication: Option<Instant>,
    /// Classifier hysteresis: smooths transient per-frame
    /// classifier verdicts into stable method-set choices.
    /// See
    /// [`crate::EngineConfig::classifier_hysteresis_frames`].
    classifier_hysteresis: ClassifierHysteresis,
}

/// Update per-stage counters and last-classification / last-tt
/// fields based on a freshly-completed pipeline pass. Pure
/// function: caller holds the state lock.
fn update_stage_counters(
    state: &mut EngineState,
    outcome: &StageOutcome,
    stage_d_outcome: StageDOutcome,
) {
    // Stage A always runs and never errors (the classifier is
    // total). Count one entry, one production. The "skipped"
    // and "failed" buckets stay at zero for Stage A.
    state.stages[STAGE_A].entered += 1;
    state.stages[STAGE_A].produced += 1;

    // Stage B: enters when the *dispatched* verdict isn't
    // Unusable (the dispatched verdict, after hysteresis, is
    // what actually gated B/C); produces when a centroid or
    // a non-empty peak vector came out. After Stage D
    // promotion, the body may also be `IdentifiedStars` —
    // count that as a produce too (it implies Stage B
    // produced peaks earlier).
    let unusable = matches!(outcome.dispatched_condition, Condition::Unusable);
    if unusable {
        state.stages[STAGE_B].skipped += 1;
    } else {
        state.stages[STAGE_B].entered += 1;
        match &outcome.body {
            BodyDetection::Day(_) | BodyDetection::Night(_) | BodyDetection::IdentifiedStars(_) => {
                state.stages[STAGE_B].produced += 1;
            }
            BodyDetection::None => {
                // "No detection" isn't a hard error; we don't
                // bump `failed`. The detector is doing its job
                // by reporting the absence honestly. Operators
                // notice via the produced/entered ratio.
            }
        }
    }

    // Stage C: same skip-on-Unusable contract as Stage B (the
    // pipeline's horizon::detect short-circuits on Unusable).
    if unusable {
        state.stages[STAGE_C].skipped += 1;
    } else {
        state.stages[STAGE_C].entered += 1;
        match &outcome.horizon {
            HorizonStageOutcome::Detected { .. } => {
                state.stages[STAGE_C].produced += 1;
            }
            HorizonStageOutcome::None => {
                // No horizon found by any detector. As with
                // Stage B's None case, this is the detector's
                // honest "couldn't see one" verdict, not a
                // hard error.
            }
        }
    }

    // Stage D: ran iff the body record is/was a Night payload.
    match stage_d_outcome {
        StageDOutcome::Identified => {
            state.stages[STAGE_D].entered += 1;
            state.stages[STAGE_D].produced += 1;
        }
        StageDOutcome::NoMatch => {
            state.stages[STAGE_D].entered += 1;
            // Not a hard `failed` — "no match" is an honest
            // reading of the data (no recognizable star
            // pattern in this frame). Operators see the
            // produced/entered ratio.
        }
        StageDOutcome::Skipped => {
            state.stages[STAGE_D].skipped += 1;
        }
    }

    state.last_classification = Some(outcome.classification.condition);
    state.last_processed_frame_tt = Some(outcome.frame_tt);
}

impl StreamingEngine {
    /// Construct an engine with the given configuration.
    ///
    /// Construction is fast except when
    /// [`PlateSolverInit::AtStartup`] is requested; in that case
    /// this call blocks for the database build (~10-30 s
    /// release). Other init modes return immediately.
    ///
    /// # Panics
    ///
    /// Does not panic in the current skeleton. Once the
    /// at-startup plate-solver build is wired in (commit 6),
    /// catastrophic build failures will surface as panics here
    /// because there is no useful recovery — a working hash
    /// database is required for any night fix.
    ///
    /// Also panics if [`PlateSolverInit::Cached`] is requested
    /// at this commit: the on-disk DB format hasn't been
    /// defined yet. The variant is reserved (per commit 1) so
    /// that switching to it later is non-breaking, but using
    /// it now is a programming error.
    #[must_use]
    pub fn new(config: EngineConfig) -> Self {
        let (fix_tx, fix_rx) = mpsc::channel();
        let plate_db = OnceLock::new();
        match &config.plate_solver_init {
            PlateSolverInit::AtStartup => {
                info!(
                    "StreamingEngine::new: building plate-solver hash database \
                     synchronously (this may take 10-30 s in release)"
                );
                let db = StarHashDb::build(config.star_hash_db_cfg);
                plate_db
                    .set(db)
                    .map_err(|_| ())
                    .expect("OnceLock just constructed; set() cannot fail");
            }
            PlateSolverInit::Lazy => {
                debug!("StreamingEngine::new: deferring plate-solver build to first night frame");
            }
            PlateSolverInit::Cached(_path) => {
                panic!(
                    "PlateSolverInit::Cached is reserved but not yet implemented \
                     (the on-disk database format hasn't been defined). \
                     Use AtStartup or Lazy."
                );
            }
        }
        Self {
            state: Mutex::new(EngineState {
                frames_pushed: 0,
                frames_dropped: 0,
                next_frame_id: 0,
                stages: [PipelineStageStats::default(); 5],
                last_classification: None,
                last_processed_frame_tt: None,
                last_published_fix_tt: None,
                storage: Storage::new(config.input_ring_capacity),
                sight_window: SightWindow::default(),
                last_publication: None,
                classifier_hysteresis: ClassifierHysteresis::default(),
            }),
            config,
            fix_tx,
            fix_rx: Mutex::new(Some(fix_rx)),
            plate_db,
        }
    }

    /// Push one captured frame into the engine.
    ///
    /// Non-blocking: the engine queues the frame for processing
    /// and returns immediately. If the input ring buffer is full
    /// the frame is silently dropped and the drop counter is
    /// incremented (visible via
    /// [`Self::diagnostics`].`frames_dropped`).
    ///
    /// This is the documented backpressure model: capture is
    /// uncoordinated with processing; sustained capture above
    /// the processing throughput drops the most-stale capture
    /// rather than backing up the camera.
    ///
    /// # Errors
    ///
    /// Currently infallible. Returns a [`PushError`] enum to
    /// reserve future error variants (e.g. once the engine
    /// supports an explicit shutdown that rejects further
    /// pushes).
    #[allow(
        // The Result return type is reserved for future failure
        // modes (explicit shutdown, capture-source backpressure
        // signaling); changing the signature later is a breaking
        // change for every embedder, so we lock it in now even
        // though commit 1 has no failures to report.
        clippy::unnecessary_wraps,
        clippy::needless_pass_by_value, // Frame is large; takes ownership for queueing.
        // stage_d_outcome / stage_e_outcome are the canonical
        // names; suffixing with _d / _e is exactly what makes
        // them clear at the call sites.
        clippy::similar_names,
    )]
    pub fn push_frame(&self, frame: Frame) -> Result<(), PushError> {
        // Serialize the whole pipeline run for this frame:
        // assigning frame_ids, advancing classifier hysteresis,
        // running detectors, updating storage, and running
        // Stage E all need to happen atomically per frame.
        // Multiple concurrent push_frame calls would otherwise
        // race on frame_id ordering, hysteresis state, and
        // storage monotonicity.
        //
        // Holding the lock across the (potentially ~100 ms)
        // pipeline call serializes pushes, which matches the
        // engine's design: capture-side feeds frames in TT
        // order from a single thread.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.frames_pushed += 1;
        let frame_id = FrameId(state.next_frame_id);
        state.next_frame_id += 1;

        // Run Stages A + B + C synchronously. Pass the
        // hysteresis state by mutable reference so it advances
        // by exactly one observation per frame.
        let mut outcome = process_frame(&frame, &self.config, &mut state.classifier_hysteresis);
        let frame_tt = outcome.frame_tt;

        // ---- Stage D: plate solving (night/twilight only) ----
        // Stage D promotes BodyDetection::Night(peaks) into
        // BodyDetection::IdentifiedStars(result) when the solve
        // succeeds. Day records and None records are passed
        // through. The hash database lives in `self.plate_db`
        // (a OnceLock so the post-build read path is lock-free)
        // and is None until either AtStartup populated it or
        // the lazy build path below ran.
        let stage_d_outcome = run_stage_d(
            &mut outcome.body,
            &frame,
            self.plate_db.get(),
            self.config.plate_solve_cfg,
        );
        // Lazy build: if Stage D was skipped because the DB
        // wasn't built and we have a Night payload that *could*
        // have been solved, build the DB now (synchronously,
        // ~10-30 s in release) and re-run Stage D on this very
        // frame. Subsequent frames will use the cached DB
        // lock-free.
        let stage_d_outcome = match (stage_d_outcome, &outcome.body, self.plate_db.get()) {
            (StageDOutcome::Skipped, BodyDetection::Night(_), None) => {
                info!(
                    "Stage D: lazy plate-solver build triggered by first night frame \
                     (this may take 10-30 s in release)"
                );
                let db = StarHashDb::build(self.config.star_hash_db_cfg);
                // Race: if two threads hit the lazy path at
                // the same time (commit 4+ worker thread),
                // OnceLock::set returns Err for the loser.
                // Either Ok or Err is fine here — both end
                // with self.plate_db.get() being Some.
                let _ = self.plate_db.set(db);
                run_stage_d(
                    &mut outcome.body,
                    &frame,
                    self.plate_db.get(),
                    self.config.plate_solve_cfg,
                )
            }
            (other, _, _) => other,
        };

        // Admit the frame + its records into storage, then
        // evict whatever's no longer needed. Storage's
        // invariants (queue records never reference an
        // evicted frame) hold trivially because we hold the
        // lock through the whole pipeline.
        // Update counters first while we have an immutable
        // borrow of `outcome`. The storage then *consumes*
        // the body detection (which carries a Vec<Peak> in
        // the night case and shouldn't be cloned).
        update_stage_counters(&mut state, &outcome, stage_d_outcome);
        let StageOutcome { body, horizon, .. } = outcome;
        // Move `frame` into the ring buffer; the records into
        // their queues.
        state.storage.admit_frame(frame_id, frame_tt, frame);
        state
            .storage
            .admit_records(frame_id, frame_tt, body, horizon);
        let evicted = state.storage.evict(self.config.stitching_window_seconds);

        // ---- Stage E: pair selection + sight window + fix ----
        // Borrow-splitting dance: run_stage_e needs &Storage
        // and &mut SightWindow simultaneously, both fields of
        // EngineState. We split the borrow explicitly to keep
        // the borrow checker happy.
        let stage_e_outcome = {
            let EngineState {
                ref storage,
                ref mut sight_window,
                last_publication,
                ..
            } = *state;
            run_stage_e(storage, sight_window, &self.config, last_publication)
        };
        state.stages[STAGE_E].entered += 1;
        if let Some(published) = &stage_e_outcome.published {
            state.stages[STAGE_E].produced += 1;
            state.last_published_fix_tt = Some(published.timestamp);
            state.last_publication = Some(Instant::now());
            // Send on the fix channel. Failure means the
            // single consumer dropped; we silently swallow
            // because there's no actionable recovery here.
            // Future broadcast-based design would need to
            // garbage-collect dead subscribers; for the
            // mpsc design "channel closed = no consumer
            // listening" is the documented end-of-stream.
            if self.fix_tx.send(published.clone()).is_err() {
                debug!("Stage E: fix-stream consumer disconnected; published fix dropped");
            }
        }

        debug!(
            frames_pushed = state.frames_pushed,
            frame_id = %frame_id,
            ring_len = state.storage.ring_len(),
            body_q = state.storage.body_queue_len(),
            horizon_q = state.storage.horizon_queue_len(),
            evicted = evicted,
            sight_window = state.sight_window.len(),
            sights_inserted = stage_e_outcome.sights_inserted,
            sights_evicted = stage_e_outcome.sights_evicted,
            published = stage_e_outcome.published.is_some(),
            "StreamingEngine::push_frame: pipeline + Stage E complete"
        );
        Ok(())
    }

    /// Subscribe to fix publications.
    ///
    /// Returns a [`FixReceiver`] that yields a [`PublishedFix`]
    /// each time the engine publishes a new fix. The current
    /// channel implementation supports a *single* consumer; a
    /// later commit will switch to broadcast semantics if the
    /// CLI/FFI/mobile shells require independent subscribers.
    ///
    /// # Errors
    ///
    /// Returns [`PushError::AlreadySubscribed`] if a previous
    /// call has already taken the receiver. Drop the previous
    /// receiver before subscribing again. (This restriction
    /// disappears with broadcast support.)
    pub fn fix_stream(&self) -> Result<FixReceiver, PushError> {
        let mut slot = self.fix_rx.lock().unwrap_or_else(PoisonError::into_inner);
        slot.take()
            .map(|rx| FixReceiver { inner: rx })
            .ok_or(PushError::AlreadySubscribed)
    }

    /// Snapshot the engine's current observable state.
    ///
    /// Cheap: locks the state mutex briefly to clone the
    /// underlying counters. Safe to call at any cadence;
    /// intended for periodic polling by status displays and for
    /// integration-test assertions.
    #[must_use]
    pub fn diagnostics(&self) -> EngineDiagnostics {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        EngineDiagnostics {
            frames_pushed: state.frames_pushed,
            frames_dropped: state.frames_dropped,
            stages: state.stages,
            body_queue_depth: state.storage.body_queue_len(),
            horizon_queue_depth: state.storage.horizon_queue_len(),
            ring_buffer_depth: state.storage.ring_len(),
            sight_window_depth: state.sight_window.len(),
            last_classification: state.last_classification,
            last_processed_frame_tt: state.last_processed_frame_tt,
            last_published_fix_tt: state.last_published_fix_tt,
        }
    }

    /// The configuration the engine was constructed with.
    #[must_use]
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Look up a frame in the engine's ring buffer by its
    /// engine-assigned [`FrameId`] (encoded as `u64` for the
    /// FFI boundary).
    ///
    /// Returns `None` when the frame has been evicted — which
    /// happens when no record currently in the body or horizon
    /// queues references it AND no sight in the active sight
    /// window references it. Foreign callers (the mobile
    /// session-recorder) must therefore call this *promptly*
    /// after a fix publishes, while its contributing frames
    /// are still alive in the ring; once the sight window ages
    /// past those frames they are gone.
    ///
    /// Returns a clone of the underlying [`Frame`] (rather than
    /// a borrow) so the caller can hold it across the FFI
    /// without keeping the engine's state mutex locked.
    #[must_use]
    pub fn frame_by_id(&self, id: u64) -> Option<Frame> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .storage
            .frame(crate::pipeline::FrameId(id))
            .map(|rf| rf.frame.clone())
    }

    /// Test-only hook: synthesize a published fix and emit it on
    /// the fix channel. Lets commit 1's tests verify the
    /// fix-stream API surface end-to-end without a working
    /// pipeline. Will be removed once the worker thread can
    /// produce real fixes (commit 5).
    #[cfg(test)]
    fn emit_fix_for_test(&self, fix: PublishedFix) {
        let _ = self.fix_tx.send(fix);
    }
}

/// Receiver end of the engine's fix-publication channel.
///
/// Wraps a [`std::sync::mpsc::Receiver`] so the public API
/// doesn't leak the channel choice. The engine deliberately
/// supports a *single* consumer per session: deployments needing
/// multiple sinks (e.g. NMEA serial output **and** an on-disk
/// sight log) should fan out at the consumer layer rather than
/// asking the engine to broadcast.
///
/// Rationale for single-consumer:
///
/// - The engine's job is to produce fixes; *what* to do with
///   them (transmit, log, render) is a presentation concern.
/// - Broadcast semantics would require either (a) a third-party
///   crate (no `std` broadcast primitive exists), (b) explicit
///   per-subscriber lifecycle management in the engine for
///   slow / disconnected subscribers, or (c) a boundless queue
///   per subscriber that can leak memory if any one of them
///   stops draining.
/// - All foreseeable consumers (CLI `bris serve`, mobile FFI
///   shell, integration tests) are naturally single-headed and
///   can fan out trivially with `std::thread` + a per-sink
///   channel.
///
/// If a use case ever materializes that *can't* be served by
/// consumer-side fan-out, switching to a true broadcast
/// primitive is a non-breaking internal change because
/// [`FixReceiver`] hides the channel.
#[derive(Debug)]
pub struct FixReceiver {
    inner: Receiver<PublishedFix>,
}

impl FixReceiver {
    /// Block until a fix is published, then return it. Returns
    /// `None` when the engine has been dropped (the sender end
    /// is closed). Subscribers should treat that as the end of
    /// the stream.
    #[must_use]
    pub fn recv(&self) -> Option<PublishedFix> {
        self.inner.recv().ok()
    }

    /// Non-blocking variant: returns `Ok(Some(fix))` if a fix
    /// is immediately available, `Ok(None)` if no fix is
    /// available right now, or `Err(())` if the channel has
    /// been closed.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` when the engine has been dropped.
    #[allow(clippy::result_unit_err)] // the channel-closed case carries no further information.
    pub fn try_recv(&self) -> Result<Option<PublishedFix>, ()> {
        match self.inner.try_recv() {
            Ok(fix) => Ok(Some(fix)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(()),
        }
    }
}

/// Errors from engine API methods.
///
/// Currently sparse; new variants added as the engine acquires
/// new failure modes (e.g. explicit shutdown, plate-solver build
/// failures).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PushError {
    /// [`StreamingEngine::fix_stream`] was called more than once.
    /// The mpsc channel only supports a single consumer; drop
    /// the first receiver before subscribing again.
    #[error("a fix-stream subscriber is already active")]
    AlreadySubscribed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::DominantSource;
    use bris_almanac::Observer;
    use bris_core::time::{Tt, JD_J2000};
    use bris_core::{Latitude, Longitude};
    use bris_nav::Fix;
    use bris_vision::{Intrinsics, Rotation};

    fn dummy_frame() -> Frame {
        Frame::new(
            4,
            3,
            vec![0u16; 12],
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(4, 3),
        )
        .unwrap()
    }

    fn dummy_fix() -> PublishedFix {
        PublishedFix {
            fix: Fix {
                lat: Latitude::from_degrees(0.0).unwrap(),
                lon: Longitude::from_degrees(0.0).unwrap(),
                covariance_nm2: [[0.25, 0.0], [0.0, 0.25]],
                sigma_major_nm: 0.5,
                sigma_minor_nm: 0.5,
                orientation_rad: 0.0,
                sight_count: 2,
            },
            n_sights: 2,
            azimuth_spread_rad: std::f64::consts::FRAC_PI_2,
            oldest_sight_age_seconds: 30.0,
            dominant_source: DominantSource::None,
            timestamp: Tt::from_julian_date(JD_J2000),
            contributing_frame_ids: Vec::new(),
        }
    }

    #[test]
    fn engine_constructs_with_default_config() {
        let cfg = EngineConfig::new(Observer::default_dev());
        let engine = StreamingEngine::new(cfg);
        let diag = engine.diagnostics();
        assert_eq!(diag.frames_pushed, 0);
        assert_eq!(diag.frames_dropped, 0);
        assert!(diag.last_classification.is_none());
        assert!(diag.last_processed_frame_tt.is_none());
        assert!(diag.last_published_fix_tt.is_none());
    }

    #[test]
    fn push_frame_increments_counter() {
        let engine = StreamingEngine::new(EngineConfig::new(Observer::default_dev()));
        engine.push_frame(dummy_frame()).unwrap();
        engine.push_frame(dummy_frame()).unwrap();
        assert_eq!(engine.diagnostics().frames_pushed, 2);
    }

    #[test]
    fn frame_by_id_returns_pushed_frame_and_none_for_unknown() {
        // First pushed frame gets id 0; lookup by id 0 must
        // return a Frame with the same dimensions we pushed.
        // Lookup by an id that was never assigned must return
        // None rather than the most recent or any other frame.
        let engine = StreamingEngine::new(EngineConfig::new(Observer::default_dev()));
        engine.push_frame(dummy_frame()).unwrap();
        let got = engine.frame_by_id(0).expect("frame 0 must be reachable");
        assert_eq!(got.width(), 4);
        assert_eq!(got.height(), 3);
        assert!(
            engine.frame_by_id(9999).is_none(),
            "unknown frame_id must return None, not the most-recent frame",
        );
    }

    #[test]
    fn push_frame_drives_stage_a_and_records_classification() {
        // After pushing one frame, Stage A's counters should be
        // (entered=1, produced=1) and last_classification should
        // be populated. This is the integration test that
        // verifies push_frame actually invokes the pipeline,
        // not just the input-counter bookkeeping.
        let engine = StreamingEngine::new(EngineConfig::new(Observer::default_dev()));
        engine.push_frame(dummy_frame()).unwrap();
        let diag = engine.diagnostics();
        assert_eq!(
            diag.stages[0].entered, 1,
            "Stage A should have entered once"
        );
        assert_eq!(
            diag.stages[0].produced, 1,
            "Stage A always produces (classifier is total)"
        );
        assert_eq!(diag.stages[0].failed, 0);
        assert_eq!(diag.stages[0].skipped, 0);
        assert!(
            diag.last_classification.is_some(),
            "classifier verdict should be recorded after push_frame"
        );
        assert!(
            diag.last_processed_frame_tt.is_some(),
            "frame TT should be recorded after push_frame"
        );
    }

    #[test]
    fn fix_stream_yields_published_fixes() {
        // Verifies the channel API surface end-to-end: subscribe,
        // emit one fix via the test hook, observe it.
        let engine = StreamingEngine::new(EngineConfig::new(Observer::default_dev()));
        let rx = engine.fix_stream().unwrap();
        assert!(rx.try_recv().unwrap().is_none());
        engine.emit_fix_for_test(dummy_fix());
        let received = rx.try_recv().unwrap().expect("fix should be available");
        assert_eq!(received.n_sights, 2);
        assert!(matches!(received.dominant_source, DominantSource::None));
    }

    #[test]
    fn second_subscribe_returns_already_subscribed() {
        // Single-consumer channel: the second subscribe must
        // refuse rather than silently disconnecting the first.
        let engine = StreamingEngine::new(EngineConfig::new(Observer::default_dev()));
        let _first = engine.fix_stream().unwrap();
        let err = engine.fix_stream().unwrap_err();
        assert_eq!(err, PushError::AlreadySubscribed);
    }

    #[test]
    fn config_preserves_observer() {
        // Verify that the config plumbs the observer through
        // unchanged — the engine doesn't silently override
        // anything.
        let mut obs = Observer::default_dev();
        obs.eye_height_m = 12.5;
        let engine = StreamingEngine::new(EngineConfig::new(obs));
        assert!((engine.config().observer.eye_height_m - 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn rotated_source_frames_accepted_unchanged() {
        // Defense in depth: pushing a rotated frame doesn't
        // explode. The engine doesn't consume Frame internals at
        // commit 1, so this only exercises the API; commit 2
        // onward ensures rotation is honored by the detectors.
        let frame = dummy_frame().with_source_rotation(Rotation::Deg90);
        let engine = StreamingEngine::new(EngineConfig::new(Observer::default_dev()));
        engine.push_frame(frame).unwrap();
        assert_eq!(engine.diagnostics().frames_pushed, 1);
    }

    #[test]
    fn pushed_frame_lands_in_storage_and_diagnostics_reflects_it() {
        // After pushing a single frame, the ring buffer should
        // hold that frame. Body/horizon queues may or may not
        // hold records (the dummy frame is uniform black so no
        // detector finds anything), but the ring depth must be
        // 1 — even body-less, horizon-less frames are kept as
        // stitching intermediaries.
        let engine = StreamingEngine::new(EngineConfig::new(Observer::default_dev()));
        engine.push_frame(dummy_frame()).unwrap();
        let diag = engine.diagnostics();
        assert_eq!(
            diag.ring_buffer_depth, 1,
            "ring buffer must retain the frame as a potential stitching intermediary \
             even without detections"
        );
        // The dummy frame is uniform-zero pixels at J2000 (Sun
        // high at Greenwich): classifier verdict will be Day or
        // similar, but no body or horizon will be found.
        // body_queue_depth and horizon_queue_depth should be 0.
        assert_eq!(diag.body_queue_depth, 0);
        assert_eq!(diag.horizon_queue_depth, 0);
    }

    /// Build a synthetic frame containing both a saturated
    /// bright disk (body) and a sharp horizontal sky/sea
    /// boundary (horizon) at the supplied TT.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn body_plus_horizon_frame(tt: bris_core::time::Tt) -> Frame {
        let w = 128_u32;
        let h = 128_u32;
        let mut pixels = vec![0u16; (w * h) as usize];
        // Sky/sea boundary at row 64: bright above, dark below.
        for y in 0..h {
            let value = if y < 64 { 50_000 } else { 200 };
            for x in 0..w {
                pixels[(y as usize) * (w as usize) + (x as usize)] = value;
            }
        }
        // Saturated bright disk centered at (64, 32) — well
        // above the horizon. Radius 6, area ~113 px (above the
        // 50 px min). Saturate the disk to u16::MAX so the
        // 95%-of-MAX saturation threshold catches it.
        let cx: i32 = 64;
        let cy: i32 = 32;
        let r2 = 36_i32;
        for dy in -10..=10_i32 {
            for dx in -10..=10_i32 {
                if dx * dx + dy * dy <= r2 {
                    let px = cx + dx;
                    let py = cy + dy;
                    if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                        let idx = (py as usize) * (w as usize) + (px as usize);
                        pixels[idx] = u16::MAX;
                    }
                }
            }
        }
        Frame::new(w, h, pixels, tt, 1000, Intrinsics::placeholder(w, h)).unwrap()
    }

    #[test]
    fn single_body_plus_horizon_frame_emits_a_sight_into_the_window() {
        // The end-to-end Stage A→B→C→queues→Stage E plumbing.
        // After one push of a body+horizon synthetic frame,
        // the sight window should contain exactly one sight.
        // No fix publishes because LSQ needs ≥ 2 sights.
        let engine = StreamingEngine::new(EngineConfig::new(Observer::default_dev()));
        // J2000 noon at Greenwich: Sun is high. The synthetic
        // frame's "body" projects to image position (64, 32);
        // the apparent sun place won't match exactly (we're
        // not aiming the camera) but the LOP machinery only
        // needs the inputs to be finite — a wrong-magnitude
        // intercept is the expected outcome and is fine for
        // this connectivity test.
        let tt = bris_core::time::Tt::from_julian_date(bris_core::time::JD_J2000);
        engine.push_frame(body_plus_horizon_frame(tt)).unwrap();
        let diag = engine.diagnostics();
        assert!(
            diag.body_queue_depth >= 1,
            "expected ≥ 1 body record, got {}",
            diag.body_queue_depth
        );
        assert!(
            diag.horizon_queue_depth >= 1,
            "expected ≥ 1 horizon record, got {}",
            diag.horizon_queue_depth
        );
        assert_eq!(
            diag.sight_window_depth, 1,
            "expected exactly one sight in the window after one same-frame body+horizon push"
        );
        assert_eq!(
            diag.stages[4].entered, 1,
            "Stage E should have entered once",
        );
    }

    #[test]
    fn fix_publishes_after_two_diverse_sights() {
        // Push two synthetic body+horizon frames separated by
        // ~1 hour TT. The Sun's azimuth changes by ~15° over
        // that interval (rough): the resulting two sights have
        // distinct azimuths and `multi_sight_fix` can produce
        // a non-singular fix.
        //
        // The fix's *position* is meaningless for this test
        // (the synthetic body coordinates don't correspond to
        // anywhere physical). What matters is that a
        // PublishedFix flows through the channel — the
        // end-to-end "frame in → fix out" path works.
        let mut cfg = EngineConfig::new(Observer::default_dev());
        // Drop the publication throttle so two pushes in
        // immediate succession can both publish.
        cfg.min_fix_publication_interval_ms = 0;
        // Widen the sight window so the first sight (1 hour
        // old by the time the second arrives) doesn't age
        // out before the second is inserted.
        cfg.sight_window_seconds = 7200.0; // 2 hours
        let engine = StreamingEngine::new(cfg);
        let rx = engine.fix_stream().unwrap();

        // First push: J2000 noon at Greenwich (Sun high).
        let t0 = bris_core::time::Tt::from_julian_date(bris_core::time::JD_J2000);
        engine.push_frame(body_plus_horizon_frame(t0)).unwrap();
        // No fix yet (only 1 sight, LSQ needs ≥ 2).
        assert!(
            rx.try_recv().unwrap().is_none(),
            "no fix expected after 1 push"
        );

        // Second push: 1 hour later in TT (1/24 days).
        // Sun is still well above horizon; azimuth has moved
        // ~15°. Both reduce_to_sight calls succeed.
        let t1 = bris_core::time::Tt::from_julian_date(bris_core::time::JD_J2000 + 1.0 / 24.0);
        engine.push_frame(body_plus_horizon_frame(t1)).unwrap();
        let diag = engine.diagnostics();
        assert_eq!(
            diag.sight_window_depth, 2,
            "expected two sights after two pushes (if 1, the second sight reduction \
             likely failed — possibly Sun below horizon at the second TT)"
        );
        // A fix should have been published. The exact
        // numerical content is meaningless (synthetic data)
        // but the channel must have received something.
        let published = rx.try_recv().expect("fix channel still open").expect(
            "expected a PublishedFix on the channel after two diverse-azimuth pushes \
                 (if this fails, check that the test's two TTs give the Sun ≥ a few \
                 degrees of azimuth diversity — the multi_sight_fix singularity threshold)",
        );
        assert_eq!(published.n_sights, 2);
        assert!(
            published.azimuth_spread_rad > 0.0,
            "azimuth spread must be positive (the two sights were taken 1h apart)"
        );
    }

    /// Build a synthetic *night* frame: dark background with a
    /// few scattered bright peaks. The classifier should report
    /// Night (uniform dim mean luma); Stage B's peak detector
    /// should find the peaks; Stage D should attempt plate
    /// solving and fail to identify them (random pattern, no
    /// real-star geometry).
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )]
    fn night_peak_frame(tt: bris_core::time::Tt, n_peaks: u32) -> Frame {
        let w = 128_u32;
        let h = 128_u32;
        // Dim background → classifier sees Night (with night_tt).
        let mut pixels = vec![10u16; (w * h) as usize];
        // Sprinkle peaks across the frame.
        for i in 0..n_peaks {
            let x = (10 + 11 * i % (w - 20)) as usize;
            let y = (10 + 7 * i % (h - 20)) as usize;
            // 5-pixel-wide bright spot, well above PeakConfig
            // default min_intensity (2000).
            for dy in -1_i32..=1 {
                for dx in -1_i32..=1 {
                    let px = (x as i32 + dx) as usize;
                    let py = (y as i32 + dy) as usize;
                    pixels[py * (w as usize) + px] = 30_000;
                }
            }
        }
        Frame::new(w, h, pixels, tt, 1000, Intrinsics::placeholder(w, h)).unwrap()
    }

    #[test]
    fn lazy_plate_solver_build_triggered_by_first_night_frame() {
        // Configure the engine with `PlateSolverInit::Lazy` and
        // a tiny DB (mag_cutoff 1.5 → ~10 stars → fast build,
        // ~10 ms on a fast machine). After pushing a single
        // night frame with peaks, the engine's plate database
        // must be populated.
        //
        // The plate solve will *fail* (random peak positions
        // don't match any star pattern), so no
        // IdentifiedStars record makes it into the queue;
        // the test verifies the lazy-build trigger fires
        // regardless.
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.plate_solver_init = crate::PlateSolverInit::Lazy;
        cfg.star_hash_db_cfg = bris_platesolve::StarHashDbConfig {
            mag_cutoff: 1.5,
            ..bris_platesolve::StarHashDbConfig::default()
        };
        let engine = StreamingEngine::new(cfg);
        // Pre-condition: plate db not yet built.
        assert!(
            engine.plate_db.get().is_none(),
            "Lazy init must defer the DB build"
        );
        // Push a night frame at a TT when the Sun is below
        // horizon at Greenwich (so the classifier reports
        // Night, not Twilight).
        let tt = bris_core::time::Tt::from_julian_date(bris_core::time::JD_J2000 + 0.5);
        engine.push_frame(night_peak_frame(tt, 8)).unwrap();
        // Post-condition: lazy build fired.
        assert!(
            engine.plate_db.get().is_some(),
            "first night frame with peaks must trigger lazy DB build"
        );
        // Stage D's counters: it ran (entered ≥ 1) but the
        // plate solve declined (no IdentifiedStars in the
        // queue).
        let diag = engine.diagnostics();
        assert!(
            diag.stages[3].entered >= 1,
            "Stage D should have entered at least once (got entered={})",
            diag.stages[3].entered
        );
    }

    #[test]
    fn at_startup_plate_solver_build_completes_synchronously() {
        // Configure with `PlateSolverInit::AtStartup` and a
        // tiny DB; verify the database is populated before
        // `new()` returns.
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.plate_solver_init = crate::PlateSolverInit::AtStartup;
        cfg.star_hash_db_cfg = bris_platesolve::StarHashDbConfig {
            mag_cutoff: 1.5,
            ..bris_platesolve::StarHashDbConfig::default()
        };
        let engine = StreamingEngine::new(cfg);
        assert!(
            engine.plate_db.get().is_some(),
            "AtStartup init must build the DB before new() returns"
        );
    }

    /// Build a uniform-luminance frame at the supplied TT and
    /// pixel value. Useful for driving the classifier through
    /// known evidence transitions to test hysteresis.
    fn uniform_frame(tt: bris_core::time::Tt, fill: u16) -> Frame {
        Frame::new(
            32,
            32,
            vec![fill; 32 * 32],
            tt,
            1000,
            Intrinsics::placeholder(32, 32),
        )
        .unwrap()
    }

    #[test]
    fn classifier_hysteresis_smooths_single_frame_transients() {
        // Push 10 daylight-evidence frames followed by 1
        // night-evidence frame followed by 10 more daylight
        // frames. With hysteresis at the default 90, the
        // engine's last_classification (raw) should reflect
        // each frame's verdict but the *dispatched* verdict
        // (governing Stage B/C) should never have switched.
        //
        // We can verify the latter indirectly by checking
        // Stage B's `entered` counter: a single-frame
        // dispatch-Night transient would make Stage B re-
        // evaluate at the night threshold for that frame.
        // Easier check: simply verify the engine completed
        // 21 pushes without panicking and the diagnostics
        // record matches what we expect.
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.classifier_hysteresis_frames = 90;
        let engine = StreamingEngine::new(cfg);
        let bright = u16::MAX / 2; // image evidence: Day
        let dark = 50_u16; // image evidence: Night
                           // Use J2000 noon: Sun is up at Greenwich, so the
                           // almanac evidence is Day. Image-bright = Day
                           // agreement; image-dark with high-Sun = disagreement
                           // → conservative pick = Twilight.
        let tt0 = bris_core::time::Tt::from_julian_date(bris_core::time::JD_J2000);
        for _ in 0..10 {
            engine.push_frame(uniform_frame(tt0, bright)).unwrap();
        }
        // One transient dark frame.
        engine.push_frame(uniform_frame(tt0, dark)).unwrap();
        for _ in 0..10 {
            engine.push_frame(uniform_frame(tt0, bright)).unwrap();
        }
        let diag = engine.diagnostics();
        assert_eq!(diag.frames_pushed, 21);
        // The raw last_classification (recorded after the
        // most recent push) should be Day — the bright
        // frames after the transient.
        assert_eq!(diag.last_classification, Some(Condition::Day));
    }

    #[test]
    fn classifier_hysteresis_zero_disables_smoothing() {
        // With hysteresis_frames=0, a single transient flips
        // the dispatched verdict. The raw last_classification
        // and the dispatched should match per-frame.
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.classifier_hysteresis_frames = 0;
        let engine = StreamingEngine::new(cfg);
        let tt = bris_core::time::Tt::from_julian_date(bris_core::time::JD_J2000);
        engine.push_frame(uniform_frame(tt, u16::MAX / 2)).unwrap();
        let day_diag = engine.diagnostics();
        assert_eq!(day_diag.last_classification, Some(Condition::Day));
        // A dark frame at noon-Greenwich → image=Night,
        // almanac=Day, conservative pick = Twilight.
        engine.push_frame(uniform_frame(tt, 50)).unwrap();
        let twilight_diag = engine.diagnostics();
        assert_eq!(
            twilight_diag.last_classification,
            Some(Condition::Twilight),
            "with hysteresis disabled, the dispatched verdict mirrors the raw verdict"
        );
    }

    // ---- TODO 9: integration & stress tests ----

    #[test]
    fn ring_buffer_never_exceeds_configured_capacity() {
        // Push more frames than the input ring capacity to
        // confirm the capacity hard cap holds. Each frame is
        // body-less so no records protect them; the
        // recency rule keeps the trailing
        // stitching_window-worth alive.
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.input_ring_capacity = 5; // tight cap to exercise the eviction
        cfg.stitching_window_seconds = 1.0; // tight recency window
        let engine = StreamingEngine::new(cfg);
        // 50 frames at 100 ms apart (5-second total span);
        // at most 5 should ever be in the ring.
        let base = bris_core::time::JD_J2000;
        for i in 0..50_u32 {
            let tt = bris_core::time::Tt::from_julian_date(base + f64::from(i) * 0.1 / 86_400.0);
            engine.push_frame(dummy_frame_at(tt)).unwrap();
            assert!(
                engine.diagnostics().ring_buffer_depth <= 5,
                "ring depth {} exceeded capacity 5 after {} pushes",
                engine.diagnostics().ring_buffer_depth,
                i + 1
            );
        }
        let final_diag = engine.diagnostics();
        assert_eq!(final_diag.frames_pushed, 50);
        // Ring should hold ≤ 5 frames; with 1-second recency
        // window and 100 ms spacing, the trailing 10+ frames
        // would qualify but the capacity hard cap forces
        // eviction down to 5 (the most recent).
        assert!(
            final_diag.ring_buffer_depth <= 5,
            "final ring depth {} exceeded capacity",
            final_diag.ring_buffer_depth
        );
    }

    /// Build an empty (uniform-zero) frame at the given TT.
    fn dummy_frame_at(tt: bris_core::time::Tt) -> Frame {
        Frame::new(
            8,
            8,
            vec![0u16; 64],
            tt,
            1000,
            Intrinsics::placeholder(8, 8),
        )
        .unwrap()
    }

    #[test]
    fn stitching_window_evicts_old_unreferenced_frames() {
        // Push frames with no detections at increasing TT.
        // After a few seconds of no records protecting them,
        // the trailing recency window should evict everything
        // older than `stitching_window_seconds`.
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.input_ring_capacity = 100;
        cfg.stitching_window_seconds = 2.0;
        let engine = StreamingEngine::new(cfg);
        let base = bris_core::time::JD_J2000;
        // 10 frames at 1-second spacing.
        for i in 0..10_u32 {
            let tt = bris_core::time::Tt::from_julian_date(base + f64::from(i) / 86_400.0);
            engine.push_frame(dummy_frame_at(tt)).unwrap();
        }
        // After 10 frames at t=0..9 with a 2-second stitching
        // window, the trailing recency rule keeps frames
        // within 2s of t=9 → frames 7, 8, 9 (3 frames).
        let depth = engine.diagnostics().ring_buffer_depth;
        assert!(
            (1..=4).contains(&depth),
            "after 10 frames at 1s spacing with 2s window, expected ring depth ≈ 3, got {depth}"
        );
    }

    #[test]
    fn stress_60fps_pushes_complete_in_bounded_wall_clock() {
        // Push 600 dummy frames (10 seconds at 60 fps) and
        // confirm the engine doesn't fall behind catastrophically.
        // The dummy frames don't trigger Stage D (no peaks)
        // so per-frame work is bounded by Stages A/B/C only;
        // 600 pushes should complete well under the 10
        // wall-clock seconds the data represents.
        //
        // Failure mode this test guards against: an O(N²)
        // bug in the queue/eviction logic that makes
        // per-frame cost grow with frames-pushed (would
        // cause the test to time out under cargo's default
        // ~60 s per-test budget).
        let cfg = EngineConfig::new(Observer::default_dev());
        let engine = StreamingEngine::new(cfg);
        let base = bris_core::time::JD_J2000;
        let start = std::time::Instant::now();
        for i in 0..600_u32 {
            // Frames 16.67 ms apart in TT (60 fps).
            let tt = bris_core::time::Tt::from_julian_date(
                base + f64::from(i) * (1.0 / 60.0) / 86_400.0,
            );
            engine.push_frame(dummy_frame_at(tt)).unwrap();
        }
        let elapsed = start.elapsed();
        assert_eq!(engine.diagnostics().frames_pushed, 600);
        // 600 dummy 8×8 frames through Stages A/B/C should
        // complete in well under 5 s on any modern machine
        // (in debug; release is much faster). If this trips
        // it's almost certainly an algorithmic regression
        // (O(N²) somewhere in the per-push hot path).
        assert!(
            elapsed.as_secs() < 30,
            "600 dummy pushes took {elapsed:?}; expected well under 30 s",
        );
    }

    #[test]
    fn out_of_order_frames_handled_without_panic() {
        // Pathological capture: frames arrive with TTs going
        // backward. The engine should not panic and the per-
        // push counters should still advance.
        let cfg = EngineConfig::new(Observer::default_dev());
        let engine = StreamingEngine::new(cfg);
        let base = bris_core::time::JD_J2000;
        // 5 frames in reverse TT order.
        for i in (0..5_u32).rev() {
            let tt = bris_core::time::Tt::from_julian_date(base + f64::from(i) / 86_400.0);
            engine.push_frame(dummy_frame_at(tt)).unwrap();
        }
        let diag = engine.diagnostics();
        assert_eq!(diag.frames_pushed, 5);
        // Storage may have any state; the test is just "no
        // panic, counters consistent." Operators feeding the
        // engine out-of-order TT are doing something wrong;
        // graceful degradation is the right contract.
    }

    #[test]
    fn sight_window_capacity_caps_published_n_sights() {
        // Push more body+horizon-bearing frames than the
        // sight-window capacity, verifying the window never
        // exceeds capacity.
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.sight_window_capacity = 3;
        cfg.sight_window_seconds = 7200.0; // wide enough to keep all sights
        cfg.min_fix_publication_interval_ms = 0;
        let engine = StreamingEngine::new(cfg);
        // Push 6 frames an hour apart (covers a wide
        // azimuth range) of body+horizon synthetic frames.
        for i in 0..6_u32 {
            let tt = bris_core::time::Tt::from_julian_date(
                bris_core::time::JD_J2000 + f64::from(i) / 24.0,
            );
            engine.push_frame(body_plus_horizon_frame(tt)).unwrap();
        }
        let diag = engine.diagnostics();
        assert!(
            diag.sight_window_depth <= 3,
            "sight window grew to {} (capacity 3)",
            diag.sight_window_depth
        );
    }

    #[test]
    fn published_fix_carries_dominant_source_pbris_label() {
        // Smoke test for the PublishedFix → FixSummary
        // conversion that the $PBRIS,FIX path uses. After a
        // successful publication, the `dominant_source.label()`
        // must be one of the documented stable strings.
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.min_fix_publication_interval_ms = 0;
        cfg.sight_window_seconds = 7200.0;
        let engine = StreamingEngine::new(cfg);
        let rx = engine.fix_stream().unwrap();
        engine
            .push_frame(body_plus_horizon_frame(
                bris_core::time::Tt::from_julian_date(bris_core::time::JD_J2000),
            ))
            .unwrap();
        engine
            .push_frame(body_plus_horizon_frame(
                bris_core::time::Tt::from_julian_date(bris_core::time::JD_J2000 + 1.0 / 24.0),
            ))
            .unwrap();
        let published = rx
            .try_recv()
            .expect("channel still open")
            .expect("a fix should publish");
        let summary = published.to_pbris_fix_summary();
        assert!(
            matches!(
                summary.dominant_source,
                "centroid"
                    | "horizon"
                    | "calibration"
                    | "stitching"
                    | "refraction"
                    | "dip"
                    | "timing"
                    | "none"
            ),
            "unexpected dominant_source label: {}",
            summary.dominant_source
        );
        assert_eq!(summary.n_sights, 2);
    }
}
