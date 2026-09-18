//! `bris fix` and `bris log` end-to-end tests.
//!
//! Both subcommands drive the continuous `StreamingEngine` over a
//! recorded capture. We reuse the same synthetic bundle the
//! `replay` smoke test uses: its horizon-vs-body geometry is not
//! physically consistent, so the engine correctly publishes NO
//! fix. That lets us assert the documented behaviour of each
//! subcommand deterministically:
//!
//! - `bris fix` on a no-fix capture exits non-zero with a clear
//!   "no fix published" message (honest silence, not a fake 0,0).
//! - `bris log` on a no-fix capture still creates the session
//!   sight log and reports "0 fix record(s) appended" — a real,
//!   inspectable artifact.
//!
//! The engine-drives-and-emits path itself is exercised
//! end-to-end (manifest resolve, frame enumeration, engine feed,
//! fix collection); only the count of published fixes is zero for
//! this deliberately-degenerate fixture.

use std::fs;
use std::process::Command;

#[allow(clippy::similar_names)]
fn write_synthetic_pgm(path: &std::path::Path, horizon_y: u32) {
    let width = 320u32;
    let height = 240u32;
    let mut pixels = vec![0u16; (width as usize) * (height as usize)];
    for y in 0..height {
        for x in 0..width {
            let v: u16 = if y < horizon_y { 50_000 } else { 5_000 };
            pixels[(y as usize) * (width as usize) + (x as usize)] = v;
        }
    }
    let buf = image::ImageBuffer::<image::Luma<u16>, _>::from_raw(width, height, pixels).unwrap();
    buf.save_with_format(path, image::ImageFormat::Pnm).unwrap();
}

fn make_bundle(bundle_dir: &std::path::Path) {
    let media = bundle_dir.join("media");
    fs::create_dir_all(&media).unwrap();
    for i in 0..3u32 {
        let pgm = media.join(format!("{i:012}.pgm"));
        write_synthetic_pgm(&pgm, 120);
        let sidecar = pgm.with_extension("json");
        let ts = 1_700_000_000_000i64 + i64::from(i) * 100;
        let s = format!(r#"{{"seq":{i},"captured_unix_ms":{ts},"width":320,"height":240}}"#);
        fs::write(&sidecar, s).unwrap();
    }
    let bundle = r#"{
        "schema_version": 1,
        "bundle_id": "synthetic-test",
        "device": { "model": "synthetic" },
        "capture": {
            "source_rotation_deg": 0,
            "frame_count": 3,
            "started_unix_ms": 1700000000000,
            "ended_unix_ms": 1700000000200
        },
        "intrinsics": {
            "source": { "kind": "placeholder" },
            "width": 320, "height": 240,
            "fx": 1000.0, "fy": 1000.0, "cx": 160.0, "cy": 120.0,
            "distortion": { "model": "none" }
        }
    }"#;
    fs::write(bundle_dir.join("bundle.json"), bundle).unwrap();
}

#[test]
fn fix_reports_honest_silence_on_no_fix_capture() {
    let tmp = tempfile::tempdir().unwrap();
    make_bundle(tmp.path());
    let exe = env!("CARGO_BIN_EXE_bris");
    let out = Command::new(exe)
        .args(["fix", "--bundle", tmp.path().to_str().unwrap()])
        .output()
        .expect("invoke bris fix");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "fix should exit non-zero when no fix is published.\n{stderr}"
    );
    assert!(
        stderr.contains("no fix published"),
        "expected an honest no-fix message.\nSTDERR:\n{stderr}"
    );
}

#[test]
fn log_creates_session_sight_log() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = tmp.path().join("bundle");
    fs::create_dir_all(&bundle_dir).unwrap();
    make_bundle(&bundle_dir);

    let corpus = tmp.path().join("corpus");
    let exe = env!("CARGO_BIN_EXE_bris");

    // Create a session to log into.
    let new = Command::new(exe)
        .args([
            "session",
            "new",
            "--title",
            "log-test",
            "--corpus",
            corpus.to_str().unwrap(),
        ])
        .output()
        .expect("invoke bris session new");
    assert!(new.status.success(), "session new failed");
    let session_id = String::from_utf8_lossy(&new.stdout).trim().to_string();
    assert!(!session_id.is_empty(), "session new printed no UUID");

    let out = Command::new(exe)
        .args([
            "log",
            "--session",
            &session_id,
            "--corpus",
            corpus.to_str().unwrap(),
            "--bundle",
            bundle_dir.to_str().unwrap(),
        ])
        .output()
        .expect("invoke bris log");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "bris log exited non-zero.\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}"
    );
    assert!(
        stdout.contains("fix record(s) appended"),
        "log did not report its append summary.\nSTDOUT:\n{stdout}"
    );
    // The structured sight log must exist under the session dir.
    let log_path = corpus
        .join("sessions")
        .join(&session_id)
        .join("sight-log.jsonl");
    assert!(
        log_path.exists(),
        "sight-log.jsonl was not created at {}",
        log_path.display()
    );
}
