//! HTTP routes for the collector.
//!
//! Endpoints:
//!
//! - `GET  /v1/healthz` (plus the bare `GET /healthz` the Kubernetes
//!   deploy probes hit and `GET /v1/health` the deploy acceptance
//!   check curls — all the same handler) — liveness, no auth.
//! - `POST /v1/devices/register` — first-contact device
//!   registration; issues a per-device bearer token. Auth:
//!   admin/bootstrap token.
//! - `POST /v1/submissions` — multipart-form submission.
//!   Auth: per-device bearer token (with the
//!   `X-Bris-Device-Uuid` header) or the admin/bootstrap token.
//! - `GET  /v1/submissions` — list submissions (index-mirror
//!   query). Auth: admin/bootstrap token. Spike-grade: no
//!   pagination, capped at 200 most-recent rows.
//!
//! See [`crate::auth`] for the two-token model.
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
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::auth::{bearer, device_or_admin_bearer, hashed_device_id};
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
///
/// Two distinct auth layers, applied per route group (never one
/// blanket layer over the whole router):
/// - `POST /v1/submissions` accepts the admin/bootstrap token OR
///   a per-device token (see [`device_or_admin_bearer`]).
/// - Every other non-public endpoint — device registration and
///   the review/admin surface (list, get manifest, get media) —
///   requires the admin/bootstrap token ([`bearer`]).
pub fn build_app(state: Arc<AppState>) -> Router {
    let body_limit = state.config.max_submission_bytes;

    let device_submission_routes = Router::new()
        .route("/v1/submissions", post(post_submission))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            device_or_admin_bearer,
        ));

    let admin_routes = Router::new()
        .route("/v1/devices/register", post(register_device))
        .route("/v1/submissions", get(list_submissions))
        .route("/v1/submissions/:id", get(get_submission_manifest))
        .route(
            "/v1/submissions/:id/media/:filename",
            get(get_submission_media),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), bearer));

    Router::new()
        .route("/v1/healthz", get(healthz))
        // Health aliases so EVERY documented probe path resolves to the
        // same unauthenticated liveness handler:
        // - `/healthz` — the bare path the Kubernetes readiness/liveness
        //   probes in the deploy contract (`deploy/reference/
        //   reference-deployment.yaml` and the flux
        //   `infrastructure/bris-collector` manifests) hit.
        // - `/v1/health` — the path the deploy acceptance check curls
        //   (`GET /v1/health` behind the private host).
        // Keeping all three aligned is what lets the pod reach Ready
        // and the post-deploy health probe answer 2xx behind the
        // private host. All are unauthenticated (a liveness probe must
        // not require the bearer token); an Authorization header, if
        // sent, is simply ignored.
        .route("/healthz", get(healthz))
        .route("/v1/health", get(healthz))
        .merge(device_submission_routes)
        .merge(admin_routes)
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(state)
}

/// `GET /v1/healthz` (plus the `/healthz` and `/v1/health` aliases)
/// — liveness probe. Returns "ok" so that `docker compose
/// healthcheck`, the Kubernetes readiness/liveness probes, and the
/// deploy acceptance health check are all trivial to wire.
async fn healthz() -> &'static str {
    "ok"
}

/// `POST /v1/devices/register` — first-contact device
/// registration. Admin/bootstrap-token authenticated (see
/// [`crate::auth::bearer`]): a device flashed with the shared
/// bootstrap token calls this once to obtain its own per-device
/// token, which it then uses for every subsequent
/// `POST /v1/submissions` instead of the shared token. Calling
/// this again for an already-registered `device_uuid` rotates
/// the token (see [`Store::register_device`]).
async fn register_device(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DeviceRegisterRequest>,
) -> Result<Json<DeviceRegisterResponse>, ErrorResponse> {
    if req.device_uuid.trim().is_empty() {
        return Err(ErrorResponse::bad_request(
            "device_uuid must not be empty".to_owned(),
        ));
    }
    let token = state.store.register_device(&req.device_uuid).map_err(|e| {
        warn!(
            device = %hashed_device_id(&req.device_uuid),
            error = %e,
            "register_device failed"
        );
        ErrorResponse::internal(format!("register_device: {e}"))
    })?;
    info!(
        device = %hashed_device_id(&req.device_uuid),
        "device registered; per-device token issued"
    );
    Ok(Json(DeviceRegisterResponse {
        device_uuid: req.device_uuid,
        token,
    }))
}

/// Request body for `POST /v1/devices/register`.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceRegisterRequest {
    /// The device's stable per-install UUID (the same value that
    /// lands in every submission's `manifest.device.uuid`).
    pub device_uuid: String,
}

/// Response body for `POST /v1/devices/register`.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceRegisterResponse {
    /// Echoes the registered UUID.
    pub device_uuid: String,
    /// The freshly-minted per-device bearer token. Returned only
    /// this once — the collector persists only its hash.
    pub token: String,
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

    // Checksum validation is a separate pass over the actual
    // bytes (size-only `validate` above already confirmed every
    // declared filename resolves to a received part).
    let received: HashMap<String, Vec<u8>> = files.iter().cloned().collect();
    manifest
        .validate_checksums(&received)
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
        device = %hashed_device_id(&manifest.device.uuid),
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
    // Files land in one of three places depending on role (see
    // `store::media_destination`): `media/`, `calibration/`, or
    // the submission root (`pbris.log`). Try each rather than
    // re-deriving the role here.
    let candidates = [
        dir.join("media").join(&filename),
        dir.join("calibration").join(&filename),
        dir.join(&filename),
    ];
    let media_path = candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| ErrorResponse::not_found(format!("{id}/media/{filename} not found")))?;
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
