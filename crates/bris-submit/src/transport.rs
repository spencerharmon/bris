//! Transport: turning an operator-approved submission into a
//! `POST /v1/submissions` request.
//!
//! The [`Transport`] trait is the seam between this crate's
//! transport-agnostic logic (payload building, the operator
//! gate, the retry queue) and the actual bytes-on-the-wire. It
//! retains the reviewed CLI API. Scoped automatic uploads use the same
//! encoder and HTTP implementation, gated by the queue's authorization.
//!
//! [`CollectorEndpoint`] carries the two RUNTIME config values —
//! the collector base URL and the bearer token — that must never
//! be compiled into source. Under the `http` feature,
//! [`HttpTransport`] is a blocking, rustls-TLS implementation
//! suitable for the CLI and for an on-device sync caller.

use crate::manifest::MediaItem;
use crate::review::ReviewedSubmission;
use std::path::Path;

/// The collector endpoint — supplied at RUNTIME, never compiled
/// into source. `base_url` is e.g. `https://collector.example`;
/// `bearer_token` authenticates the POST.
#[derive(Clone)]
pub struct CollectorEndpoint {
    /// Base URL of the collector, without a trailing slash
    /// (e.g. `https://collector.example`). The submitter POSTs
    /// to `{base_url}/v1/submissions`.
    pub base_url: String,
    /// Bearer token for `Authorization: Bearer <token>`.
    pub bearer_token: String,
    /// Device UUID to send in the `X-Bris-Device-Uuid` header
    /// when `bearer_token` is a per-device token (see
    /// `bris_collector::auth::device_or_admin_bearer`). `None`
    /// when `bearer_token` is the shared admin/bootstrap token,
    /// which the collector accepts on `POST /v1/submissions`
    /// without the device header.
    pub device_uuid: Option<String>,
}

impl std::fmt::Debug for CollectorEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the token in logs / debug output.
        f.debug_struct("CollectorEndpoint")
            .field("base_url", &self.base_url)
            .field("bearer_token", &"<redacted>")
            .field("device_uuid", &self.device_uuid)
            .finish()
    }
}

impl CollectorEndpoint {
    /// Construct an endpoint, trimming any trailing slash from
    /// `base_url` so URL joining is unambiguous. No device UUID
    /// is attached — use this when `bearer_token` is the shared
    /// admin/bootstrap token. For a per-device token, use
    /// [`Self::with_device_uuid`] afterwards, or the submission
    /// will be rejected `401` (the collector requires the
    /// `X-Bris-Device-Uuid` header to look up a per-device
    /// token's hash).
    #[must_use]
    pub fn new(base_url: impl Into<String>, bearer_token: impl Into<String>) -> Self {
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self {
            base_url,
            bearer_token: bearer_token.into(),
            device_uuid: None,
        }
    }

    /// Attach the device UUID to send as `X-Bris-Device-Uuid`
    /// alongside a per-device bearer token, per
    /// `bris_collector::auth::device_or_admin_bearer`. Required
    /// for `bearer_token` to be accepted as a per-device token;
    /// the shared admin token needs no device UUID.
    #[must_use]
    pub fn with_device_uuid(mut self, device_uuid: impl Into<String>) -> Self {
        self.device_uuid = Some(device_uuid.into());
        self
    }

    /// The full submissions URL.
    #[must_use]
    pub fn submissions_url(&self) -> String {
        format!("{}/v1/submissions", self.base_url)
    }
}

/// A successful submission outcome: the collector-assigned id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionOutcome {
    /// Server-assigned submission id (ULID).
    pub id: String,
}

/// Transport error, distinguishing retryable from terminal
/// failures so the queue knows whether to re-try.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The actual HTTP future was dropped; this is not a failed attempt.
    #[error("upload cancelled")]
    Cancelled,
    /// The request never reached a decision (DNS, connect,
    /// timeout, TLS, 5xx). The queue SHOULD retry.
    #[error("transient transport error (retryable): {0}")]
    Transient(String),
    /// The collector rejected the submission definitively (4xx —
    /// bad token, schema mismatch, checksum mismatch). Retrying
    /// the identical bytes will not help. The queue must NOT
    /// spin on it.
    #[error("permanent submission rejection ({status}): {message}")]
    Permanent {
        /// HTTP status code (or a synthetic code for non-HTTP
        /// terminal errors).
        status: u16,
        /// Server-supplied or synthesized message.
        message: String,
    },
}

impl TransportError {
    /// Whether the queue should retry this error later.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// A pluggable submission transport. Implementors POST the
/// approved submission and report the outcome.
pub trait Transport {
    /// Send an operator-approved submission. Returns the
    /// collector's assigned id on success.
    ///
    /// # Errors
    /// [`TransportError::Transient`] for retryable failures,
    /// [`TransportError::Permanent`] for terminal rejections.
    fn send(&self, submission: &ReviewedSubmission) -> Result<SubmissionOutcome, TransportError>;
}

/// A MIME multipart boundary token. Chosen to be improbable in
/// any payload; the encoder additionally guards against a body
/// that happens to contain it.
const BOUNDARY: &str = "----bris-submit-boundary-b8f1c2a4d6e09371";

/// Encode an approved submission as a `multipart/form-data`
/// body. Part order: `manifest` first, then every file part.
///
/// Returns the raw body bytes. The matching `Content-Type`
/// header value is [`multipart_content_type`].
///
/// # Errors
/// JSON serialization of the manifest, or a payload that
/// contains the multipart boundary (astronomically unlikely;
/// surfaced rather than silently corrupting the stream).
pub fn encode_multipart(submission: &ReviewedSubmission) -> Result<Vec<u8>, TransportError> {
    encode_submission(&submission.submission)
}

fn encode_submission(s: &crate::Submission) -> Result<Vec<u8>, TransportError> {
    let manifest_json = serde_json::to_vec(&s.manifest).map_err(|e| TransportError::Permanent {
        status: 0,
        message: format!("manifest serialize: {e}"),
    })?;

    let boundary_bytes = BOUNDARY.as_bytes();
    let mut body: Vec<u8> = Vec::new();

    // manifest part.
    write_part_header(
        &mut body,
        "manifest",
        Some("manifest.json"),
        "application/json",
    );
    guard_boundary(&manifest_json)?;
    body.extend_from_slice(&manifest_json);
    body.extend_from_slice(b"\r\n");

    // one part per file. The multipart part NAME must equal the
    // filename the manifest's `media[].filename` references (the
    // collector keys received parts by name).
    for part in &s.parts {
        guard_boundary(&part.bytes)?;
        write_part_header(
            &mut body,
            &part.filename,
            Some(&part.filename),
            content_type_for(&part.filename),
        );
        body.extend_from_slice(&part.bytes);
        body.extend_from_slice(b"\r\n");
    }

    // closing boundary.
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary_bytes);
    body.extend_from_slice(b"--\r\n");
    Ok(body)
}

/// The `Content-Type` header value that pairs with
/// [`encode_multipart`]'s body.
#[must_use]
pub fn multipart_content_type() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

fn write_part_header(body: &mut Vec<u8>, name: &str, filename: Option<&str>, ctype: &str) {
    body.extend_from_slice(b"--");
    body.extend_from_slice(BOUNDARY.as_bytes());
    body.extend_from_slice(b"\r\n");
    match filename {
        Some(fname) => body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{fname}\"\r\n")
                .as_bytes(),
        ),
        None => body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
        ),
    }
    body.extend_from_slice(format!("Content-Type: {ctype}\r\n\r\n").as_bytes());
}

fn guard_boundary(bytes: &[u8]) -> Result<(), TransportError> {
    if find_subslice(bytes, BOUNDARY.as_bytes()).is_some() {
        return Err(TransportError::Permanent {
            status: 0,
            message: "payload contains the multipart boundary token".to_owned(),
        });
    }
    Ok(())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Coarse content-type from a filename extension — matches the
/// collector's `guess_content_type` set.
fn content_type_for(filename: &str) -> &'static str {
    let ext = Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("pgm") => "image/x-portable-graymap",
        Some("json") => "application/json",
        Some("toml") => "application/toml",
        Some("jsonl") => "application/jsonl",
        Some("log" | "txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Assert (in debug) that the manifest's media list and the
/// actual parts agree — a cheap internal invariant check that
/// guards against a builder bug shipping mismatched
/// declaration/payload.
#[must_use]
pub fn media_matches_parts(submission: &ReviewedSubmission) -> bool {
    let s = &submission.submission;
    if s.manifest.media.len() != s.parts.len() {
        return false;
    }
    s.manifest
        .media
        .iter()
        .zip(&s.parts)
        .all(|(m, p): (&MediaItem, _)| {
            m.filename == p.filename
                && m.size_bytes == p.bytes.len() as u64
                && m.checksum_sha256.as_deref() == Some(p.checksum_sha256.as_str())
        })
}

#[cfg(feature = "http")]
mod http_impl {
    use super::{
        encode_submission, multipart_content_type, CollectorEndpoint, ReviewedSubmission,
        SubmissionOutcome, Transport, TransportError,
    };
    use crate::authorization::Cancellation;

    /// Synchronous HTTPS API driving cancellable async I/O with rustls.
    /// Runtime callers use a joined worker so the synchronous API remains usable.
    #[derive(Debug)]
    pub struct HttpTransport {
        endpoint: CollectorEndpoint,
    }

    impl HttpTransport {
        /// Build a transport for `endpoint` with a default
        /// timeout profile.
        #[must_use]
        pub fn new(endpoint: CollectorEndpoint) -> Self {
            Self { endpoint }
        }

        /// Validate config and hash the destination, credential, and device identity.
        /// Only the hash is persisted; bearer credentials never enter queue state.
        pub(crate) fn fingerprint(&self) -> Result<String, crate::QueueError> {
            use sha2::{Digest, Sha256};
            let url = reqwest::Url::parse(&self.endpoint.submissions_url())
                .map_err(|_| crate::QueueError::Unauthorized)?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || self.endpoint.bearer_token.trim().is_empty()
                || self
                    .endpoint
                    .device_uuid
                    .as_ref()
                    .is_some_and(|s| s.trim().is_empty())
            {
                return Err(crate::QueueError::Unauthorized);
            }
            reqwest::header::HeaderValue::from_str(&format!(
                "Bearer {}",
                self.endpoint.bearer_token
            ))
            .map_err(|_| crate::QueueError::Unauthorized)?;
            if let Some(uuid) = &self.endpoint.device_uuid {
                reqwest::header::HeaderValue::from_str(uuid)
                    .map_err(|_| crate::QueueError::Unauthorized)?;
            }
            let bytes = serde_json::to_vec(&(
                url.as_str(),
                &self.endpoint.bearer_token,
                &self.endpoint.device_uuid,
            ))?;
            Ok(hex::encode(Sha256::digest(bytes)))
        }

        pub(crate) fn send_cancellable(
            &self,
            submission: &crate::Submission,
            cancellation: &Cancellation,
        ) -> Result<SubmissionOutcome, TransportError> {
            if cancellation.is_cancelled() {
                return Err(TransportError::Cancelled);
            }
            self.fingerprint().map_err(|_| TransportError::Permanent {
                status: 0,
                message: "invalid collector endpoint".into(),
            })?;
            if tokio::runtime::Handle::try_current().is_ok() {
                // Preserve the existing synchronous transport API for callers
                // already inside a runtime. Join, never detach, the I/O owner.
                return std::thread::scope(|scope| {
                    scope
                        .spawn(|| self.send_cancellable(submission, cancellation))
                        .join()
                        .map_err(|_| TransportError::Transient("HTTP worker panicked".into()))?
                });
            }
            let body = encode_submission(submission)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| TransportError::Transient(format!("HTTP runtime: {e}")))?;
            runtime.block_on(async {
                // This runtime owns the connection tasks too. Returning from the
                // select drops the request AND runtime, not a detached HTTP thread.
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => Err(TransportError::Cancelled),
                    result = async {
                        let client = reqwest::Client::builder()
                            .redirect(reqwest::redirect::Policy::none())
                            .connect_timeout(std::time::Duration::from_secs(15))
                            .timeout(std::time::Duration::from_secs(120))
                            .build()
                            .map_err(|_| TransportError::Transient("HTTP client setup failed".into()))?;
                        let mut request = client.post(self.endpoint.submissions_url())
                            .bearer_auth(&self.endpoint.bearer_token)
                            .header("Content-Type", multipart_content_type())
                            .body(body);
                        if let Some(uuid) = &self.endpoint.device_uuid {
                            request = request.header("X-Bris-Device-Uuid", uuid);
                        }
                        let response = request.send().await
                            .map_err(|_| TransportError::Transient("HTTP request failed".into()))?;
                        let status = response.status();
                        let bytes = response.bytes().await
                            .map_err(|_| TransportError::Transient("HTTP response read failed".into()))?;
                        if status.is_success() {
                            let parsed: SubmissionAccepted = serde_json::from_slice(&bytes)
                                .map_err(|_| TransportError::Transient("invalid acceptance response".into()))?;
                            if parsed.id.is_empty() {
                                return Err(TransportError::Transient("empty collector id".into()));
                            }
                            Ok(SubmissionOutcome { id: parsed.id })
                        } else if status.is_server_error() || status.as_u16() == 429 {
                            Err(TransportError::Transient(format!("collector returned {status}")))
                        } else {
                            Err(TransportError::Permanent {
                                status: status.as_u16(),
                                message: "collector rejected submission (redirects disabled)".into(),
                            })
                        }
                    } => result,
                }
            })
        }
    }

    impl Transport for HttpTransport {
        fn send(
            &self,
            submission: &ReviewedSubmission,
        ) -> Result<SubmissionOutcome, TransportError> {
            self.send_cancellable(&submission.submission, &Cancellation::default())
        }
    }

    #[derive(serde::Deserialize)]
    struct SubmissionAccepted {
        id: String,
    }
}

#[cfg(feature = "http")]
pub use http_impl::HttpTransport;
