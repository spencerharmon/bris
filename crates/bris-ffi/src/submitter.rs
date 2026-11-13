//! UniFFI surface for the on-device diagnostic **Submitter**.
//!
//! This is a thin wrapper over [`bris_submit`] — the real logic
//! (multipart `bris-bundle v1` payload build, the explicit
//! operator gate, the retrying persistent queue) lives there and
//! is shared with `bris-cli`. The FFI only adapts the types to
//! UniFFI value/handle shapes and exposes them to the Android
//! (Kotlin) shell.
//!
//! Manual uploads require review. Toggle-authorized automatic uploads use
//! the identical payload builder without manufacturing a manual approval.
//! Both Android modes require explicit, independently revocable generations.
//! The legacy reviewed API cannot flush scoped Android entries.
//!
//! # Runtime config, never compiled
//!
//! The collector base URL and bearer token are passed to
//! [`Submitter::flush`] as arguments ([`FfiCollectorEndpoint`]),
//! never baked into this crate.

use std::sync::Arc;

use bris_submit::review::SubmissionReview;
use bris_submit::transport::CollectorEndpoint;
use bris_submit::{build_submission, AttemptResult, SubmissionQueue, SubmissionSource};

use crate::FfiError;

/// Runtime identity/version facts stamped into a submission's
/// manifest. Supplied by the Android shell (per-install UUID,
/// device model/OS, app version, running core version).
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiSubmissionSource {
    /// Per-install device UUID / ULID.
    pub device_uuid: String,
    /// Device model name.
    pub device_model: String,
    /// Device OS string.
    pub device_os: String,
    /// Shell app version.
    pub app_version: String,
    /// Running `bris-core` version.
    pub bris_core_version: String,
    /// `bris-data` OTA payload version, if any.
    pub bris_data_version: Option<String>,
    /// Operator-entered free-text note, if any.
    pub note: Option<String>,
}

impl From<FfiSubmissionSource> for SubmissionSource {
    fn from(s: FfiSubmissionSource) -> Self {
        Self {
            device_uuid: s.device_uuid,
            device_model: s.device_model,
            device_os: s.device_os,
            app_version: s.app_version,
            bris_core_version: s.bris_core_version,
            bris_data_version: s.bris_data_version,
            note: s.note,
        }
    }
}

/// One line of the pre-upload review to render for the operator.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiReviewLine {
    /// Short label.
    pub label: String,
    /// Human-readable value.
    pub value: String,
}

/// The pre-upload review: exactly what is about to leave the
/// device, plus the opaque handle needed to approve it.
#[derive(Debug, uniffi::Object)]
pub struct FfiSubmissionReview {
    review: std::sync::Mutex<Option<SubmissionReview>>,
    lines: Vec<FfiReviewLine>,
    kind_label: String,
}

#[uniffi::export]
impl FfiSubmissionReview {
    /// The review lines to render on the one-screen review.
    #[must_use]
    pub fn lines(&self) -> Vec<FfiReviewLine> {
        self.lines.clone()
    }

    /// The submission kind label (`fix` / `calibration` /
    /// `debug_capture`).
    #[must_use]
    pub fn kind(&self) -> String {
        self.kind_label.clone()
    }
}

/// Runtime collector endpoint. Both fields are RUNTIME config,
/// never compiled into the app.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiCollectorEndpoint {
    /// Collector base URL (e.g. `https://collector.example`).
    pub base_url: String,
    /// Bearer token.
    pub bearer_token: String,
    /// Device UUID sent as `X-Bris-Device-Uuid`. REQUIRED when
    /// `bearer_token` is a per-device token issued by
    /// `POST /v1/devices/register`; `None` for the admin token.
    pub device_uuid: Option<String>,
}

/// Outcome of a single flush pass over one queued entry.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum FfiFlushOutcome {
    /// Sent successfully; collector assigned `collector_id`.
    Sent {
        /// Local queue id.
        queue_id: String,
        /// Collector-assigned id.
        collector_id: String,
    },
    /// Transient failure; will retry later.
    Retrying {
        /// Local queue id.
        queue_id: String,
        /// Attempts so far.
        attempts: u32,
    },
    /// Permanently rejected; moved to dead-letter.
    DeadLettered {
        /// Local queue id.
        queue_id: String,
        /// Reason.
        reason: String,
    },
    /// Still gated by backoff; not attempted this pass.
    Deferred {
        /// Local queue id.
        queue_id: String,
    },
}

impl From<AttemptResult> for FfiFlushOutcome {
    fn from(r: AttemptResult) -> Self {
        match r {
            AttemptResult::Sent { id, collector_id } => Self::Sent {
                queue_id: id,
                collector_id,
            },
            AttemptResult::Retrying { id, attempts } => Self::Retrying {
                queue_id: id,
                attempts,
            },
            AttemptResult::DeadLettered { id, reason } => Self::DeadLettered {
                queue_id: id,
                reason,
            },
            AttemptResult::Deferred { id } => Self::Deferred { queue_id: id },
        }
    }
}

/// The on-device Submitter: builds operator-gated submissions
/// and drives a durable retry queue rooted at a caller-supplied
/// directory.
#[derive(Debug, uniffi::Object)]
pub struct Submitter {
    queue: SubmissionQueue,
}

#[uniffi::export]
impl Submitter {
    /// Open (creating if needed) a submitter whose durable queue
    /// lives under `queue_root`.
    ///
    /// # Errors
    /// I/O error creating the queue directory.
    #[uniffi::constructor]
    pub fn open(queue_root: String) -> Result<Arc<Self>, FfiError> {
        let queue = SubmissionQueue::open(queue_root).map_err(|e| FfiError::Engine {
            detail: format!("open submission queue: {e}"),
        })?;
        Ok(Arc::new(Self { queue }))
    }

    /// Configure shared upload authorization for this queue root. `None`
    /// disables a mode. Supply opaque generations bound by Android to config
    /// and mode epoch; never reuse a revoked generation. Revocation cancels
    /// real HTTP immediately without waiting for an active drain to finish.
    /// A process restart defaults to disabled until this method succeeds.
    pub fn configure_uploads(
        &self,
        manual_scope: Option<String>,
        automatic_scope: Option<String>,
    ) -> Result<(), FfiError> {
        self.queue
            .configure_uploads(manual_scope, automatic_scope)
            .map_err(|e| FfiError::Engine {
                detail: format!("configure uploads: {e}"),
            })
    }

    /// Consume explicit manual review under the current manual generation.
    /// Returns the durable local queue id. Errors never enqueue unscoped work.
    pub fn approve_scoped(
        &self,
        review: Arc<FfiSubmissionReview>,
        scope: String,
    ) -> Result<String, FfiError> {
        let approved = review
            .review
            .lock()
            .map_err(|_| FfiError::Engine {
                detail: "review lock poisoned".into(),
            })?
            .take()
            .ok_or_else(|| FfiError::InvalidArgument {
                detail: "review already approved/consumed".into(),
            })?
            .approve();
        self.queue
            .enqueue_scoped(&approved, scope)
            .map_err(|e| FfiError::Engine {
                detail: format!("enqueue reviewed submission: {e}"),
            })
    }

    /// Build the same BundleManifest payload and enqueue only under current
    /// automatic-toggle authorization. No manual review and no network call.
    pub fn enqueue_automatic(
        &self,
        bundle_dir: String,
        source: FfiSubmissionSource,
        scope: String,
    ) -> Result<String, FfiError> {
        let submission = build_submission(std::path::Path::new(&bundle_dir), &source.into(), None)
            .map_err(|e| FfiError::InvalidArgument {
                detail: format!("build submission: {e}"),
            })?;
        self.queue
            .enqueue_automatic(&submission, scope)
            .map_err(|e| FfiError::Engine {
                detail: format!("enqueue automatic submission: {e}"),
            })
    }

    /// Drain exactly one authorized mode/generation, serialized across all
    /// Submitter instances sharing the root. Cancellation is an error, not a
    /// failed attempt. Endpoint/token/UUID binding persists across restarts.
    pub fn flush_scoped(
        &self,
        endpoint: FfiCollectorEndpoint,
        scope: String,
        automatic: bool,
    ) -> Result<Vec<FfiFlushOutcome>, FfiError> {
        self.queue
            .flush_scoped(endpoint.into(), scope, automatic)
            .map(|results| results.into_iter().map(Into::into).collect())
            .map_err(|e| FfiError::Engine {
                detail: format!("flush scoped queue: {e}"),
            })
    }

    /// Cancel active and waiting drains immediately (WorkManager stop).
    /// Still-authorized entries remain retryable without consuming an attempt.
    pub fn cancel_flush(&self) -> Result<(), FfiError> {
        self.queue.cancel_flush().map_err(|e| FfiError::Engine {
            detail: format!("cancel flush: {e}"),
        })
    }

    /// Build a pre-upload review for a capture bundle. This does
    /// NO network activity and grants NO permission to upload —
    /// it only prepares what the operator must approve.
    ///
    /// `calibration_dir`, when set, attaches calibration
    /// artifacts (making the submission a `calibration` kind).
    ///
    /// # Errors
    /// The bundle could not be read, or was empty.
    pub fn review(
        &self,
        bundle_dir: String,
        source: FfiSubmissionSource,
        calibration_dir: Option<String>,
    ) -> Result<Arc<FfiSubmissionReview>, FfiError> {
        let cal = calibration_dir.as_ref().map(std::path::Path::new);
        let submission = build_submission(std::path::Path::new(&bundle_dir), &source.into(), cal)
            .map_err(|e| FfiError::InvalidArgument {
            detail: format!("build submission: {e}"),
        })?;
        let kind_label = submission.manifest.submission_kind.label().to_owned();
        let review = SubmissionReview::new(submission);
        let lines = review
            .lines()
            .into_iter()
            .map(|l| FfiReviewLine {
                label: l.label,
                value: l.value,
            })
            .collect();
        Ok(Arc::new(FfiSubmissionReview {
            review: std::sync::Mutex::new(Some(review)),
            lines,
            kind_label,
        }))
    }

    /// The explicit operator approval: consume a review and
    /// persist the approved submission to the durable queue.
    /// Returns the local queue id.
    ///
    /// The Android shell MUST call this only in direct response
    /// to the operator confirming the review — never
    /// automatically.
    ///
    /// # Errors
    /// The review was already approved, or the enqueue failed.
    pub fn approve_and_enqueue(
        &self,
        review: Arc<FfiSubmissionReview>,
    ) -> Result<String, FfiError> {
        let approved = {
            let mut guard = review.review.lock().map_err(|_| FfiError::Engine {
                detail: "review lock poisoned".to_owned(),
            })?;
            guard
                .take()
                .ok_or_else(|| FfiError::InvalidArgument {
                    detail: "review already approved/consumed".to_owned(),
                })?
                .approve()
        };
        self.queue.enqueue(&approved).map_err(|e| FfiError::Engine {
            detail: format!("enqueue submission: {e}"),
        })
    }

    /// Drive one pass over every eligible queued submission,
    /// POSTing each to `endpoint`. An explicit operator action
    /// ("Upload now") — never a background poll.
    ///
    /// Returns one [`FfiFlushOutcome`] per pending entry.
    ///
    /// # Errors
    /// An I/O / corruption error reading the queue (a transport
    /// failure is reflected per-entry in the outcomes, not as an
    /// error).
    pub fn flush(&self, endpoint: FfiCollectorEndpoint) -> Result<Vec<FfiFlushOutcome>, FfiError> {
        let results = self
            .queue
            .flush(endpoint.into())
            .map_err(|e| FfiError::Engine {
                detail: format!("flush queue: {e}"),
            })?;
        Ok(results.into_iter().map(Into::into).collect())
    }

    /// Local ids of submissions still awaiting upload.
    ///
    /// # Errors
    /// I/O error reading the queue.
    pub fn pending_ids(&self) -> Result<Vec<String>, FfiError> {
        self.queue.pending_ids().map_err(|e| FfiError::Engine {
            detail: format!("list pending: {e}"),
        })
    }

    /// Local ids of permanently-rejected submissions (operator
    /// inspection).
    ///
    /// # Errors
    /// I/O error reading the queue.
    pub fn dead_ids(&self) -> Result<Vec<String>, FfiError> {
        self.queue.dead_ids().map_err(|e| FfiError::Engine {
            detail: format!("list dead: {e}"),
        })
    }
}

impl From<FfiCollectorEndpoint> for CollectorEndpoint {
    fn from(endpoint: FfiCollectorEndpoint) -> Self {
        let mut result = Self::new(endpoint.base_url, endpoint.bearer_token);
        result.device_uuid = endpoint.device_uuid;
        result
    }
}
