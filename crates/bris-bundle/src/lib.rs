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
use uuid::Uuid;

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
    /// Build provenance of the FFI engine that wrote this bundle.
    /// Optional for backward-compat with pre-Phase-8.5 bundles;
    /// absent in those, populated in everything written after.
    /// When absent, regression tooling treats the bundle as
    /// "unknown build" and excludes it from baseline comparisons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildInfo>,
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
    /// Back-reference to the session this capture belongs to,
    /// per `docs/design/testing_strategy.md`. `UUIDv4` string;
    /// `None` for bundles produced before the session model
    /// existed (orphan captures).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
}

/// Build provenance: which Rust source tree produced the
/// `bris-ffi` shared object that wrote this bundle.
///
/// Populated from `bris_ffi::version()` (which is in turn
/// populated by `crates/bris-ffi/build.rs` at compile time).
/// Mirrored across the FFI as `VersionInfo`; this struct is
/// the persisted form.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildInfo {
    /// Full git SHA of the source tree at build time, or
    /// `"unknown"` for non-git builds.
    pub git_sha: String,
    /// `git describe --always --tags --dirty` output.
    pub git_describe: String,
    /// `true` when the worktree had uncommitted changes at
    /// build time. Regression baselines refuse dirty builds.
    pub git_dirty: bool,
    /// `git rev-list --count HEAD` — monotone commit index.
    pub commit_count: u32,
    /// Build-time UTC timestamp, ISO 8601.
    pub build_timestamp_utc: String,
    /// Semver of the `bris-ffi` crate at build time.
    pub bris_ffi_semver: String,
    /// Android `versionName` from the APK that bundled this
    /// FFI, when written from the Android shell. `None` for
    /// non-Android writers (CLI, tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android_version_name: Option<String>,
    /// Android `versionCode` from the APK that bundled this
    /// FFI, when written from the Android shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android_version_code: Option<u32>,
}

/// Current bundle schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Session schema version. Sessions and bundles are
/// versioned independently — a session manifest can evolve
/// without forcing the per-capture bundle schema to bump.
pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// Top-level session manifest, persisted as `session.json` at
/// `<corpus-root>/sessions/<session-id>/session.json`.
///
/// A session groups one or more captures (each a
/// [`BundleManifest`] under `captures/<cap-id>/`) sharing
/// the operator's intent (same vessel, same trip, same
/// observing window). See `docs/design/testing_strategy.md`
/// for the full model. The streaming engine itself is
/// session-aware only through [`SessionManifest::kinematics`]
/// and the `sight_retention_*` fields, which override the
/// corresponding `EngineConfig` defaults at engine
/// construction.
///
/// Sessions are never explicitly "ended". They exist until
/// the operator deletes them. `ordered_capture_ids` is
/// append-only; `session.json` is rewritten in place on each
/// capture save.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    /// Schema version. Currently [`SESSION_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Stable session identifier, `UUIDv4` string. The on-disk
    /// directory name matches this value.
    pub session_id: Uuid,
    /// Operator-supplied display title; shown in the
    /// "Resume session" picker. Mutable after creation.
    pub title: String,
    /// Unix-ms timestamp of the operator's "New session"
    /// action.
    pub created_unix_ms: i64,
    /// Device that owns this session (the device the operator
    /// created it on). Copied from the first capture's
    /// [`DeviceInfo`] for indexing; not authoritative once
    /// captures exist.
    pub device: DeviceInfo,
    /// Build provenance of the FFI engine that wrote this
    /// session. Mirrors the per-capture
    /// [`BundleManifest::build`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildInfo>,
    /// Free-text notes about the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Operator-entered assumed position at session create.
    /// Threaded into [`EngineConfig`] for the captures in this
    /// session. `None` = cold-start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ap_seed: Option<ApInput>,
    /// Use-case classification. Today only [`UseCaseProfile::Custom`]
    /// is meaningful; the other variants are reserved and
    /// behave identically. Eventually drives smart defaults
    /// for [`Self::kinematics`] and the retention fields.
    #[serde(default)]
    pub profile: UseCaseProfile,
    /// Operator's claim about motion across this session.
    /// Maps to `PublicationGateConfig::assumed_max_speed_kn`.
    #[serde(default)]
    pub kinematics: SessionKinematics,
    /// Override for `EngineConfig::sight_window_seconds`.
    /// Lets multi-day stationary sessions (e.g. window-sill
    /// sun sights over a week) combine sights spanning the
    /// full window.
    pub sight_retention_seconds: u64,
    /// Override for `EngineConfig::sight_window_capacity`.
    pub sight_retention_capacity: u32,
    /// Adversarial-corpus flag: `true` marks a session where
    /// no fix is the correct answer. Default `false`; the
    /// regression harness flips a "published fix here is a
    /// regression" assertion when `true`.
    #[serde(default)]
    pub expected_to_fail: bool,
    /// Captures belonging to this session, in chronological
    /// (replay) order. Append-only.
    #[serde(default)]
    pub ordered_capture_ids: Vec<String>,
}

/// Operator's claim about observer motion across a session.
///
/// Drives `PublicationGateConfig::assumed_max_speed_kn`, which
/// in turn inflates published fix σ by
/// `assumed_max_speed_kn * oldest_age_seconds / 3600` (RSS).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionKinematics {
    /// Observer not moving. σ-inflation contribution from
    /// motion is zero.
    #[default]
    Stationary,
    /// `MaxSpeedKn` { kn }: observer may move up to `kn` knots.
    MaxSpeedKn {
        /// Bound on observer speed, in knots.
        kn: f64,
    },
}

/// Use-case classification for a session. Drives a small set
/// of opinionated [`EngineConfig`] defaults via
/// `bris_streaming::apply_profile`; the operator's explicit
/// session.json / CLI overrides always win.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum UseCaseProfile {
    /// Operator sets `kinematics` / retention fields directly;
    /// the profile dispatcher is a no-op.
    #[default]
    Custom,
    /// Open-water sight-taking: full marine-friendly provider
    /// set (gradient, sky-region, night, night-textured,
    /// segmentation, reflection-pair, ML-gravity), cold-start
    /// enabled.
    Marine,
    /// Airborne capture: drops night and reflection-pair
    /// providers, narrows the publication gate's azimuth
    /// spread to 20°, defaults the speed gate to 250 kn when
    /// the operator left kinematics unset.
    Aeronautical,
    /// Stationary terrestrial observer: full default provider
    /// set, explicit 0-kn speed gate.
    LandBased,
    /// Cluttered urban scene: vanishing-point provider on,
    /// sky-region and reflection-pair off; night detectors
    /// retained.
    Urban,
}

impl SessionManifest {
    /// Default sight-retention window: 2 hours, matching
    /// `EngineConfig::sight_window_seconds`.
    pub const DEFAULT_RETENTION_SECONDS: u64 = 7200;
    /// Default sight-retention capacity: 50 sights, matching
    /// `EngineConfig::sight_window_capacity`.
    pub const DEFAULT_RETENTION_CAPACITY: u32 = 50;

    /// Construct a fresh session with the engine-default
    /// retention and `Stationary` kinematics. Caller supplies
    /// the `UUIDv4` string, title, device, and (Unix-ms) create
    /// time.
    #[must_use]
    pub fn new(session_id: Uuid, title: String, device: DeviceInfo, created_unix_ms: i64) -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            session_id,
            title,
            created_unix_ms,
            device,
            build: None,
            notes: None,
            ap_seed: None,
            profile: UseCaseProfile::default(),
            kinematics: SessionKinematics::default(),
            sight_retention_seconds: Self::DEFAULT_RETENTION_SECONDS,
            sight_retention_capacity: Self::DEFAULT_RETENTION_CAPACITY,
            expected_to_fail: false,
            ordered_capture_ids: Vec::new(),
        }
    }

    /// Write `session.json` to `<dir>/session.json`. Creates
    /// the directory if necessary. Pretty-printed.
    ///
    /// # Errors
    ///
    /// Filesystem or JSON serialization failure.
    pub fn save_to_dir(&self, dir: &Path) -> Result<(), BundleError> {
        fs::create_dir_all(dir)?;
        let path = dir.join("session.json");
        let raw = serde_json::to_vec_pretty(self).map_err(|source| BundleError::Json {
            path: path.clone(),
            source,
        })?;
        fs::write(path, raw)?;
        Ok(())
    }

    /// Read `<dir>/session.json`. Rejects with
    /// [`BundleError::UnsupportedSchema`] when
    /// `schema_version != SESSION_SCHEMA_VERSION`.
    ///
    /// # Errors
    ///
    /// Filesystem, JSON parse, or schema mismatch.
    pub fn load_from_dir(dir: &Path) -> Result<Self, BundleError> {
        let path = dir.join("session.json");
        let raw = fs::read(&path)?;
        let manifest: Self = serde_json::from_slice(&raw).map_err(|source| BundleError::Json {
            path: path.clone(),
            source,
        })?;
        if manifest.schema_version != SESSION_SCHEMA_VERSION {
            return Err(BundleError::UnsupportedSchema {
                found: manifest.schema_version,
                supported: SESSION_SCHEMA_VERSION,
            });
        }
        Ok(manifest)
    }
}

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
    /// Marks intrinsics emitted from
    /// [`IntrinsicsSource::Placeholder`] so replay tooling
    /// can distinguish synthetic placeholders (no
    /// calibration loaded) from real calibrations that
    /// happen to land near the same numeric values. Absent
    /// for measured (operator / factory / device-reported)
    /// intrinsics. Additive within `schema_version: 1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<bool>,
}

/// Provenance of an [`IntrinsicsRecord`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntrinsicsSource {
    /// Baked-in factory profile keyed by device model + lens.
    Factory,
    /// User-supplied calibration from `bris calibrate`.
    UserCalibration {
        /// Stable identifier of the calibration run (file name,
        /// ULID, etc). Distinct from a capture-session id; see
        /// `docs/design/testing_strategy.md` on the "session"
        /// term overloading. `session_id` accepted as an alias
        /// for backward-compat with bundles written before the
        /// rename.
        #[serde(alias = "session_id")]
        calibration_id: String,
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
    /// Unit gravity vector in the camera frame at capture time
    /// (image-right = +x, image-down = +y, lens-forward = +z),
    /// as reported by the platform's gravity sensor. Replay
    /// reconstructs the live `Frame::gravity_camera_frame` from
    /// this; absent means the bundle predates per-frame gravity
    /// recording and replay falls back to image-down (the
    /// historical behavior, which silently miscomputes
    /// artificial-horizon reflection pairs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gravity_camera_frame: Option<[f64; 3]>,
    /// Per-frame ground-truth GPS stamp (debug feature). A
    /// capture running hours at speed has truth that varies
    /// per frame; the bundle-level `gps_truth` field is
    /// implicitly wrong for any moving capture, so truth
    /// lives here. **Never** substituted for `ap_input` at
    /// engine time; replay scoring is the only consumer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_truth: Option<GpsTruth>,
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

/// One line of the optional `index.jsonl` sidecar written
/// next to a `frames/` directory by the on-device
/// `DebugCaptureBuffer`.
///
/// This file is **not required for replay**: the canonical
/// per-frame metadata lives in the JSON sidecars enumerated
/// by [`enumerate_frames`]. `index.jsonl` is a convenience
/// catalog the on-device writer maintains for fast
/// directory scans (one line per persisted frame, in the
/// order the writer flushed them), and it carries
/// `pgm_bytes` / `json_bytes` / `retention` fields that are
/// not duplicated in the per-frame sidecar.
///
/// The schema matches the on-disk shape produced by
/// `bris-android/.../DebugCaptureBuffer.kt`. Additive within
/// `schema_version: 1` (unknown trailing fields are ignored).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Frame sequence number within the capture (0-based),
    /// matching the [`FrameSidecar::seq`] of the same frame.
    pub seq: u32,
    /// Wall-clock capture timestamp (Unix milliseconds),
    /// matching [`FrameSidecar::captured_unix_ms`].
    pub captured_unix_ms: i64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Size of the PGM payload on disk, in bytes.
    pub pgm_bytes: u64,
    /// Size of the per-frame JSON sidecar on disk, in bytes.
    pub json_bytes: u64,
    /// Retention class the on-device writer tagged this
    /// frame with (`"debug"`, `"fix_frame"`, ...). Free-form
    /// string at this layer; consumers interpret it.
    pub retention: String,
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

/// Load the optional `index.jsonl` catalog from a bundle
/// directory.
///
/// Returns `Ok(None)` when the file is absent (the file is
/// not required for replay; the canonical per-frame metadata
/// lives in the JSON sidecars). Returns `Ok(Some(entries))`
/// when present, with one [`IndexEntry`] per non-empty line
/// in the file. Blank lines are skipped; any other parse
/// failure surfaces as [`BundleError::Json`].
pub fn load_index_jsonl(bundle_dir: &Path) -> Result<Option<Vec<IndexEntry>>, BundleError> {
    let path = bundle_dir.join("index.jsonl");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    let text = std::str::from_utf8(&bytes).map_err(|e| BundleError::Json {
        path: path.clone(),
        source: serde::de::Error::custom(e.to_string()),
    })?;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: IndexEntry = serde_json::from_str(line).map_err(|source| BundleError::Json {
            path: path.clone(),
            source,
        })?;
        out.push(entry);
    }
    Ok(Some(out))
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

/// Non-fatal advisories about a bundle's on-disk shape.
///
/// Returned by [`validate_intrinsics_against_first_pgm`] so
/// load-time callers can log them without aborting replay
/// of bundles produced by older / buggy writers. The Phase
/// 7.5 follow-up that adds per-frame rotation in sidecars
/// will let replay recover automatically; until then the
/// honest answer for an existing on-disk corpus is
/// "surface the mismatch, keep loading".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleWarning {
    /// Intrinsics record's `(width, height)` does not match
    /// the first PGM's `(width, height)`. Almost always a
    /// rotation bug: the writer emitted sensor-native
    /// intrinsics next to gravity-rotated frame bytes.
    IntrinsicsDimensionMismatch {
        /// Width declared in `bundle.json` `intrinsics.width`.
        intrinsics_width: u32,
        /// Height declared in `bundle.json` `intrinsics.height`.
        intrinsics_height: u32,
        /// Width of the first PGM file on disk.
        pgm_width: u32,
        /// Height of the first PGM file on disk.
        pgm_height: u32,
    },
}

/// Compare the manifest's intrinsics dimensions against the
/// first PGM on disk and return any mismatches as warnings.
///
/// **Warning, not error**, by design: bundles produced before
/// the Android writer learned to rotate its intrinsics block
/// to match `FrameAnalyzer`'s rotated bytes ship inconsistent
/// pairs, and aborting load on every such bundle would break
/// existing operator workflows that still need to replay
/// them (acknowledging the math is wrong). New bundles
/// produced by the fixed writer pass this check cleanly;
/// callers that want strictness can promote the warning to
/// an error themselves.
///
/// Returns `Ok(vec![])` for a self-consistent bundle and
/// `Ok(vec![warning, ...])` otherwise. Filesystem and JSON
/// errors still propagate.
///
/// # Errors
///
/// Filesystem I/O, missing sidecar, or an unparseable PGM
/// header.
pub fn validate_intrinsics_against_first_pgm(
    manifest: &BundleManifest,
    bundle_dir: &Path,
) -> Result<Vec<BundleWarning>, BundleError> {
    let pairs = enumerate_frames(bundle_dir)?;
    let Some(first) = pairs.first() else {
        return Ok(Vec::new());
    };
    let (pgm_w, pgm_h) = read_pgm_dimensions(&first.pgm)?;
    let mut out = Vec::new();
    if manifest.intrinsics.width != pgm_w || manifest.intrinsics.height != pgm_h {
        out.push(BundleWarning::IntrinsicsDimensionMismatch {
            intrinsics_width: manifest.intrinsics.width,
            intrinsics_height: manifest.intrinsics.height,
            pgm_width: pgm_w,
            pgm_height: pgm_h,
        });
    }
    Ok(out)
}

/// Parse the header of a P5 (binary) PGM and return
/// `(width, height)`. Skips `#`-prefixed comment lines per
/// the PGM spec. Returns a [`BundleError::Json`] (re-using
/// the typed parse error variant for path provenance)
/// when the file does not start with `P5\n` or has
/// non-integer dimensions.
fn read_pgm_dimensions(path: &Path) -> Result<(u32, u32), BundleError> {
    let bytes = fs::read(path)?;
    let mk_err = |msg: &str| BundleError::Json {
        path: path.to_path_buf(),
        source: serde::de::Error::custom(msg.to_string()),
    };
    // We don't decode the pixels — only the header. The header
    // is ASCII, terminated by whitespace, in the order
    // magic, width, height, maxval.
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() && tokens.len() < 3 {
        // Skip whitespace.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Comment line: skip to end of line.
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Read non-whitespace token.
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        tokens.push(&bytes[start..i]);
    }
    if tokens.len() < 3 {
        return Err(mk_err("truncated PGM header"));
    }
    if tokens[0] != b"P5" && tokens[0] != b"P2" {
        return Err(mk_err("not a PGM (missing P5/P2 magic)"));
    }
    let parse = |t: &[u8]| -> Result<u32, BundleError> {
        let s = std::str::from_utf8(t).map_err(|e| mk_err(&e.to_string()))?;
        s.parse::<u32>().map_err(|e| mk_err(&e.to_string()))
    };
    Ok((parse(tokens[1])?, parse(tokens[2])?))
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
    // Treat both `None` and `Some("")` as "no checksum
    // recorded". The schema (docs/design/debug_bundle_schema.md)
    // says absent = no checksum, but the Android writer
    // regression in e8a7211 shipped `""` instead of `null`,
    // and every such bundle would otherwise abort at replay
    // before the engine runs. Defensive on the verifier side
    // so existing on-disk corpora remain replayable.
    let expected = match manifest.capture.first_frame_blake3.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(()),
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
            build: Some(BuildInfo {
                git_sha: "795b888941e78d9e49602637b5695d2dc3ea1c87".into(),
                git_describe: "v0.0.1-12-g795b888".into(),
                git_dirty: false,
                commit_count: 142,
                build_timestamp_utc: "2026-05-29T23:57:22Z".into(),
                bris_ffi_semver: "0.0.1".into(),
                android_version_name: Some("0.1.0".into()),
                android_version_code: Some(142),
            }),
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
                placeholder: None,
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
            session_id: Some("550e8400-e29b-41d4-a716-446655440000".parse().unwrap()),
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
        m.build = None;
        let s = serde_json::to_string(&m).unwrap();
        // None-valued options should be omitted from the JSON.
        assert!(!s.contains("ap_input"));
        assert!(!s.contains("gps_truth"));
        assert!(!s.contains("\"build\""));
        let back: BundleManifest = serde_json::from_str(&s).unwrap();
        assert!(back.ap_input.is_none());
        assert!(back.gps_truth.is_none());
        assert!(back.build.is_none());
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

    fn full_session() -> SessionManifest {
        let mut s = SessionManifest::new(
            "550e8400-e29b-41d4-a716-446655440000".parse().unwrap(),
            "Window-sill sun sights".into(),
            DeviceInfo {
                model: "TestPhone".into(),
                os: Some("Android 11".into()),
                app_version: Some("0.0.1".into()),
            },
            1_700_000_000_000,
        );
        s.notes = Some("morning sun, partial cloud".into());
        s.kinematics = SessionKinematics::MaxSpeedKn { kn: 5.0 };
        s.sight_retention_seconds = 86_400 * 3;
        s.profile = UseCaseProfile::Marine;
        s.expected_to_fail = false;
        s.ordered_capture_ids.push("cap-0019e7634306b".into());
        s.ordered_capture_ids.push("cap-0019e7634310c".into());
        s
    }

    #[test]
    fn session_round_trips_through_json() {
        let s = full_session();
        let raw = serde_json::to_vec(&s).unwrap();
        let back: SessionManifest = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back.session_id, s.session_id);
        assert_eq!(back.title, s.title);
        assert_eq!(back.ordered_capture_ids, s.ordered_capture_ids);
        assert_eq!(back.kinematics, s.kinematics);
        assert_eq!(back.profile, s.profile);
        assert_eq!(back.sight_retention_seconds, s.sight_retention_seconds);
    }

    #[test]
    fn session_save_load_round_trip() {
        let dir = tempdir().unwrap();
        let s = full_session();
        s.save_to_dir(dir.path()).unwrap();
        let back = SessionManifest::load_from_dir(dir.path()).unwrap();
        assert_eq!(back.session_id, s.session_id);
        assert_eq!(back.ordered_capture_ids, s.ordered_capture_ids);
    }

    #[test]
    fn session_schema_version_mismatch_errors() {
        let dir = tempdir().unwrap();
        let mut s = full_session();
        s.schema_version = 999;
        s.save_to_dir(dir.path()).unwrap();
        let err = SessionManifest::load_from_dir(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            BundleError::UnsupportedSchema { found: 999, .. }
        ));
    }

    #[test]
    fn session_defaults_match_engine_defaults() {
        let s = SessionManifest::new(
            Uuid::nil(),
            "t".into(),
            DeviceInfo {
                model: "m".into(),
                os: None,
                app_version: None,
            },
            0,
        );
        // These mirror EngineConfig defaults (sight_window_seconds = 7200,
        // sight_window_capacity = 50, assumed_max_speed_kn = 0 ↔ Stationary).
        assert_eq!(s.sight_retention_seconds, 7200);
        assert_eq!(s.sight_retention_capacity, 50);
        assert_eq!(s.kinematics, SessionKinematics::Stationary);
        assert_eq!(s.profile, UseCaseProfile::Custom);
        assert!(!s.expected_to_fail);
    }

    #[test]
    fn intrinsics_source_user_calibration_accepts_session_id_alias() {
        // Backward-compat: pre-rename bundles wrote `session_id`.
        let raw = r#"{"kind":"user_calibration","session_id":"abc-123"}"#;
        let parsed: IntrinsicsSource = serde_json::from_str(raw).unwrap();
        match parsed {
            IntrinsicsSource::UserCalibration { calibration_id } => {
                assert_eq!(calibration_id, "abc-123");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn intrinsics_source_user_calibration_serializes_calibration_id() {
        let v = IntrinsicsSource::UserCalibration {
            calibration_id: "abc-123".into(),
        };
        let raw = serde_json::to_string(&v).unwrap();
        assert!(raw.contains("calibration_id"));
        assert!(!raw.contains("session_id"));
    }

    #[test]
    fn intrinsics_record_placeholder_true_round_trips() {
        let mut m = full_manifest();
        m.intrinsics.source = IntrinsicsSource::Placeholder;
        m.intrinsics.placeholder = Some(true);
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"placeholder\":true"));
        let back: BundleManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.intrinsics.placeholder, Some(true));
    }

    #[test]
    fn intrinsics_record_placeholder_none_is_omitted() {
        let mut m = full_manifest();
        m.intrinsics.placeholder = None;
        let s = serde_json::to_string(&m).unwrap();
        // Should not appear at all (skip_serializing_if).
        assert!(
            !s.contains("placeholder"),
            "unexpected placeholder field in JSON: {s}"
        );
        let back: BundleManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.intrinsics.placeholder, None);
    }

    #[test]
    fn sidecar_round_trips_gravity_and_gps_truth() {
        let s = FrameSidecar {
            seq: 0,
            captured_unix_ms: 1_700_000_000_000,
            width: 4032,
            height: 3024,
            exposure_us: Some(8333),
            sensor_gain: Some(2.5),
            diagnostic_snapshot: None,
            gravity_camera_frame: Some([0.0, 1.0, 0.0]),
            gps_truth: Some(GpsTruth {
                lat: 30.1488,
                lon: -97.8432,
                lat_sigma_m: 4.0,
                lon_sigma_m: 4.0,
                altitude_m: None,
                altitude_sigma_m: None,
                captured_unix_ms: 1_700_000_000_000,
                source: "android_gps".into(),
                satellites_used: Some(11),
            }),
        };
        let raw = serde_json::to_vec(&s).unwrap();
        let back: FrameSidecar = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back.gravity_camera_frame, Some([0.0, 1.0, 0.0]));
        let g = back.gps_truth.expect("gps_truth round-trip");
        assert!((g.lat - 30.1488).abs() < 1e-9);
        assert_eq!(g.satellites_used, Some(11));
    }

    #[test]
    fn sidecar_old_schema_loads_without_new_fields() {
        // Pre-Phase-8.5 bundles wrote no gravity / gps_truth.
        let raw = r#"{"seq":0,"captured_unix_ms":1700000000000,"width":4032,"height":3024}"#;
        let s: FrameSidecar = serde_json::from_str(raw).unwrap();
        assert!(s.gravity_camera_frame.is_none());
        assert!(s.gps_truth.is_none());
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
            gravity_camera_frame: None,
            gps_truth: None,
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
    fn checksum_empty_string_treated_as_absent() {
        // Defensive against the Android writer regression in
        // e8a7211 that shipped `Some("")` instead of `None`.
        let dir = tempdir().unwrap();
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("000000000000.pgm"), b"frame-bytes").unwrap();
        write_sidecar(&media.join("000000000000.json"), 0, 100);
        let mut m = full_manifest();
        m.capture.first_frame_blake3 = Some(String::new());
        verify_first_frame_checksum(&m, dir.path()).unwrap();
    }

    #[test]
    fn checksum_nonempty_mismatch_still_errors() {
        // Positive control: a non-empty wrong hash must still
        // produce ChecksumMismatch (i.e. the empty-string
        // carve-out does not over-broaden).
        let dir = tempdir().unwrap();
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("000000000000.pgm"), b"frame-bytes").unwrap();
        write_sidecar(&media.join("000000000000.json"), 0, 100);
        let mut m = full_manifest();
        m.capture.first_frame_blake3 = Some("deadbeef".into());
        let err = verify_first_frame_checksum(&m, dir.path()).unwrap_err();
        assert!(matches!(err, BundleError::ChecksumMismatch { .. }));
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

    #[test]
    fn load_index_jsonl_returns_none_when_absent() {
        let dir = tempdir().unwrap();
        assert!(load_index_jsonl(dir.path()).unwrap().is_none());
    }

    #[test]
    fn load_index_jsonl_parses_real_on_disk_schema() {
        let dir = tempdir().unwrap();
        let body = concat!(
            "{\"seq\":0,\"captured_unix_ms\":1780382794242,\"width\":3024,\"height\":4032,\"pgm_bytes\":12192785,\"json_bytes\":89,\"retention\":\"debug\"}\n",
            "{\"seq\":1,\"captured_unix_ms\":1780382794883,\"width\":3024,\"height\":4032,\"pgm_bytes\":12192785,\"json_bytes\":89,\"retention\":\"debug\"}\n",
            "\n",
            "{\"seq\":2,\"captured_unix_ms\":1780382797092,\"width\":3024,\"height\":4032,\"pgm_bytes\":12192785,\"json_bytes\":89,\"retention\":\"fix_frame\"}\n",
        );
        std::fs::write(dir.path().join("index.jsonl"), body).unwrap();
        let entries = load_index_jsonl(dir.path()).unwrap().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[0].captured_unix_ms, 1_780_382_794_242);
        assert_eq!(entries[0].width, 3024);
        assert_eq!(entries[0].height, 4032);
        assert_eq!(entries[0].pgm_bytes, 12_192_785);
        assert_eq!(entries[0].json_bytes, 89);
        assert_eq!(entries[0].retention, "debug");
        assert_eq!(entries[2].seq, 2);
        assert_eq!(entries[2].retention, "fix_frame");
    }

    fn write_pgm(path: &Path, w: u32, h: u32) {
        // Minimal valid P5 header + width*height bytes of
        // payload. Pixel content is irrelevant; only the
        // header is parsed by the validator.
        let mut buf = format!("P5\n{w} {h}\n255\n").into_bytes();
        buf.extend(std::iter::repeat_n(0u8, (w * h) as usize));
        std::fs::write(path, buf).unwrap();
    }

    #[test]
    fn validate_intrinsics_consistent_returns_no_warnings() {
        let dir = tempdir().unwrap();
        let frames = dir.path().join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        write_pgm(&frames.join("000000000000.pgm"), 3024, 4032);
        write_sidecar(&frames.join("000000000000.json"), 0, 100);
        let mut m = full_manifest();
        m.intrinsics.width = 3024;
        m.intrinsics.height = 4032;
        let warns = validate_intrinsics_against_first_pgm(&m, dir.path()).unwrap();
        assert!(warns.is_empty(), "expected no warnings, got {warns:?}");
    }

    #[test]
    fn validate_intrinsics_mismatch_yields_warning() {
        // Classic landscape-cal-vs-portrait-frame bug: the
        // Android writer shipped sensor-native intrinsics
        // alongside gravity-rotated frame bytes.
        let dir = tempdir().unwrap();
        let frames = dir.path().join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        write_pgm(&frames.join("000000000000.pgm"), 3024, 4032);
        write_sidecar(&frames.join("000000000000.json"), 0, 100);
        let mut m = full_manifest();
        m.intrinsics.width = 4032;
        m.intrinsics.height = 3024;
        let warns = validate_intrinsics_against_first_pgm(&m, dir.path()).unwrap();
        assert_eq!(warns.len(), 1);
        assert_eq!(
            warns[0],
            BundleWarning::IntrinsicsDimensionMismatch {
                intrinsics_width: 4032,
                intrinsics_height: 3024,
                pgm_width: 3024,
                pgm_height: 4032,
            }
        );
    }

    #[test]
    fn validate_intrinsics_no_frames_returns_no_warnings() {
        // An empty bundle (Debug OFF, no fix observed) has
        // nothing to compare against; the validator must
        // not invent a complaint.
        let dir = tempdir().unwrap();
        let frames = dir.path().join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        let m = full_manifest();
        let warns = validate_intrinsics_against_first_pgm(&m, dir.path()).unwrap();
        assert!(warns.is_empty());
    }

    #[test]
    fn index_entry_round_trips_through_jsonl() {
        let entries = vec![
            IndexEntry {
                seq: 0,
                captured_unix_ms: 1_700_000_000_000,
                width: 1920,
                height: 1080,
                pgm_bytes: 4_147_200,
                json_bytes: 128,
                retention: "debug".into(),
            },
            IndexEntry {
                seq: 1,
                captured_unix_ms: 1_700_000_000_100,
                width: 1920,
                height: 1080,
                pgm_bytes: 4_147_200,
                json_bytes: 130,
                retention: "fix_frame".into(),
            },
        ];
        let mut text = String::new();
        for e in &entries {
            text.push_str(&serde_json::to_string(e).unwrap());
            text.push('\n');
        }
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("index.jsonl"), text).unwrap();
        let back = load_index_jsonl(dir.path()).unwrap().unwrap();
        assert_eq!(back, entries);
    }
}
