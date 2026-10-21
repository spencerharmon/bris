//! Collector filesystem to annotation-corpus CLI regression coverage.

use std::{fs, path::Path, process::Command};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn submission(root: &Path, id: &str, kind: &str) -> std::path::PathBuf {
    let dir = root.join("submissions/2026/10/21").join(id);
    let media = dir.join(if kind == "calibration" {
        "calibration"
    } else {
        "media"
    });
    fs::create_dir_all(&media).unwrap();
    let bundle = json!({
        "schema_version": 1, "bundle_id": format!("capture-{id}"),
        "device": {"model": "fixture"},
        "capture": {"source_rotation_deg": 90, "frame_count": 2,
            "started_unix_ms": 1_700_000_000_000_i64, "ended_unix_ms": 1_700_000_001_000_i64},
        "intrinsics": {"source": {"kind": "placeholder"}, "width": 3, "height": 2,
            "fx": 10.0, "fy": 10.0, "cx": 1.5, "cy": 1.0,
            "distortion": {"model": "none"}}
    });
    fs::write(
        media.join("bundle.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();
    let mut items = vec![json!({"filename":"bundle.json", "role":"bundle_manifest",
        "size_bytes":fs::metadata(media.join("bundle.json")).unwrap().len()})];
    for seq in [7, 2] {
        let name = format!("frame-{seq}");
        let pgm = b"P5\n3 2\n255\n\x00\x10\x20\x30\x40\x50";
        fs::write(media.join(format!("{name}.pgm")), pgm).unwrap();
        let sidecar = json!({"seq":seq, "captured_unix_ms":1_700_000_000_000_i64,
            "width":3,"height":2,"exposure_us":1200,"sensor_gain":2.0,
            "gravity_camera_frame":[0.0,1.0,0.0],"retention":"fix_frame",
            "diagnostic_snapshot":{"horizon":{"slope":0.1}}});
        let bytes = serde_json::to_vec(&sidecar).unwrap();
        fs::write(media.join(format!("{name}.json")), &bytes).unwrap();
        items.push(
            json!({"filename":format!("{name}.pgm"),"role":"capture_frame",
            "frame_index":seq,"size_bytes":pgm.len()}),
        );
        items.push(
            json!({"filename":format!("{name}.json"),"role":"frame_sidecar",
            "frame_index":seq,"size_bytes":bytes.len()}),
        );
    }
    let mut manifest = json!({"schema_version":1,"submission_kind":kind,
        "submitted_at":"2026-10-21T00:00:00Z","captured_at":"2023-11-14T22:13:20Z",
        "device":{"uuid":"fixture-device","model":"fixture","os":"test"},
        "versions":{"app":"test","bris_core":"test","submission_schema":1},
        "media":items});
    manifest[kind] = json!({});
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    dir
}

fn export(root: &Path, output: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bris"))
        .args(["corpus", "training-index", "--data-root"])
        .arg(root)
        .arg("--output")
        .arg(output)
        .output()
        .unwrap()
}

#[test]
fn training_index_enumerates_stored_frames_and_preserves_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    submission(tmp.path(), "b", "calibration");
    submission(tmp.path(), "a", "debug_capture");
    let deleted = submission(tmp.path(), "deleted", "fix");
    fs::write(deleted.join("deleted_at.txt"), "2026-10-21T12:00:00Z").unwrap();
    fs::create_dir_all(tmp.path().join("submissions/.staging/incomplete")).unwrap();
    fs::write(
        tmp.path()
            .join("submissions/.staging/incomplete/manifest.json"),
        "invalid",
    )
    .unwrap();
    let output = tmp.path().join("training.json");
    let result = export(tmp.path(), &output);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes = fs::read(&output).unwrap();
    let index: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(index["format"], "bris-training-corpus");
    assert_eq!(index["schema_version"], 1);
    let captures = index["captures"].as_array().unwrap();
    assert_eq!(captures.len(), 2);
    assert_eq!(captures[0]["submission_id"], "a");
    assert_eq!(captures[1]["submission_kind"], "calibration");
    for capture in captures {
        assert_eq!(capture["annotation_status"], "unlabeled");
        assert_eq!(
            capture["bundle_metadata"]["capture"]["source_rotation_deg"],
            90
        );
        assert!(tmp
            .path()
            .join(capture["bundle_manifest"].as_str().unwrap())
            .is_file());
        let frames = capture["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["metadata"]["seq"], 2);
        assert_eq!(frames[1]["metadata"]["seq"], 7);
        for frame in frames {
            let image = tmp.path().join(frame["image"].as_str().unwrap());
            assert_eq!(image::image_dimensions(image).unwrap(), (3, 2));
            assert_eq!(frame["width"], 3);
            assert_eq!(frame["height"], 2);
            assert_eq!(frame["pixel_format"], "gray8");
            assert_eq!(frame["checksum_sha256"].as_str().unwrap().len(), 64);
            assert_eq!(frame["metadata"]["retention"], "fix_frame");
            assert_eq!(frame["metadata"]["exposure_us"], 1200);
            assert_eq!(
                frame["metadata"]["gravity_camera_frame"],
                json!([0.0, 1.0, 0.0])
            );
        }
    }
    let second = tmp.path().join("second.json");
    assert!(export(tmp.path(), &second).status.success());
    assert_eq!(bytes, fs::read(second).unwrap());
    assert!(!export(tmp.path(), &output).status.success());
    assert_eq!(bytes, fs::read(output).unwrap());
    assert!(!tmp.path().join("index.sqlite").exists());
}

#[test]
fn training_index_rejects_invalid_submissions_without_output() {
    for failure in [
        "version",
        "missing",
        "checksum",
        "dimensions",
        "rotation",
        "bundle_version",
        "blake3",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = submission(tmp.path(), "a", "debug_capture");
        let path = dir.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match failure {
            "version" => manifest["schema_version"] = json!(2),
            "checksum" => manifest["media"][1]["checksum_sha256"] = json!("0".repeat(64)),
            "missing" => fs::remove_file(dir.join("media/frame-7.json")).unwrap(),
            "dimensions" => {
                let sidecar = dir.join("media/frame-7.json");
                let text = fs::read_to_string(&sidecar)
                    .unwrap()
                    .replace("\"width\":3", "\"width\":4");
                fs::write(sidecar, text).unwrap();
            }
            "rotation" | "bundle_version" | "blake3" => {
                let bundle = dir.join("media/bundle.json");
                let mut data: Value = serde_json::from_slice(&fs::read(&bundle).unwrap()).unwrap();
                if failure == "rotation" {
                    data["capture"]["source_rotation_deg"] = json!(45);
                } else if failure == "bundle_version" {
                    data["schema_version"] = json!(2);
                } else {
                    data["capture"]["first_frame_blake3"] = json!("0".repeat(64));
                }
                let bytes = serde_json::to_vec(&data).unwrap();
                manifest["media"][0]["size_bytes"] = json!(bytes.len());
                fs::write(bundle, bytes).unwrap();
            }
            _ => unreachable!(),
        }
        fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let output = tmp.path().join("training.json");
        let result = export(tmp.path(), &output);
        assert!(!result.status.success(), "accepted {failure}");
        let error = String::from_utf8_lossy(&result.stderr);
        let expected = match failure {
            "version" => "unsupported submission schema_version",
            "missing" => "No such file",
            "checksum" => "media checksum mismatch",
            "dimensions" => "frame dimensions disagree",
            "rotation" => "unsupported source_rotation_deg",
            "bundle_version" => "unsupported schema_version",
            "blake3" => "first-frame checksum mismatch",
            _ => unreachable!(),
        };
        assert!(error.contains(expected), "{failure}: {error}");
        assert!(!output.exists(), "partial index for {failure}");
    }
}

#[test]
fn training_index_verifies_checksums_and_orders_by_capture_time() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = submission(tmp.path(), "a", "debug_capture");
    let media = dir.join("media");
    let first = b"P5\n3 2\n255\n\x50\x40\x30\x20\x10\x00";
    fs::write(media.join("frame-7.pgm"), first).unwrap();
    let sidecar = media.join("frame-2.json");
    let mut data: Value = serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
    data["captured_unix_ms"] = json!(1_700_000_001_000_i64);
    fs::write(sidecar, serde_json::to_vec(&data).unwrap()).unwrap();
    let bundle = media.join("bundle.json");
    let mut data: Value = serde_json::from_slice(&fs::read(&bundle).unwrap()).unwrap();
    data["capture"]["first_frame_blake3"] = json!(blake3::hash(first).to_hex().as_str());
    fs::write(bundle, serde_json::to_vec(&data).unwrap()).unwrap();
    let manifest_path = dir.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    for item in manifest["media"].as_array_mut().unwrap() {
        let bytes = fs::read(media.join(item["filename"].as_str().unwrap())).unwrap();
        item["size_bytes"] = json!(bytes.len());
        item["checksum_sha256"] = json!(hex::encode(Sha256::digest(&bytes)).to_uppercase());
    }
    fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let output = tmp.path().join("training.json");
    let result = export(tmp.path(), &output);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let index: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    let frames = &index["captures"][0]["frames"];
    assert_eq!(frames[0]["metadata"]["seq"], 7);
    assert_eq!(frames[1]["metadata"]["seq"], 2);
    assert_eq!(
        frames[0]["checksum_sha256"],
        hex::encode(Sha256::digest(first))
    );
}

#[test]
fn training_index_skips_non_capture_submissions_and_accepts_empty_store() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("submissions")).unwrap();
    for kind in [None, Some("calibration"), Some("fix")] {
        if let Some(kind) = kind {
            let dir = submission(tmp.path(), kind, kind);
            let path = dir.join("manifest.json");
            let mut data: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            data["media"]
                .as_array_mut()
                .unwrap()
                .retain(|item| kind == "calibration" && item["role"] == "bundle_manifest");
            fs::write(path, serde_json::to_vec(&data).unwrap()).unwrap();
        }
        let output = tmp.path().join(format!("{kind:?}.json"));
        let result = export(tmp.path(), &output);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let data: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        assert_eq!(data["captures"], json!([]));
    }
}

#[cfg(unix)]
#[test]
fn training_index_rejects_symlinked_media() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = submission(tmp.path(), "a", "debug_capture");
    let image = dir.join("media/frame-7.pgm");
    let outside = tmp.path().join("outside.pgm");
    fs::rename(&image, &outside).unwrap();
    std::os::unix::fs::symlink(outside, image).unwrap();
    let output = tmp.path().join("training.json");
    let result = export(tmp.path(), &output);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("media escapes submission"));
    assert!(!output.exists());
}
