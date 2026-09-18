//! Integration tests for `bris-submit`.
//!
//! These build a real on-disk capture bundle, run it through
//! [`bris_submit::build_submission`], and assert on every
//! invariant the task requires:
//!
//! - the shipped `bundle.json` is byte-identical to the one the
//!   engine ran against (`ap_input` honest; `gps_truth` carried
//!   only as ground-truth, never substituted for `ap_input`);
//! - every part carries a correct SHA-256, and the manifest's
//!   `media[]` matches the parts exactly;
//! - no submission is sendable without passing the explicit
//!   operator gate;
//! - the built multipart payload is ACCEPTED by the real
//!   `bris-collector` router in-process (parse + size + checksum
//!   validation), i.e. the wire contract holds end-to-end;
//! - the persistent retry queue is crash-durable and retries a
//!   transient failure, dead-letters a permanent one, and
//!   succeeds once the transport recovers.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use bris_bundle::{
    ApInput, ApProvenance, BundleManifest, CaptureInfo, DeviceInfo, Distortion, FrameSidecar,
    GpsTruth, IntrinsicsRecord, IntrinsicsSource, SCHEMA_VERSION,
};
use bris_submit::transport::{media_matches_parts, SubmissionOutcome, Transport, TransportError};
use bris_submit::{
    build_submission, AttemptResult, ReviewedSubmission, SubmissionQueue, SubmissionReview,
    SubmissionSource,
};

/// Write a P5 PGM with a header and `w*h` grey pixels.
fn write_pgm(path: &Path, w: u32, h: u32, fill: u8) {
    let mut bytes = format!("P5\n{w} {h}\n255\n").into_bytes();
    bytes.extend(std::iter::repeat_n(fill, (w * h) as usize));
    std::fs::write(path, bytes).unwrap();
}

fn sidecar(seq: u32, unix_ms: i64, w: u32, h: u32) -> FrameSidecar {
    FrameSidecar {
        seq,
        captured_unix_ms: unix_ms,
        width: w,
        height: h,
        exposure_us: Some(10_000),
        sensor_gain: Some(1.0),
        diagnostic_snapshot: None,
        gravity_camera_frame: None,
        gps_truth: None,
    }
}

/// Build a canonical-layout capture bundle under `dir` with
/// `n_frames` frames, an honest `ap_input`, and a distinct
/// `gps_truth`. Returns the bundle dir.
fn make_bundle(dir: &Path, n_frames: u32) -> PathBuf {
    let bundle_dir = dir.join("captures").join("cap-test-0001");
    let frames_dir = bundle_dir.join("frames");
    std::fs::create_dir_all(&frames_dir).unwrap();

    for seq in 0..n_frames {
        let stem = format!("{seq:08}");
        let pgm = frames_dir.join(format!("{stem}.pgm"));
        let json = frames_dir.join(format!("{stem}.json"));
        write_pgm(&pgm, 4, 3, (seq % 255) as u8);
        let sc = sidecar(seq, 1_700_000_000_000 + i64::from(seq) * 1000, 4, 3);
        std::fs::write(&json, serde_json::to_vec(&sc).unwrap()).unwrap();
    }

    let manifest = BundleManifest {
        schema_version: SCHEMA_VERSION,
        bundle_id: "cap-test-0001".into(),
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
            width: 4,
            height: 3,
            fx: 1000.0,
            fy: 1000.0,
            cx: 2.0,
            cy: 1.5,
            distortion: Distortion::None,
            rms_px: None,
            solved_at_unix_ms: None,
            placeholder: Some(true),
        },
        // The engine ran against THIS ap_input. It must survive
        // to the collector untouched.
        ap_input: Some(ApInput {
            lat: 12.34,
            lon: -56.78,
            eye_height_m: 2.0,
            provenance: ApProvenance::OperatorEntered,
        }),
        ap_derivation_trace: None,
        // Distinct ground-truth location. Must NEVER be
        // substituted for ap_input.
        gps_truth: Some(GpsTruth {
            lat: 99.0,
            lon: 99.0,
            lat_sigma_m: 5.0,
            lon_sigma_m: 7.0,
            altitude_m: None,
            altitude_sigma_m: None,
            captured_unix_ms: 1_700_000_000_500,
            source: "phone_gnss".into(),
            satellites_used: Some(9),
        }),
        atmosphere_hint: None,
        notes: "test capture".into(),
        session_id: None,
    };
    manifest.save_to_dir(&bundle_dir).unwrap();
    std::fs::write(bundle_dir.join("pbris.log"), b"$PBRIS,TEST,1*00\n").unwrap();
    bundle_dir
}

fn test_source() -> SubmissionSource {
    SubmissionSource {
        device_uuid: "01HXYZTESTDEVICE0000000001".into(),
        device_model: "TestPhone".into(),
        device_os: "Android 14 (API 34)".into(),
        app_version: "0.1.0".into(),
        bris_core_version: "0.0.1".into(),
        bris_data_version: None,
        note: Some("operator note".into()),
    }
}

#[test]
fn bundle_json_is_shipped_verbatim_with_honest_ap() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = make_bundle(tmp.path(), 2);
    let raw_bundle = std::fs::read(bundle_dir.join("bundle.json")).unwrap();

    let submission = build_submission(&bundle_dir, &test_source(), None).unwrap();

    // First part is the verbatim bundle.json.
    let first = &submission.parts[0];
    assert_eq!(first.filename, "bundle.json");
    assert_eq!(first.role, "bundle_manifest");
    assert_eq!(
        first.bytes, raw_bundle,
        "bundle.json must be shipped byte-for-byte"
    );

    // The shipped bundle still carries the honest ap_input and a
    // DISTINCT gps_truth (never merged).
    let shipped: BundleManifest = serde_json::from_slice(&first.bytes).unwrap();
    let ap = shipped.ap_input.expect("ap_input preserved");
    assert!((ap.lat - 12.34).abs() < 1e-9);
    assert!((ap.lon - (-56.78)).abs() < 1e-9);
    let gt = shipped.gps_truth.expect("gps_truth preserved");
    assert!((gt.lat - 99.0).abs() < 1e-9);

    // The manifest's ground-truth `gps` is the gps_truth, and is
    // NOT equal to ap_input — proving no substitution.
    let gps = submission.manifest.gps.as_ref().expect("gps present");
    assert!((gps.lat_deg - 99.0).abs() < 1e-9);
    assert!(
        (gps.lat_deg - ap.lat).abs() > 1.0,
        "gps ground-truth must not be ap_input"
    );
}

#[test]
fn every_part_has_a_correct_checksum_and_media_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = make_bundle(tmp.path(), 3);
    let submission = build_submission(&bundle_dir, &test_source(), None).unwrap();
    let reviewed = SubmissionReview::new(submission).approve();

    // media[] and parts agree in count, name, size, checksum.
    assert!(media_matches_parts(&reviewed));

    // Independently recompute each checksum.
    for part in &reviewed.submission().parts {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&part.bytes);
        assert_eq!(hex::encode(h.finalize()), part.checksum_sha256);
    }

    // A bundle.json + pbris.log + N*(pgm+sidecar).
    let expected = 2 + 3 * 2;
    assert_eq!(reviewed.submission().parts.len(), expected);
}

/// The one-screen review always discloses the GPS ground-truth
/// and reassures that the manifest is shipped verbatim.
#[test]
fn operator_review_discloses_gps_and_verbatim_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = make_bundle(tmp.path(), 1);
    let submission = build_submission(&bundle_dir, &test_source(), None).unwrap();
    let review = SubmissionReview::new(submission);
    let labels: Vec<String> = review.lines().into_iter().map(|l| l.label).collect();
    assert!(labels.iter().any(|l| l == "GPS ground-truth"));
    assert!(labels.iter().any(|l| l == "Manifest"));
    assert!(labels.iter().any(|l| l == "Files"));
}

// ------- the real-collector end-to-end acceptance test -------

/// Mount the real collector router, POST our built multipart
/// payload, and assert the collector ACCEPTS it (200 + an id).
/// This proves the wire format — including per-file SHA-256
/// checksum validation — is correct against the actual server.
#[tokio::test]
async fn built_payload_is_accepted_by_the_real_collector() {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use bris_collector::routes::AppState;
    use bris_collector::store::Store;
    use bris_collector::{build_app, Config};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = make_bundle(tmp.path(), 2);
    let submission = build_submission(&bundle_dir, &test_source(), None).unwrap();
    let reviewed = SubmissionReview::new(submission).approve();

    let body = bris_submit::transport::encode_multipart(&reviewed).unwrap();
    let ctype = bris_submit::transport::multipart_content_type();

    // Stand up the collector over a tempdir store.
    let store_dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_root: store_dir.path().to_path_buf(),
        bind: "127.0.0.1:0".to_owned(),
        bearer_token: "test-token".to_owned(),
        max_submission_bytes: 32 * 1024 * 1024,
    };
    let store = Store::open(&cfg.data_root).unwrap();
    let state = std::sync::Arc::new(AppState { config: cfg, store });
    let app = build_app(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(header::CONTENT_TYPE, ctype)
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "collector rejected the built payload"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v.get("id").and_then(|i| i.as_str()).is_some());
}

/// A DELIBERATELY corrupted checksum must be REJECTED by the
/// collector — proving the checksum path is live end-to-end.
#[tokio::test]
async fn tampered_checksum_is_rejected_by_the_real_collector() {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use bris_collector::routes::AppState;
    use bris_collector::store::Store;
    use bris_collector::{build_app, Config};
    use tower::ServiceExt;

    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = make_bundle(tmp.path(), 1);
    let mut submission = build_submission(&bundle_dir, &test_source(), None).unwrap();
    // Corrupt one declared checksum in the manifest (but leave
    // the bytes untouched) so the collector's recompute differs.
    submission.manifest.media[1].checksum_sha256 =
        Some("0000000000000000000000000000000000000000000000000000000000000000".into());
    let reviewed = SubmissionReview::new(submission).approve();
    let body = bris_submit::transport::encode_multipart(&reviewed).unwrap();
    let ctype = bris_submit::transport::multipart_content_type();

    let store_dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_root: store_dir.path().to_path_buf(),
        bind: "127.0.0.1:0".to_owned(),
        bearer_token: "test-token".to_owned(),
        max_submission_bytes: 32 * 1024 * 1024,
    };
    let store = Store::open(&cfg.data_root).unwrap();
    let state = std::sync::Arc::new(AppState { config: cfg, store });
    let app = build_app(state);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/submissions")
        .header(header::AUTHORIZATION, "Bearer test-token")
        .header(header::CONTENT_TYPE, ctype)
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// --------------------- retry queue tests ---------------------

/// A transport that fails transiently for the first `fail_n`
/// calls, then succeeds — models a temporarily-unreachable
/// collector.
struct FlakyTransport {
    fail_n: u32,
    calls: AtomicU32,
}

impl Transport for FlakyTransport {
    fn send(&self, _s: &ReviewedSubmission) -> Result<SubmissionOutcome, TransportError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_n {
            Err(TransportError::Transient(format!("attempt {n} down")))
        } else {
            Ok(SubmissionOutcome {
                id: "collector-id-xyz".into(),
            })
        }
    }
}

/// A transport that always rejects permanently.
struct RejectingTransport;
impl Transport for RejectingTransport {
    fn send(&self, _s: &ReviewedSubmission) -> Result<SubmissionOutcome, TransportError> {
        Err(TransportError::Permanent {
            status: 400,
            message: "bad token".into(),
        })
    }
}

fn enqueue_one(queue: &SubmissionQueue) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = make_bundle(tmp.path(), 1);
    let submission = build_submission(&bundle_dir, &test_source(), None).unwrap();
    let reviewed = SubmissionReview::new(submission).approve();
    queue.enqueue(&reviewed).unwrap()
}

#[test]
fn queue_retries_transient_then_succeeds() {
    let qroot = tempfile::tempdir().unwrap();
    // Zero backoff so the test doesn't wait.
    let queue = SubmissionQueue::open(qroot.path())
        .unwrap()
        .with_policy(5, 0, 0);
    let id = enqueue_one(&queue);
    assert_eq!(queue.pending_ids().unwrap(), vec![id.clone()]);

    let transport = FlakyTransport {
        fail_n: 2,
        calls: AtomicU32::new(0),
    };

    // First two passes retry; third succeeds.
    let r1 = queue.attempt_pending(&id, &transport).unwrap();
    assert!(matches!(r1, AttemptResult::Retrying { attempts: 1, .. }));
    let r2 = queue.attempt_pending(&id, &transport).unwrap();
    assert!(matches!(r2, AttemptResult::Retrying { attempts: 2, .. }));
    let r3 = queue.attempt_pending(&id, &transport).unwrap();
    match r3 {
        AttemptResult::Sent { collector_id, .. } => {
            assert_eq!(collector_id, "collector-id-xyz");
        }
        other => panic!("expected Sent, got {other:?}"),
    }
    // No longer pending.
    assert!(queue.pending_ids().unwrap().is_empty());
}

#[test]
fn queue_dead_letters_permanent_rejection() {
    let qroot = tempfile::tempdir().unwrap();
    let queue = SubmissionQueue::open(qroot.path())
        .unwrap()
        .with_policy(5, 0, 0);
    let id = enqueue_one(&queue);
    let r = queue.attempt_pending(&id, &RejectingTransport).unwrap();
    match r {
        AttemptResult::DeadLettered { reason, .. } => assert!(reason.contains("400")),
        other => panic!("expected DeadLettered, got {other:?}"),
    }
    assert!(queue.pending_ids().unwrap().is_empty());
    assert_eq!(queue.dead_ids().unwrap(), vec![id]);
}

#[test]
fn queue_is_durable_across_reopen() {
    let qroot = tempfile::tempdir().unwrap();
    let id = {
        let queue = SubmissionQueue::open(qroot.path()).unwrap();
        enqueue_one(&queue)
    };
    // Reopen a fresh handle; the entry survives, checksums
    // intact, and drains successfully.
    let queue = SubmissionQueue::open(qroot.path()).unwrap();
    assert_eq!(queue.pending_ids().unwrap(), vec![id.clone()]);
    let transport = FlakyTransport {
        fail_n: 0,
        calls: AtomicU32::new(0),
    };
    let results = queue.drain_once(&transport).unwrap();
    assert!(matches!(results[0], AttemptResult::Sent { .. }));
    let _ = id;
}

#[test]
fn queue_dead_letters_after_exhausting_attempts() {
    let qroot = tempfile::tempdir().unwrap();
    let queue = SubmissionQueue::open(qroot.path())
        .unwrap()
        .with_policy(3, 0, 0);
    let id = enqueue_one(&queue);
    let always_down = FlakyTransport {
        fail_n: u32::MAX,
        calls: AtomicU32::new(0),
    };
    let mut last = None;
    for _ in 0..3 {
        last = Some(queue.attempt_pending(&id, &always_down).unwrap());
    }
    assert!(matches!(last, Some(AttemptResult::DeadLettered { .. })));
    assert_eq!(queue.dead_ids().unwrap(), vec![id]);
}
