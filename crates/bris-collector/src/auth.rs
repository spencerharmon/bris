//! Bearer-token authentication middleware.
//!
//! Spike-grade: a single shared token compiled into every
//! deployed device. Per-device tokens issued on first contact
//! are tracked as the obvious follow-up.

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

use crate::routes::AppState;

/// Reject requests whose `Authorization` header does not match
/// the configured bearer token. Empty configured token disables
/// auth (tests only).
pub async fn bearer(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if state.config.bearer_token.is_empty() {
        return Ok(next.run(req).await);
    }
    let Some(value) = req.headers().get(header::AUTHORIZATION) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let Ok(s) = value.to_str() else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let Some(token) = s.strip_prefix("Bearer ") else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    // Constant-time compare to avoid a side channel on the
    // shared token.
    if !constant_time_eq(token.as_bytes(), state.config.bearer_token.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
