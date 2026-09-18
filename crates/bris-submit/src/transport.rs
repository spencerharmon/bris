//! Transport: turning an operator-approved submission into a
//! `POST /v1/submissions` request.
//!
//! The [`Transport`] trait is the seam between this crate's
//! transport-agnostic logic (payload building, the operator
//! gate, the retry queue) and the actual bytes-on-the-wire. It
//! takes ONLY a [`ReviewedSubmission`], so no transport can ever
//! send an unreviewed submission.
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
    let s = &submission.submission;
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
        encode_multipart, multipart_content_type, CollectorEndpoint, ReviewedSubmission,
        SubmissionOutcome, Transport, TransportError,
    };

    /// A blocking HTTPS transport built on `ureq` (rustls TLS).
    ///
    /// Suitable for the CLI (`bris submit`) and for a
    /// synchronous on-device caller. The Android shell may
    /// instead implement [`Transport`] over its own coroutine
    /// HTTP stack; this is the reference implementation.
    #[derive(Debug)]
    pub struct HttpTransport {
        endpoint: CollectorEndpoint,
        agent: ureq::Agent,
    }

    impl HttpTransport {
        /// Build a transport for `endpoint` with a default
        /// timeout profile.
        #[must_use]
        pub fn new(endpoint: CollectorEndpoint) -> Self {
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(120))
                .build();
            Self { endpoint, agent }
        }
    }

    impl Transport for HttpTransport {
        fn send(
            &self,
            submission: &ReviewedSubmission,
        ) -> Result<SubmissionOutcome, TransportError> {
            let body = encode_multipart(submission)?;
            let url = self.endpoint.submissions_url();
            let mut req = self
                .agent
                .post(&url)
                .set(
                    "Authorization",
                    &format!("Bearer {}", self.endpoint.bearer_token),
                )
                .set("Content-Type", &multipart_content_type());
            // A per-device token is meaningless to the collector
            // without the device UUID it was minted for (see
            // `bris_collector::auth::device_or_admin_bearer`) —
            // omitting this header is exactly why a per-device
            // submission over this transport used to 401 even
            // with a correct token.
            if let Some(device_uuid) = &self.endpoint.device_uuid {
                req = req.set("X-Bris-Device-Uuid", device_uuid);
            }
            let resp = req.send_bytes(&body);
            match resp {
                Ok(response) => {
                    let text = response
                        .into_string()
                        .map_err(|e| TransportError::Transient(format!("read body: {e}")))?;
                    let parsed: SubmissionAccepted =
                        serde_json::from_str(&text).map_err(|e| TransportError::Permanent {
                            status: 200,
                            message: format!("unparseable accept body: {e}: {text}"),
                        })?;
                    Ok(SubmissionOutcome { id: parsed.id })
                }
                Err(ureq::Error::Status(code, response)) => {
                    let message = response.into_string().unwrap_or_default();
                    // 4xx is a definitive rejection; 5xx / 429 are
                    // retryable.
                    if (400..500).contains(&code) && code != 429 {
                        Err(TransportError::Permanent {
                            status: code,
                            message,
                        })
                    } else {
                        Err(TransportError::Transient(format!(
                            "collector returned {code}: {message}"
                        )))
                    }
                }
                Err(ureq::Error::Transport(t)) => {
                    Err(TransportError::Transient(format!("transport: {t}")))
                }
            }
        }
    }

    #[derive(serde::Deserialize)]
    struct SubmissionAccepted {
        id: String,
    }
}

#[cfg(feature = "http")]
pub use http_impl::HttpTransport;
