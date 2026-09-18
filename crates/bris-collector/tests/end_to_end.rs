//! End-to-end integration tests against the in-process axum
//! router.
//!
//! These tests build an [`AppState`] over a tempdir-rooted
//! store, mount the router, and exercise the same multipart
//! POST shape the Android `Submitter` produces. Locking the
//! wire contract here means future refactors of the collector
//! that break the contract surface as test failures rather
//! than failed Android uploads.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use bris_collector::routes::AppState;
use bris_collector::store::Store;
use bris_collector::{build_app, Config};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Boundary string used in our hand-built multipart bodies.
/// Real clients pick their own; the server doesn't care as
/// long as it matches the `Content-Type` header.
const BOUNDARY: &str = "bris-collector-test-boundary";

fn test_state(token: &str) -> Arc<AppState> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config {
        data_root: tmp.path().to_path_buf(),
        bind: "127.0.0.1:0".to_owned(),
        bearer_token: token.to_owned(),
        max_submission_bytes: 32 * 1024 * 1024,
        retention_days: 30,
    };
    let store = Store::open(&cfg.data_root).expect("store open");
    // Leak the tempdir so it survives the test (the State holds
    // a path into it). The OS reaps the tempdir when the process
    // exits, which for a test binary is right after the test.
    std::mem::forget(tmp);
    Arc::new(AppState { config: cfg, store })
}

fn multipart_body(parts: &[(&str, &str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, ctype, bytes) in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {ctype}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn fix_manifest_json(media_filename: &str, media_size: u64) -> String {
    serde_json::json!({
        "schema_version": 1,
        "submission_kind": "fix",
        "submitted_at": "2026-05-13T14:22:01Z",
        "device": {
            "uuid": "01HXYZTESTDEVICE0000000001",
            "model": "Test Device",
            "os": "Android 14 (API 34)",
        },
        "versions": {
            "app": "0.1.0",
            "bris_core": "0.0.1",
            "bris_data": null,
            "submission_schema": 1,
        },
        "captured_at": "2026-05-13T14:18:55Z",
        "gps": null,
        "note": null,
        "fix": { "lat_deg": 47.6062, "lon_deg": -122.3321 },
        "calibration": null,
        "debug_capture": null,
        "media": [
            {
                "filename": media_filename,
                "role": "fix_frame",
                "size_bytes": media_size,
                "frame_index": 1,
                "captured_at": "2026-05-13T14:18:55.123Z",
            }
        ],
    })
    .to_string()
}

fn fix_manifest_json_with_checksum(
    media_filename: &str,
    media_size: u64,
    checksum: &str,
) -> String {
    serde_json::json!({
        "schema_version": 1,
        "submission_kind": "fix",
        "submitted_at": "2026-05-13T14:22:01Z",
        "device": {
            "uuid": "01HXYZTESTDEVICE0000000001",
            "model": "Test Device",
            "os": "Android 14 (API 34)",
        },
        "versions": {
            "app": "0.1.0",
            "bris_core": "0.0.1",
            "bris_data": null,
            "submission_schema": 1,
        },
        "captured_at": "2026-05-13T14:18:55Z",
        "gps": null,
        "note": null,
        "fix": { "lat_deg": 47.6062, "lon_deg": -122.3321 },
        "calibration": null,
        "debug_capture": null,
        "media": [
            {
                "filename": media_filename,
                "role": "fix_frame",
                "size_bytes": media_size,
                "frame_index": 1,
                "captured_at": "2026-05-13T14:18:55.123Z",
                "checksum_sha256": checksum,
            }
        ],
    })
    .to_string()
}

#[tokio::test]
async fn healthz_responds_ok() {
    let state = test_state("");
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn submission_round_trip_lands_on_disk_and_in_index() {
    let state = test_state("test-token");
    let data_root = state.config.data_root.clone();
    let app = build_app(state);

    let frame_bytes: Vec<u8> = (0u32..1024)
        .map(|i| u8::try_from(i % 256).unwrap())
        .collect();
    let manifest = fix_manifest_json("frame_0001.png", frame_bytes.len() as u64);
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &frame_bytes),
    ]);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST /v1/submissions: body={:?}",
        String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes())
    );

    // List endpoint should now show one row.
    let list_resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/submissions")
                .header(header::AUTHORIZATION, "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_body = list_resp.into_body().collect().await.unwrap().to_bytes();
    let list: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(list.as_array().expect("array").len(), 1);
    assert_eq!(list[0]["kind"], "fix");
    assert_eq!(list[0]["device_uuid"], "01HXYZTESTDEVICE0000000001");

    // Filesystem layout: 2026/05/13/<ulid>/manifest.json + media/.
    let day_dir = data_root.join("submissions/2026/05/13");
    let entries: Vec<_> = std::fs::read_dir(&day_dir)
        .expect("day dir exists")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(entries.len(), 1, "exactly one submission dir");
    let sub_dir = entries[0].path();
    assert!(sub_dir.join("manifest.json").exists());
    assert!(sub_dir.join("media/frame_0001.png").exists());
    let stored_frame = std::fs::read(sub_dir.join("media/frame_0001.png")).unwrap();
    assert_eq!(stored_frame, frame_bytes);
}

#[tokio::test]
async fn rejects_missing_bearer_token() {
    let state = test_state("required-token");
    let app = build_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/submissions")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=x")
                .body(Body::from(Vec::<u8>::new()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rejects_size_mismatch() {
    let state = test_state("test-token");
    let app = build_app(state);
    // Manifest declares size_bytes = 1024, actual file is 10 bytes.
    let manifest = fix_manifest_json("frame_0001.png", 1024);
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &[0u8; 10]),
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("size mismatch"), "body was: {s}");
}

#[tokio::test]
async fn rejects_unknown_schema_version() {
    let state = test_state("test-token");
    let app = build_app(state);
    let manifest = serde_json::json!({
        "schema_version": 99,
        "submission_kind": "fix",
        "submitted_at": "2026-05-13T14:22:01Z",
        "device": { "uuid": "x", "model": "x", "os": "x" },
        "versions": { "app": "x", "bris_core": "x", "bris_data": null, "submission_schema": 99 },
        "captured_at": "2026-05-13T14:18:55Z",
        "gps": null,
        "note": null,
        "fix": {},
        "calibration": null,
        "debug_capture": null,
        "media": [],
    })
    .to_string();
    let body_bytes = multipart_body(&[("manifest", "application/json", manifest.as_bytes())]);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("schema_version"), "body was: {s}");
}

#[tokio::test]
async fn get_manifest_and_media_round_trip() {
    let state = test_state("test-token");
    let app = build_app(state);

    let frame_bytes: Vec<u8> = (0u32..256)
        .map(|i| u8::try_from(i % 256).unwrap())
        .collect();
    let manifest = fix_manifest_json("frame_0001.png", frame_bytes.len() as u64);
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &frame_bytes),
    ]);
    let post_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/submissions")
                .header(header::AUTHORIZATION, "Bearer test-token")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body_bytes))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(post_resp.status(), StatusCode::OK);
    let id_body = post_resp.into_body().collect().await.unwrap().to_bytes();
    let id_obj: serde_json::Value = serde_json::from_slice(&id_body).unwrap();
    let id = id_obj["id"].as_str().unwrap().to_owned();

    // GET manifest
    let m_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/submissions/{id}"))
                .header(header::AUTHORIZATION, "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(m_resp.status(), StatusCode::OK);
    let m_body = m_resp.into_body().collect().await.unwrap().to_bytes();
    let m_obj: serde_json::Value = serde_json::from_slice(&m_body).unwrap();
    assert_eq!(m_obj["submission_kind"], "fix");

    // GET media
    let f_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/submissions/{id}/media/frame_0001.png"))
                .header(header::AUTHORIZATION, "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(f_resp.status(), StatusCode::OK);
    let f_body = f_resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&f_body[..], &frame_bytes[..]);

    // 404 on unknown id
    let nf = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/submissions/01HDOESNOTEXIST")
                .header(header::AUTHORIZATION, "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(nf.status(), StatusCode::NOT_FOUND);

    // 400 on path-traversal filename
    let bad = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/submissions/{id}/media/..%2Fpasswd"))
                .header(header::AUTHORIZATION, "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // axum will percent-decode the path segment, so the
    // handler receives `../passwd`. Sanitization rejects it.
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn accepts_correct_checksum() {
    use sha2::{Digest, Sha256};

    let state = test_state("test-token");
    let app = build_app(state);

    let frame_bytes: Vec<u8> = (0u32..512)
        .map(|i| u8::try_from(i % 256).unwrap())
        .collect();
    let mut hasher = Sha256::new();
    hasher.update(&frame_bytes);
    let checksum = hex::encode(hasher.finalize());

    let manifest =
        fix_manifest_json_with_checksum("frame_0001.png", frame_bytes.len() as u64, &checksum);
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &frame_bytes),
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn rejects_checksum_mismatch() {
    let state = test_state("test-token");
    let app = build_app(state);

    let frame_bytes: Vec<u8> = (0u32..512)
        .map(|i| u8::try_from(i % 256).unwrap())
        .collect();
    // Deliberately wrong checksum (valid hex, wrong digest).
    let wrong_checksum = "0".repeat(64);

    let manifest = fix_manifest_json_with_checksum(
        "frame_0001.png",
        frame_bytes.len() as u64,
        &wrong_checksum,
    );
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &frame_bytes),
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("checksum mismatch"), "body was: {s}");
}

#[tokio::test]
async fn atomic_finalize_leaves_no_staging_residue() {
    let state = test_state("test-token");
    let data_root = state.config.data_root.clone();
    let app = build_app(state);

    let frame_bytes: Vec<u8> = (0u32..64).map(|i| u8::try_from(i % 256).unwrap()).collect();
    let manifest = fix_manifest_json("frame_0001.png", frame_bytes.len() as u64);
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &frame_bytes),
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The staging directory used for the atomic-finalize rename
    // must be empty after a successful ingest — no leftover
    // partial writes.
    let staging = data_root.join("submissions").join(".staging");
    if staging.exists() {
        let remaining: Vec<_> = std::fs::read_dir(&staging).unwrap().collect();
        assert!(
            remaining.is_empty(),
            "staging dir should be empty after finalize, found {remaining:?}"
        );
    }
}

#[tokio::test]
async fn index_rebuild_recovers_from_manifests_on_disk() {
    let state = test_state("test-token");
    let state_ref = state.clone();
    let app = build_app(state);

    let frame_bytes: Vec<u8> = (0u32..64).map(|i| u8::try_from(i % 256).unwrap()).collect();
    let manifest = fix_manifest_json("frame_0001.png", frame_bytes.len() as u64);
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &frame_bytes),
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Clear the index mirror directly (simulating loss/
    // corruption), then rebuild it purely from the on-disk
    // manifests via the same live store handle — no second
    // connection to the same sqlite file, which would race the
    // first.
    {
        let conn = state_ref.store.lock_index();
        conn.execute("DELETE FROM submissions", []).unwrap();
    }
    let count = state_ref.store.rebuild_index().expect("rebuild_index");
    assert_eq!(count, 1, "exactly one submission recovered from disk");
}

#[tokio::test]
async fn soft_delete_hides_then_retention_sweep_hard_deletes() {
    let state = test_state("test-token");
    let data_root = state.config.data_root.clone();
    let state_ref = state.clone();
    let app = build_app(state);

    let frame_bytes: Vec<u8> = (0u32..64).map(|i| u8::try_from(i % 256).unwrap()).collect();
    let manifest = fix_manifest_json("frame_0001.png", frame_bytes.len() as u64);
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &frame_bytes),
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let id_body = resp.into_body().collect().await.unwrap().to_bytes();
    let id: String = serde_json::from_slice::<serde_json::Value>(&id_body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // List still shows it before soft-delete.
    let list_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/submissions")
                .header(header::AUTHORIZATION, "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body = list_resp.into_body().collect().await.unwrap().to_bytes();
    let list: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);

    let store = &state_ref.store;
    store.soft_delete(&id).expect("soft_delete");

    // Directory is untouched by soft-delete: files stay on disk.
    let day_dir = data_root.join("submissions/2026/05/13");
    let sub_dir = std::fs::read_dir(&day_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(sub_dir.join("manifest.json").exists());
    assert!(sub_dir.join("deleted_at.txt").exists());

    // A retention sweep with a future-dated retention window
    // (0 days => cutoff = now) should hard-delete it, since the
    // soft-delete timestamp is already in the past relative to
    // "now".
    let removed = store
        .retention_sweep(chrono::Duration::days(0), false)
        .expect("retention_sweep");
    assert_eq!(removed, vec![id]);
    assert!(!sub_dir.exists(), "hard-deleted directory should be gone");
}

#[tokio::test]
async fn pbris_log_and_calibration_media_land_off_media_dir() {
    let state = test_state("test-token");
    let data_root = state.config.data_root.clone();
    let app = build_app(state);

    let pbris_bytes = b"$PBRIS,FIX,...\r\n".to_vec();
    let intrinsics_bytes = b"[intrinsics]\nfx = 1000.0\n".to_vec();

    let manifest = serde_json::json!({
        "schema_version": 1,
        "submission_kind": "calibration",
        "submitted_at": "2026-05-13T14:22:01Z",
        "device": {
            "uuid": "01HXYZTESTDEVICE0000000001",
            "model": "Test Device",
            "os": "Android 14 (API 34)",
        },
        "versions": {
            "app": "0.1.0",
            "bris_core": "0.0.1",
            "bris_data": null,
            "submission_schema": 1,
        },
        "captured_at": "2026-05-13T14:18:55Z",
        "gps": null,
        "note": null,
        "fix": null,
        "calibration": { "reprojection_error_px": 0.42 },
        "debug_capture": null,
        "media": [
            {
                "filename": "pbris.log",
                "role": "pbris_log",
                "size_bytes": pbris_bytes.len() as u64,
                "frame_index": null,
                "captured_at": null,
            },
            {
                "filename": "intrinsics.toml",
                "role": "intrinsics_toml",
                "size_bytes": intrinsics_bytes.len() as u64,
                "frame_index": null,
                "captured_at": null,
            }
        ],
    })
    .to_string();

    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("pbris.log", "text/plain", &pbris_bytes),
        ("intrinsics.toml", "application/toml", &intrinsics_bytes),
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "body={:?}",
        String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes())
    );

    let day_dir = data_root.join("submissions/2026/05/13");
    let sub_dir = std::fs::read_dir(&day_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        sub_dir.join("pbris.log").exists(),
        "pbris_log role should land at submission root, not media/"
    );
    assert!(
        sub_dir.join("calibration").join("intrinsics.toml").exists(),
        "intrinsics_toml role should land under calibration/"
    );
    assert!(
        !sub_dir.join("media").join("pbris.log").exists(),
        "pbris.log must not also be duplicated under media/"
    );
}

#[tokio::test]
async fn device_register_requires_admin_token() {
    let state = test_state("admin-token");
    let app = build_app(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/devices/register")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "device_uuid": "01HXYZTESTDEVICE0000000001" }).to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn device_register_then_submit_with_per_device_token() {
    let state = test_state("admin-token");
    let data_root = state.config.data_root.clone();
    let app = build_app(state);

    // First contact: register the device using the admin/bootstrap
    // token compiled into the device, obtaining a per-device token.
    let register_req = Request::builder()
        .method("POST")
        .uri("/v1/devices/register")
        .header(header::AUTHORIZATION, "Bearer admin-token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "device_uuid": "01HXYZTESTDEVICE0000000001" }).to_string(),
        ))
        .unwrap();
    let register_resp = app.clone().oneshot(register_req).await.unwrap();
    assert_eq!(register_resp.status(), StatusCode::OK);
    let register_body = register_resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let register_json: serde_json::Value = serde_json::from_slice(&register_body).unwrap();
    let device_token = register_json["token"]
        .as_str()
        .expect("token field")
        .to_owned();
    assert_ne!(device_token, "admin-token");
    assert_eq!(register_json["device_uuid"], "01HXYZTESTDEVICE0000000001");

    // Subsequent submission uses the per-device token plus the
    // device-identifying header instead of the admin token.
    let frame_bytes: Vec<u8> = (0u32..1024)
        .map(|i| u8::try_from(i % 256).unwrap())
        .collect();
    let manifest = fix_manifest_json("frame_0001.png", frame_bytes.len() as u64);
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &frame_bytes),
    ]);
    let submit_req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, format!("Bearer {device_token}"))
        .header("x-bris-device-uuid", "01HXYZTESTDEVICE0000000001")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let submit_resp = app.clone().oneshot(submit_req).await.unwrap();
    assert_eq!(
        submit_resp.status(),
        StatusCode::OK,
        "body={:?}",
        String::from_utf8_lossy(&submit_resp.into_body().collect().await.unwrap().to_bytes())
    );

    let day_dir = data_root.join("submissions/2026/05/13");
    assert!(
        day_dir.exists(),
        "submission via per-device token should land on disk"
    );

    // The admin token continues to work for review/admin
    // endpoints (list) -- it is never replaced, only supplemented
    // by the per-device flow.
    let list_resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/submissions")
                .header(header::AUTHORIZATION, "Bearer admin-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn submission_rejects_wrong_device_token() {
    let state = test_state("admin-token");
    let app = build_app(state);

    // Register device A, then try to submit as device A using an
    // unrelated (unregistered) bearer token -- must be rejected.
    let register_req = Request::builder()
        .method("POST")
        .uri("/v1/devices/register")
        .header(header::AUTHORIZATION, "Bearer admin-token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "device_uuid": "01HXYZTESTDEVICE0000000001" }).to_string(),
        ))
        .unwrap();
    let register_resp = app.clone().oneshot(register_req).await.unwrap();
    assert_eq!(register_resp.status(), StatusCode::OK);

    let frame_bytes: Vec<u8> = (0u32..1024)
        .map(|i| u8::try_from(i % 256).unwrap())
        .collect();
    let manifest = fix_manifest_json("frame_0001.png", frame_bytes.len() as u64);
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &frame_bytes),
    ]);
    let submit_req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer totally-wrong-token")
        .header("x-bris-device-uuid", "01HXYZTESTDEVICE0000000001")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let submit_resp = app.oneshot(submit_req).await.unwrap();
    assert_eq!(submit_resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn submission_rejects_unregistered_device_uuid() {
    let state = test_state("admin-token");
    let app = build_app(state);

    let frame_bytes: Vec<u8> = (0u32..1024)
        .map(|i| u8::try_from(i % 256).unwrap())
        .collect();
    let manifest = fix_manifest_json("frame_0001.png", frame_bytes.len() as u64);
    let body_bytes = multipart_body(&[
        ("manifest", "application/json", manifest.as_bytes()),
        ("frame_0001.png", "image/png", &frame_bytes),
    ]);
    let submit_req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(
            header::AUTHORIZATION,
            "Bearer some-token-that-was-never-issued",
        )
        .header("x-bris-device-uuid", "01HXYZTESTDEVICE0000000099")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let submit_resp = app.oneshot(submit_req).await.unwrap();
    assert_eq!(submit_resp.status(), StatusCode::UNAUTHORIZED);
}
