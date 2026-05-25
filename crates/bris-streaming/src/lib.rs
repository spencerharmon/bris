//! Continuous-operation streaming engine for Bris.
//!
//! `bris-streaming` orchestrates the existing per-stage primitives
//! ([`bris_vision`] for detection, [`bris_almanac`] for ephemerides,
//! [`bris_platesolve`] for night plate solving, [`bris_nav`] for sight
//! reduction and fix combination, [`bris_nmea`] for NMEA emission)
//! into a single engine that:
//!
//! 1. Accepts a stream of [`bris_vision::Frame`] values from a
//!    capture source via [`StreamingEngine::push_frame`].
//! 2. Runs each frame through a staged pipeline (classify → detect
//!    body → detect horizon → optional plate solve → sight assembly)
//!    with per-stage early rejection driven by accumulated σ.
//! 3. Maintains independent priority queues for body and horizon
//!    detection records, each keyed on σ. A frame's body record can
//!    pair with a *different* frame's horizon record via stitching.
//! 4. Holds raw frames in a ring buffer covering the stitching
//!    window so that body-less / horizon-less frames remain
//!    available as stitching intermediaries.
//! 5. Emits [`PublishedFix`] values whenever the active sight
//!    window changes meaningfully.
//!
//! The architecture rationale, full stage taxonomy with σ
//! accounting, eviction criteria, and sight-window cap math are in
//! [`docs/design/frame_scheduling.md`](https://github.com/anomalyco/bris/blob/main/docs/design/frame_scheduling.md).
//! This crate's docstrings explain *what* each module does; that
//! design document explains *why* the engine has the shape it does.
//!
//! # Stage of development
//!
//! At Phase 3.5 commit 1 the public API surface is defined and
//! compiles against the existing crates' types. The processing
//! pipeline itself is stubbed out; subsequent commits fill in the
//! stages in the order given in `plan.org`'s Phase 3.5 TODOs.
//!
//! # Threading model
//!
//! The design doc specifies "start with one worker thread; add
//! parallelism only when measurement demands it." This crate's
//! types are designed for that single-worker baseline:
//! [`StreamingEngine::push_frame`] is non-blocking (drops on full
//! input ring), and [`StreamingEngine::fix_stream`] returns a
//! receiver that the consumer drains on whatever cadence it
//! prefers. The worker-thread implementation lands in a later
//! commit.

#![allow(
    // Some configuration enums (PlateSolverInit::Cached) carry
    // path data that hasn't been wired through to the solver yet.
    // Re-enable once the cached-db on-disk format exists.
    clippy::missing_const_for_fn,
    // The skeleton intentionally has unused fields in
    // EngineConfig / PublishedFix that the staged-pipeline
    // implementation will read; suppressing here keeps the
    // commit-1 build quiet.
    dead_code
)]

mod config;
mod diagnostics;
mod engine;
mod fix;
mod nmea;
mod pipeline;
mod store;

pub use config::{ColdStartEngineConfig, EngineConfig, PlateSolverInit};
pub use diagnostics::{EngineDiagnostics, PipelineStageStats};
pub use engine::PoolSight;
pub use engine::{FixReceiver, PushError, StreamingEngine};
pub use fix::{DominantSource, FixProvenance, PublishedFix};
pub use nmea::format_fix_as_nmea;
pub use store::{SightStore, StoreConfig, StoreError};
