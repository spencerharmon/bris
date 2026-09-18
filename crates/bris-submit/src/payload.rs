//! Build a `bris-bundle v1` [`Submission`] from an on-disk
//! capture bundle.
//!
//! The input is a capture directory in the canonical on-device
//! layout (see `docs/design/diagnostic_collection.md`):
//!
//! ```text
//! captures/<cap-id>/
//!   bundle.json          # bris_bundle::BundleManifest
//!   pbris.log            # optional $PBRIS narrative
//!   index.jsonl          # optional (Debug ON) frame catalog
//!   frames/ | media/
//!     NNNN.pgm
//!     NNNN.json          # per-frame sidecar
//! calibration/           # optional, for calibration submissions
//!   *.toml *.json *.jpg ...
//! ```
//!
//! The output [`Submission`] carries the collector-shaped
//! [`SubmissionManifest`] plus one [`SubmissionPart`] per file.
//! **The `bundle.json` is shipped verbatim** as the first part
//! (`role = "bundle_manifest"`); its `ap_input` / `gps_truth`
//! are preserved exactly (the manifest sent is the same
//! `BundleManifest` the engine ran against), and the manifest's
//! optional top-level `gps` is copied from the bundle's
//! ground-truth without ever standing in for `ap_input`.

use std::path::{Path, PathBuf};

use bris_bundle::{enumerate_frames, BundleManifest, SubmissionKindHint};
use sha2::{Digest, Sha256};

use crate::manifest::{
    Device, Gps, MediaItem, SubmissionKind, SubmissionManifest, Versions, SCHEMA_VERSION,
};
use crate::{Submission, SubmitError};

/// One file part of a submission: its multipart part-name /
/// on-disk filename, its role, its bytes, and its SHA-256.
#[derive(Clone)]
pub struct SubmissionPart {
    /// Filename (== multipart part name == collector on-disk
    /// name). Sanitized to a bare basename, no path separators.
    pub filename: String,
    /// Role string (see [`MediaItem::role`]).
    pub role: String,
    /// Optional frame index.
    pub frame_index: Option<u32>,
    /// Optional per-frame capture time, ISO 8601.
    pub captured_at: Option<String>,
    /// The raw file bytes.
    pub bytes: Vec<u8>,
    /// Lowercase-hex SHA-256 of `bytes`.
    pub checksum_sha256: String,
}

impl std::fmt::Debug for SubmissionPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubmissionPart")
            .field("filename", &self.filename)
            .field("role", &self.role)
            .field("frame_index", &self.frame_index)
            .field("captured_at", &self.captured_at)
            .field("size_bytes", &self.bytes.len())
            .field("checksum_sha256", &self.checksum_sha256)
            .finish()
    }
}

/// Runtime identity/version facts the caller supplies (they are
/// NOT compiled infra facts — a per-install UUID, the shell app
/// version, the running `bris-core` version, etc.).
#[derive(Debug, Clone)]
pub struct SubmissionSource {
    /// Per-install device UUID / ULID.
    pub device_uuid: String,
    /// Device model name.
    pub device_model: String,
    /// Device OS string.
    pub device_os: String,
    /// Shell app version (Android app version, or the CLI's).
    pub app_version: String,
    /// Running `bris-core` version.
    pub bris_core_version: String,
    /// `bris-data` OTA payload version, if any.
    pub bris_data_version: Option<String>,
    /// Operator-entered free-text note, if any.
    pub note: Option<String>,
}

/// Compute a lowercase-hex SHA-256 digest of a byte slice.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, SubmitError> {
    std::fs::read(path).map_err(|source| SubmitError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn basename(path: &Path) -> Result<String, SubmitError> {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| SubmitError::Invalid(format!("path has no filename: {}", path.display())))
}

/// Map an ISO-8601 (or unix-ms) capture time. The bundle records
/// `captured_unix_ms`; the manifest wants ISO 8601. We convert
/// via chrono.
fn unix_ms_to_iso8601(unix_ms: i64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_millis_opt(unix_ms).single().map_or_else(
        || format!("{unix_ms}ms-since-epoch"),
        |dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}

/// Build a [`Submission`] from a capture bundle directory.
///
/// # Arguments
/// - `bundle_dir` — the capture directory containing
///   `bundle.json` and a `frames/` (or `media/`) subdirectory.
/// - `source` — runtime identity/version facts.
/// - `calibration_dir` — optional directory of calibration
///   artifacts (`*.toml`, residuals `*.json`, checkerboard
///   frames). When `Some`, its files are attached under
///   `role = "calibration_frame"` / `"intrinsics_toml"`.
///
/// The `submission_kind` is derived from the bundle's own
/// content (a calibration dir ⇒ `Calibration`; otherwise the
/// bundle's frame retention decides `Fix` vs `DebugCapture`).
///
/// # Errors
/// I/O, bundle-parse, or JSON errors; or an empty bundle.
pub fn build_submission(
    bundle_dir: &Path,
    source: &SubmissionSource,
    calibration_dir: Option<&Path>,
) -> Result<Submission, SubmitError> {
    // 1. Load + ship the BundleManifest VERBATIM. We read the
    //    raw bytes (not a re-serialization) so the stored copy
    //    is byte-identical to what the engine ran against, and
    //    we parse a typed copy only to derive manifest fields
    //    (kind, captured_at, gps ground-truth) honestly.
    let bundle_json_path = bundle_dir.join("bundle.json");
    let bundle_bytes = read_bytes(&bundle_json_path)?;
    let bundle: BundleManifest = serde_json::from_slice(&bundle_bytes)?;

    let mut parts: Vec<SubmissionPart> = Vec::new();
    let mut media: Vec<MediaItem> = Vec::new();

    // The verbatim bundle.json is ALWAYS the first part.
    push_part(
        &mut parts,
        &mut media,
        SubmissionPart {
            filename: "bundle.json".to_owned(),
            role: "bundle_manifest".to_owned(),
            frame_index: None,
            captured_at: Some(unix_ms_to_iso8601(bundle.capture.started_unix_ms)),
            checksum_sha256: sha256_hex(&bundle_bytes),
            bytes: bundle_bytes,
        },
    );

    // 2. Enumerate frames (PGM + sidecar) and attach each. The
    //    sidecar's `retention` class distinguishes fix vs debug
    //    frames; we surface it in the role.
    let frames = enumerate_frames(bundle_dir)?;
    for pair in &frames {
        let sidecar = &pair.sidecar_data;
        let captured_at = Some(unix_ms_to_iso8601(sidecar.captured_unix_ms));

        // PGM part. The sidecar's `seq` is the authoritative
        // frame index; fix vs debug retention is recoverable
        // from the shipped sidecar + index.jsonl, so the role is
        // the neutral "capture_frame".
        let pgm_bytes = read_bytes(&pair.pgm)?;
        let pgm_name = basename(&pair.pgm)?;
        push_part(
            &mut parts,
            &mut media,
            SubmissionPart {
                filename: pgm_name,
                role: "capture_frame".to_owned(),
                frame_index: Some(sidecar.seq),
                captured_at: captured_at.clone(),
                checksum_sha256: sha256_hex(&pgm_bytes),
                bytes: pgm_bytes,
            },
        );

        // Sidecar part.
        let sidecar_bytes = read_bytes(&pair.sidecar)?;
        let sidecar_name = basename(&pair.sidecar)?;
        push_part(
            &mut parts,
            &mut media,
            SubmissionPart {
                filename: sidecar_name,
                role: "frame_sidecar".to_owned(),
                frame_index: Some(sidecar.seq),
                captured_at: captured_at.clone(),
                checksum_sha256: sha256_hex(&sidecar_bytes),
                bytes: sidecar_bytes,
            },
        );
    }

    // 3. Optional companion files at the bundle root.
    attach_optional(
        &mut parts,
        &mut media,
        &bundle_dir.join("pbris.log"),
        "pbris_log",
    )?;
    attach_optional(
        &mut parts,
        &mut media,
        &bundle_dir.join("index.jsonl"),
        "index_jsonl",
    )?;

    // 4. Optional calibration artifacts.
    let mut has_calibration = false;
    if let Some(cal_dir) = calibration_dir {
        if cal_dir.is_dir() {
            attach_calibration_dir(&mut parts, &mut media, cal_dir)?;
            has_calibration = true;
        }
    }

    if frames.is_empty() && !has_calibration {
        return Err(SubmitError::Invalid(format!(
            "bundle {} has no frames and no calibration artifacts to submit",
            bundle_dir.display()
        )));
    }

    // 5. Derive the submission kind + assemble the manifest.
    let submission_kind = if has_calibration {
        SubmissionKind::Calibration
    } else {
        match bundle.submission_kind_hint() {
            SubmissionKindHint::Fix => SubmissionKind::Fix,
            SubmissionKindHint::DebugCapture => SubmissionKind::DebugCapture,
        }
    };
    let manifest = assemble_manifest(&bundle, source, submission_kind, media);
    Ok(Submission { manifest, parts })
}

/// Assemble the collector-shaped [`SubmissionManifest`] from the
/// parsed bundle, runtime source facts, the derived kind, and
/// the collected media list.
fn assemble_manifest(
    bundle: &BundleManifest,
    source: &SubmissionSource,
    submission_kind: SubmissionKind,
    media: Vec<MediaItem>,
) -> SubmissionManifest {
    // GPS ground-truth: copy the bundle's gps_truth into the
    // manifest's `gps` field. This is ground-truth ONLY; it is
    // never used as ap_input (which lives, honest and untouched,
    // inside the shipped bundle.json).
    let gps = bundle.gps_truth.as_ref().map(|gt| Gps {
        lat_deg: gt.lat,
        lon_deg: gt.lon,
        horizontal_accuracy_m: gt.lat_sigma_m.max(gt.lon_sigma_m),
        source: gt.source.clone(),
    });

    let submitted_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let captured_at = unix_ms_to_iso8601(bundle.capture.started_unix_ms);

    // The kind-specific sub-object is required by the collector.
    // We do not fabricate rich fix/calibration detail we don't
    // have; instead we point the reviewer at the authoritative
    // bundle.json (shipped verbatim) with a minimal, honest
    // marker object so `Manifest::validate` passes.
    let kind_payload = serde_json::json!({
        "source": "bundle_manifest",
        "bundle_id": bundle.bundle_id,
        "note": "authoritative detail lives in the verbatim bundle.json media part",
    });
    let (fix, calibration, debug_capture) = match submission_kind {
        SubmissionKind::Fix => (Some(kind_payload), None, None),
        SubmissionKind::Calibration => (None, Some(kind_payload), None),
        SubmissionKind::DebugCapture => (None, None, Some(kind_payload)),
    };

    SubmissionManifest {
        schema_version: SCHEMA_VERSION,
        submission_kind,
        submitted_at,
        device: Device {
            uuid: source.device_uuid.clone(),
            model: source.device_model.clone(),
            os: source.device_os.clone(),
        },
        versions: Versions {
            app: source.app_version.clone(),
            bris_core: source.bris_core_version.clone(),
            bris_data: source.bris_data_version.clone(),
            submission_schema: SCHEMA_VERSION,
        },
        captured_at,
        gps,
        note: source.note.clone(),
        fix,
        calibration,
        debug_capture,
        media,
    }
}

/// Push a part and register the matching media entry.
fn push_part(parts: &mut Vec<SubmissionPart>, media: &mut Vec<MediaItem>, part: SubmissionPart) {
    media.push(MediaItem {
        filename: part.filename.clone(),
        role: part.role.clone(),
        frame_index: part.frame_index,
        captured_at: part.captured_at.clone(),
        size_bytes: part.bytes.len() as u64,
        checksum_sha256: Some(part.checksum_sha256.clone()),
    });
    parts.push(part);
}

/// Attach a single optional file if it exists.
fn attach_optional(
    parts: &mut Vec<SubmissionPart>,
    media: &mut Vec<MediaItem>,
    path: &Path,
    role: &str,
) -> Result<(), SubmitError> {
    if !path.exists() {
        return Ok(());
    }
    let bytes = read_bytes(path)?;
    let filename = basename(path)?;
    push_part(
        parts,
        media,
        SubmissionPart {
            filename,
            role: role.to_owned(),
            frame_index: None,
            captured_at: None,
            checksum_sha256: sha256_hex(&bytes),
            bytes,
        },
    );
    Ok(())
}

/// Attach every regular file directly under a calibration
/// directory. Role is derived from extension.
fn attach_calibration_dir(
    parts: &mut Vec<SubmissionPart>,
    media: &mut Vec<MediaItem>,
    cal_dir: &Path,
) -> Result<(), SubmitError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(cal_dir)
        .map_err(|source| SubmitError::Io {
            path: cal_dir.to_path_buf(),
            source,
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    entries.sort();
    for path in entries {
        let bytes = read_bytes(&path)?;
        let filename = basename(&path)?;
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase);
        let role = match ext.as_deref() {
            Some("toml") => "intrinsics_toml",
            Some("json") => "calibration_residuals",
            _ => "calibration_frame",
        };
        push_part(
            parts,
            media,
            SubmissionPart {
                filename,
                role: role.to_owned(),
                frame_index: None,
                captured_at: None,
                checksum_sha256: sha256_hex(&bytes),
                bytes,
            },
        );
    }
    Ok(())
}
