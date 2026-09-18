//! On-device diagnostic-submission client for Bris.
//!
//! This crate is the shared, infrastructure-agnostic core of
//! the **collection path** (ROI Priority §1): it turns a
//! captured debug bundle into a `bris-bundle v1` submission and
//! POSTs it to a [`bris-collector`] `POST /v1/submissions`
//! endpoint behind an explicit operator gate. `bris-ffi`
//! (Android Submitter) and `bris-cli` (`bris submit`, the
//! embedded frontend) both consume it so there is exactly one
//! implementation of the wire format, the operator gate, and
//! the retry queue.
//!
//! # The non-negotiable invariants
//!
//! Bris does **no automatic network calls** ([`AGENTS.md`] hard
//! rule). This crate is the one network surface, and it upholds
//! the rule structurally:
//!
//! 1. **Every submission is an explicit operator action.** There
//!    is no code path that POSTs without a caller first
//!    constructing a [`SubmissionReview`] (the one-screen
//!    pre-upload review) and calling
//!    [`SubmissionReview::approve`]. A [`ReviewedSubmission`] —
//!    the *only* type any transport accepts — cannot be built
//!    except through that approval. There is no timer, no
//!    background poll, no "submit on capture" hook anywhere in
//!    this crate.
//! 2. **The manifest sent is the SAME [`BundleManifest`] the
//!    engine ran against.** [`build_submission`] reads the
//!    capture's on-disk `bundle.json` and ships it *verbatim* as
//!    a media item (`role = "bundle_manifest"`), so the stored
//!    submission is byte-identical to what replay would consume.
//!    `ap_input` is copied honestly; `gps_truth` is carried only
//!    as ground-truth and is **never** substituted for a missing
//!    `ap_input`.
//! 3. **Bearer token + collector base URL are RUNTIME config**,
//!    threaded through [`CollectorEndpoint`] — never compiled
//!    into this (or any) source tree.
//!
//! # Layers
//!
//! - [`manifest`] — the submission manifest schema (mirrors the
//!   collector's `POST /v1/submissions` contract).
//! - [`payload`] — [`build_submission`]: bundle directory →
//!   [`Submission`] (manifest + parts, each with a SHA-256).
//! - [`review`] — the operator gate: [`SubmissionReview`] →
//!   [`ReviewedSubmission`].
//! - [`transport`] — the [`Transport`] trait and (under the
//!   `http` feature) a blocking HTTPS implementation.
//! - [`queue`] — [`SubmissionQueue`]: an on-disk, crash-durable,
//!   retrying queue so a failed upload is retried later rather
//!   than lost.
//!
//! [`bris-collector`]: ../bris_collector/index.html
//! [`AGENTS.md`]: https://github.com/anomalyco/bris
//! [`BundleManifest`]: bris_bundle::BundleManifest

pub mod manifest;
pub mod payload;
pub mod queue;
pub mod review;
pub mod transport;

pub use manifest::{Device, Gps, MediaItem, SubmissionKind, SubmissionManifest, Versions};
pub use payload::{build_submission, SubmissionPart, SubmissionSource};
pub use queue::AttemptResult;
pub use queue::{QueueError, QueuedState, SubmissionQueue};
pub use review::{ReviewLine, ReviewedSubmission, SubmissionReview};
pub use transport::{CollectorEndpoint, SubmissionOutcome, Transport, TransportError};

use std::path::PathBuf;

/// A fully-built, not-yet-approved submission: the manifest plus
/// every part (each with its role, bytes, and SHA-256). It is
/// inert — it carries no transport and cannot be sent until it
/// passes through the operator gate ([`SubmissionReview`]).
#[derive(Debug, Clone)]
pub struct Submission {
    /// The submission manifest (the `manifest` multipart part).
    pub manifest: SubmissionManifest,
    /// Every file part, in the order they were collected. The
    /// first part is always the verbatim `bundle.json`
    /// (`role = "bundle_manifest"`).
    pub parts: Vec<SubmissionPart>,
}

impl Submission {
    /// Total byte size of all parts (media only; excludes the
    /// serialized manifest itself). Shown in the operator
    /// review so "the exact bytes about to leave the device"
    /// are quantified.
    #[must_use]
    pub fn total_media_bytes(&self) -> u64 {
        self.parts.iter().map(|p| p.bytes.len() as u64).sum()
    }

    /// Number of file parts.
    #[must_use]
    pub fn part_count(&self) -> usize {
        self.parts.len()
    }
}

/// Top-level error for the crate.
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    /// Wrapped I/O error, with the path that triggered it.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path being read/written.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The capture bundle could not be loaded/parsed.
    #[error("bundle error: {0}")]
    Bundle(#[from] bris_bundle::BundleError),
    /// JSON serialization failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// A required input was missing or invalid.
    #[error("invalid submission: {0}")]
    Invalid(String),
}
