//! Persistence: read/write calibration results as TOML.
//!
//! The calibration solve produces a [`crate::CalibrationResult`].
//! Operators want to persist that to a file so subsequent
//! runs of `bris serve` and `bris capture` can load the
//! same intrinsics without re-calibrating.
//!
//! # File format
//!
//! TOML, single section, schema-versioned:
//!
//! ```toml
//! schema_version = 1
//!
//! [intrinsics]
//! image_width = 640
//! image_height = 480
//! fx = 612.34
//! fy = 612.71
//! cx = 318.91
//! cy = 240.50
//! k1 = -0.0823
//! k2 = 0.1421
//! k3 = 0.0
//! p1 = -0.0007
//! p2 = 0.0011
//!
//! [quality]
//! mean_reproj_error_px = 0.31
//! view_count = 28
//! observation_count = 2156
//! ```
//!
//! # Default location
//!
//! The CLI's `bris calibrate` writes to
//! `$XDG_DATA_HOME/bris/intrinsics.toml` (falling back to
//! `~/.local/share/bris/intrinsics.toml`) by default;
//! `bris serve` reads from the same path. Operators can
//! override with `--intrinsics <path>` or via the config
//! file's `[camera] intrinsics = "..."` field.

use std::path::{Path, PathBuf};

use bris_vision::Intrinsics;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info};

use crate::solve::CalibrationResult;

/// Schema version of the persisted file. Bumped when fields
/// are added/removed/renamed in a way that breaks readers.
pub const PERSIST_SCHEMA_VERSION: u32 = 1;

/// Persisted calibration intrinsics + quality summary.
///
/// Owns its own `Intrinsics` value plus the resolution and
/// quality numbers that an operator (or the engine) needs
/// to assess whether to trust the calibration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedIntrinsics {
    /// Schema version of the file. Readers compare against
    /// [`PERSIST_SCHEMA_VERSION`] and reject mismatches; an
    /// operator who shipped intrinsics from a newer Bris
    /// version sees a clear error.
    pub schema_version: u32,

    /// The fitted intrinsics + image dimensions.
    pub intrinsics: PersistedCameraIntrinsics,

    /// Quality summary; operators inspect this to decide
    /// whether the calibration is trustworthy.
    pub quality: PersistedQuality,
}

/// Camera intrinsics + the image resolution they were
/// calibrated against.
///
/// Resolution is part of the persisted format so
/// `bris serve` can refuse to load intrinsics calibrated
/// against a different resolution than the camera is
/// currently producing — focal length scales with sensor
/// crop / binning and a 640×480 calibration will produce
/// wrong altitudes at 1280×720.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedCameraIntrinsics {
    /// Image width in pixels at calibration time.
    pub image_width: u32,
    /// Image height in pixels at calibration time.
    pub image_height: u32,
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

/// Calibration quality summary persisted alongside the
/// intrinsics for operator inspection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedQuality {
    /// Mean RMS reprojection error in pixels.
    pub mean_reproj_error_px: f64,
    /// Number of distinct calibration views.
    pub view_count: usize,
    /// Total number of observed corners across all views.
    pub observation_count: usize,
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
    /// TOML parse failure.
    #[error("parse {path}: {source}")]
    Parse {
        /// File that couldn't be parsed.
        path: PathBuf,
        /// Underlying TOML deserializer error.
        #[source]
        source: toml::de::Error,
    },
    /// TOML serialization failure (vanishingly rare;
    /// fields are all primitives).
    #[error("serialize calibration: {0}")]
    Serialize(#[from] toml::ser::Error),
    /// Schema version mismatch — a file written by a
    /// future Bris version with a different layout.
    #[error(
        "intrinsics file {path} has schema version {file_version}; this Bris expects {expected_version}. \
         Re-run `bris calibrate` to regenerate."
    )]
    SchemaMismatch {
        /// File path.
        path: PathBuf,
        /// Version found in the file.
        file_version: u32,
        /// Version this Bris build expects.
        expected_version: u32,
    },
}

impl PersistedIntrinsics {
    /// Wrap a [`CalibrationResult`] for serialization.
    #[must_use]
    pub fn from_result(result: &CalibrationResult) -> Self {
        Self {
            schema_version: PERSIST_SCHEMA_VERSION,
            intrinsics: PersistedCameraIntrinsics {
                image_width: result.image_width,
                image_height: result.image_height,
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
            quality: PersistedQuality {
                mean_reproj_error_px: result.mean_reproj_error_px,
                view_count: result.view_count,
                observation_count: result.observation_count,
            },
        }
    }

    /// Convert into a [`bris_vision::Intrinsics`] for direct
    /// use by `bris_vision::Frame::new`. Drops the resolution
    /// and quality metadata; callers that need those keep the
    /// `PersistedIntrinsics` around.
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

/// Write a calibration result to a TOML file.
///
/// Creates parent directories as needed. Overwrites any
/// existing file at the path.
///
/// # Errors
///
/// See [`PersistError`].
pub fn write_intrinsics(path: &Path, result: &CalibrationResult) -> Result<(), PersistError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| PersistError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
    }
    let persisted = PersistedIntrinsics::from_result(result);
    let toml_text = toml::to_string_pretty(&persisted)?;
    std::fs::write(path, toml_text).map_err(|e| PersistError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    info!(path = %path.display(), "bris-calibrate: wrote intrinsics");
    Ok(())
}

/// Read a calibration from a TOML file.
///
/// # Errors
///
/// See [`PersistError`]. Most importantly,
/// [`PersistError::SchemaMismatch`] if the file's schema
/// version doesn't match this build's
/// [`PERSIST_SCHEMA_VERSION`].
pub fn read_intrinsics(path: &Path) -> Result<PersistedIntrinsics, PersistError> {
    debug!(path = %path.display(), "bris-calibrate: reading intrinsics");
    let text = std::fs::read_to_string(path).map_err(|e| PersistError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let persisted: PersistedIntrinsics =
        toml::from_str(&text).map_err(|e| PersistError::Parse {
            path: path.to_path_buf(),
            source: e,
        })?;
    if persisted.schema_version != PERSIST_SCHEMA_VERSION {
        return Err(PersistError::SchemaMismatch {
            path: path.to_path_buf(),
            file_version: persisted.schema_version,
            expected_version: PERSIST_SCHEMA_VERSION,
        });
    }
    Ok(persisted)
}

/// Default search path for persisted intrinsics:
/// `$XDG_DATA_HOME/bris/intrinsics.toml`, falling back to
/// `~/.local/share/bris/intrinsics.toml`.
///
/// Returns `None` if neither `$XDG_DATA_HOME` nor `$HOME` is
/// set (very unusual; service contexts that strip the
/// environment).
#[must_use]
pub fn default_intrinsics_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg).join("bris").join("intrinsics.toml"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("bris")
                .join("intrinsics.toml"),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("intrinsics.toml");
        let r = sample_result();
        write_intrinsics(&path, &r).unwrap();
        let loaded = read_intrinsics(&path).unwrap();
        let i = loaded.intrinsics();
        assert!((i.fx - r.intrinsics.fx).abs() < 1e-9);
        assert!((i.k1 - r.intrinsics.k1).abs() < 1e-9);
        assert_eq!(loaded.intrinsics.image_width, 640);
        assert!((loaded.quality.mean_reproj_error_px - 0.31).abs() < 1e-12);
    }

    #[test]
    fn write_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("subdir").join("i.toml");
        write_intrinsics(&path, &sample_result()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn read_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(
            &path,
            r"
schema_version = 1
unknown_top_level = 42
[intrinsics]
image_width = 640
image_height = 480
fx = 1.0
fy = 1.0
cx = 0.5
cy = 0.5
k1 = 0.0
k2 = 0.0
k3 = 0.0
p1 = 0.0
p2 = 0.0
[quality]
mean_reproj_error_px = 0.5
view_count = 10
observation_count = 100
",
        )
        .unwrap();
        let err = read_intrinsics(&path).unwrap_err();
        assert!(matches!(err, PersistError::Parse { .. }), "got: {err:?}");
    }

    #[test]
    fn read_rejects_schema_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.toml");
        // Construct a "future" schema version (current is 1).
        let bogus = format!(
            r"
schema_version = {future}
[intrinsics]
image_width = 640
image_height = 480
fx = 1.0
fy = 1.0
cx = 0.5
cy = 0.5
k1 = 0.0
k2 = 0.0
k3 = 0.0
p1 = 0.0
p2 = 0.0
[quality]
mean_reproj_error_px = 0.5
view_count = 10
observation_count = 100
",
            future = PERSIST_SCHEMA_VERSION + 1
        );
        std::fs::write(&path, bogus).unwrap();
        let err = read_intrinsics(&path).unwrap_err();
        assert!(matches!(err, PersistError::SchemaMismatch { .. }));
    }

    #[test]
    fn missing_file_errors_with_io() {
        let err = read_intrinsics(std::path::Path::new("/no/such/file.toml")).unwrap_err();
        assert!(matches!(err, PersistError::Io { .. }));
    }
}
