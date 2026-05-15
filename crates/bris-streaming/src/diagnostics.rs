//! Engine diagnostics snapshot.
//!
//! [`EngineDiagnostics`] captures the engine's observable state at
//! one instant in time: per-stage processing counts, queue
//! occupancies, sight window contents, and the most recent
//! classification verdict. Intended for periodic polling by a
//! status display (CLI `bris status`, mobile overlay) and for
//! verification in the integration tests (TODO 9 in `plan.org`).
//!
//! Diagnostics are *snapshots*: cheap to acquire (the engine
//! holds the underlying state behind a mutex / atomics), safe to
//! read at any cadence, and never block the worker thread for
//! long. The returned struct is `Clone` and contains owned data.

use bris_core::time::Tt;
use bris_vision::Condition;

/// Engine state snapshot.
#[derive(Debug, Clone)]
pub struct EngineDiagnostics {
    /// Total number of frames pushed to the engine since
    /// construction. Incremented by each
    /// [`crate::StreamingEngine::push_frame`] call (including
    /// drops).
    pub frames_pushed: u64,

    /// Number of frames silently dropped because the input ring
    /// buffer was full at push time. Always ≤ `frames_pushed`.
    /// Persistent non-zero growth means processing isn't keeping
    /// up with capture; either reduce capture rate or relax
    /// per-stage cost.
    pub frames_dropped: u64,

    /// Per-stage processing statistics, in stage order
    /// (classifier, body, horizon, plate-solve, sight-assembly).
    /// Mostly counts and σ summaries; see [`PipelineStageStats`].
    pub stages: [PipelineStageStats; 5],

    /// Number of body detection records currently in the body
    /// queue (Stage B output retained for Stage E pairing).
    pub body_queue_depth: usize,

    /// Number of horizon line records currently in the horizon
    /// queue (Stage C output retained for Stage E pairing).
    pub horizon_queue_depth: usize,

    /// Number of raw frames currently in the ring buffer (kept as
    /// stitching intermediaries within
    /// [`crate::EngineConfig::stitching_window_seconds`]).
    pub ring_buffer_depth: usize,

    /// Number of sights currently in the active sight window.
    pub sight_window_depth: usize,

    /// Most recent classifier verdict. `None` until the first
    /// frame has been processed.
    pub last_classification: Option<Condition>,

    /// Capture timestamp (TT) of the most recent processed
    /// frame. `None` until the first frame has completed Stage
    /// A.
    pub last_processed_frame_tt: Option<Tt>,

    /// Capture timestamp (TT) of the most recent published fix.
    /// `None` until the first publication.
    pub last_published_fix_tt: Option<Tt>,

    /// Resolution `(width, height)` at which Stage C (horizon
    /// detection) ran on the most recent processed frame.
    /// Equals the source frame's resolution unless
    /// [`crate::EngineConfig::horizon_analysis_size`] was set
    /// to a smaller resolution and the pyramid level was
    /// successfully computed. `None` until the first frame
    /// has completed Stage C.
    pub last_horizon_analysis_size: Option<(u32, u32)>,
}

/// Per-stage processing statistics.
///
/// Counts are monotonic since engine construction. σ summaries
/// are over the most recent processed frame for that stage; they
/// reset per frame, not per query.
#[derive(Debug, Clone, Copy, Default)]
pub struct PipelineStageStats {
    /// Number of frames that entered this stage (regardless of
    /// outcome).
    pub entered: u64,
    /// Number of frames that produced one or more output records
    /// from this stage.
    pub produced: u64,
    /// Number of frames where this stage failed (no records
    /// produced and the error was not "no detections" but a
    /// hard error). For Stage A the classifier never errors so
    /// this is always 0; for later stages it counts genuine
    /// failures.
    pub failed: u64,
    /// Number of frames where this stage was *skipped* under the
    /// per-stage early-rejection rule (the accumulated σ from
    /// prior stages exceeded what we could already get from the
    /// active sight window). Always 0 for Stage A.
    pub skipped: u64,
}
