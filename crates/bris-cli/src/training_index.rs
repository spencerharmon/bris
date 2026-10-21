//! Read-only collector export for Phase-2 segmentation annotation/training.
//!
//! Pixels remain in collector storage. This index preserves source metadata,
//! not inferred labels; see `docs/operator/training_corpus.md` for its contract.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use bris_bundle::{BundleManifest, FrameSidecar};
use bris_submit::manifest::{SubmissionKind, SubmissionManifest};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Export a deterministic index without mutating collector storage or `SQLite`.
pub(crate) fn export(data_root: &Path, output: &Path) -> Result<()> {
    let root = data_root
        .canonicalize()
        .context("open collector data root")?;
    let mut directories = Vec::new();
    collect_submissions(&root.join("submissions"), &mut directories)?;
    directories.sort();
    let mut captures = Vec::new();
    for dir in directories {
        if let Some(capture) = index_submission(&root, &dir)
            .with_context(|| format!("index submission {}", dir.display()))?
        {
            captures.push(capture);
        }
    }
    let index = json!({"format": "bris-training-corpus", "schema_version": 1,
        "path_base": "collector-data-root", "captures": captures});
    // Validate everything before publishing; never truncate a previous export or
    // leave a partially written JSON file visible to a training consumer.
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(file.as_file_mut(), &index)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(output)
        .context("publish new training index")?;
    Ok(())
}

fn collect_submissions(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    ensure!(
        fs::symlink_metadata(dir)?.is_dir(),
        "expected real directory: {}",
        dir.display()
    );
    if dir.join("deleted_at.txt").try_exists()? {
        return Ok(());
    }
    if dir.join("manifest.json").try_exists()? {
        found.push(dir.to_owned());
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == ".staging" {
            continue;
        }
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "symlink in submission tree: {}",
            entry.path().display()
        );
        if kind.is_dir() {
            collect_submissions(&entry.path(), found)?;
        }
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "expected regular file: {}",
        path.display()
    );
    fs::read(path).with_context(|| format!("read {}", path.display()))
}

// Mirrors collector store::media_destination without linking the HTTP/SQLite
// server into the CLI. Manifest filenames are mapped exactly as at ingest.
fn media_path(kind: SubmissionKind, role: &str, filename: &str) -> PathBuf {
    if role == "pbris_log" {
        return PathBuf::from("pbris.log");
    }
    let name = filename
        .trim_start_matches(['.', '/', '\\'])
        .replace(['/', '\\'], "_");
    let folder = if kind == SubmissionKind::Calibration
        || matches!(role, "calibration_frame" | "intrinsics_toml")
    {
        "calibration"
    } else {
        "media"
    };
    Path::new(folder).join(name)
}

// Keep submission validation and its provenance assembly in a single pipeline.
#[allow(clippy::too_many_lines)]
fn index_submission(root: &Path, dir: &Path) -> Result<Option<Value>> {
    let manifest_bytes = read_file(&dir.join("manifest.json"))?;
    let submission: SubmissionManifest = serde_json::from_slice(&manifest_bytes)?;
    ensure!(
        submission.schema_version == bris_submit::manifest::SCHEMA_VERSION,
        "unsupported submission schema_version {}",
        submission.schema_version
    );
    let kind_payload = match submission.submission_kind {
        SubmissionKind::Fix => &submission.fix,
        SubmissionKind::Calibration => &submission.calibration,
        SubmissionKind::DebugCapture => &submission.debug_capture,
    };
    ensure!(kind_payload.is_some(), "missing submission kind payload");
    let bundles: Vec<_> = submission
        .media
        .iter()
        .filter(|item| item.role == "bundle_manifest")
        .collect();
    if bundles.is_empty() {
        // Fix-only logs and calibration artifacts need not contain image bundles.
        return Ok(None);
    }
    ensure!(bundles.len() == 1, "expected one bundle manifest");
    let mut files = BTreeMap::new();
    for item in &submission.media {
        let relative = media_path(submission.submission_kind, &item.role, &item.filename);
        let path = dir.join(relative);
        // Reject symlinked media directories as well as symlinked files.
        ensure!(
            path.canonicalize()?.starts_with(dir.canonicalize()?),
            "media escapes submission"
        );
        ensure!(
            fs::symlink_metadata(path.parent().context("media parent")?)?.is_dir(),
            "media parent must be a real directory"
        );
        let bytes = read_file(&path)?;
        ensure!(
            bytes.len() as u64 == item.size_bytes,
            "media size mismatch: {}",
            item.filename
        );
        let checksum = hex::encode(Sha256::digest(&bytes));
        if let Some(expected) = &item.checksum_sha256 {
            ensure!(
                expected.eq_ignore_ascii_case(&checksum),
                "media checksum mismatch: {}",
                item.filename
            );
        }
        ensure!(
            files.insert(path, (item, checksum)).is_none(),
            "colliding media destinations"
        );
    }
    let bundle_path = dir.join(media_path(
        submission.submission_kind,
        &bundles[0].role,
        &bundles[0].filename,
    ));
    ensure!(
        bundle_path.file_name().is_some_and(|n| n == "bundle.json"),
        "expected bundle.json"
    );
    let bundle_dir = bundle_path.parent().context("bundle directory")?;
    let bundle = BundleManifest::load_from_dir(bundle_dir)?;
    ensure!(!bundle.bundle_id.is_empty(), "empty bundle_id");
    ensure!(
        matches!(bundle.capture.source_rotation_deg, 0 | 90 | 180 | 270),
        "unsupported source_rotation_deg"
    );
    let mut frames = Vec::new();
    let mut sequences = std::collections::BTreeSet::new();
    // Use the submission's inventory, not a glob: calibration/ may also hold
    // unrelated checkerboard PGMs with no capture sidecar.
    for (path, (item, checksum)) in &files {
        if !matches!(
            item.role.as_str(),
            "capture_frame" | "fix_frame" | "debug_frame"
        ) {
            continue;
        }
        ensure!(
            path.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pgm")),
            "expected PGM capture frame"
        );
        let sidecar_path = path.with_extension("json");
        ensure!(
            files
                .get(&sidecar_path)
                .is_some_and(|(m, _)| m.role == "frame_sidecar"),
            "missing declared frame sidecar"
        );
        let sidecar_bytes = read_file(&sidecar_path)?;
        let sidecar: FrameSidecar = serde_json::from_slice(&sidecar_bytes)?;
        ensure!(sequences.insert(sidecar.seq), "duplicate frame sequence");
        if let Some(seq) = item.frame_index {
            ensure!(seq == sidecar.seq, "frame_index disagrees with sidecar");
        }
        let bytes = read_file(path)?;
        let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Pnm)?;
        ensure!(
            (image.width(), image.height()) == (sidecar.width, sidecar.height),
            "frame dimensions disagree with sidecar"
        );
        let pixel_format = match image.color() {
            image::ColorType::L8 => "gray8",
            image::ColorType::L16 => "gray16",
            _ => bail!("expected grayscale PGM"),
        };
        let record = json!({"image": path.strip_prefix(root)?,
            "sidecar": sidecar_path.strip_prefix(root)?, "checksum_sha256": checksum,
            "width": image.width(), "height": image.height(), "pixel_format": pixel_format,
            "metadata": serde_json::from_slice::<Value>(&sidecar_bytes)?});
        frames.push((sidecar.captured_unix_ms, sidecar.seq, record, path));
    }
    if frames.is_empty() && submission.submission_kind == SubmissionKind::Calibration {
        return Ok(None);
    }
    ensure!(!frames.is_empty(), "bundle has no declared capture frames");
    frames.sort_by_key(|(time, seq, _, _)| (*time, *seq));
    if let Some(expected) = bundle
        .capture
        .first_frame_blake3
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        let bytes = read_file(frames[0].3)?;
        ensure!(
            blake3::hash(&bytes).to_hex().as_str() == expected,
            "first-frame checksum mismatch"
        );
    }
    let frames: Vec<_> = frames.into_iter().map(|(_, _, record, _)| record).collect();
    Ok(Some(json!({
        "submission_id": dir.file_name().context("submission id")?.to_str().context("UTF-8 submission id")?,
        "submission_kind": submission.submission_kind, "submitted_at": submission.submitted_at,
        "submission_manifest": dir.join("manifest.json").strip_prefix(root)?,
        "bundle_manifest": bundle_path.strip_prefix(root)?,
        "bundle_metadata": serde_json::from_slice::<Value>(&read_file(&bundle_path)?)?,
        "annotation_status": "unlabeled", "frames": frames
    })))
}
