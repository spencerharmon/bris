//! Shared schema and on-disk layout for Bris **debug bundles**.
//!
//! A debug bundle is a self-describing directory that captures
//! everything needed to re-run a fix attempt offline:
//!
//! - `bundle.json` — the [`BundleManifest`] (device, capture
//!   metadata, intrinsics, assumed position, GPS ground-truth,
//!   atmosphere hint, free-text notes).
//! - frame payload, in one of two layouts seen in the wild:
//!     - `media/NNNN.{pgm,json}` (legacy `bris-exports/...`
//!       on-device capture, before this crate existed),
//!     - `frames/NNNN.{pgm,json}` + `index.jsonl` (newer
//!       `bris-debug-...` on-device capture).
//!
//!   Per-frame metadata lives in a [`FrameSidecar`] JSON file
//!   next to the PGM.
//!
//! # Purpose
//!
//! This crate is the **shared schema** between the Android capture
//! path (which writes bundles), `bris-cli replay` (which consumes
//! them offline), and `bris-collector` (which ingests them). It
//! contains zero engine logic — it's pure serde plus a couple of
//! filesystem-layout helpers.
//!
//! # AP / GPS-truth / derivation: three independent axes
//!
//! The schema is deliberate about distinguishing:
//!
//! 1. [`ApInput`] — the **assumed position** that was fed into the
//!    engine at the start of the session. The engine's intercept
//!    method is referenced to this.
//! 2. [`GpsTruth`] — a **ground-truth** location, optionally
//!    captured out-of-band. Used by replay tooling to score the
//!    engine's published fix; **never** silently substituted for a
//!    missing `ApInput`.
//! 3. [`ApDerivationTrace`] — how the AP came to be (operator
//!    typed it; cold-start `CoP` produced it; etc). Loose by design
//!    so we can evolve it without breaking the schema.
//!
//! See `docs/design/debug_bundle_schema.md` for the full schema
//! reference.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Top-level bundle manifest, persisted as `bundle.json` at the
/// bundle directory root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleManifest {
    /// On-disk schema version. Additive changes within a major
    /// keep the same number; breaking changes bump it. Currently
    /// [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Stable, human-readable identifier for the bundle (matches
    /// the directory name in typical layouts).
    pub bundle_id: String,
    /// Device that captured this bundle.
    pub device: DeviceInfo,
    /// Capture-window metadata (frame count, timestamps,
    /// rotation declaration, optional first-frame checksum).
    pub capture: CaptureInfo,
    /// Camera intrinsics in force for the capture.
    pub intrinsics: IntrinsicsRecord,
    /// Assumed position fed into the engine (if any). `None`
    /// means the on-device session ran cold-start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ap_input: Option<ApInput>,
    /// Provenance trace of how the AP was derived. Loose by
    /// design; absent for older bundles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ap_derivation_trace: Option<ApDerivationTrace>,
    /// Ground-truth GPS location, optionally captured out-of-band
    /// (handheld GNSS receiver, post-hoc operator entry, etc.).
    /// **Replay tooling never substitutes this for a missing
    /// `ap_input`.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_truth: Option<GpsTruth>,
    /// Atmosphere hint at capture time. Falls back to
    /// `bris_almanac::Atmosphere::STANDARD` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atmosphere_hint: Option<AtmosphereHint>,
    /// Free-text operator notes. Empty string when absent.
    #[serde(default)]
    pub notes: String,
}

/// Current bundle schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Device identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Human-readable device model (e.g. `"Cat S62 Pro"`).
    pub model: String,
    /// Operating system + version (e.g. `"Android 11"`),
    /// optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Bris app version that produced the bundle, optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
}

/// Capture-window metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureInfo {
    /// CW rotation, in degrees, that callers must apply to the
    /// PGM bytes before feeding them to the engine. Valid values
    /// are 0, 90, 180, 270. The engine reads gravity-up frames,
    /// so legacy captures saved sensor-native frames declare
    /// non-zero rotation here and the replay path applies it
    /// at load time.
    pub source_rotation_deg: u16,
    /// If the on-device pipeline applied any rotation *before*
    /// writing the PGM, the angle it applied. Audit field;
    /// usually equal to `source_rotation_deg` for newer
    /// captures and `None` (with `source_rotation_deg != 0`)
    /// for older sensor-native captures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_rotation_was_deg: Option<u16>,
    /// Total frame count in the bundle.
    pub frame_count: u32,
    /// First-frame `captured_unix_ms` (mirrors the sidecar).
    pub started_unix_ms: i64,
    /// Last-frame `captured_unix_ms`.
    pub ended_unix_ms: i64,
    /// BLAKE3 hex digest of the first frame's PGM bytes. When
    /// `source_rotation_deg == 0` the PGM is the same buffer
    /// the engine sees; when non-zero, the PGM bytes are
    /// sensor-native and the checksum is of the raw file (the
    /// rotation has not yet been applied). `None` skips the
    /// integrity check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_frame_blake3: Option<String>,
}

/// Camera intrinsics record. Carries enough information to
/// reconstruct a `bris_vision::Intrinsics` plus source
/// provenance for replay tooling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntrinsicsRecord {
    /// Where these intrinsics came from. Drives operator UX
    /// (factory vs user-calibration vs placeholder).
    pub source: IntrinsicsSource,
    /// Lookup key into the factory profile table, if the
    /// intrinsics came from a baked-in factory profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_key: Option<ProfileKey>,
    /// Image width these intrinsics are calibrated against.
    pub width: u32,
    /// Image height these intrinsics are calibrated against.
    pub height: u32,
    /// Focal length (pixels) along x.
    pub fx: f64,
    /// Focal length (pixels) along y.
    pub fy: f64,
    /// Principal point x (pixels).
    pub cx: f64,
    /// Principal point y (pixels).
    pub cy: f64,
    /// Distortion model. `Distortion::None` is valid for
    /// pinhole-only captures.
    pub distortion: Distortion,
    /// RMS reprojection error from the calibration that
    /// produced these intrinsics (pixels), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rms_px: Option<f64>,
    /// Unix-ms timestamp of when the underlying calibration
    /// solved, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solved_at_unix_ms: Option<i64>,
}

/// Provenance of an [`IntrinsicsRecord`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntrinsicsSource {
    /// Baked-in factory profile keyed by device model + lens.
    Factory,
    /// User-supplied calibration from `bris calibrate`.
    UserCalibration {
        /// Stable identifier of the calibration session
        /// (file name, ULID, etc).
        session_id: String,
    },
    /// Whatever the platform layer (`CameraX`, `V4L2`) reported.
    DeviceReported,
    /// Identity-ish placeholder (`fx=fy=1000`, principal point
    /// at image center, no distortion). Real fixes against
    /// placeholder intrinsics are wrong by the calibration
    /// error; replay tooling warns loudly.
    Placeholder,
}

/// Factory-profile lookup key, recorded for audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileKey {
    /// Device model string (matches `android.os.Build.MODEL` or
    /// equivalent).
    pub model: String,
    /// Lens identifier (Camera2 logical-camera id, V4L2 device
    /// index, etc).
    pub lens_id: String,
    /// Calibrated image width.
    pub width: u32,
    /// Calibrated image height.
    pub height: u32,
}

/// Lens-distortion model.
///
/// All three variants are reserved from day one so adding
/// support for fisheye-equidistant captures (planned) doesn't
/// require a schema bump. Only `BrownConrady` is non-trivially
/// used today; `FisheyeEquidistant` ships zero-valued in
/// existing bundles and `None` is a valid choice for pinhole
/// captures.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "snake_case")]
pub enum Distortion {
    /// Brown-Conrady (radial k1/k2/k3 + tangential p1/p2).
    BrownConrady {
        /// Radial coefficient 1.
        k1: f64,
        /// Radial coefficient 2.
        k2: f64,
        /// Radial coefficient 3.
        k3: f64,
        /// Tangential coefficient 1.
        p1: f64,
        /// Tangential coefficient 2.
        p2: f64,
    },
    /// Fisheye equidistant (`OpenCV` `fisheye::calibrate` model).
    /// Reserved; not yet consumed by the engine.
    FisheyeEquidistant {
        /// Coefficient 1.
        k1: f64,
        /// Coefficient 2.
        k2: f64,
        /// Coefficient 3.
        k3: f64,
        /// Coefficient 4.
        k4: f64,
    },
    /// Pinhole (no distortion).
    None,
}

/// Assumed-position input fed into the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApInput {
    /// Latitude (degrees, N positive).
    pub lat: f64,
    /// Longitude (degrees, E positive).
    pub lon: f64,
    /// Eye height above sea level (metres).
    pub eye_height_m: f64,
    /// How the AP was decided.
    pub provenance: ApProvenance,
}

/// Provenance of an [`ApInput`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApProvenance {
    /// Operator typed the AP into the device UI.
    OperatorEntered,
    /// Carried over from a previous engine fix.
    PriorFix,
    /// Produced by the cold-start circle-of-position solver.
    ColdStartCop,
    /// Re-derived because the prior was deemed stale (e.g.
    /// `cold_start.stale_prior_intercept_threshold_nm`
    /// trigger).
    StalePriorTrigger,
    /// Catch-all with a free-text detail field.
    Other {
        /// Free-text description.
        detail: String,
    },
}

/// Loose trace of how the AP was derived. Evolves as the
/// engine grows more AP sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApDerivationTrace {
    /// Method name (matches `ApProvenance` variants by
    /// convention but free-form).
    pub method: String,
    /// Bundle-relative path to the `CoP` intersection data, if
    /// the AP came from cold-start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cop_intersections_bundle_ref: Option<String>,
    /// Age of the prior fix when the stale-prior trigger
    /// fired, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_prior_age_s: Option<f64>,
    /// 1σ of the prior fix at the time it was reused
    /// (nautical miles).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_fix_sigma_nm: Option<f64>,
}

/// GPS ground-truth location. Optional and never substituted
/// for `ap_input`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpsTruth {
    /// Latitude (degrees, N positive).
    pub lat: f64,
    /// Longitude (degrees, E positive).
    pub lon: f64,
    /// 1σ latitude error (metres).
    pub lat_sigma_m: f64,
    /// 1σ longitude error (metres).
    pub lon_sigma_m: f64,
    /// Altitude (metres MSL), optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<f64>,
    /// 1σ altitude error (metres), optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_sigma_m: Option<f64>,
    /// Unix-ms timestamp when the GPS fix was taken.
    pub captured_unix_ms: i64,
    /// Free-text source label (e.g. `"phone_gnss"`,
    /// `"operator_supplied_post_hoc"`).
    pub source: String,
    /// Number of satellites used in the GPS solution, if
    /// known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satellites_used: Option<u16>,
}

/// Atmosphere hint for refraction modeling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtmosphereHint {
    /// Temperature (kelvin).
    pub temperature_k: f64,
    /// Pressure (pascals).
    pub pressure_pa: f64,
    /// Relative humidity (0..=1).
    pub humidity: f64,
    /// Source label (`"manual"`, `"openmeteo"`, etc).
    pub source: String,
}

/// Per-frame sidecar JSON, written next to each PGM.
///
/// Deserializes the on-device capture's existing schema so old
/// bundles still load; the new optional fields default to
/// `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameSidecar {
    /// Frame sequence number within the bundle (0-based).
    pub seq: u32,
    /// Wall-clock capture timestamp (Unix milliseconds).
    pub captured_unix_ms: i64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Exposure (microseconds), if reported by the capture
    /// stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure_us: Option<u32>,
    /// Sensor gain multiplier, if reported (1.0 = unity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor_gain: Option<f64>,
    /// Engine diagnostic snapshot at the moment the frame was
    /// captured. Opaque to this crate; round-tripped as a
    /// `serde_json::Value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic_snapshot: Option<serde_json::Value>,
}

impl FrameSidecar {
    /// Resolve the effective exposure for this frame, falling
    /// back to `default_us` when the sidecar didn't record one.
    #[must_use]
    pub fn exposure_us_or(&self, default_us: u32) -> u32 {
        self.exposure_us.unwrap_or(default_us)
    }

    /// Resolve the effective sensor gain for this frame,
    /// falling back to `default` when the sidecar didn't
    /// record one.
    #[must_use]
    pub fn sensor_gain_or(&self, default: f64) -> f64 {
        self.sensor_gain.unwrap_or(default)
    }
}

/// One frame's worth of (PGM, sidecar) paths.
#[derive(Debug, Clone)]
pub struct FramePathPair {
    /// Path to the PGM (raw 16-bit grayscale or 8-bit grey;
    /// loader picks).
    pub pgm: PathBuf,
    /// Path to the JSON sidecar that describes it.
    pub sidecar: PathBuf,
    /// Sidecar loaded eagerly so callers can sort by
    /// `captured_unix_ms` without re-reading the file.
    pub sidecar_data: FrameSidecar,
}

/// Bundle error type.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// Wrapped I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON parse failure, with the offending file path.
    #[error("json error in {path}: {source}")]
    Json {
        /// Path of the file that failed to parse.
        path: PathBuf,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// No `bundle.json` found at the expected path.
    #[error("no bundle.json found at {searched}")]
    MissingManifest {
        /// Directory that was searched.
        searched: PathBuf,
    },
    /// Manifest declares an unsupported schema version.
    #[error("unsupported schema_version: found {found}, this build supports {supported}")]
    UnsupportedSchema {
        /// Version found in the manifest.
        found: u32,
        /// Version this build supports.
        supported: u32,
    },
    /// A frame's sidecar JSON is missing.
    #[error("missing sidecar for frame {frame}")]
    MissingSidecar {
        /// Frame PGM whose sidecar is missing.
        frame: PathBuf,
    },
    /// First-frame BLAKE3 checksum mismatch.
    #[error("first-frame checksum mismatch: expected {expected}, got {got}")]
    ChecksumMismatch {
        /// Expected BLAKE3 hex.
        expected: String,
        /// Computed BLAKE3 hex.
        got: String,
    },
}

impl BundleManifest {
    /// Load a manifest from `bundle.json` at the given
    /// directory root.
    pub fn load_from_dir(dir: &Path) -> Result<Self, BundleError> {
        let path = dir.join("bundle.json");
        if !path.exists() {
            return Err(BundleError::MissingManifest { searched: path });
        }
        let bytes = fs::read(&path)?;
        let manifest: Self =
            serde_json::from_slice(&bytes).map_err(|source| BundleError::Json {
                path: path.clone(),
                source,
            })?;
        if manifest.schema_version != SCHEMA_VERSION {
            return Err(BundleError::UnsupportedSchema {
                found: manifest.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        Ok(manifest)
    }

    /// Save a manifest to `bundle.json` at the given directory
    /// root, creating the directory if it doesn't exist.
    pub fn save_to_dir(&self, dir: &Path) -> Result<(), BundleError> {
        fs::create_dir_all(dir)?;
        let path = dir.join("bundle.json");
        let json = serde_json::to_vec_pretty(self).map_err(|source| BundleError::Json {
            path: path.clone(),
            source,
        })?;
        fs::write(&path, json)?;
        Ok(())
    }
}

/// Load a single sidecar JSON file.
pub fn load_sidecar(path: &Path) -> Result<FrameSidecar, BundleError> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|source| BundleError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Enumerate the (PGM, sidecar) pairs in a bundle, sorted by
/// the sidecar's `captured_unix_ms`.
///
/// Handles both layouts seen in the wild:
///
/// - `<dir>/media/NNN.{pgm,json}` (legacy `bris-exports/...`),
/// - `<dir>/frames/NNN.{pgm,json}` + `<dir>/index.jsonl`
///   (newer `bris-debug-...`).
///
/// The function picks whichever subdirectory exists.
pub fn enumerate_frames(dir: &Path) -> Result<Vec<FramePathPair>, BundleError> {
    let frames_dir = if dir.join("frames").is_dir() {
        dir.join("frames")
    } else if dir.join("media").is_dir() {
        dir.join("media")
    } else {
        // No subdirectory? Try the bundle root itself as a last
        // resort; some hand-rolled corpora put PGMs at the root.
        dir.to_path_buf()
    };
    let mut pairs = Vec::new();
    for entry in fs::read_dir(&frames_dir)? {
        let entry = entry?;
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase);
        if ext.as_deref() != Some("pgm") {
            continue;
        }
        let sidecar_path = path.with_extension("json");
        if !sidecar_path.exists() {
            return Err(BundleError::MissingSidecar { frame: path });
        }
        let sidecar = load_sidecar(&sidecar_path)?;
        pairs.push(FramePathPair {
            pgm: path,
            sidecar: sidecar_path,
            sidecar_data: sidecar,
        });
    }
    pairs.sort_by_key(|p| p.sidecar_data.captured_unix_ms);
    Ok(pairs)
}

/// Verify the first-frame BLAKE3 checksum recorded in the
/// manifest, if any.
///
/// **Important:** the checksum is over the raw PGM file on
/// disk, *not* over a post-rotation buffer. For bundles with
/// `source_rotation_deg != 0` the PGM is sensor-native and
/// the rotation has not yet been applied. Documented in the
/// manifest schema so the on-device writer and this verifier
/// agree.
pub fn verify_first_frame_checksum(
    manifest: &BundleManifest,
    bundle_dir: &Path,
) -> Result<(), BundleError> {
    let Some(expected) = manifest.capture.first_frame_blake3.as_deref() else {
        return Ok(());
    };
    let pairs = enumerate_frames(bundle_dir)?;
    let Some(first) = pairs.first() else {
        return Ok(());
    };
    let bytes = fs::read(&first.pgm)?;
    let got = blake3::hash(&bytes).to_hex().to_string();
    if got != expected {
        return Err(BundleError::ChecksumMismatch {
            expected: expected.to_string(),
            got,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn full_manifest() -> BundleManifest {
        BundleManifest {
            schema_version: SCHEMA_VERSION,
            bundle_id: "test-id".into(),
            device: DeviceInfo {
                model: "TestPhone".into(),
                os: Some("Android 11".into()),
                app_version: Some("0.0.1".into()),
            },
            capture: CaptureInfo {
                source_rotation_deg: 90,
                pre_rotation_was_deg: Some(90),
                frame_count: 2,
                started_unix_ms: 1_700_000_000_000,
                ended_unix_ms: 1_700_000_001_000,
                first_frame_blake3: None,
            },
            intrinsics: IntrinsicsRecord {
                source: IntrinsicsSource::Factory,
                profile_key: Some(ProfileKey {
                    model: "TestPhone".into(),
                    lens_id: "0".into(),
                    width: 4032,
                    height: 3024,
                }),
                width: 4032,
                height: 3024,
                fx: 3100.0,
                fy: 3090.0,
                cx: 2016.0,
                cy: 1512.0,
                distortion: Distortion::BrownConrady {
                    k1: 0.02,
                    k2: -0.03,
                    k3: 0.0,
                    p1: -0.001,
                    p2: -0.002,
                },
                rms_px: Some(0.73),
                solved_at_unix_ms: Some(1_700_000_000_000),
            },
            ap_input: Some(ApInput {
                lat: 30.0,
                lon: -97.0,
                eye_height_m: 1.7,
                provenance: ApProvenance::OperatorEntered,
            }),
            ap_derivation_trace: Some(ApDerivationTrace {
                method: "operator_entered".into(),
                cop_intersections_bundle_ref: None,
                stale_prior_age_s: None,
                prior_fix_sigma_nm: None,
            }),
            gps_truth: Some(GpsTruth {
                lat: 30.001,
                lon: -97.001,
                lat_sigma_m: 5.0,
                lon_sigma_m: 5.0,
                altitude_m: Some(150.0),
                altitude_sigma_m: Some(10.0),
                captured_unix_ms: 1_700_000_000_500,
                source: "phone_gnss".into(),
                satellites_used: Some(8),
            }),
            atmosphere_hint: Some(AtmosphereHint {
                temperature_k: 288.15,
                pressure_pa: 101_325.0,
                humidity: 0.5,
                source: "manual".into(),
            }),
            notes: "test bundle".into(),
        }
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let m = full_manifest();
        let s = serde_json::to_string(&m).unwrap();
        let back: BundleManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.bundle_id, m.bundle_id);
        assert!(back.ap_input.is_some());
        assert!(back.gps_truth.is_some());
        assert!(matches!(
            back.intrinsics.distortion,
            Distortion::BrownConrady { .. }
        ));
    }

    #[test]
    fn manifest_round_trips_without_optional_fields() {
        let mut m = full_manifest();
        m.ap_input = None;
        m.gps_truth = None;
        m.ap_derivation_trace = None;
        m.atmosphere_hint = None;
        let s = serde_json::to_string(&m).unwrap();
        // None-valued options should be omitted from the JSON.
        assert!(!s.contains("ap_input"));
        assert!(!s.contains("gps_truth"));
        let back: BundleManifest = serde_json::from_str(&s).unwrap();
        assert!(back.ap_input.is_none());
        assert!(back.gps_truth.is_none());
    }

    #[test]
    fn schema_version_mismatch_errors() {
        let dir = tempdir().unwrap();
        let mut m = full_manifest();
        m.schema_version = 999;
        let raw = serde_json::to_vec(&m).unwrap();
        std::fs::write(dir.path().join("bundle.json"), raw).unwrap();
        let err = BundleManifest::load_from_dir(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            BundleError::UnsupportedSchema { found: 999, .. }
        ));
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let m = full_manifest();
        m.save_to_dir(dir.path()).unwrap();
        let back = BundleManifest::load_from_dir(dir.path()).unwrap();
        assert_eq!(back.bundle_id, m.bundle_id);
    }

    fn write_sidecar(path: &Path, seq: u32, captured_unix_ms: i64) {
        let s = FrameSidecar {
            seq,
            captured_unix_ms,
            width: 4032,
            height: 3024,
            exposure_us: None,
            sensor_gain: None,
            diagnostic_snapshot: None,
        };
        std::fs::write(path, serde_json::to_vec(&s).unwrap()).unwrap();
    }

    #[test]
    fn enumerate_handles_media_layout() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        // Out of order on disk; should sort by captured_unix_ms.
        std::fs::write(media.join("000000000001.pgm"), b"x").unwrap();
        write_sidecar(&media.join("000000000001.json"), 1, 200);
        std::fs::write(media.join("000000000000.pgm"), b"x").unwrap();
        write_sidecar(&media.join("000000000000.json"), 0, 100);
        let pairs = enumerate_frames(dir.path()).unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].sidecar_data.captured_unix_ms, 100);
        assert_eq!(pairs[1].sidecar_data.captured_unix_ms, 200);
    }

    #[test]
    fn enumerate_handles_frames_layout() {
        let dir = tempdir().unwrap();
        let frames = dir.path().join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        std::fs::write(frames.join("000000000000.pgm"), b"x").unwrap();
        write_sidecar(&frames.join("000000000000.json"), 0, 100);
        let pairs = enumerate_frames(dir.path()).unwrap();
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn checksum_verifies_on_match_and_errors_on_mismatch() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let payload = b"frame-bytes";
        std::fs::write(media.join("000000000000.pgm"), payload).unwrap();
        write_sidecar(&media.join("000000000000.json"), 0, 100);
        let expected = blake3::hash(payload).to_hex().to_string();
        let mut m = full_manifest();
        m.capture.first_frame_blake3 = Some(expected);
        verify_first_frame_checksum(&m, dir.path()).unwrap();
        // Tamper.
        std::fs::write(media.join("000000000000.pgm"), b"other-bytes").unwrap();
        let err = verify_first_frame_checksum(&m, dir.path()).unwrap_err();
        assert!(matches!(err, BundleError::ChecksumMismatch { .. }));
    }
}
