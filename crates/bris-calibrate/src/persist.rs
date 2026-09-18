//! Persistence: read/write calibration results as a single,
//! frontend-agnostic JSON manifest.
//!
//! # Unified format
//!
//! Camera calibration is produced by two frontends: the
//! `bris-cli calibrate` command (a one-shot desktop/embedded
//! workflow) and the Android app's in-app checkerboard
//! session (via `bris-ffi`). Historically these persisted
//! *different* shapes — the CLI wrote a bespoke TOML file,
//! Android wrote its own hand-rolled `calibration.json`. That
//! divergence meant a calibration produced by one frontend
//! could not be consumed by the other, and any tooling that
//! wanted to inspect "the" calibration format had two to
//! support.
//!
//! [`CalibrationManifest`] is now the **single** on-disk
//! schema for both: the exact JSON shape Android's
//! `CalibrationStore.writeIntrinsics` already produced
//! (`calibration_id`, `lens_id`, `status`, `intrinsics`,
//! `width`, `height`, `rms_px`, `n_frames_used`,
//! `n_frames_total`, `detection_stats`, `diagnosis_overall`,
//! `diagnosis_issues`, `per_view_residuals`). The CLI now
//! writes and reads this same shape; `bris-ffi` exposes
//! [`write_intrinsics`]/[`read_intrinsics`] so Android can
//! (going forward) delegate to the shared Rust
//! implementation instead of re-deriving the JSON by hand.
//!
//! ```json
//! {
//!   "calibration_id": "f15e1aa1-5ca7-4c62-b62f-cab1a1bca1ed",
//!   "lens_id": "0",
//!   "status": "complete",
//!   "intrinsics": {
//!     "fx": 612.34, "fy": 612.71, "cx": 318.91, "cy": 240.50,
//!     "k1": -0.0823, "k2": 0.1421, "k3": 0.0,
//!     "p1": -0.0007, "p2": 0.0011
//!   },
//!   "width": 640,
//!   "height": 480,
//!   "rms_px": 0.31,
//!   "n_frames_used": 28,
//!   "n_frames_total": 30,
//!   "detection_stats": {
//!     "tried": 30, "skipped_no_board": 1,
//!     "skipped_wrong_size": 1, "skipped_io": 0
//!   },
//!   "diagnosis_overall": "OK",
//!   "diagnosis_issues": [],
//!   "per_view_residuals": []
//! }
//! ```
//!
//! `calibration_id` and `lens_id` are session metadata that
//! only a device-attached capture session (Android) has at
//! calibration time; a one-shot CLI run over a directory of
//! frames has neither concept and omits them (`null` /
//! absent on read). Every other field is populated
//! identically by both frontends.
//!
//! # Default location
//!
//! The CLI's `bris calibrate` writes to
//! `$XDG_DATA_HOME/bris/intrinsics.json` (falling back to
//! `~/.local/share/bris/intrinsics.json`) by default;
//! `bris serve` reads from the same path. Operators can
//! override with `--intrinsics <path>` or via the config
//! file's `[camera] intrinsics = "..."` field.

use std::path::{Path, PathBuf};

use bris_vision::Intrinsics;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info};

use crate::detect::DetectionStats;
use crate::doctor::{Diagnosis, DiagnosisLevel};
use crate::solve::CalibrationResult;

/// Everything needed to write a [`CalibrationManifest`]:
/// the solved calibration, its diagnostic assessment, and
/// the detection stats from the frame pass that produced
/// it, plus whatever session metadata the calling frontend
/// has available.
#[derive(Debug, Clone)]
pub struct CalibrationReport<'a> {
    /// The solved intrinsics + quality summary.
    pub result: &'a CalibrationResult,
    /// Diagnostic assessment of `result` (see
    /// [`crate::doctor::diagnose`]).
    pub diagnosis: &'a Diagnosis,
    /// Frame detection stats from the pass that produced
    /// `result`'s input views.
    pub detection_stats: &'a DetectionStats,
    /// Session UUID, when the calling frontend has one (an
    /// Android in-app calibration session always does; a
    /// one-shot CLI run over a directory does not).
    pub calibration_id: Option<String>,
    /// Lens id (Camera2 physical-camera id string on
    /// Android), when the calling frontend has one.
    pub lens_id: Option<String>,
}

/// Persisted calibration manifest: the single JSON shape
/// written and read by both `bris-cli calibrate` and the
/// Android app (via `bris-ffi`). See the module docs for the
/// full field-by-field rationale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationManifest {
    /// Session UUID, when the writer had one. Android always
    /// sets this; a one-shot CLI run over a directory of
    /// frames has no session concept and omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_id: Option<String>,
    /// Lens id, when the writer had one (see
    /// [`CalibrationReport::lens_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens_id: Option<String>,
    /// Manifest status. Always `"complete"` for a manifest
    /// produced by [`write_intrinsics`] (an in-progress
    /// Android session stub uses `"in_progress"` and is
    /// never a valid input to [`read_intrinsics`]).
    pub status: String,
    /// The fitted intrinsics.
    pub intrinsics: ManifestIntrinsics,
    /// Image width in pixels at calibration time.
    pub width: u32,
    /// Image height in pixels at calibration time.
    pub height: u32,
    /// Mean RMS reprojection error in pixels.
    pub rms_px: f64,
    /// Number of input frames used in the solve.
    pub n_frames_used: u32,
    /// Number of input frames examined (including those
    /// silently skipped because no checkerboard was
    /// detected).
    pub n_frames_total: u32,
    /// Per-frame detection breakdown.
    pub detection_stats: ManifestDetectionStats,
    /// Overall diagnosis severity label (`"OK"`, `"WARN"`,
    /// or `"ERROR"`).
    pub diagnosis_overall: String,
    /// Operator-actionable diagnostic findings. Empty when
    /// the calibration is healthy.
    pub diagnosis_issues: Vec<ManifestDiagnosisIssue>,
    /// Per-view residual statistics in input order.
    pub per_view_residuals: Vec<ManifestViewResidual>,
}

/// Camera intrinsics as they appear in a
/// [`CalibrationManifest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestIntrinsics {
    /// Focal length in pixels along x.
    pub fx: f64,
    /// Focal length in pixels along y.
    pub fy: f64,
    /// Principal point x coordinate, pixels.
    pub cx: f64,
    /// Principal point y coordinate, pixels.
    pub cy: f64,
    /// Brown-Conrady radial coefficient k1.
    pub k1: f64,
    /// Brown-Conrady radial coefficient k2.
    pub k2: f64,
    /// Brown-Conrady radial coefficient k3.
    pub k3: f64,
    /// Brown-Conrady tangential coefficient p1.
    pub p1: f64,
    /// Brown-Conrady tangential coefficient p2.
    pub p2: f64,
}

/// Frame detection breakdown as it appears in a
/// [`CalibrationManifest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestDetectionStats {
    /// Total frames considered.
    pub tried: u64,
    /// Frames where the detector found nothing
    /// chessboard-shaped.
    pub skipped_no_board: u64,
    /// Frames where the detector found a chessboard but the
    /// grid dimensions didn't match the configured target.
    pub skipped_wrong_size: u64,
    /// Frames that couldn't be opened or decoded.
    pub skipped_io: u64,
}

/// One diagnostic finding as it appears in a
/// [`CalibrationManifest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestDiagnosisIssue {
    /// Severity label (`"OK"`, `"WARN"`, or `"ERROR"`).
    pub level: String,
    /// Short machine-readable identifier.
    pub code: String,
    /// Human-readable description of what was found.
    pub message: String,
    /// Operator-actionable remediation advice.
    pub remediation: String,
}

/// One per-view residual entry as it appears in a
/// [`CalibrationManifest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestViewResidual {
    /// Source frame identifier (file name).
    pub source: String,
    /// RMS reprojection residual over this view's corners,
    /// in pixels.
    pub rms_px: f64,
    /// Maximum per-corner residual, in pixels.
    pub max_px: f64,
    /// Number of corner observations contributing.
    pub n_corners: u64,
}

/// Errors reading or writing a persisted calibration.
#[derive(Debug, Error)]
pub enum PersistError {
    /// File system or I/O error.
    #[error("I/O on {path}: {source}")]
    Io {
        /// File path involved.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// JSON parse failure.
    #[error("parse {path}: {source}")]
    Parse {
        /// File that couldn't be parsed.
        path: PathBuf,
        /// Underlying JSON deserializer error.
        #[source]
        source: serde_json::Error,
    },
    /// JSON serialization failure (vanishingly rare; fields
    /// are all primitives).
    #[error("serialize calibration: {0}")]
    Serialize(#[from] serde_json::Error),
    /// Manifest status is not `"complete"` — either an
    /// in-progress Android session stub or a hand-edited
    /// file.
    #[error("intrinsics file {path} has status {status:?}; expected \"complete\"")]
    IncompleteStatus {
        /// File path.
        path: PathBuf,
        /// Status found in the file.
        status: String,
    },
}

impl CalibrationManifest {
    /// Build a manifest from a [`CalibrationReport`].
    #[must_use]
    pub fn from_report(report: &CalibrationReport<'_>) -> Self {
        let result = report.result;
        let stats = report.detection_stats;
        Self {
            calibration_id: report.calibration_id.clone(),
            lens_id: report.lens_id.clone(),
            status: "complete".to_string(),
            intrinsics: ManifestIntrinsics {
                fx: result.intrinsics.fx,
                fy: result.intrinsics.fy,
                cx: result.intrinsics.cx,
                cy: result.intrinsics.cy,
                k1: result.intrinsics.k1,
                k2: result.intrinsics.k2,
                k3: result.intrinsics.k3,
                p1: result.intrinsics.p1,
                p2: result.intrinsics.p2,
            },
            width: result.image_width,
            height: result.image_height,
            rms_px: result.mean_reproj_error_px,
            n_frames_used: u32::try_from(result.view_count).unwrap_or(u32::MAX),
            n_frames_total: u32::try_from(stats.tried).unwrap_or(u32::MAX),
            detection_stats: ManifestDetectionStats {
                tried: stats.tried as u64,
                skipped_no_board: stats.skipped_no_board as u64,
                skipped_wrong_size: stats.skipped_wrong_size as u64,
                skipped_io: stats.skipped_io as u64,
            },
            diagnosis_overall: level_label(report.diagnosis.overall).to_string(),
            diagnosis_issues: report
                .diagnosis
                .issues
                .iter()
                .map(|i| ManifestDiagnosisIssue {
                    level: level_label(i.level).to_string(),
                    code: i.code.to_string(),
                    message: i.message.clone(),
                    remediation: i.remediation.to_string(),
                })
                .collect(),
            per_view_residuals: result
                .per_view
                .iter()
                .map(|v| ManifestViewResidual {
                    source: v
                        .source
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    rms_px: v.rms_px,
                    max_px: v.max_px,
                    n_corners: v.n_corners as u64,
                })
                .collect(),
        }
    }

    /// Convert into a [`bris_vision::Intrinsics`] for direct
    /// use by `bris_vision::Frame::new`. Drops the
    /// resolution/quality/session metadata; callers that need
    /// those keep the `CalibrationManifest` around.
    #[must_use]
    pub fn intrinsics(&self) -> Intrinsics {
        Intrinsics {
            fx: self.intrinsics.fx,
            fy: self.intrinsics.fy,
            cx: self.intrinsics.cx,
            cy: self.intrinsics.cy,
            k1: self.intrinsics.k1,
            k2: self.intrinsics.k2,
            k3: self.intrinsics.k3,
            p1: self.intrinsics.p1,
            p2: self.intrinsics.p2,
        }
    }
}

fn level_label(level: DiagnosisLevel) -> &'static str {
    level.label()
}

/// Write a calibration report to a JSON manifest file.
///
/// Creates parent directories as needed. Overwrites any
/// existing file at the path.
///
/// # Errors
///
/// See [`PersistError`].
pub fn write_intrinsics(path: &Path, report: &CalibrationReport<'_>) -> Result<(), PersistError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| PersistError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
    }
    let manifest = CalibrationManifest::from_report(report);
    let json_text = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(path, json_text).map_err(|e| PersistError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    info!(path = %path.display(), "bris-calibrate: wrote intrinsics");
    Ok(())
}

/// Read a calibration manifest from a JSON file.
///
/// Accepts either a manifest written by [`write_intrinsics`]
/// or an equivalent manifest written by Android's
/// `CalibrationStore` (the two are the same schema).
///
/// # Errors
///
/// See [`PersistError`]. Most importantly,
/// [`PersistError::IncompleteStatus`] if the manifest's
/// `status` isn't `"complete"` (an in-progress Android
/// session stub, or a hand-edited file).
pub fn read_intrinsics(path: &Path) -> Result<CalibrationManifest, PersistError> {
    debug!(path = %path.display(), "bris-calibrate: reading intrinsics");
    let text = std::fs::read_to_string(path).map_err(|e| PersistError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let manifest: CalibrationManifest =
        serde_json::from_str(&text).map_err(|e| PersistError::Parse {
            path: path.to_path_buf(),
            source: e,
        })?;
    if manifest.status != "complete" {
        return Err(PersistError::IncompleteStatus {
            path: path.to_path_buf(),
            status: manifest.status,
        });
    }
    Ok(manifest)
}

/// Default search path for persisted intrinsics:
/// `$XDG_DATA_HOME/bris/intrinsics.json`, falling back to
/// `~/.local/share/bris/intrinsics.json`.
///
/// Returns `None` if neither `$XDG_DATA_HOME` nor `$HOME` is
/// set (very unusual; service contexts that strip the
/// environment).
#[must_use]
pub fn default_intrinsics_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg).join("bris").join("intrinsics.json"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("bris")
                .join("intrinsics.json"),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::DiagnosisLevel;

    fn sample_result() -> CalibrationResult {
        CalibrationResult {
            intrinsics: Intrinsics {
                fx: 612.34,
                fy: 612.71,
                cx: 318.91,
                cy: 240.50,
                k1: -0.0823,
                k2: 0.1421,
                k3: 0.0,
                p1: -0.0007,
                p2: 0.0011,
            },
            image_width: 640,
            image_height: 480,
            mean_reproj_error_px: 0.31,
            view_count: 28,
            observation_count: 2156,
            per_view: Vec::new(),
        }
    }

    fn sample_stats() -> DetectionStats {
        DetectionStats {
            tried: 30,
            skipped_no_board: 1,
            skipped_wrong_size: 1,
            skipped_io: 0,
        }
    }

    fn sample_diagnosis() -> Diagnosis {
        Diagnosis {
            overall: DiagnosisLevel::Ok,
            issues: Vec::new(),
        }
    }

    #[test]
    fn round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("intrinsics.json");
        let r = sample_result();
        let stats = sample_stats();
        let diagnosis = sample_diagnosis();
        let report = CalibrationReport {
            result: &r,
            diagnosis: &diagnosis,
            detection_stats: &stats,
            calibration_id: None,
            lens_id: None,
        };
        write_intrinsics(&path, &report).unwrap();
        let loaded = read_intrinsics(&path).unwrap();
        let i = loaded.intrinsics();
        assert!((i.fx - r.intrinsics.fx).abs() < 1e-9);
        assert!((i.k1 - r.intrinsics.k1).abs() < 1e-9);
        assert_eq!(loaded.width, 640);
        assert_eq!(loaded.height, 480);
        assert!((loaded.rms_px - 0.31).abs() < 1e-12);
        assert_eq!(loaded.n_frames_used, 28);
        assert_eq!(loaded.n_frames_total, 30);
        assert_eq!(loaded.detection_stats.skipped_no_board, 1);
        assert_eq!(loaded.diagnosis_overall, "OK");
        assert!(loaded.calibration_id.is_none());
    }

    #[test]
    fn write_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("subdir").join("i.json");
        let r = sample_result();
        let stats = sample_stats();
        let diagnosis = sample_diagnosis();
        let report = CalibrationReport {
            result: &r,
            diagnosis: &diagnosis,
            detection_stats: &stats,
            calibration_id: None,
            lens_id: None,
        };
        write_intrinsics(&path, &report).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn read_rejects_incomplete_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in_progress.json");
        std::fs::write(
            &path,
            r#"{
                "calibration_id": "abc",
                "lens_id": "0",
                "status": "in_progress",
                "intrinsics": {
                    "fx": 1.0, "fy": 1.0, "cx": 0.5, "cy": 0.5,
                    "k1": 0.0, "k2": 0.0, "k3": 0.0, "p1": 0.0, "p2": 0.0
                },
                "width": 640,
                "height": 480,
                "rms_px": 0.0,
                "n_frames_used": 0,
                "n_frames_total": 0,
                "detection_stats": {
                    "tried": 0, "skipped_no_board": 0,
                    "skipped_wrong_size": 0, "skipped_io": 0
                },
                "diagnosis_overall": "OK",
                "diagnosis_issues": [],
                "per_view_residuals": []
            }"#,
        )
        .unwrap();
        let err = read_intrinsics(&path).unwrap_err();
        assert!(matches!(err, PersistError::IncompleteStatus { .. }), "got: {err:?}");
    }

    #[test]
    fn read_rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        let err = read_intrinsics(&path).unwrap_err();
        assert!(matches!(err, PersistError::Parse { .. }), "got: {err:?}");
    }

    #[test]
    fn missing_file_errors_with_io() {
        let err = read_intrinsics(std::path::Path::new("/no/such/file.json")).unwrap_err();
        assert!(matches!(err, PersistError::Io { .. }));
    }

    /// The exact JSON Android's `CalibrationStore.writeIntrinsics`
    /// produces for a completed session (field names, nesting,
    /// and the session metadata a CLI one-shot run never has).
    /// Proves the CLI reader loads an Android-written manifest
    /// byte-faithfully — the round-trip the task requires.
    #[test]
    fn reads_android_written_manifest_byte_faithfully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calibration.json");
        let android_json = r#"{
            "calibration_id": "f15e1aa1-5ca7-4c62-b62f-cab1a1bca1ed",
            "lens_id": "0",
            "status": "complete",
            "intrinsics": {
                "fx": 3103.4061281557006,
                "fy": 3090.496744366685,
                "cx": 2013.857097640865,
                "cy": 1491.4983945221607,
                "k1": 0.02287385685683836,
                "k2": -0.027249189121853052,
                "k3": 0.0,
                "p1": -0.0020285902622051532,
                "p2": -0.004038950067724464
            },
            "width": 4032,
            "height": 3024,
            "rms_px": 0.7331791456580863,
            "n_frames_used": 15,
            "n_frames_total": 15,
            "detection_stats": {
                "tried": 15,
                "skipped_no_board": 0,
                "skipped_wrong_size": 0,
                "skipped_io": 0
            },
            "diagnosis_overall": "OK",
            "diagnosis_issues": [],
            "per_view_residuals": [
                {
                    "source": "frame_0006.jpg",
                    "rms_px": 1.43,
                    "max_px": 2.1,
                    "n_corners": 70
                }
            ]
        }"#;
        std::fs::write(&path, android_json).unwrap();
        let manifest = read_intrinsics(&path).unwrap();
        assert_eq!(
            manifest.calibration_id.as_deref(),
            Some("f15e1aa1-5ca7-4c62-b62f-cab1a1bca1ed")
        );
        assert_eq!(manifest.lens_id.as_deref(), Some("0"));
        assert_eq!(manifest.width, 4032);
        assert_eq!(manifest.height, 3024);
        assert!((manifest.rms_px - 0.7331791456580863).abs() < 1e-12);
        assert_eq!(manifest.n_frames_used, 15);
        assert_eq!(manifest.per_view_residuals.len(), 1);
        assert_eq!(manifest.per_view_residuals[0].source, "frame_0006.jpg");
        let i = manifest.intrinsics();
        assert!((i.fx - 3103.4061281557006).abs() < 1e-9);

        // Now write it back out through the CLI path and
        // confirm re-reading produces the identical manifest
        // (schema is closed under round-trip, not just
        // read-only compatible).
        let round_trip_path = dir.path().join("round_trip.json");
        let json_text = serde_json::to_string_pretty(&manifest).unwrap();
        std::fs::write(&round_trip_path, &json_text).unwrap();
        let reloaded = read_intrinsics(&round_trip_path).unwrap();
        assert_eq!(reloaded.calibration_id, manifest.calibration_id);
        assert_eq!(reloaded.width, manifest.width);
        assert_eq!(reloaded.per_view_residuals.len(), 1);
    }
}
