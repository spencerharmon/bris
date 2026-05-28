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

    /// Provenance of the horizon emitted on the most recent
    /// processed frame (or `None` if the frame produced no
    /// horizon, or before the first frame). Surfaced to the
    /// FFI for HUD display so operators know which provider
    /// the on-screen horizon came from.
    pub last_horizon_provenance: Option<bris_vision::HorizonProvenance>,

    /// `altitude_sigma` of the horizon emitted on the most
    /// recent processed frame, in radians (1σ). `None` when
    /// no horizon was emitted on the most recent frame.
    pub last_horizon_altitude_sigma_rad: Option<f64>,

    /// Number of times the reflection-pair provider was
    /// invoked (i.e. ≥ 2 body candidates were present and
    /// the dispatched condition was actionable).
    pub reflection_pair_attempts: u64,
    /// Number of frames where the reflection-pair provider
    /// produced a hypothesis (Tests 1–4 all passed). A
    /// hypothesis may not have won the best-σ merge against
    /// the optical horizon; see [`Self::reflection_pair_used`]
    /// for emission count.
    pub reflection_pair_hypothesized: u64,
    /// Number of frames where the reflection-pair provider's
    /// hypothesis won the best-σ merge and was emitted as the
    /// frame's horizon outcome.
    pub reflection_pair_used: u64,
    /// Pair-level rejections inside the reflection-pair
    /// provider, broken down by test. A single attempt can
    /// increment multiple rejection counters (one per
    /// rejected pair).
    pub reflection_pair_rejected_geometric: u64,
    /// Pair-level rejections by Test 2 (photometric).
    pub reflection_pair_rejected_photometric: u64,
    /// Pair-level rejections by Test 3 (catalog consistency).
    pub reflection_pair_rejected_catalog: u64,
    /// Attempts that produced ≥ 1 surviving pair but no
    /// cluster met the minimum-size threshold (Test 4).
    pub reflection_pair_rejected_no_cluster: u64,

    /// Number of frames where the vertical-line provider
    /// produced a hypothesis (≥ 1 near-vertical line passed
    /// all filters). May not have won the best-σ merge; see
    /// [`Self::vertical_line_used`].
    pub vertical_line_hypothesized: u64,
    /// Number of frames where the vertical-line provider's
    /// hypothesis won the best-σ merge and is the frame's
    /// horizon outcome.
    pub vertical_line_used: u64,
    /// Number of frames where the vertical-line provider was
    /// invoked but found no near-vertical line above the
    /// minimum length / orientation gates.
    pub vertical_line_rejected_no_lines: u64,

    /// Number of frames where the vanishing-point provider
    /// was invoked (Stage C dispatched it after cheap
    /// detectors did not satisfy the early-termination
    /// threshold).
    pub vanishing_point_hypothesized: u64,
    /// Number of frames where the vanishing-point provider's
    /// hypothesis won the best-σ merge and was emitted as the
    /// frame's horizon outcome.
    pub vanishing_point_used: u64,
    /// Number of frames where the vanishing-point provider
    /// ran but found no VP cluster meeting the inlier and
    /// classification gates.
    pub vanishing_point_rejected_no_cluster: u64,

    /// Highest cluster size produced by the horizon-fusion
    /// layer across the session.
    pub horizon_fusion_cluster_size_max: usize,
    /// Frames where ≥ 2 providers produced concordant
    /// hypotheses and were fused for a tighter σ.
    pub horizon_fusion_clustered_frames: u64,
    /// Frames where ≥ 2 providers produced hypotheses but
    /// none were concordant; outcome fell back to the
    /// lowest-σ singleton. Non-zero values are an operator-
    /// visible signal that something is wrong (bad
    /// calibration, false-positive provider, multi-modal
    /// scene).
    pub horizon_fusion_discordant_frames: u64,
    /// Frames where only one provider produced any hypothesis;
    /// no fusion was possible.
    pub horizon_fusion_singleton_frames: u64,

    /// Total reduced sights persisted to disk since engine
    /// construction.
    pub sights_persisted_total: u64,
    /// Sights hydrated from disk at engine startup.
    pub sights_loaded_on_start: u64,
    /// Total published fixes persisted to disk since engine
    /// construction.
    pub fixes_persisted_total: u64,
    /// Append failures (disk full, permission, missing
    /// directory). Incremented per failed record; the record
    /// is dropped and the engine continues.
    pub store_append_failures: u64,
    /// Records skipped on load due to short trailing bytes or
    /// magic mismatch within a file.
    pub store_corrupted_records_skipped: u64,
    /// Archive files removed during retention pruning over the
    /// life of the engine. Today this is set at startup; future
    /// rotations will add to it.
    pub store_archive_files_pruned: u64,
    /// Current size of `sights/current.log` in bytes.
    pub store_current_log_bytes: u64,

    // Cold-start fix fallback counters.
    /// Number of times Stage E attempted the cold-start fix
    /// solver as a fallback to `multi_sight_fix`.
    pub cold_start_attempts: u64,
    /// Cold-start fallback runs that produced a fix that was
    /// published (either `Fix` or hemisphere-resolved
    /// `TwoCandidates`).
    pub cold_start_published: u64,
    /// Cold-start runs that returned `TwoCandidates` with no
    /// configured hemisphere hint; nothing was published.
    pub cold_start_ambiguous_skipped: u64,
    /// Cold-start runs that returned `Inconsistent`.
    pub cold_start_inconsistent_count: u64,
    /// Cold-start runs that errored with `Disjoint` (two
    /// circles with coincident / antipodal GPs).
    pub cold_start_disjoint_count: u64,
    /// Cold-start runs that beat a successful but stale-prior
    /// Saint-Hilaire fix and were published instead. Triggered
    /// when SH's max |intercept| exceeds
    /// `EngineConfig::cold_start.stale_prior_intercept_threshold_nm`
    /// and cold-start converges with a tighter `sigma_major_nm`.
    pub cold_start_preferred_over_stale_sh: u64,

    // Cumulative publication/gate counters.
    /// Cumulative number of fixes successfully published by
    /// Stage E since engine construction. Increments only on
    /// fixes that cleared the publication gate.
    pub fixes_published_total: u64,
    /// Cumulative number of times Stage E reached the
    /// publication step (i.e. window changed, throttle clear,
    /// `try_publish` invoked). Equals the sum of
    /// `fixes_published_total`, `singular_geometry_rejections`,
    /// and `publication_gate_rejections`.
    pub fix_publish_attempts: u64,
    /// Cumulative count of sights inserted into the active
    /// window since engine construction (post-hydration).
    pub sights_inserted_total: u64,
    /// Cumulative count of sights age-evicted from the active
    /// window since engine construction.
    pub sights_evicted_total: u64,
    /// Cumulative count of `multi_sight_fix` rejections for
    /// singular geometry (or any other LSQ refusal).
    pub singular_geometry_rejections: u64,
    /// Cumulative count of fixes the publication gate
    /// (azimuth spread / axis ratio / absolute σ / motion
    /// staleness) rejected after `multi_sight_fix` accepted.
    pub publication_gate_rejections: u64,

    /// Number of times the engine suppressed an AP re-derivation
    /// because [`crate::EngineConfig::lock_ap_for_replay`] was
    /// set. Diagnostic-only; production engines leave the lock
    /// flag off and this counter stays at 0.
    pub ap_rederive_suppressed_count: u64,

    /// Cumulative number of cross-frame sights (body and
    /// horizon detected in different frames) emitted into the
    /// sight window. Produced by
    /// [`bris_vision::panorama_altitude_for_pair`] at sight-
    /// emission time; the stitch σ reported by that helper is
    /// the executed Kabsch RMS residual, not the cheap
    /// time-gap estimate used during pair selection.
    pub cross_frame_sights_emitted: u64,
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
