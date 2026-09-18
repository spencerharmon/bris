//! End-to-end acceptance test for the **network diagnostic-
//! collection path** (ROI Priority §1), T1's terminal
//! acceptance: it proves there is no seam between the submitter
//! and the collector. No per-leaf check in `bris-submit` or
//! `bris-collector` sees the whole traversal; this one does.
//!
//! The invariant under test:
//!
//! > An operator-gated submission of a `bris-bundle v1` artifact
//! > traverses **submitter → POST /v1/submissions → durable
//! > store**, and the stored artifact is byte-verifiable and
//! > **replayable in place** by `bris replay`, with `ap_input`
//! > honest and `gps_truth` never substituted.
//!
//! Concretely, this single test:
//!
//! 1. Builds a real `bris-bundle v1` fixture on disk (an honest
//!    `ap_input`, a DISTINCT `gps_truth`, real P5 PGM frames +
//!    sidecars, a `pbris.log`).
//! 2. Stands up a `bris-collector` over a temp data-root and
//!    registers a device, obtaining a **per-device token**.
//! 3. Runs the fixture through the ACTUAL submitter code path
//!    (`bris_submit::build_submission` → the explicit operator
//!    gate `SubmissionReview::approve` → `encode_multipart`) and
//!    POSTs it to `/v1/submissions` with the per-device token.
//! 4. Asserts the stored submission VALIDATES (per-file SHA-256
//!    checksums recomputed on disk + schema-version) and LANDS
//!    under `submissions/<yyyy>/<mm>/<dd>/<ulid>/`.
//! 5. Rebuilds the `SQLite` index purely from the on-disk
//!    manifests and asserts the rebuilt row EQUALS the row the
//!    live ingest wrote — proving the index is a truthful,
//!    rebuildable cache of the durable store.
//! 6. Runs the real `bris replay` binary against the stored
//!    submission's `media/` directory (where the verbatim
//!    `bundle.json` + frames landed) and asserts it replays IN
//!    PLACE — the stored bytes are a valid, engine-consumable
//!    bundle, not merely a blob.
//!
//! The `bundle.json` shipped is asserted byte-identical to the
//! one written to disk pre-submission, and its `ap_input` is
//! preserved untouched while `gps_truth` is carried only as
//! ground-truth (never substituted for `ap_input`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use bris_bundle::{
    ApInput, ApProvenance, BundleManifest, CaptureInfo, DeviceInfo, Distortion, FrameSidecar,
    GpsTruth, IntrinsicsRecord, IntrinsicsSource, SCHEMA_VERSION,
};
use bris_collector::routes::AppState;
use bris_collector::store::Store;
use bris_collector::{build_app, Config};
use bris_submit::{build_submission, SubmissionReview, SubmissionSource};

const DEVICE_UUID: &str = "01HXYZTESTDEVICE0000000001";

/// One index row, in the exact column order the store persists,
/// so we can compare the live-ingested row against the
/// rebuilt-from-disk row for byte-level equality.
#[derive(Debug, PartialEq, Eq)]
struct IndexRow {
    id: String,
    kind: String,
    submitted_at: String,
    captured_at: String,
    device_uuid: String,
    app_version: String,
    bris_core_version: String,
    has_gps: i64,
    note_present: i64,
    soft_deleted_at: Option<String>,
}

/// Read every index row (ordered by id for a stable comparison).
/// `manifest_path` is intentionally EXCLUDED: it is an absolute
/// on-disk path that is identical across the live-write and the
/// rebuild (same data-root), so it carries no independent signal
/// and only complicates the struct.
fn read_index_rows(store: &Store) -> Vec<IndexRow> {
    let conn = store.lock_index();
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, submitted_at, captured_at, device_uuid,
                    app_version, bris_core_version, has_gps, note_present,
                    soft_deleted_at
             FROM submissions
             ORDER BY id",
        )
        .expect("prepare index query");
    let rows = stmt
        .query_map([], |row| {
            Ok(IndexRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                submitted_at: row.get(2)?,
                captured_at: row.get(3)?,
                device_uuid: row.get(4)?,
                app_version: row.get(5)?,
                bris_core_version: row.get(6)?,
                has_gps: row.get(7)?,
                note_present: row.get(8)?,
                soft_deleted_at: row.get(9)?,
            })
        })
        .expect("query index rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect index rows");
    rows
}

/// Write a valid P5 (binary) PGM the `image` crate — and thus
/// the `bris` replay engine — can load. A simple two-band image
/// (bright sky over darker sea) so the frame is a real gradient,
/// not a flat fill.
fn write_pgm(path: &Path, seq: u32) {
    let width = 320u32;
    let height = 240u32;
    let horizon_y = 120u32;
    let mut pixels = vec![0u16; (width as usize) * (height as usize)];
    for y in 0..height {
        for x in 0..width {
            // Slight per-seq variation so frames are not identical.
            let base: u16 = if y < horizon_y { 50_000 } else { 5_000 };
            let jitter = u16::try_from((seq * 7 + x + y) % 97).unwrap();
            pixels[(y as usize) * (width as usize) + (x as usize)] = base.saturating_add(jitter);
        }
    }
    let buf = image::ImageBuffer::<image::Luma<u16>, _>::from_raw(width, height, pixels)
        .expect("pgm buf");
    buf.save_with_format(path, image::ImageFormat::Pnm)
        .expect("save pgm");
}

/// Build a canonical-layout `bris-bundle v1` capture bundle with
/// `n_frames` frames, an honest `ap_input`, and a DISTINCT
/// `gps_truth`. Returns the capture directory.
fn make_bundle(root: &Path, n_frames: u32) -> PathBuf {
    let bundle_dir = root.join("captures").join("cap-e2e-0001");
    let frames_dir = bundle_dir.join("frames");
    std::fs::create_dir_all(&frames_dir).expect("mkdir frames");

    for seq in 0..n_frames {
        let stem = format!("{seq:08}");
        let pgm = frames_dir.join(format!("{stem}.pgm"));
        let json = frames_dir.join(format!("{stem}.json"));
        write_pgm(&pgm, seq);
        let sc = FrameSidecar {
            seq,
            captured_unix_ms: 1_700_000_000_000 + i64::from(seq) * 1000,
            width: 320,
            height: 240,
            exposure_us: Some(10_000),
            sensor_gain: Some(1.0),
            diagnostic_snapshot: None,
            gravity_camera_frame: None,
            gps_truth: None,
        };
        std::fs::write(&json, serde_json::to_vec(&sc).unwrap()).expect("write sidecar");
    }

    let manifest = BundleManifest {
        schema_version: SCHEMA_VERSION,
        bundle_id: "cap-e2e-0001".into(),
        device: DeviceInfo {
            model: "TestPhone".into(),
            os: Some("Android 14".into()),
            app_version: Some("0.1.0".into()),
        },
        build: None,
        capture: CaptureInfo {
            source_rotation_deg: 0,
            pre_rotation_was_deg: None,
            frame_count: n_frames,
            started_unix_ms: 1_700_000_000_000,
            ended_unix_ms: 1_700_000_000_000 + i64::from(n_frames) * 1000,
            first_frame_blake3: None,
        },
        intrinsics: IntrinsicsRecord {
            source: IntrinsicsSource::Placeholder,
            profile_key: None,
            width: 320,
            height: 240,
            fx: 1000.0,
            fy: 1000.0,
            cx: 160.0,
            cy: 120.0,
            distortion: Distortion::None,
            rms_px: None,
            solved_at_unix_ms: None,
            placeholder: Some(true),
        },
        // The engine ran against THIS ap_input; it must survive
        // to the collector byte-for-byte, never merged with the
        // ground-truth below.
        ap_input: Some(ApInput {
            lat: 12.34,
            lon: -56.78,
            eye_height_m: 2.0,
            provenance: ApProvenance::OperatorEntered,
        }),
        ap_derivation_trace: None,
        // Distinct ground-truth location. NEVER substituted for
        // ap_input.
        gps_truth: Some(GpsTruth {
            lat: 41.0,
            lon: -71.0,
            lat_sigma_m: 5.0,
            lon_sigma_m: 7.0,
            altitude_m: None,
            altitude_sigma_m: None,
            captured_unix_ms: 1_700_000_000_500,
            source: "phone_gnss".into(),
            satellites_used: Some(9),
        }),
        atmosphere_hint: None,
        notes: "e2e collection-path capture".into(),
        session_id: None,
    };
    manifest.save_to_dir(&bundle_dir).expect("save bundle.json");
    std::fs::write(bundle_dir.join("pbris.log"), b"$PBRIS,TEST,1*00\n").expect("pbris.log");
    bundle_dir
}

fn test_source() -> SubmissionSource {
    SubmissionSource {
        device_uuid: DEVICE_UUID.into(),
        device_model: "TestPhone".into(),
        device_os: "Android 14 (API 34)".into(),
        app_version: "0.1.0".into(),
        bris_core_version: "0.0.1".into(),
        bris_data_version: None,
        note: Some("operator note".into()),
    }
}

/// Recompute the SHA-256 of a file on disk and compare it to the
/// hex digest the manifest declared for that media item. This is
/// the durable, on-disk byte-verifiability check — independent of
/// what the collector validated at ingest time.
fn assert_stored_checksum(path: &Path, expected_hex: &str) {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut h = Sha256::new();
    h.update(&bytes);
    let got = hex::encode(h.finalize());
    assert_eq!(
        got,
        expected_hex,
        "stored file {} checksum mismatch",
        path.display()
    );
}

/// Find the single submission directory under
/// `<data_root>/submissions/<yyyy>/<mm>/<dd>/<ulid>/`, asserting
/// the yyyy/mm/dd/ulid shape and that exactly one exists.
fn find_single_submission_dir(data_root: &Path, expected_id: &str) -> PathBuf {
    let submissions = data_root.join("submissions");
    let mut found = Vec::new();
    // submissions/<yyyy>/<mm>/<dd>/<ulid>
    for yyyy in read_child_dirs(&submissions) {
        assert!(
            yyyy.file_name().unwrap().to_str().unwrap().len() == 4,
            "year dir must be yyyy: {}",
            yyyy.display()
        );
        for mm in read_child_dirs(&yyyy) {
            assert_eq!(
                mm.file_name().unwrap().to_str().unwrap().len(),
                2,
                "month dir must be mm: {}",
                mm.display()
            );
            for dd in read_child_dirs(&mm) {
                assert_eq!(
                    dd.file_name().unwrap().to_str().unwrap().len(),
                    2,
                    "day dir must be dd: {}",
                    dd.display()
                );
                for ulid in read_child_dirs(&dd) {
                    found.push(ulid);
                }
            }
        }
    }
    assert_eq!(
        found.len(),
        1,
        "exactly one submission dir expected, found {found:?}"
    );
    let dir = found.into_iter().next().unwrap();
    assert_eq!(
        dir.file_name().unwrap().to_str().unwrap(),
        expected_id,
        "submission dir name must be the collector-assigned ULID"
    );
    dir
}

fn read_child_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
        })
        .collect();
    v.sort();
    v
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn collection_path_submitter_to_store_is_real_and_replayable() {
    // ---- 1. Build the bris-bundle v1 fixture. -----------------
    let src_tmp = tempfile::tempdir().expect("src tempdir");
    let bundle_dir = make_bundle(src_tmp.path(), 3);
    let raw_bundle_json = std::fs::read(bundle_dir.join("bundle.json")).expect("read bundle.json");

    // ---- 2. Stand up the collector over a temp data-root. -----
    let data_tmp = tempfile::tempdir().expect("data tempdir");
    let data_root = data_tmp.path().to_path_buf();
    let cfg = Config {
        data_root: data_root.clone(),
        bind: "127.0.0.1:0".to_owned(),
        bearer_token: "admin-token".to_owned(),
        max_submission_bytes: 32 * 1024 * 1024,
    };
    let store = Store::open(&cfg.data_root).expect("store open");
    let state = Arc::new(AppState { config: cfg, store });
    let app = build_app(state.clone());

    // Register the device to obtain a per-device token (the
    // per-device-token collection path, not the admin token).
    let register_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/devices/register")
                .header(header::AUTHORIZATION, "Bearer admin-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "device_uuid": DEVICE_UUID }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(register_resp.status(), StatusCode::OK, "device register");
    let register_body = register_resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let register_json: serde_json::Value = serde_json::from_slice(&register_body).unwrap();
    let device_token = register_json["token"]
        .as_str()
        .expect("per-device token")
        .to_owned();
    assert_ne!(device_token, "admin-token");

    // ---- 3. Submitter code path → operator gate → POST. -------
    let submission = build_submission(&bundle_dir, &test_source(), None).expect("build_submission");

    // The submitter ships bundle.json byte-for-byte; ap_input
    // honest, gps_truth distinct (never substituted).
    let first = &submission.parts[0];
    assert_eq!(first.filename, "bundle.json");
    assert_eq!(first.role, "bundle_manifest");
    assert_eq!(
        first.bytes, raw_bundle_json,
        "bundle.json must be shipped byte-for-byte"
    );
    let shipped: BundleManifest = serde_json::from_slice(&first.bytes).unwrap();
    let ap = shipped.ap_input.clone().expect("ap_input preserved");
    assert!((ap.lat - 12.34).abs() < 1e-9 && (ap.lon - (-56.78)).abs() < 1e-9);
    let gt = shipped.gps_truth.clone().expect("gps_truth preserved");
    assert!((gt.lat - 41.0).abs() < 1e-9 && (gt.lon - (-71.0)).abs() < 1e-9);
    assert!(
        (gt.lat - ap.lat).abs() > 1.0,
        "gps_truth must never be substituted for ap_input"
    );
    // The manifest's ground-truth `gps` is the gps_truth.
    let gps = submission.manifest.gps.as_ref().expect("gps present");
    assert!((gps.lat_deg - 41.0).abs() < 1e-9);

    // The explicit operator gate is the only route to a sendable
    // submission.
    let reviewed = SubmissionReview::new(submission).approve();
    let body = bris_submit::transport::encode_multipart(&reviewed).expect("encode multipart");
    let ctype = bris_submit::transport::multipart_content_type();

    let submit_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/submissions")
                .header(header::AUTHORIZATION, format!("Bearer {device_token}"))
                .header("x-bris-device-uuid", DEVICE_UUID)
                .header(header::CONTENT_TYPE, ctype)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = submit_resp.status();
    let submit_body = submit_resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "collector rejected the submitter payload: {}",
        String::from_utf8_lossy(&submit_body)
    );
    let accepted: serde_json::Value = serde_json::from_slice(&submit_body).unwrap();
    let submission_id = accepted["id"].as_str().expect("submission id").to_owned();

    // ---- 4. Stored submission validates + correct layout. -----
    let sub_dir = find_single_submission_dir(&data_root, &submission_id);
    let stored_manifest_path = sub_dir.join("manifest.json");
    assert!(stored_manifest_path.exists(), "manifest.json on disk");

    // schema-version of the stored collector manifest.
    let stored_manifest_bytes = std::fs::read(&stored_manifest_path).unwrap();
    let stored_manifest: serde_json::Value =
        serde_json::from_slice(&stored_manifest_bytes).unwrap();
    assert_eq!(
        stored_manifest["schema_version"].as_u64(),
        Some(1),
        "stored manifest must declare schema_version 1"
    );

    // The verbatim bris-bundle v1 manifest landed under media/,
    // byte-identical to what the engine ran against.
    let stored_bundle_json = sub_dir.join("media").join("bundle.json");
    assert!(
        stored_bundle_json.exists(),
        "bundle.json stored under media/"
    );
    assert_eq!(
        std::fs::read(&stored_bundle_json).unwrap(),
        raw_bundle_json,
        "stored bundle.json must be byte-identical to the engine's"
    );

    // Every declared media item's file exists on disk with a
    // matching recomputed SHA-256 (durable byte-verifiability).
    for item in &reviewed.submission().manifest.media {
        let rel = if item.role == "pbris_log" {
            PathBuf::from("pbris.log")
        } else {
            PathBuf::from("media").join(&item.filename)
        };
        let path = sub_dir.join(&rel);
        let checksum = item
            .checksum_sha256
            .as_deref()
            .expect("every media item carries a checksum");
        assert_stored_checksum(&path, checksum);
    }

    // ---- 5. Rebuild the `SQLite` index from disk == live row. ---
    let live_rows = read_index_rows(&state.store);
    assert_eq!(live_rows.len(), 1, "one live-ingested index row");
    assert_eq!(live_rows[0].id, submission_id);
    assert_eq!(live_rows[0].kind, "fix");
    assert_eq!(live_rows[0].has_gps, 1);
    assert_eq!(live_rows[0].device_uuid, DEVICE_UUID);

    // Clear the index mirror and rebuild it purely from the
    // on-disk manifests; the rebuilt row must EQUAL the live row.
    {
        let conn = state.store.lock_index();
        conn.execute("DELETE FROM submissions", []).unwrap();
    }
    let rebuilt_count = state.store.rebuild_index().expect("rebuild_index");
    assert_eq!(rebuilt_count, 1, "one submission recovered from disk");
    let rebuilt_rows = read_index_rows(&state.store);
    assert_eq!(
        rebuilt_rows, live_rows,
        "index rebuilt from disk must equal the live-ingested index"
    );

    // ---- 6. `bris replay` replays the stored bundle IN PLACE. -
    // The stored `media/` directory is itself a valid bundle
    // layout: bundle.json at its root, PGM+sidecar frames
    // alongside. Point the real replay binary at it.
    let media_dir = sub_dir.join("media");
    let bris_bin = escargot::CargoBuild::new()
        .package("bris-cli")
        .bin("bris")
        .current_release()
        .run()
        .expect("build bris binary");

    let output = bris_bin
        .command()
        .args([
            "replay",
            "--bundle",
            media_dir.to_str().unwrap(),
            "--disable-store",
        ])
        .output()
        .expect("run bris replay");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = strip_ansi(&format!("STDOUT:\n{stdout}\nSTDERR:\n{stderr}"));

    assert!(
        output.status.success(),
        "bris replay of the STORED bundle exited non-zero.\n{combined}"
    );
    assert!(
        combined.contains("replay: bundle resolved"),
        "stored bundle did not resolve for replay.\n{combined}"
    );
    assert!(
        combined.contains("frames_pushed=3"),
        "replay did not push all 3 stored frames.\n{combined}"
    );
    assert!(
        combined.contains("mode complete"),
        "replay mode did not complete on the stored bundle.\n{combined}"
    );
}

/// Strip ANSI escape sequences so substring checks are robust to
/// colourized tracing output.
fn strip_ansi(s: &str) -> String {
    s.chars()
        .scan(false, |in_esc, c| {
            if *in_esc {
                if c.is_ascii_alphabetic() {
                    *in_esc = false;
                }
                Some(None)
            } else if c == '\x1b' {
                *in_esc = true;
                Some(None)
            } else {
                Some(Some(c))
            }
        })
        .flatten()
        .collect()
}
