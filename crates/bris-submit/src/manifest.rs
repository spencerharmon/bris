//! The submission manifest schema.
//!
//! This is the client-side mirror of the collector's
//! `POST /v1/submissions` `manifest` part
//! (`bris_collector::manifest::Manifest`). It is duplicated
//! rather than shared as a crate dependency on purpose: the
//! collector is a server component whose axum/tokio/rusqlite
//! stack has no business linking into an on-device submitter (or
//! into `bris-ffi`'s Android cdylib). The two schemas are kept
//! in lockstep by [`SCHEMA_VERSION`] and a round-trip
//! compatibility test in `tests/`.
//!
//! `schema_version` is `1` — the `bris-bundle v1` submission
//! format.

use serde::{Deserialize, Serialize};

/// Current submission manifest schema version. Must match
/// `bris_collector::manifest::SCHEMA_VERSION`.
pub const SCHEMA_VERSION: u32 = 1;

/// The manifest sent as the `manifest` multipart part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubmissionManifest {
    /// Manifest schema version. Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// What kind of submission this is.
    pub submission_kind: SubmissionKind,
    /// Wall-clock UTC of submission origination, ISO 8601.
    pub submitted_at: String,
    /// Originating device.
    pub device: Device,
    /// Component versions on the originating device.
    pub versions: Versions,
    /// Wall-clock UTC of the captured event, ISO 8601.
    pub captured_at: String,
    /// Optional GPS at capture time. **Ground-truth only** — a
    /// present `gps` here is never a substitute for the honest
    /// `ap_input` carried inside the shipped `bundle.json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gps: Option<Gps>,
    /// Optional operator-supplied note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Populated when `submission_kind = Fix`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<serde_json::Value>,
    /// Populated when `submission_kind = Calibration`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration: Option<serde_json::Value>,
    /// Populated when `submission_kind = DebugCapture`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_capture: Option<serde_json::Value>,
    /// One entry per uploaded file part.
    pub media: Vec<MediaItem>,
}

/// Submission kind — mirrors the collector's enum
/// (`serde(rename_all = "snake_case")`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionKind {
    /// On-device retained data for a single published fix.
    Fix,
    /// Full calibration session bundle.
    Calibration,
    /// Debug-capture buffer contents (frames, sidecars,
    /// bundle.json, pbris.log).
    DebugCapture,
}

impl SubmissionKind {
    /// The kind's stable string label, for logs / operator UI.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Fix => "fix",
            Self::Calibration => "calibration",
            Self::DebugCapture => "debug_capture",
        }
    }
}

/// Device metadata. `uuid` is a per-install random identifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    /// Per-install UUID / ULID (runtime config, not compiled).
    pub uuid: String,
    /// Device model name.
    pub model: String,
    /// OS version string.
    pub os: String,
}

/// Component versions carried in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Versions {
    /// Shell app version (Android app or CLI).
    pub app: String,
    /// `bris-core` version reported by the engine.
    pub bris_core: String,
    /// `bris-data` OTA payload version, or `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bris_data: Option<String>,
    /// Manifest schema version (denormalized). Must match the
    /// top-level `schema_version`.
    pub submission_schema: u32,
}

/// GPS at capture time — ground-truth, coarse.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Gps {
    /// Latitude in degrees, north positive.
    pub lat_deg: f64,
    /// Longitude in degrees, east positive.
    pub lon_deg: f64,
    /// Reported 1σ horizontal accuracy, meters.
    pub horizontal_accuracy_m: f64,
    /// Source of the fix: `"gps"`, `"fused"`, or `"network"`.
    pub source: String,
}

/// One uploaded file referenced by the manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaItem {
    /// Filename — matches the multipart part name and the
    /// on-disk name the collector writes.
    pub filename: String,
    /// Role: `"bundle_manifest"`, `"fix_frame"`, `"debug_frame"`,
    /// `"frame_sidecar"`, `"pbris_log"`, `"calibration_frame"`,
    /// `"intrinsics_toml"`, `"index_jsonl"`, `"debug_log"`, etc.
    pub role: String,
    /// Optional frame index for sequenced captures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_index: Option<u32>,
    /// Optional per-frame capture time, ISO 8601.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    /// Size in bytes. The collector re-verifies on receive.
    pub size_bytes: u64,
    /// Lowercase-hex SHA-256 of the file contents. Always
    /// populated by this crate (never elided): it is what lets
    /// the stored submission be *proven* byte-identical to what
    /// the device captured. The collector recomputes it and
    /// rejects a mismatch with 400.
    pub checksum_sha256: Option<String>,
}
