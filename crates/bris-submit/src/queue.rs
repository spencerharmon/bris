//! A crash-durable, retrying on-disk submission queue.
//!
//! An operator can approve a submission while offline, or the
//! collector can be transiently unreachable. Rather than lose
//! the submission or block the UI, [`SubmissionQueue`] persists
//! each approved submission to disk and drives retries with
//! bounded exponential backoff. A permanent rejection (4xx) is
//! moved to a dead-letter directory rather than retried forever.
//!
//! On-disk layout under `<root>/`:
//!
//! ```text
//! pending/<id>/          # awaiting (re)send
//!   manifest.json        # the submission manifest
//!   parts/<filename>     # each file part, verbatim
//!   state.json           # attempts, next_not_before, roles/checksums
//! .staging/<id>/         # transient assembly dir (atomic rename in)
//! sent/<id>/state.json   # succeeded (id + collector id) — audit trail
//! dead/<id>/             # permanently rejected — operator inspects
//! ```
//!
//! Enqueue is atomic: a submission is fully assembled under
//! `.staging/<id>/` and `rename(2)`d into `pending/<id>/`, so a
//! crash mid-enqueue never leaves a half-written pending entry.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::manifest::SubmissionManifest;
use crate::payload::SubmissionPart;
use crate::review::ReviewedSubmission;
use crate::transport::{Transport, TransportError};
use crate::Submission;

/// Persisted per-entry state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueuedState {
    /// Local queue id (ULID). Distinct from the collector's id.
    pub id: String,
    /// Number of send attempts made so far.
    pub attempts: u32,
    /// Unix-ms wall-clock before which the entry must not be
    /// retried (backoff gate). `0` means "eligible now".
    pub next_not_before_unix_ms: i64,
    /// Ordered part descriptors (filename + role + checksum),
    /// mirroring `parts/`. Lets the queue rebuild the exact
    /// [`Submission`] from disk without re-reading the manifest's
    /// media list.
    pub parts: Vec<PartDescriptor>,
    /// The collector-assigned id, once sent successfully.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collector_id: Option<String>,
    /// A terminal-rejection message, when dead-lettered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection: Option<String>,
}

/// A part's metadata, persisted in `state.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PartDescriptor {
    /// Filename (== part name == on-disk name under `parts/`).
    pub filename: String,
    /// Role string.
    pub role: String,
    /// Optional frame index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_index: Option<u32>,
    /// Optional per-frame capture time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    /// Lowercase-hex SHA-256 of the part bytes.
    pub checksum_sha256: String,
}

/// Queue error.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// I/O error with the offending path.
    #[error("queue io error at {path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// JSON (de)serialization error.
    #[error("queue json error: {0}")]
    Json(#[from] serde_json::Error),
    /// A queued entry on disk was malformed.
    #[error("corrupt queue entry {id}: {reason}")]
    Corrupt {
        /// The entry id.
        id: String,
        /// What was wrong.
        reason: String,
    },
}

fn io_err(path: &Path, source: std::io::Error) -> QueueError {
    QueueError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The result of a single [`SubmissionQueue::attempt_pending`]
/// pass over one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptResult {
    /// Sent successfully; entry moved to `sent/`.
    Sent {
        /// Local queue id.
        id: String,
        /// Collector-assigned id.
        collector_id: String,
    },
    /// Transient failure; entry stays pending with backoff.
    Retrying {
        /// Local queue id.
        id: String,
        /// The attempt count now recorded.
        attempts: u32,
    },
    /// Permanent rejection; entry moved to `dead/`.
    DeadLettered {
        /// Local queue id.
        id: String,
        /// Reason.
        reason: String,
    },
    /// Entry not yet eligible (backoff gate not elapsed).
    Deferred {
        /// Local queue id.
        id: String,
    },
}

/// The retrying, persistent submission queue.
#[derive(Debug)]
pub struct SubmissionQueue {
    root: PathBuf,
    /// Max attempts before giving up on a *transient* error and
    /// dead-lettering. A permanent error dead-letters
    /// immediately regardless.
    max_attempts: u32,
    /// Base backoff in milliseconds (doubles each attempt, up to
    /// [`Self::max_backoff_ms`]).
    base_backoff_ms: i64,
    /// Backoff ceiling.
    max_backoff_ms: i64,
}

impl SubmissionQueue {
    /// Open (creating if needed) a queue rooted at `root`.
    ///
    /// # Errors
    /// I/O error creating the directory tree.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, QueueError> {
        let root = root.into();
        for sub in ["pending", "sent", "dead", ".staging"] {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        }
        Ok(Self {
            root,
            max_attempts: 8,
            base_backoff_ms: 5_000,
            max_backoff_ms: 6 * 60 * 60 * 1_000,
        })
    }

    /// Override the retry policy (attempts + backoff bounds).
    #[must_use]
    pub fn with_policy(
        mut self,
        max_attempts: u32,
        base_backoff_ms: i64,
        max_backoff_ms: i64,
    ) -> Self {
        self.max_attempts = max_attempts.max(1);
        self.base_backoff_ms = base_backoff_ms.max(0);
        self.max_backoff_ms = max_backoff_ms.max(self.base_backoff_ms);
        self
    }

    fn pending_dir(&self) -> PathBuf {
        self.root.join("pending")
    }

    /// Persist an operator-approved submission to the queue,
    /// returning its local queue id. Crash-safe: assembled under
    /// `.staging/` then atomically renamed into `pending/`.
    ///
    /// # Errors
    /// I/O or JSON error while writing the entry.
    pub fn enqueue(&self, approved: &ReviewedSubmission) -> Result<String, QueueError> {
        let submission = &approved.submission;
        let id = ulid::Ulid::new().to_string();
        let staging = self.root.join(".staging").join(&id);
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|e| io_err(&staging, e))?;
        }
        let parts_dir = staging.join("parts");
        std::fs::create_dir_all(&parts_dir).map_err(|e| io_err(&parts_dir, e))?;

        // manifest.json
        let manifest_path = staging.join("manifest.json");
        let manifest_bytes = serde_json::to_vec_pretty(&submission.manifest)?;
        std::fs::write(&manifest_path, &manifest_bytes).map_err(|e| io_err(&manifest_path, e))?;

        // parts/<filename>
        let mut descriptors = Vec::with_capacity(submission.parts.len());
        for part in &submission.parts {
            let part_path = parts_dir.join(&part.filename);
            std::fs::write(&part_path, &part.bytes).map_err(|e| io_err(&part_path, e))?;
            descriptors.push(PartDescriptor {
                filename: part.filename.clone(),
                role: part.role.clone(),
                frame_index: part.frame_index,
                captured_at: part.captured_at.clone(),
                checksum_sha256: part.checksum_sha256.clone(),
            });
        }

        // state.json
        let state = QueuedState {
            id: id.clone(),
            attempts: 0,
            next_not_before_unix_ms: 0,
            parts: descriptors,
            collector_id: None,
            rejection: None,
        };
        Self::write_state(&staging, &state)?;

        // Atomic move into pending/.
        let dest = self.pending_dir().join(&id);
        std::fs::rename(&staging, &dest).map_err(|e| io_err(&dest, e))?;
        tracing::info!(queue_id = %id, "submission enqueued (durable)");
        Ok(id)
    }

    fn write_state(entry_dir: &Path, state: &QueuedState) -> Result<(), QueueError> {
        let path = entry_dir.join("state.json");
        let bytes = serde_json::to_vec_pretty(state)?;
        // Write to a temp then rename so a crash never leaves a
        // torn state.json.
        let tmp = entry_dir.join("state.json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| io_err(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))
    }

    /// List the local ids of all pending entries.
    ///
    /// # Errors
    /// I/O error reading the pending directory.
    pub fn pending_ids(&self) -> Result<Vec<String>, QueueError> {
        let dir = self.pending_dir();
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| io_err(&dir, e))? {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_owned());
                }
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Load a pending entry's state.
    ///
    /// # Errors
    /// Missing/corrupt entry, or I/O/JSON error.
    pub fn load_state(&self, id: &str) -> Result<QueuedState, QueueError> {
        let path = self.pending_dir().join(id).join("state.json");
        let bytes = std::fs::read(&path).map_err(|e| io_err(&path, e))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Rebuild the in-memory [`Submission`] for a pending entry
    /// from disk (manifest + part bytes).
    fn load_submission(&self, id: &str, state: &QueuedState) -> Result<Submission, QueueError> {
        let entry_dir = self.pending_dir().join(id);
        let manifest_path = entry_dir.join("manifest.json");
        let manifest_bytes =
            std::fs::read(&manifest_path).map_err(|e| io_err(&manifest_path, e))?;
        let manifest: SubmissionManifest = serde_json::from_slice(&manifest_bytes)?;
        let parts_dir = entry_dir.join("parts");
        let mut parts = Vec::with_capacity(state.parts.len());
        for d in &state.parts {
            let part_path = parts_dir.join(&d.filename);
            let bytes = std::fs::read(&part_path).map_err(|e| io_err(&part_path, e))?;
            // Verify integrity on load: the persisted bytes must
            // still match the recorded checksum. A mismatch is a
            // corrupt entry, never silently sent.
            let got = {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(&bytes);
                hex::encode(h.finalize())
            };
            if got != d.checksum_sha256 {
                return Err(QueueError::Corrupt {
                    id: id.to_owned(),
                    reason: format!(
                        "part {} checksum drift on disk (recorded {}, got {got})",
                        d.filename, d.checksum_sha256
                    ),
                });
            }
            parts.push(SubmissionPart {
                filename: d.filename.clone(),
                role: d.role.clone(),
                frame_index: d.frame_index,
                captured_at: d.captured_at.clone(),
                checksum_sha256: d.checksum_sha256.clone(),
                bytes,
            });
        }
        Ok(Submission { manifest, parts })
    }

    fn backoff_ms(&self, attempts: u32) -> i64 {
        // attempts is the count AFTER this failure.
        let shift = attempts.saturating_sub(1).min(30);
        let scaled = self
            .base_backoff_ms
            .saturating_mul(1_i64.checked_shl(shift).unwrap_or(i64::MAX));
        scaled.min(self.max_backoff_ms)
    }

    /// Attempt to send one pending entry through `transport`.
    ///
    /// Honors the entry's backoff gate. On success moves the
    /// entry to `sent/`; on a transient failure bumps attempts +
    /// backoff (dead-lettering once `max_attempts` is exhausted);
    /// on a permanent failure dead-letters immediately.
    ///
    /// # Errors
    /// I/O / JSON / corruption errors reading or moving the
    /// entry (a transport failure is NOT an error here — it is
    /// reflected in the returned [`AttemptResult`]).
    pub fn attempt_pending<T: Transport>(
        &self,
        id: &str,
        transport: &T,
    ) -> Result<AttemptResult, QueueError> {
        let mut state = self.load_state(id)?;
        if state.next_not_before_unix_ms > now_unix_ms() {
            return Ok(AttemptResult::Deferred { id: id.to_owned() });
        }
        let submission = self.load_submission(id, &state)?;
        let approved = ReviewedSubmission { submission };
        match transport.send(&approved) {
            Ok(outcome) => {
                state.collector_id = Some(outcome.id.clone());
                self.move_entry(id, "sent", &state)?;
                tracing::info!(queue_id = %id, collector_id = %outcome.id, "submission sent");
                Ok(AttemptResult::Sent {
                    id: id.to_owned(),
                    collector_id: outcome.id,
                })
            }
            Err(TransportError::Permanent { status, message }) => {
                let reason = format!("HTTP {status}: {message}");
                state.rejection = Some(reason.clone());
                self.move_entry(id, "dead", &state)?;
                tracing::warn!(queue_id = %id, %reason, "submission permanently rejected; dead-lettered");
                Ok(AttemptResult::DeadLettered {
                    id: id.to_owned(),
                    reason,
                })
            }
            Err(TransportError::Transient(msg)) => {
                state.attempts += 1;
                if state.attempts >= self.max_attempts {
                    let reason = format!(
                        "exhausted {} transient attempts; last: {msg}",
                        self.max_attempts
                    );
                    state.rejection = Some(reason.clone());
                    self.move_entry(id, "dead", &state)?;
                    tracing::warn!(queue_id = %id, %reason, "submission gave up after retries; dead-lettered");
                    return Ok(AttemptResult::DeadLettered {
                        id: id.to_owned(),
                        reason,
                    });
                }
                state.next_not_before_unix_ms = now_unix_ms() + self.backoff_ms(state.attempts);
                let entry_dir = self.pending_dir().join(id);
                Self::write_state(&entry_dir, &state)?;
                tracing::info!(queue_id = %id, attempts = state.attempts, %msg, "submission retrying after backoff");
                Ok(AttemptResult::Retrying {
                    id: id.to_owned(),
                    attempts: state.attempts,
                })
            }
        }
    }

    /// Drive one pass over every eligible pending entry.
    /// Returns the per-entry results. Entries still gated by
    /// backoff are reported as [`AttemptResult::Deferred`].
    ///
    /// # Errors
    /// The first I/O/corruption error encountered.
    pub fn drain_once<T: Transport>(
        &self,
        transport: &T,
    ) -> Result<Vec<AttemptResult>, QueueError> {
        let mut results = Vec::new();
        for id in self.pending_ids()? {
            results.push(self.attempt_pending(&id, transport)?);
        }
        Ok(results)
    }

    /// Move a pending entry into `sent/` or `dead/`, writing the
    /// final state first.
    fn move_entry(&self, id: &str, target: &str, state: &QueuedState) -> Result<(), QueueError> {
        let src = self.pending_dir().join(id);
        Self::write_state(&src, state)?;
        let dst = self.root.join(target).join(id);
        if dst.exists() {
            std::fs::remove_dir_all(&dst).map_err(|e| io_err(&dst, e))?;
        }
        std::fs::rename(&src, &dst).map_err(|e| io_err(&dst, e))
    }

    /// List dead-lettered entry ids (for operator inspection).
    ///
    /// # Errors
    /// I/O error reading the dead directory.
    pub fn dead_ids(&self) -> Result<Vec<String>, QueueError> {
        let dir = self.root.join("dead");
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| io_err(&dir, e))? {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_owned());
                }
            }
        }
        ids.sort();
        Ok(ids)
    }
}
