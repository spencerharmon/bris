//! Bearer-token authentication middleware.
//!
//! Two token classes:
//!
//! - **Admin/bootstrap token** (`Config::bearer_token`): compiled
//!   into every device at flash time and held by the operator.
//!   It authorizes device registration
//!   (`POST /v1/devices/register`) and every review/admin
//!   endpoint (list/get submissions, media download). The
//!   collector refuses to start if it is empty (see
//!   `bin/bris_collector.rs`).
//! - **Per-device token**: minted by the collector the first
//!   time a device registers (see [`crate::store::Store::register_device`])
//!   and persisted (hashed, never in the clear) in the index.
//!   Devices use it for every subsequent
//!   `POST /v1/submissions`, identifying themselves with the
//!   `X-Bris-Device-Uuid` header alongside the bearer token. The
//!   admin token also remains accepted on `POST /v1/submissions`
//!   (operator resubmission / tooling), so the per-device flow is
//!   additive, not a hard cutover.
//!
//! Device UUIDs are never logged raw: [`hashed_device_id`]
//! truncates a SHA-256 digest for log lines, so a leaked log
//! cannot be used to impersonate or correlate a specific device
//! outside the collector's own index.

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::routes::AppState;

/// Header a device sends alongside its per-device bearer token
/// on `POST /v1/submissions` so the collector knows which
/// device's token to check against.
pub const DEVICE_UUID_HEADER: &str = "x-bris-device-uuid";

/// Number of hex characters of the SHA-256 digest kept when
/// logging a device UUID. Long enough to disambiguate devices in
/// logs across a realistic fleet size; short enough that it is
/// not practically reversible to the original UUID.
const LOG_HASH_CHARS: usize = 12;

/// Reject requests whose `Authorization` header does not match
/// the configured admin/bootstrap token. Empty configured token
/// disables auth (tests only; the binary refuses to start with
/// an empty token).
pub async fn bearer(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if state.config.bearer_token.is_empty() {
        return Ok(next.run(req).await);
    }
    let Some(token) = extract_bearer(req.headers()) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if !constant_time_eq(token.as_bytes(), state.config.bearer_token.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

/// Authenticate `POST /v1/submissions`: accept either the
/// admin/bootstrap token (unchanged operator/tooling path) or a
/// per-device token, matched by SHA-256 hash against the value
/// [`crate::store::Store::register_device`] persisted for the
/// UUID named in the `X-Bris-Device-Uuid` header. Constant-time
/// compare on both paths so a submission's rejection latency
/// never leaks how much of the token matched.
pub async fn device_or_admin_bearer(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(token) = extract_bearer(req.headers()) else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    if !state.config.bearer_token.is_empty()
        && constant_time_eq(token.as_bytes(), state.config.bearer_token.as_bytes())
    {
        return Ok(next.run(req).await);
    }

    let Some(device_uuid) = req
        .headers()
        .get(DEVICE_UUID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let authenticated = state
        .store
        .verify_device_token(&device_uuid, token)
        .map_err(|e| {
            tracing::warn!(
                device = %hashed_device_id(&device_uuid),
                error = %e,
                "device token verification failed (store error)"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if !authenticated {
        tracing::warn!(
            device = %hashed_device_id(&device_uuid),
            "device submission rejected: token mismatch or unregistered device"
        );
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}

/// Constant-time byte compare (no early exit on the first
/// mismatch) to avoid a timing side channel on token equality
/// checks.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Hash + truncate a device UUID for logging. Never log
/// [`crate::manifest::Device::uuid`] or a registration/auth
/// header's device UUID raw — always route it through this
/// first.
#[must_use]
pub fn hashed_device_id(device_uuid: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(device_uuid.as_bytes());
    let digest = hasher.finalize();
    let full = hex::encode(digest);
    full.chars().take(LOG_HASH_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_device_id_is_stable_truncated_and_never_the_raw_uuid() {
        let uuid = "01HXYZTESTDEVICE0000000001";
        let hashed = hashed_device_id(uuid);
        assert_eq!(hashed.len(), LOG_HASH_CHARS);
        assert_ne!(hashed, uuid);
        assert!(!hashed.contains(uuid));
        // Deterministic: same UUID always hashes the same way, so
        // logs from the same device correlate without exposing it.
        assert_eq!(hashed, hashed_device_id(uuid));
        // Different UUIDs hash differently (overwhelmingly likely;
        // not a security property we assert cryptographically here,
        // just that the truncation didn't collide these two).
        assert_ne!(hashed, hashed_device_id("01HXYZTESTDEVICE0000000002"));
    }

    #[test]
    fn constant_time_eq_matches_and_rejects_mismatch() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
