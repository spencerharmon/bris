//! Submission manifest schema.
//!
//! The schema is documented in
//! `docs/design/diagnostic_collection.md`. The canonical
//! version is `1`; the collector accepts only this version
//! today and rejects others with a clear error message so the
//! foreign client knows to upgrade.

use serde::{Deserialize, Serialize};

/// Current manifest schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Top-level manifest as POSTed by the device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Manifest schema version. Must equal [`SCHEMA_VERSION`]
    /// for the spike collector.
    pub schema_version: u32,
    /// What kind of submission this is. Drives which optional
    /// sub-object must be populated and which subdirectories
    /// are expected.
    pub submission_kind: SubmissionKind,
    /// Wall-clock UTC of submission origination on the device,
    /// ISO 8601.
    pub submitted_at: String,
    /// Originating device.
    pub device: Device,
    /// Component versions on the originating device.
    pub versions: Versions,
    /// Wall-clock UTC of the captured event (fix produced,
    /// calibration completed, debug-capture session started).
    /// ISO 8601.
    pub captured_at: String,
    /// Optional GPS at capture time.
    pub gps: Option<Gps>,
    /// Optional operator-supplied note.
    pub note: Option<String>,
    /// Populated when `submission_kind = Fix`. JSON-opaque to
    /// the collector — the review UI renders the contents.
    pub fix: Option<serde_json::Value>,
    /// Populated when `submission_kind = Calibration`.
    pub calibration: Option<serde_json::Value>,
    /// Populated when `submission_kind = DebugCapture`.
    pub debug_capture: Option<serde_json::Value>,
    /// One entry per uploaded file. The `filename` references
    /// the multipart part name and the file's on-disk name
    /// under `media/`.
    pub media: Vec<MediaItem>,
}

/// Submission kind, drives storage layout and validation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionKind {
    /// On-device retained data for a single published fix.
    Fix,
    /// Full calibration session bundle.
    Calibration,
    /// Rolling debug-capture buffer contents.
    DebugCapture,
}

/// Device metadata. UUID is a per-install random identifier
/// generated on first app launch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    /// Per-install UUID (ULID).
    pub uuid: String,
    /// Device model name (e.g. `"Pixel 7"`).
    pub model: String,
    /// OS version (e.g. `"Android 14 (API 34)"`).
    pub os: String,
}

/// Component versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Versions {
    /// Android app version (or whichever shell submitted).
    pub app: String,
    /// `bris-core` version as reported by the FFI.
    pub bris_core: String,
    /// `bris-data` OTA payload version, or `None`.
    pub bris_data: Option<String>,
    /// Manifest schema version (denormalized into versions for
    /// the review UI's filtering convenience). Must match the
    /// top-level `schema_version`.
    pub submission_schema: u32,
}

/// GPS at capture time. Coarse on Android (no FINE_LOCATION
/// permission requested even in debug mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gps {
    /// Latitude in degrees, north positive.
    pub lat_deg: f64,
    /// Longitude in degrees, east positive.
    pub lon_deg: f64,
    /// Reported 1σ horizontal accuracy in meters.
    pub horizontal_accuracy_m: f64,
    /// Source of the fix: `"gps"`, `"fused"`, or `"network"`.
    pub source: String,
}

/// One uploaded media file referenced by the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    /// Filename — must match the multipart part name and is
    /// the on-disk name under `media/`.
    pub filename: String,
    /// What role this file plays: `"fix_frame"`,
    /// `"calibration_frame"`, `"pbris_log"`,
    /// `"intrinsics_toml"`, `"debug_log"`, `"video"`, etc.
    pub role: String,
    /// Optional frame index for sequenced captures.
    pub frame_index: Option<u32>,
    /// Optional per-frame capture time, ISO 8601.
    pub captured_at: Option<String>,
    /// Size in bytes (denormalized; collector verifies on
    /// receive).
    pub size_bytes: u64,
}

/// Validation errors against a received manifest.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// Wrong schema version.
    #[error("unsupported schema_version {got} (collector accepts {expected})")]
    SchemaVersion {
        /// What the client sent.
        got: u32,
        /// What this collector accepts.
        expected: u32,
    },
    /// The kind-specific sub-object is missing.
    #[error("submission_kind={kind:?} but the corresponding sub-object is missing")]
    MissingKindPayload {
        /// Which kind was declared.
        kind: SubmissionKind,
    },
    /// A media item references a filename that wasn't uploaded.
    #[error("media references unknown filename: {filename}")]
    UnknownMedia {
        /// The dangling filename.
        filename: String,
    },
    /// A media item's size disagrees with the uploaded part.
    #[error("media size mismatch for {filename}: manifest={manifest}, received={received}")]
    SizeMismatch {
        /// The disputed filename.
        filename: String,
        /// What the manifest declared.
        manifest: u64,
        /// What the uploaded part actually was.
        received: u64,
    },
}

impl Manifest {
    /// Cross-check the manifest against a map of received file
    /// sizes keyed by part name.
    ///
    /// # Errors
    ///
    /// Returns the first inconsistency.
    pub fn validate(
        &self,
        received_files: &std::collections::HashMap<String, u64>,
    ) -> Result<(), ManifestError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ManifestError::SchemaVersion {
                got: self.schema_version,
                expected: SCHEMA_VERSION,
            });
        }
        match self.submission_kind {
            SubmissionKind::Fix if self.fix.is_none() => {
                return Err(ManifestError::MissingKindPayload {
                    kind: self.submission_kind,
                });
            }
            SubmissionKind::Calibration if self.calibration.is_none() => {
                return Err(ManifestError::MissingKindPayload {
                    kind: self.submission_kind,
                });
            }
            SubmissionKind::DebugCapture if self.debug_capture.is_none() => {
                return Err(ManifestError::MissingKindPayload {
                    kind: self.submission_kind,
                });
            }
            _ => {}
        }
        for item in &self.media {
            let received =
                received_files
                    .get(&item.filename)
                    .ok_or_else(|| ManifestError::UnknownMedia {
                        filename: item.filename.clone(),
                    })?;
            if *received != item.size_bytes {
                return Err(ManifestError::SizeMismatch {
                    filename: item.filename.clone(),
                    manifest: item.size_bytes,
                    received: *received,
                });
            }
        }
        Ok(())
    }
}
