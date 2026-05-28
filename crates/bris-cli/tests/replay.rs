//! `bris replay` end-to-end smoke test.
//!
//! The old subcommand was a one-shot panorama solver; the new
//! one drives the full streaming engine off a debug bundle (or a
//! `--frames`-only fallback for orphan corpora). The test here
//! synthesizes a tiny bundle and asserts the CLI exits 0 and the
//! engine pushes every frame. We deliberately don't assert on a
//! published fix \u2014 synthetic horizon-vs-sun geometry is wrong
//! and honest silence is the correct outcome.
//!
//! See `crates/bris-bundle` for the schema and
//! `docs/design/replay_modes.md` for the AP-mode contract.

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

#[test]
fn replay_bundle_runs_end_to_end_on_synthetic_frames() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = tmp.path();
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
    // Minimal bundle.json (schema_version 1, placeholder intrinsics).
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

    let exe = env!("CARGO_BIN_EXE_bris");
    let out = Command::new(exe)
        .args([
            "replay",
            "--bundle",
            bundle_dir.to_str().unwrap(),
            "--disable-store",
        ])
        .output()
        .expect("invoke bris");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("STDOUT:\n{stdout}\nSTDERR:\n{stderr}");

    // Strip ANSI escape sequences so substring checks work
    // regardless of whether tracing colourized output.
    let combined: String = combined
        .chars()
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
        .collect();

    assert!(
        out.status.success(),
        "bris replay exited non-zero.\n{combined}"
    );
    assert!(
        combined.contains("replay: bundle resolved"),
        "bundle did not resolve.\n{combined}"
    );
    assert!(
        combined.contains("frames_pushed=3"),
        "engine did not push all 3 frames.\n{combined}"
    );
    assert!(
        combined.contains("mode complete"),
        "mode did not complete.\n{combined}"
    );
}
