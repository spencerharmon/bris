//! HTTP routes for the collector.
//!
//! Endpoints:
//!
//! - `GET  /v1/healthz` — liveness, no auth.
//! - `POST /v1/submissions` — multipart-form submission.
//!   Auth: bearer token.
//! - `GET  /v1/submissions` — list submissions (index-mirror
//!   query). Auth: bearer token. Spike-grade: no pagination,
//!   capped at 200 most-recent rows.
//!
//! Review-UI endpoints (download manifest, download a media
//! file) are tracked as follow-ups; the spike's review tooling
//! is `ls` + a JSON viewer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Serialize;
use tracing::{info, warn};

use crate::auth::bearer;
use crate::config::Config;
use crate::manifest::Manifest;
use crate::store::Store;

/// Shared state for handlers.
#[derive(Debug)]
pub struct AppState {
    /// Effective collector configuration.
    pub config: Config,
    /// Filesystem store + SQLite index.
    pub store: Store,
}

/// Construct the axum router.
///
/// Public so integration tests can mount the app in-process
/// against a tempdir store.
pub fn build_app(state: Arc<AppState>) -> Router {
    let body_limit = state.config.max_submission_bytes;
    Router::new()
        .route("/v1/healthz", get(healthz))
        .route(
            "/v1/submissions",
            post(post_submission).get(list_submissions),
        )
        .route("/v1/submissions/:id", get(get_submission_manifest))
        .route(
            "/v1/submissions/:id/media/:filename",
            get(get_submission_media),
        )
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(middleware::from_fn_with_state(state.clone(), bearer))
        .with_state(state)
}

/// `GET /v1/healthz` — liveness probe. Returns "ok" so that
/// `docker compose healthcheck` is trivial to wire.
async fn healthz() -> &'static str {
    "ok"
}

/// `POST /v1/submissions` — accept a multipart form. One part
/// must be named `manifest` and contain the manifest JSON; the
/// remaining parts are media files whose part-name = filename.
async fn post_submission(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<Json<SubmissionAccepted>, ErrorResponse> {
    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ErrorResponse::bad_request(format!("multipart error: {e}")))?
    {
        let name = field
            .name()
            .ok_or_else(|| ErrorResponse::bad_request("multipart part missing name".to_owned()))?
            .to_owned();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| ErrorResponse::bad_request(format!("multipart read: {e}")))?
            .to_vec();
        if name == "manifest" {
            manifest_bytes = Some(bytes);
        } else {
            files.push((name, bytes));
        }
    }

    let manifest_bytes = manifest_bytes
        .ok_or_else(|| ErrorResponse::bad_request("missing required `manifest` part".to_owned()))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| ErrorResponse::bad_request(format!("manifest parse: {e}")))?;

    // Cross-check declared media against uploaded files.
    let sizes: HashMap<String, u64> = files
        .iter()
        .map(|(name, bytes)| (name.clone(), bytes.len() as u64))
        .collect();
    manifest
        .validate(&sizes)
        .map_err(|e| ErrorResponse::bad_request(format!("manifest validate: {e}")))?;

    let id = ulid::Ulid::new().to_string();
    let dir = state
        .store
        .save_submission(&id, &manifest, &files)
        .map_err(|e| {
            warn!(error = %e, "save_submission failed");
            ErrorResponse::internal(format!("save: {e}"))
        })?;

    info!(
        submission_id = %id,
        kind = ?manifest.submission_kind,
        files = files.len(),
        path = %dir.display(),
        "submission accepted"
    );
    Ok(Json(SubmissionAccepted { id }))
}

/// `GET /v1/submissions` — return the 200 most recent
/// submissions from the index mirror. Spike-grade: no
/// pagination, no filtering.
async fn list_submissions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<SubmissionListEntry>>, ErrorResponse> {
    let conn = state.store_conn();
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, submitted_at, captured_at, device_uuid,
                    app_version, bris_core_version, has_gps, note_present
             FROM submissions
             WHERE soft_deleted_at IS NULL
             ORDER BY submitted_at DESC
             LIMIT 200",
        )
        .map_err(|e| ErrorResponse::internal(format!("sqlite prepare: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SubmissionListEntry {
                id: row.get(0)?,
                kind: row.get(1)?,
                submitted_at: row.get(2)?,
                captured_at: row.get(3)?,
                device_uuid: row.get(4)?,
                app_version: row.get(5)?,
                bris_core_version: row.get(6)?,
                has_gps: row.get::<_, i32>(7)? != 0,
                note_present: row.get::<_, i32>(8)? != 0,
            })
        })
        .map_err(|e| ErrorResponse::internal(format!("sqlite query: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ErrorResponse::internal(format!("sqlite collect: {e}")))?;
    Ok(Json(rows))
}

impl AppState {
    /// Borrow the index connection mutex for direct query use.
    /// `pub(crate)` so handlers can use it; not part of the
    /// stable surface.
    pub(crate) fn store_conn(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.store.lock_index()
    }
}

/// `GET /v1/submissions/:id` — return the manifest for one
/// submission. The body is the manifest bytes verbatim
/// (including any pretty-printing the collector produced when
/// it persisted them).
async fn get_submission_manifest(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ErrorResponse> {
    let manifest_path = lookup_manifest_path(&state, &id)?;
    let bytes = tokio::fs::read(&manifest_path).await.map_err(|e| {
        warn!(error = %e, path = %manifest_path.display(), "manifest read failed");
        ErrorResponse::internal(format!("manifest read: {e}"))
    })?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response())
}

/// `GET /v1/submissions/:id/media/:filename` — stream a single
/// media file. The filename is sanitized identically to the
/// write path; path-traversal attempts return 400.
async fn get_submission_media(
    State(state): State<Arc<AppState>>,
    AxumPath((id, filename)): AxumPath<(String, String)>,
) -> Result<Response, ErrorResponse> {
    if filename.contains('/') || filename.contains('\\') || filename.starts_with('.') {
        return Err(ErrorResponse::bad_request(format!(
            "filename {filename} contains a path separator or leading dot"
        )));
    }
    let manifest_path = lookup_manifest_path(&state, &id)?;
    let dir = manifest_path
        .parent()
        .ok_or_else(|| ErrorResponse::internal("manifest path has no parent".to_owned()))?;
    let media_path = dir.join("media").join(&filename);
    if !media_path.exists() {
        return Err(ErrorResponse::not_found(format!(
            "{id}/media/{filename} not found"
        )));
    }
    let bytes = tokio::fs::read(&media_path).await.map_err(|e| {
        warn!(error = %e, path = %media_path.display(), "media read failed");
        ErrorResponse::internal(format!("media read: {e}"))
    })?;
    let ctype = guess_content_type(&filename);
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, ctype)],
        Body::from(bytes),
    )
        .into_response())
}

fn lookup_manifest_path(state: &AppState, id: &str) -> Result<PathBuf, ErrorResponse> {
    let conn = state.store_conn();
    let row: rusqlite::Result<String> = conn.query_row(
        "SELECT manifest_path FROM submissions WHERE id = ? AND soft_deleted_at IS NULL",
        [id],
        |r| r.get(0),
    );
    match row {
        Ok(p) => Ok(PathBuf::from(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(ErrorResponse::not_found(format!(
            "submission {id} not found"
        ))),
        Err(e) => Err(ErrorResponse::internal(format!("sqlite lookup: {e}"))),
    }
}

/// Guess a coarse content-type from a filename extension.
/// Sufficient for the spike's media set (PNG, JPEG, PGM, JSON,
/// plain text); falls back to `application/octet-stream`.
fn guess_content_type(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".pgm") {
        "image/x-portable-graymap"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".toml") {
        "application/toml"
    } else if lower.ends_with(".log") || lower.ends_with(".txt") {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

/// Successful POST response.
#[derive(Debug, Clone, Serialize)]
pub struct SubmissionAccepted {
    /// Server-assigned ULID for the new submission.
    pub id: String,
}

/// One row in the list response.
#[derive(Debug, Clone, Serialize)]
pub struct SubmissionListEntry {
    /// ULID.
    pub id: String,
    /// Submission kind label (`fix`, `calibration`,
    /// `debug_capture`).
    pub kind: String,
    /// Wall-clock UTC of submission.
    pub submitted_at: String,
    /// Wall-clock UTC of the captured event.
    pub captured_at: String,
    /// Per-install device UUID (un-hashed in this listing;
    /// access requires bearer auth).
    pub device_uuid: String,
    /// App version on the originating device.
    pub app_version: String,
    /// `bris-core` version on the originating device.
    pub bris_core_version: String,
    /// Whether the submission has GPS.
    pub has_gps: bool,
    /// Whether the submission has a note.
    pub note_present: bool,
}

/// Unified error response. Always renders as a JSON object
/// with `error` and (when present) `detail`.
#[derive(Debug)]
pub struct ErrorResponse {
    status: StatusCode,
    body: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    detail: Option<String>,
}

impl ErrorResponse {
    fn bad_request(detail: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: ErrorBody {
                error: "bad_request".to_owned(),
                detail: Some(detail),
            },
        }
    }
    fn not_found(detail: String) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: ErrorBody {
                error: "not_found".to_owned(),
                detail: Some(detail),
            },
        }
    }
    fn internal(detail: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: ErrorBody {
                error: "internal".to_owned(),
                detail: Some(detail),
            },
        }
    }
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(self.body)).into_response()
    }
}
