//! The explicit operator gate — the one-screen pre-upload
//! review.
//!
//! This module is the structural enforcement of Bris's
//! non-negotiable rule: **no automatic network calls; every
//! submission is an explicit operator action shown in a
//! one-screen pre-upload review.** A [`Submission`] is inert;
//! the only way to obtain a [`ReviewedSubmission`] — the sole
//! type any [`Transport`](crate::Transport) will accept — is to
//! wrap the submission in a [`SubmissionReview`], surface its
//! [`SubmissionReview::lines`] to the operator, and call
//! [`SubmissionReview::approve`]. There is no `From`, no
//! `Default`, no back door: an unreviewed submission literally
//! cannot be sent.

use crate::manifest::SubmissionKind;
use crate::Submission;

/// One line of the pre-upload review, describing exactly what is
/// about to leave the device. The shell renders these on the
/// one-screen review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLine {
    /// Short label (e.g. `"Kind"`, `"Files"`, `"GPS truth"`).
    pub label: String,
    /// Human-readable value.
    pub value: String,
}

impl ReviewLine {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// The pre-upload review wrapping a built [`Submission`].
///
/// Construct one with [`SubmissionReview::new`], show
/// [`Self::lines`] to the operator, and — only on an explicit
/// operator confirmation — call [`Self::approve`] to obtain the
/// sendable [`ReviewedSubmission`].
#[derive(Debug)]
pub struct SubmissionReview {
    submission: Submission,
}

impl SubmissionReview {
    /// Wrap a built submission in a review. This performs NO
    /// network activity and grants NO permission to send — it
    /// only prepares the review the operator must approve.
    #[must_use]
    pub fn new(submission: Submission) -> Self {
        Self { submission }
    }

    /// The exact bytes-about-to-leave summary, one line per
    /// salient fact. Rendered on the one-screen review.
    #[must_use]
    pub fn lines(&self) -> Vec<ReviewLine> {
        let m = &self.submission.manifest;
        let mut lines = vec![
            ReviewLine::new("Kind", m.submission_kind.label()),
            ReviewLine::new("Captured at", &m.captured_at),
            ReviewLine::new(
                "Files",
                format!(
                    "{} ({} bytes total)",
                    self.submission.part_count(),
                    self.submission.total_media_bytes()
                ),
            ),
            ReviewLine::new("Device model", &m.device.model),
            ReviewLine::new("App version", &m.versions.app),
        ];
        // GPS ground-truth is shown so the operator knows their
        // location is included — it is NEVER hidden.
        match &m.gps {
            Some(g) => lines.push(ReviewLine::new(
                "GPS ground-truth",
                format!(
                    "{:.5}, {:.5} (±{:.0} m, {})",
                    g.lat_deg, g.lon_deg, g.horizontal_accuracy_m, g.source
                ),
            )),
            None => lines.push(ReviewLine::new("GPS ground-truth", "not included")),
        }
        if let Some(note) = &m.note {
            lines.push(ReviewLine::new("Note", note));
        }
        // Reassure the operator that the assumed-position input
        // the engine ran against travels honestly inside the
        // verbatim bundle.json part.
        lines.push(ReviewLine::new(
            "Manifest",
            "bundle.json shipped verbatim (assumed-position input unchanged)",
        ));
        lines
    }

    /// The kind of submission under review.
    #[must_use]
    pub fn kind(&self) -> SubmissionKind {
        self.submission.manifest.submission_kind
    }

    /// Borrow the underlying submission (read-only) so the shell
    /// can render richer detail than [`Self::lines`] provides.
    #[must_use]
    pub fn submission(&self) -> &Submission {
        &self.submission
    }

    /// The explicit operator approval. Calling this consumes the
    /// review and yields the ONLY type a transport accepts.
    /// Callers MUST invoke this in direct response to an
    /// operator confirmation on the review screen — never
    /// automatically.
    #[must_use]
    pub fn approve(self) -> ReviewedSubmission {
        tracing::info!(
            kind = self.submission.manifest.submission_kind.label(),
            files = self.submission.part_count(),
            bytes = self.submission.total_media_bytes(),
            "submission approved by operator (explicit pre-upload review)"
        );
        ReviewedSubmission {
            submission: self.submission,
        }
    }
}

/// An operator-approved submission — the only thing a
/// [`Transport`](crate::Transport) will send. It can be produced
/// solely by [`SubmissionReview::approve`], so its mere
/// existence is proof the operator gate was passed.
#[derive(Debug, Clone)]
pub struct ReviewedSubmission {
    pub(crate) submission: Submission,
}

impl ReviewedSubmission {
    /// Borrow the approved submission.
    #[must_use]
    pub fn submission(&self) -> &Submission {
        &self.submission
    }
}
