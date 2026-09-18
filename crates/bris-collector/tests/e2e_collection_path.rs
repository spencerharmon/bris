//! `collection-path-e2e` — the terminal acceptance test for the
//! network diagnostic-collection path (ROI: "network
//! diagnostic-collection path"). No per-leaf unit/integration
//! test elsewhere in the workspace sees the FULL seam this
//! covers:
//!
//!   submitter (bris-submit, over a REAL loopback HTTP
//!   connection, using a per-device token) ->
//!   `POST /v1/submissions` (bris-collector, real axum router
//!   bound to a real TCP port) -> durable filesystem store ->
//!   `SQLite` index (rebuilt from disk, not merely trusted) ->
//!   `bris replay` (the real CLI binary) against the stored
//!   bundle IN PLACE (no copy out of the collector's data-root).
//!
//! Per the invariant this task encodes: an operator-gated
//! submission of a bris-bundle v1 artifact must be
//! byte-verifiable and replayable in place, with `ap_input`
//! honest and `gps_truth` never substituted for it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use bris_collector::routes::AppState;
use bris_collector::store::Store;
use bris_collector::{build_app, Config};
use bris_submit::review::SubmissionReview;
use bris_submit::transport::{CollectorEndpoint, HttpTransport, Transport};
use bris_submit::{build_submission, SubmissionSource};
use sha2::{Digest, Sha256};

/// Write a minimal valid P5 PGM.
fn write_pgm(path: &Path, w: u32, h: u32, fill: u8) {
    let mut bytes = format!("P5\n{w} {h}\n255\n").into_bytes();
    bytes.extend(std::iter::repeat_n(fill, (w * h) as usize));
    std::fs::write(path, bytes).unwrap();
}

/// Build a real, on-disk capture bundle (`bundle.json` +
/// `frames/`) in the canonical layout `bris_submit::build_submission`
/// expects: an honest `ap_input` and a DISTINCT `gps_truth`, so
/// the test can assert neither is ever substituted for the
/// other en route to the collector.
fn make_bundle(root: &Path) -> PathBuf {
    let bundle_dir = root.join("captures").join("e2e-cap-0001");
    let frames_dir = bundle_dir.join("frames");
    std::fs::create_dir_all(&frames_dir).unwrap();

    for seq in 0..2u32 {
        let stem = format!("{seq:08}");
        write_pgm(
            &frames_dir.join(format!("{stem}.pgm")),
            4,
            3,
            u8::try_from(seq * 40).unwrap(),
        );
        let sidecar = serde_json::json!({
            "seq": seq,
            "captured_unix_ms": 1_700_000_000_000i64 + i64::from(seq) * 1000,
            "width": 4,
            "height": 3,
        });
        std::fs::write(
            frames_dir.join(format!("{stem}.json")),
            serde_json::to_vec(&sidecar).unwrap(),
        )
        .unwrap();
    }

    let bundle = serde_json::json!({
        "schema_version": 1,
        "bundle_id": "e2e-cap-0001",
        "device": { "model": "e2e-test-device" },
        "capture": {
            "source_rotation_deg": 0,
            "frame_count": 2,
            "started_unix_ms": 1_700_000_000_000i64,
            "ended_unix_ms": 1_700_000_001_000i64,
        },
        "intrinsics": {
            "source": { "kind": "placeholder" },
            "width": 4, "height": 3,
            "fx": 1000.0, "fy": 1000.0, "cx": 2.0, "cy": 1.5,
            "distortion": { "model": "none" },
        },
        // The engine ran against THIS ap_input. It must survive
        // to the collector untouched.
        "ap_input": {
            "lat": 12.34,
            "lon": -56.78,
            "eye_height_m": 2.0,
            "provenance": "operator_entered",
        },
        // Distinct ground-truth location. Must NEVER be
        // substituted for ap_input.
        "gps_truth": {
            "lat": 99.0,
            "lon": 99.0,
            "lat_sigma_m": 5.0,
            "lon_sigma_m": 7.0,
            "captured_unix_ms": 1_700_000_000_500i64,
            "source": "phone_gnss",
            "satellites_used": 9,
        },
        "notes": "collection-path-e2e capture",
    });
    std::fs::write(
        bundle_dir.join("bundle.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();
    bundle_dir
}

/// Absolute path to the workspace root (two levels up from this
/// crate's manifest dir: `crates/bris-collector` -> workspace).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// Strip ANSI escape sequences so substring assertions on CLI
/// output work regardless of whether tracing colourized it.
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

// `HttpTransport::send` is a BLOCKING (ureq) call — the same
// submitter code path a synchronous on-device caller uses. It
// must run on a real OS thread distinct from the one polling
// the in-process `axum::serve` task, or the two deadlock (the
// blocking call never yields, so the single-threaded runtime
// never polls the listener). `flavor = "multi_thread"` gives the
// spawned server task its own worker thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collection_path_traverses_submitter_collector_store_and_is_replayable_in_place() {
    // ---- 1. Stand up a real bris-collector over a tempdir store, bound to a real TCP port. ----
    let store_dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_root: store_dir.path().to_path_buf(),
        bind: "127.0.0.1:0".to_owned(),
        bearer_token: "admin-bootstrap-token".to_owned(),
        max_submission_bytes: 32 * 1024 * 1024,
        retention_days: 30,
    };
    let store = Store::open(&cfg.data_root).unwrap();

    // Per-device token: the collector's own device-registration
    // path (first-contact provisioning), not a hand-rolled
    // sentinel.
    let device_uuid = "01HE2ECOLLECTIONPATHDEVICE1";
    let device_token = store.register_device(device_uuid).unwrap();

    let state = Arc::new(AppState { config: cfg, store });
    let app = build_app(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let base_url = format!("http://{addr}");

    // ---- 2. Build a real capture bundle and drive it through the submitter code path (bris-submit) over a real HTTP POST. ----
    let capture_root = tempfile::tempdir().unwrap();
    let bundle_dir = make_bundle(capture_root.path());

    let source = SubmissionSource {
        device_uuid: device_uuid.to_owned(),
        device_model: "e2e-test-device".to_owned(),
        device_os: "TestOS 1.0".to_owned(),
        app_version: "0.0.1-e2e".to_owned(),
        bris_core_version: "0.0.1".to_owned(),
        bris_data_version: None,
        note: Some("collection-path-e2e".to_owned()),
    };
    let submission = build_submission(&bundle_dir, &source, None).expect("build_submission");

    // Honest ap_input / distinct gps_truth survive the payload
    // build untouched (asserted again after the round trip below).
    let shipped_bundle_json = submission.parts[0].bytes.clone();
    assert_eq!(submission.parts[0].filename, "bundle.json");
    assert_eq!(submission.parts[0].role, "bundle_manifest");

    let reviewed = SubmissionReview::new(submission).approve();
    let endpoint = CollectorEndpoint::new(base_url.clone(), device_token.clone())
        .with_device_uuid(device_uuid);
    let transport = HttpTransport::new(endpoint);
    let outcome = transport
        .send(&reviewed)
        .expect("submitter POST to the real collector must succeed");
    let submission_id = outcome.id;

    // ---- 3. The stored submission validates (checksum + schema-version) and lands under submissions/<yyyy>/<mm>/<dd>/<ulid>/. ----
    let data_root = state.config.data_root.clone();
    let submissions_root = data_root.join("submissions");
    let day_dir = find_single_day_dir(&submissions_root);
    let sub_dir = day_dir.join(&submission_id);
    assert!(
        sub_dir.is_dir(),
        "submission dir {} must exist under the dated path",
        sub_dir.display()
    );
    let media_dir = assert_submission_validates_on_disk(&sub_dir, &shipped_bundle_json);

    // ---- 4. Rebuild the SQLite index from disk and assert equality with the live index. ----
    assert_index_rebuild_matches_live(&state.store, &submission_id);

    // ---- 5. `bris replay` against the data-root confirms the stored bundle is replayable IN PLACE. ----
    assert_replay_in_place(&media_dir);
}

/// Assert the submission's on-disk artifacts validate: schema
/// version, byte-identical `bundle.json`, and every declared
/// `media[]` checksum matching the actual bytes on disk. Returns
/// the submission's `media/` directory.
fn assert_submission_validates_on_disk(sub_dir: &Path, shipped_bundle_json: &[u8]) -> PathBuf {
    let manifest_bytes = std::fs::read(sub_dir.join("manifest.json")).expect("manifest.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("manifest.json parses");
    assert_eq!(manifest["schema_version"], 1, "schema-version validated");

    // The stored bundle.json (media/bundle.json — see media_destination:
    // bundle_manifest, capture_frame and frame_sidecar all land under
    // media/, only pbris_log and calibration roles land elsewhere) is
    // byte-identical to what the submitter built, and every media[]
    // entry's declared checksum matches the actual on-disk bytes —
    // proving checksum validation, not merely acceptance.
    let media_dir = sub_dir.join("media");
    let stored_bundle_json =
        std::fs::read(media_dir.join("bundle.json")).expect("stored bundle.json");
    assert_eq!(
        stored_bundle_json, shipped_bundle_json,
        "stored bundle.json must be byte-identical to the submitted one"
    );
    let stored_bundle: serde_json::Value =
        serde_json::from_slice(&stored_bundle_json).expect("stored bundle.json parses");
    assert!(
        (stored_bundle["ap_input"]["lat"].as_f64().unwrap() - 12.34).abs() < 1e-9,
        "ap_input must survive to the stored bundle untouched"
    );
    assert!(
        (stored_bundle["gps_truth"]["lat"].as_f64().unwrap() - 99.0).abs() < 1e-9,
        "gps_truth must survive as ground-truth, never substituted for ap_input"
    );

    let media_array = manifest["media"].as_array().expect("media array");
    assert!(!media_array.is_empty());
    for item in media_array {
        let filename = item["filename"].as_str().expect("filename");
        let declared_checksum = item["checksum_sha256"]
            .as_str()
            .expect("checksum_sha256 present");
        let on_disk = std::fs::read(media_dir.join(filename))
            .unwrap_or_else(|e| panic!("reading stored media {filename}: {e}"));
        let mut hasher = Sha256::new();
        hasher.update(&on_disk);
        let actual_checksum = hex::encode(hasher.finalize());
        assert_eq!(
            actual_checksum, declared_checksum,
            "byte-verifiable: on-disk {filename} must match its declared checksum"
        );
    }
    media_dir
}

/// Rebuild the `SQLite` index purely from the on-disk manifests
/// and assert it equals the live index built at ingest time.
fn assert_index_rebuild_matches_live(store: &Store, submission_id: &str) {
    let before: Vec<(String, String)> = {
        let conn = store.lock_index();
        let mut stmt = conn
            .prepare("SELECT id, manifest_path FROM submissions ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(before.len(), 1, "exactly one submission indexed live");
    assert_eq!(before[0].0, submission_id);

    // Corrupt the mirror, then rebuild it purely from the
    // on-disk manifests, and assert the two views agree.
    {
        let conn = store.lock_index();
        conn.execute("DELETE FROM submissions", []).unwrap();
    }
    let rebuilt_count = store.rebuild_index().expect("rebuild_index");
    assert_eq!(
        rebuilt_count, 1,
        "rebuild_index recovers exactly one submission from disk"
    );
    let after: Vec<(String, String)> = {
        let conn = store.lock_index();
        let mut stmt = conn
            .prepare("SELECT id, manifest_path FROM submissions ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        before, after,
        "rebuilt index must equal the original live index"
    );
}

/// Shell out to the real `bris replay` CLI against `media_dir`
/// (the submission's `media/` directory, which already IS a
/// valid bundle root — see `media_destination` — so this
/// replays the collector's own stored copy with no copy-out
/// step) and assert it completes successfully.
fn assert_replay_in_place(media_dir: &Path) {
    let manifest_path = workspace_root().join("Cargo.toml");
    let out = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "--manifest-path",
            manifest_path.to_str().unwrap(),
            "-p",
            "bris-cli",
            "--bin",
            "bris",
            "--",
            "replay",
            "--bundle",
            media_dir.to_str().unwrap(),
            "--disable-store",
        ])
        .output()
        .expect("invoke `bris replay`");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = strip_ansi(&format!("STDOUT:\n{stdout}\nSTDERR:\n{stderr}"));

    assert!(
        out.status.success(),
        "bris replay exited non-zero against the stored submission.\n{combined}"
    );
    assert!(
        combined.contains("replay: bundle resolved"),
        "bris replay must resolve the stored bundle in place.\n{combined}"
    );
    assert!(
        combined.contains("frames_pushed=2"),
        "bris replay must push every stored frame.\n{combined}"
    );
    assert!(
        combined.contains("mode complete"),
        "bris replay must complete against the stored submission.\n{combined}"
    );
}

/// Locate the single `<yyyy>/<mm>/<dd>/` day directory under
/// `submissions/`, failing loudly if there isn't exactly one
/// (this test submits exactly one bundle).
fn find_single_day_dir(submissions_root: &Path) -> PathBuf {
    let years: Vec<_> = std::fs::read_dir(submissions_root)
        .expect("submissions root exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()) != Some(".staging"))
        .collect();
    assert_eq!(years.len(), 1, "exactly one year dir: {years:?}");
    let months: Vec<_> = std::fs::read_dir(&years[0])
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(months.len(), 1, "exactly one month dir: {months:?}");
    let days: Vec<_> = std::fs::read_dir(&months[0])
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(days.len(), 1, "exactly one day dir: {days:?}");
    days[0].clone()
}
