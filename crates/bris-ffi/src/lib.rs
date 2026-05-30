//! UniFFI bindings layer for Bris.
//!
//! This crate is the **stable, FFI-friendly facade** over the
//! Bris streaming engine, the calibration workflow, and the
//! version surface. Android (Kotlin) and the eventual iOS
//! (Swift) shells consume the bindings generated from the types
//! exported here.
//!
//! # Scope
//!
//! - [`Engine`] wraps [`bris_streaming::StreamingEngine`].
//!   Lifecycle: construct via [`engine_new`], push frames via
//!   [`Engine::push_frame`], poll diagnostics via
//!   [`Engine::snapshot`], subscribe to fixes via
//!   [`Engine::subscribe_fixes`], shut down by dropping the
//!   `Arc<Engine>`.
//! - [`run_calibration`] is a one-shot wrapper around the
//!   [`bris_calibrate`] crate's CLI-equivalent workflow.
//! - [`version`] reports the bound `bris-core` version (the
//!   single source-of-truth version exposed to the operator).
//!
//! # Design constraints (see `docs/design/diagnostic_collection.md`)
//!
//! - Types crossing the FFI are **value types** (owned, no
//!   borrows) unless explicitly `Arc`-shared.
//! - This crate adds **no behavior** beyond what
//!   `bris-streaming`, `bris-calibrate`, and friends already do.
//!   It is a wrapper layer.
//! - `DiagnosticSnapshot` is the contract consumed by the
//!   Android debug overlay *and* serialized into diagnostic
//!   submissions. Single source of truth for "what the engine
//!   currently thinks."
//!
//! # Stage of development
//!
//! Spike-grade scaffold. The public API surface is in place and
//! compiles against `bris-streaming`; the fix-subscription
//! callback wiring and the calibration wrapper are stubs
//! returning a clear error or no-op until the Kotlin side is
//! exercising them.

#![allow(
    // The FFI types intentionally hold `Option`s for fields the
    // engine doesn't always populate (last-classification before
    // any frame has been processed, etc.); the conversions
    // sometimes look like they could be const but aren't because
    // the underlying constructors aren't const.
    clippy::missing_const_for_fn,
    // UniFFI-generated scaffolding has its own warnings posture;
    // suppressing here keeps the crate quiet without affecting
    // the rest of the workspace's lint policy.
    clippy::module_name_repetitions,
    // `bytes_per_pixel * pixel_count` style multiplications are
    // bounded by the prior `checked_mul`; the lint can't see that.
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    // Proper nouns (UniFFI, CameraX, Bris, ONNX) recur in docs;
    // backticking each occurrence harms readability.
    clippy::doc_markdown,
    // The FFI is intentionally take-by-value at the boundary
    // (UniFFI ownership model); references would force foreign-
    // side lifetime management we don't want.
    clippy::needless_pass_by_value,
    // `Engine::subscribe_fixes` is a method on the engine handle
    // even though the current stub does not read `&self`; the
    // wired-up version will.
    clippy::unused_self
)]

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use bris_almanac::Observer;
use bris_core::SensorGain;
use bris_core::{time, Hemisphere, Latitude, Longitude};
use bris_streaming::{
    EngineConfig as CoreEngineConfig, EngineDiagnostics, PublishedFix, PushError, StreamingEngine,
};
use bris_vision::{Frame, Intrinsics, Rotation};

uniffi::setup_scaffolding!();

/// Errors that can be returned across the FFI boundary.
///
/// Kept deliberately coarse: the Kotlin/Swift side renders these
/// as human-readable strings; precise error categorization lives
/// in the core crates' typed errors and is logged via `tracing`.
///
/// The variant payload field is named `detail` rather than
/// `message` to avoid a name clash with `Throwable.message` in
/// the generated Kotlin bindings (UniFFI 0.28 generates an
/// `override val message` whose body collides with a `message`
/// constructor parameter).
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    /// Invalid argument from the foreign caller (out-of-range
    /// number, malformed string, wrong-sized byte buffer, etc.).
    /// `detail` is a human-readable explanation suitable for a
    /// developer log; foreign code should not key behavior off
    /// the contents.
    #[error("invalid argument: {detail}")]
    InvalidArgument {
        /// Human-readable explanation.
        detail: String,
    },

    /// The underlying Rust engine returned a hard error. Should
    /// be rare; the engine's normal "no record produced" outcomes
    /// do not surface as errors at the FFI.
    #[error("engine error: {detail}")]
    Engine {
        /// Engine-side error detail.
        detail: String,
    },
}

/// Build/runtime version information for the bound Rust core.
///
/// Surfaced by the Android settings screen as "core version"
/// and stamped into every diagnostic submission's manifest.
#[derive(Debug, Clone, uniffi::Record)]
pub struct VersionInfo {
    /// Semver of the `bris-ffi` crate (which transitively pins
    /// `bris-core` via `Cargo.lock`).
    pub bris_ffi: String,
    /// Build-time UTC timestamp (ISO 8601) of the FFI shared
    /// object, or `None` if the build did not record it.
    /// Reserved; currently `None`.
    pub build_timestamp_utc: Option<String>,
}

/// Report the bound `bris-ffi` version. Cheap; no engine needed.
#[uniffi::export]
#[must_use]
pub fn version() -> VersionInfo {
    VersionInfo {
        bris_ffi: env!("CARGO_PKG_VERSION").to_owned(),
        build_timestamp_utc: option_env!("BRIS_FFI_BUILD_TIMESTAMP").map(str::to_owned),
    }
}

/// Observer geometry as supplied across the FFI.
///
/// The Rust-side [`Observer`] type carries an atmospheric model
/// and other knobs that nearly all callers leave at defaults;
/// exposing those across the FFI would make the surface noisy
/// without operator benefit. The FFI variant carries the four
/// numbers the operator actually sees in the Android settings;
/// the rest take defaults from [`Observer::default_dev`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiObserver {
    /// Latitude in degrees, north positive, range \[-90, 90\].
    pub latitude_deg: f64,
    /// Longitude in degrees, east positive, range \[-180, 180\].
    pub longitude_deg: f64,
    /// Height of the observer's eye above the sea, in meters.
    /// Default 2.0 (a person standing on a small-boat deck).
    pub eye_height_m: f64,
    /// 1σ uncertainty on `eye_height_m`, in meters. Default 0.5.
    /// Widen in significant seas to inflate horizon-dip σ.
    pub eye_height_sigma_m: f64,
}

impl FfiObserver {
    fn into_core(self) -> Result<Observer, FfiError> {
        let latitude =
            Latitude::from_degrees(self.latitude_deg).map_err(|e| FfiError::InvalidArgument {
                detail: format!("observer.latitude_deg={}: {e:?}", self.latitude_deg),
            })?;
        let longitude =
            Longitude::from_degrees(self.longitude_deg).map_err(|e| FfiError::InvalidArgument {
                detail: format!("observer.longitude_deg={}: {e:?}", self.longitude_deg),
            })?;
        if !self.eye_height_m.is_finite() || self.eye_height_m < 0.0 {
            return Err(FfiError::InvalidArgument {
                detail: format!("observer.eye_height_m={} invalid", self.eye_height_m),
            });
        }
        if !self.eye_height_sigma_m.is_finite() || self.eye_height_sigma_m < 0.0 {
            return Err(FfiError::InvalidArgument {
                detail: format!(
                    "observer.eye_height_sigma_m={} invalid",
                    self.eye_height_sigma_m
                ),
            });
        }
        // Start from the dev default to inherit the atmospheric
        // model, then overwrite operator-facing values.
        let mut obs = Observer::default_dev();
        obs.latitude = latitude;
        obs.longitude = longitude;
        obs.eye_height_m = self.eye_height_m;
        obs.eye_height_sigma_m = self.eye_height_sigma_m;
        Ok(obs)
    }
}

/// FFI-friendly engine configuration.
///
/// Mirrors the operator-meaningful subset of
/// [`bris_streaming::EngineConfig`]. All other knobs take the
/// Rust-side defaults.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiEngineConfig {
    /// Observer geometry. See [`FfiObserver`].
    pub observer: FfiObserver,

    /// Maximum age difference (seconds) between two frames
    /// considered for cross-frame stitching. Default 2.0.
    pub stitching_window_seconds: f64,

    /// Maximum age (seconds) of a sight retained in the active
    /// sight window. Default 600.0.
    pub sight_window_seconds: f64,

    /// Maximum number of sights kept in the active window.
    /// Default 10.
    pub sight_window_capacity: u32,

    /// Minimum interval (milliseconds) between fix publications.
    /// Default 1000.
    pub min_fix_publication_interval_ms: u64,

    /// Capacity of the input ring buffer of raw frames awaiting
    /// processing. Default 120.
    pub input_ring_capacity: u32,

    /// Optional path to an ONNX segmentation model for the
    /// last-resort horizon detector. `None` disables it.
    pub segmentation_model_path: Option<String>,

    /// Optional analysis resolution for Stage C (horizon
    /// detection). When `Some((w, h))`, the engine downsamples
    /// each pushed frame to `(w, h)` before running every
    /// horizon detector. `None` (default) preserves the
    /// historical "every detector sees the source frame"
    /// behavior.
    ///
    /// `(w, h)` must preserve the source frame's aspect ratio
    /// and not exceed the source dimensions; mismatches
    /// degrade to source resolution rather than failing.
    /// See the engine-side
    /// [`bris_streaming::EngineConfig::horizon_analysis_size`]
    /// for the underlying contract.
    pub horizon_analysis_width: Option<u32>,
    /// Companion to `horizon_analysis_width`; both must be set
    /// together. UniFFI doesn't expose tuple types as cleanly
    /// as paired optional scalars, so we expose the two
    /// dimensions as separate fields and combine them
    /// engine-side.
    pub horizon_analysis_height: Option<u32>,
    /// Aspect-ratio-agnostic alternative to the
    /// `horizon_analysis_width` / `horizon_analysis_height`
    /// pair: cap the long edge of the horizon-analysis frame
    /// at this many pixels and let the engine derive the
    /// short edge from the source's actual aspect ratio at
    /// runtime. Preferred when capture resolution varies by
    /// device — phone sensors are commonly 4:3 while
    /// machine-vision sensors are 16:9, and a hard-coded
    /// `(w, h)` pair only matches one of them.
    ///
    /// Mutually exclusive with the `horizon_analysis_width`
    /// / `horizon_analysis_height` pair; setting both forms
    /// returns
    /// [`FfiError::InvalidArgument`]. See
    /// [`bris_streaming::EngineConfig::horizon_analysis_max_long_edge_px`]
    /// for the resolver contract.
    pub horizon_analysis_max_long_edge_px: Option<u32>,

    /// Coarse hemisphere hint for the cold-start CoP solver,
    /// expressed as the case-insensitive string `"N"` (or
    /// `"north"`) / `"S"` (or `"south"`). `None` leaves the
    /// engine default (no hint; two-candidate cold-start
    /// results are not auto-published). Wires through to
    /// [`bris_streaming::ColdStartEngineConfig::coarse_hemisphere`].
    pub cold_start_coarse_hemisphere: Option<String>,
}

impl FfiEngineConfig {
    fn into_core(self) -> Result<CoreEngineConfig, FfiError> {
        let observer = self.observer.into_core()?;
        let mut cfg = CoreEngineConfig::new(observer);
        cfg.stitching_window_seconds = self.stitching_window_seconds;
        cfg.sight_window_seconds = self.sight_window_seconds;
        cfg.sight_window_capacity = self.sight_window_capacity as usize;
        cfg.min_fix_publication_interval_ms = self.min_fix_publication_interval_ms;
        cfg.input_ring_capacity = self.input_ring_capacity as usize;
        cfg.segmentation_model_path = self.segmentation_model_path.map(Into::into);
        cfg.horizon_analysis_size =
            match (self.horizon_analysis_width, self.horizon_analysis_height) {
                (Some(w), Some(h)) => Some((w, h)),
                (None, None) => None,
                _ => {
                    return Err(FfiError::InvalidArgument {
                    detail:
                        "horizon_analysis_width and horizon_analysis_height must be set together"
                            .to_owned(),
                });
                }
            };
        if cfg.horizon_analysis_size.is_some() && self.horizon_analysis_max_long_edge_px.is_some() {
            return Err(FfiError::InvalidArgument {
                detail: "horizon_analysis_width/height and \
                         horizon_analysis_max_long_edge_px are mutually exclusive; \
                         set one form or the other, not both"
                    .to_owned(),
            });
        }
        // Only override the core default when the FFI caller
        // supplied an explicit value. UniFFI scalars default to
        // `null`, so a Kotlin/Swift caller that never touches
        // this field gets `None` here, which we *do* want to
        // surface as "use the core default" rather than as
        // "explicitly disable downsampling". The core default
        // is currently `Some(1280)`, so leaving it alone
        // preserves the sensible behavior.
        if let Some(cap) = self.horizon_analysis_max_long_edge_px {
            cfg.horizon_analysis_max_long_edge_px = Some(cap);
        }
        if let Some(h) = self.cold_start_coarse_hemisphere {
            cfg.cold_start.coarse_hemisphere = Some(parse_hemisphere(&h)?);
        }
        Ok(cfg)
    }
}

/// Lens intrinsics in the FFI form expected by [`Engine::push_frame`].
///
/// Mirrors `bris_vision::Intrinsics`. The operator typically
/// produces these from [`run_calibration`] or loads a persisted
/// TOML on the Kotlin side and supplies the parsed values here.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct FfiIntrinsics {
    /// Focal length in pixels, x axis.
    pub fx: f64,
    /// Focal length in pixels, y axis.
    pub fy: f64,
    /// Principal point x in pixels.
    pub cx: f64,
    /// Principal point y in pixels.
    pub cy: f64,
    /// Brown-Conrady radial distortion k1.
    pub k1: f64,
    /// Brown-Conrady radial distortion k2.
    pub k2: f64,
    /// Brown-Conrady radial distortion k3.
    pub k3: f64,
    /// Brown-Conrady tangential distortion p1.
    pub p1: f64,
    /// Brown-Conrady tangential distortion p2.
    pub p2: f64,
}

impl FfiIntrinsics {
    fn into_core(self) -> Result<Intrinsics, FfiError> {
        for (name, v) in [
            ("fx", self.fx),
            ("fy", self.fy),
            ("cx", self.cx),
            ("cy", self.cy),
            ("k1", self.k1),
            ("k2", self.k2),
            ("k3", self.k3),
            ("p1", self.p1),
            ("p2", self.p2),
        ] {
            if !v.is_finite() {
                return Err(FfiError::InvalidArgument {
                    detail: format!("intrinsics.{name}={v} is not finite"),
                });
            }
        }
        if self.fx <= 0.0 || self.fy <= 0.0 {
            return Err(FfiError::InvalidArgument {
                detail: format!("intrinsics: fx={} fy={} must be positive", self.fx, self.fy),
            });
        }
        Ok(Intrinsics {
            fx: self.fx,
            fy: self.fy,
            cx: self.cx,
            cy: self.cy,
            k1: self.k1,
            k2: self.k2,
            k3: self.k3,
            p1: self.p1,
            p2: self.p2,
        })
    }
}

/// Pixel format hint accompanying a pushed frame.
///
/// The streaming engine internally needs `u16` grayscale; the
/// Android side typically delivers 8-bit Y from CameraX. The
/// FFI widens 8-bit to 16-bit on the way in.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum FfiPixelFormat {
    /// 8-bit single-channel luminance (Y plane from YUV).
    Gray8,
    /// 16-bit single-channel luminance, native pipeline format,
    /// little-endian byte order.
    Gray16Le,
}

/// One frame pushed across the FFI.
///
/// The pixel buffer is owned by the foreign caller and copied
/// into Rust ownership at the FFI boundary (UniFFI `bytes`
/// semantic). The foreign caller may free its buffer
/// immediately after `push_frame` returns.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiFrame {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Pixel format of `pixels`.
    pub format: FfiPixelFormat,
    /// Pixel bytes. Length must equal
    /// `width * height * bytes_per_pixel(format)`.
    pub pixels: Vec<u8>,
    /// Capture timestamp, milliseconds since Unix epoch (UTC).
    /// Use milliseconds (not seconds) because Android's camera
    /// timestamp APIs return integer ms and converting back
    /// through `f64` loses sub-millisecond precision we don't
    /// need anyway.
    pub captured_unix_ms: i64,
    /// Exposure duration in microseconds. Use 0 if unknown;
    /// the engine treats 0 as "no motion-blur σ contribution
    /// from exposure" rather than erroring.
    pub exposure_us: u32,
    /// Camera intrinsics under which this frame was captured.
    pub intrinsics: FfiIntrinsics,
    /// Sensor analog conversion gain (electrons per ADU)
    /// under which the pixel intensities were quantized.
    /// The Android side derives this from the per-frame
    /// `CaptureResult.SENSOR_SENSITIVITY` (ISO) scaled by
    /// the per-camera factory profile (see
    /// `FactoryCalibration`). Pass `0.0` or `NaN` when no
    /// measured value is available; the FFI substitutes
    /// [`bris_core::SensorGain::UNITY`] (1.0 e⁻/ADU)
    /// silently, and the centroid refinement degrades to
    /// its pre-plumbing behaviour.
    pub gain_e_per_adu: f64,
}

/// One stage's processing counts, for diagnostic display.
///
/// Mirrors [`bris_streaming::PipelineStageStats`] with the
/// stage name carried as a stable string label.
#[derive(Debug, Clone, uniffi::Record)]
pub struct StageStats {
    /// Stable stage label: `"classifier"`, `"body"`, `"horizon"`,
    /// `"plate-solve"`, or `"sight-assembly"`.
    pub name: String,
    /// Number of frames that entered this stage.
    pub entered: u64,
    /// Number of frames that produced one or more records.
    pub produced: u64,
    /// Number of frames where this stage erred.
    pub failed: u64,
    /// Number of frames where this stage was skipped under
    /// early-rejection.
    pub skipped: u64,
}

/// Engine state snapshot.
///
/// FFI re-shape of [`EngineDiagnostics`]. Cheap to acquire;
/// the engine holds the underlying state behind a mutex.
/// Consumed by the Android debug overlay and serialized into
/// diagnostic submissions.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DiagnosticSnapshot {
    /// Total frames pushed.
    pub frames_pushed: u64,
    /// Frames dropped at the input ring (backpressure).
    pub frames_dropped: u64,
    /// Per-stage counts in stage order.
    pub stages: Vec<StageStats>,
    /// Number of body detection records currently queued.
    pub body_queue_depth: u32,
    /// Number of horizon detection records currently queued.
    pub horizon_queue_depth: u32,
    /// Number of raw frames currently in the ring buffer.
    pub ring_buffer_depth: u32,
    /// Number of sights in the active sight window.
    pub sight_window_depth: u32,
    /// Most recent **raw** classifier verdict as a stable
    /// label, or `None` until the first frame is processed.
    /// This is the per-frame opinion *before* hysteresis;
    /// see [`Self::last_dispatched_condition`] for what the
    /// engine actually ran detectors on.
    pub last_raw_classification: Option<String>,
    /// Most recent **dispatched** condition the engine used
    /// to pick detector families. `None` until the first
    /// frame. This is the operator-facing "what is the engine
    /// doing right now" field; prefer it over
    /// [`Self::last_raw_classification`] in UI surfaces.
    pub last_dispatched_condition: Option<String>,
    /// TT Julian Date of the most recent processed frame, or
    /// `None`.
    pub last_processed_frame_tt_jd: Option<f64>,
    /// TT Julian Date of the most recent published fix, or
    /// `None`.
    pub last_published_fix_tt_jd: Option<f64>,
    /// Width of the resolution Stage C ran at on the most
    /// recent processed frame. `None` until the first frame.
    /// Equals the source frame's width unless an analysis
    /// resolution was configured and the pyramid level
    /// successfully delivered it.
    pub last_horizon_analysis_width: Option<u32>,
    /// Companion to `last_horizon_analysis_width`.
    pub last_horizon_analysis_height: Option<u32>,

    /// Provenance of the horizon emitted on the most recent
    /// processed frame, as a short stable label. Formatted by
    /// the FFI from [`EngineDiagnostics::last_horizon_provenance`]:
    ///
    /// - `"Optical:Gradient"` / `"Optical:SkyRegion"` /
    ///   `"Optical:Night"` / `"Optical:NightTextured"` /
    ///   `"Optical:Segmentation"` — classical detectors.
    /// - `"ReflectionPair(n)"` / `"ReflectionPair(n,prior)"`
    ///   — auto-detected reflection-pair, `n` surviving pairs;
    ///   `,prior` suffix when the position-prior catalog test
    ///   was applied.
    ///
    /// Future provenance variants fall back to `"{:?}"` debug
    /// formatting so adding a provider does not break this
    /// surface.
    ///
    /// `None` until the first frame produces a horizon, or
    /// when the most recent frame produced none.
    pub last_horizon_provenance: Option<String>,
    /// `altitude_sigma` of the horizon emitted on the most
    /// recent processed frame, in arcminutes (1σ). Companion
    /// to [`Self::last_horizon_provenance`]; `None` when no
    /// horizon was emitted on the most recent frame.
    pub last_horizon_altitude_sigma_arcmin: Option<f64>,
}

impl From<&EngineDiagnostics> for DiagnosticSnapshot {
    fn from(d: &EngineDiagnostics) -> Self {
        const NAMES: [&str; 5] = [
            "classifier",
            "body",
            "horizon",
            "plate-solve",
            "sight-assembly",
        ];
        let stages = d
            .stages
            .iter()
            .zip(NAMES.iter())
            .map(|(s, name)| StageStats {
                name: (*name).to_owned(),
                entered: s.entered,
                produced: s.produced,
                failed: s.failed,
                skipped: s.skipped,
            })
            .collect();
        let last_raw_classification = d
            .last_raw_classification
            .map(|c| format!("{c:?}").to_lowercase());
        let last_dispatched_condition = d
            .last_dispatched_condition
            .map(|c| format!("{c:?}").to_lowercase());
        Self {
            frames_pushed: d.frames_pushed,
            frames_dropped: d.frames_dropped,
            stages,
            body_queue_depth: u32::try_from(d.body_queue_depth).unwrap_or(u32::MAX),
            horizon_queue_depth: u32::try_from(d.horizon_queue_depth).unwrap_or(u32::MAX),
            ring_buffer_depth: u32::try_from(d.ring_buffer_depth).unwrap_or(u32::MAX),
            sight_window_depth: u32::try_from(d.sight_window_depth).unwrap_or(u32::MAX),
            last_raw_classification,
            last_dispatched_condition,
            last_processed_frame_tt_jd: d
                .last_processed_frame_tt
                .map(bris_core::time::Tt::julian_date),
            last_published_fix_tt_jd: d
                .last_published_fix_tt
                .map(bris_core::time::Tt::julian_date),
            last_horizon_analysis_width: d.last_horizon_analysis_size.map(|(w, _)| w),
            last_horizon_analysis_height: d.last_horizon_analysis_size.map(|(_, h)| h),
            last_horizon_provenance: d.last_horizon_provenance.map(format_horizon_provenance),
            last_horizon_altitude_sigma_arcmin: d
                .last_horizon_altitude_sigma_rad
                .map(|s| s.to_degrees() * 60.0),
        }
    }
}

/// Format a [`HorizonProvenance`] into a short stable label
/// for HUD display. Documented variants are matched
/// exhaustively; any future variant falls back to `"{:?}"`
/// debug formatting so adding a provider does not gate a
/// release on this FFI shim.
fn parse_hemisphere(raw: &str) -> Result<Hemisphere, FfiError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "n" | "north" => Ok(Hemisphere::North),
        "s" | "south" => Ok(Hemisphere::South),
        other => Err(FfiError::InvalidArgument {
            detail: format!(
                "cold_start_coarse_hemisphere={other:?} invalid; expected N/S/north/south"
            ),
        }),
    }
}

fn format_horizon_provenance(p: bris_vision::HorizonProvenance) -> String {
    use bris_vision::{HorizonProvenance as HP, OpticalKind};
    #[allow(unreachable_patterns, clippy::match_wildcard_for_single_variants)]
    match p {
        HP::Optical(OpticalKind::Gradient) => "Optical:Gradient".to_owned(),
        HP::Optical(OpticalKind::SkyRegion) => "Optical:SkyRegion".to_owned(),
        HP::Optical(OpticalKind::Night) => "Optical:Night".to_owned(),
        HP::Optical(OpticalKind::NightTextured) => "Optical:NightTextured".to_owned(),
        HP::Optical(OpticalKind::Segmentation) => "Optical:Segmentation".to_owned(),
        HP::ReflectionPair {
            pair_count,
            used_position_prior,
        } => {
            if used_position_prior {
                format!("ReflectionPair({pair_count},prior)")
            } else {
                format!("ReflectionPair({pair_count})")
            }
        }
        HP::VerticalLine { line_count } => format!("VerticalLine({line_count})"),
        HP::VanishingPoint {
            vp_count,
            used_vertical,
        } => {
            if used_vertical {
                format!("VanishingPoint({vp_count},vert)")
            } else {
                format!("VanishingPoint({vp_count})")
            }
        }
        HP::Fused { cluster_size } => format!("Fused({cluster_size})"),
        // Catch-all for future provenance variants. Falls
        // back to debug formatting so the HUD continues to
        // render something useful without coordinating an
        // FFI bump.
        other => format!("{other:?}"),
    }
}

/// A published fix as it crosses the FFI.
///
/// FFI re-shape of [`bris_streaming::PublishedFix`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiPublishedFix {
    /// Latitude in degrees.
    pub latitude_deg: f64,
    /// Longitude in degrees.
    pub longitude_deg: f64,
    /// Uncertainty ellipse semi-major axis in nautical miles
    /// (1σ).
    pub sigma_major_nm: f64,
    /// Uncertainty ellipse semi-minor axis in nautical miles.
    pub sigma_minor_nm: f64,
    /// Orientation of the semi-major axis, radians clockwise
    /// from north.
    pub orientation_rad: f64,
    /// Number of sights contributing to this fix.
    pub n_sights: u32,
    /// Spread between max and min azimuth across contributing
    /// sights, in radians.
    pub azimuth_spread_rad: f64,
    /// Age of the oldest contributing sight, in seconds.
    pub oldest_sight_age_seconds: f64,
    /// Dominant per-sight σ source as a stable label.
    pub dominant_source: String,
    /// TT Julian Date of the most recent contributing sight.
    pub timestamp_tt_jd: f64,
    /// Engine-assigned IDs of every frame that contributed to
    /// this fix. Foreign callers (the Android session-recorder)
    /// pass each ID to [`Engine::frame_by_id`] to retrieve the
    /// pixel bytes that produced the fix, then write them
    /// alongside the manifest into a sight-log entry.
    ///
    /// Frames live in the engine's ring buffer only as long as
    /// some sight in the active window references them; copy
    /// promptly after receiving the fix.
    pub contributing_frame_ids: Vec<u64>,
    /// Which solver produced this fix. Stable string label:
    /// `"saint_hilaire"`, `"cold_start"`, or
    /// `"cold_start_ambiguous"`. See `bris_streaming::
    /// FixProvenance` for semantics. Surfaced so operator UIs
    /// can advise that a cold-start fix is not yet AP-anchored.
    pub provenance: String,
}

/// Foreign callback invoked once per published fix.
///
/// Kotlin: implemented as a class wrapping a coroutine channel
/// send. Swift: a closure wrapping a Combine subject. The
/// callback runs on a UniFFI-managed thread; it must not block
/// for long.
#[uniffi::export(with_foreign)]
pub trait FixSubscriber: Send + Sync {
    /// Called once per published fix in publication order.
    fn on_fix(&self, fix: FfiPublishedFix);

    /// Called once when the subscription ends (engine dropped
    /// or explicit cancellation). After this, `on_fix` will
    /// not be called again.
    fn on_closed(&self);
}

/// Engine handle.
///
/// Construct via [`engine_new`]. Multiple foreign references
/// share one engine via `Arc`. Dropping the last reference
/// stops the engine and notifies every subscriber via
/// [`FixSubscriber::on_closed`].
#[derive(uniffi::Object)]
pub struct Engine {
    inner: Arc<StreamingEngine>,
    /// Active foreign subscribers. Each receives every fix
    /// published from the moment of subscription forward; no
    /// backfill of past fixes. Mutex guards the registration
    /// list, not the callback invocations themselves (those run
    /// outside the lock so a slow subscriber doesn't block
    /// registration).
    subscribers: Arc<Mutex<Vec<Arc<dyn FixSubscriber>>>>,
    /// `JoinHandle` for the fix-pump thread. Held so dropping
    /// the `Engine` joins it cleanly. The thread observes
    /// engine drop indirectly via the closed `FixReceiver`
    /// channel.
    pump: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("subscribers", &"<dyn FixSubscriber>")
            .field("pump", &"<JoinHandle>")
            .finish_non_exhaustive()
    }
}

#[uniffi::export]
impl Engine {
    /// Push a captured frame for processing.
    ///
    /// Non-blocking. If the engine's input ring buffer is full,
    /// the frame is dropped silently (counted in
    /// [`DiagnosticSnapshot::frames_dropped`]).
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] for malformed
    /// inputs (size mismatch, non-finite intrinsics, etc.).
    /// Returns [`FfiError::Engine`] for downstream engine
    /// errors.
    pub fn push_frame(&self, frame: FfiFrame) -> Result<(), FfiError> {
        let core_frame = convert_frame(frame)?;
        self.inner
            .push_frame(core_frame)
            .map_err(|e: PushError| FfiError::Engine {
                detail: format!("push_frame: {e:?}"),
            })?;
        Ok(())
    }

    /// Cheap-to-call diagnostic snapshot.
    ///
    /// Returns the engine's observable state at the moment of
    /// the call. Safe to poll at UI cadence (every 100-250 ms).
    pub fn snapshot(&self) -> DiagnosticSnapshot {
        DiagnosticSnapshot::from(&self.inner.diagnostics())
    }

    /// Subscribe to fix publications.
    ///
    /// The subscriber's [`FixSubscriber::on_fix`] is invoked
    /// once per published fix from the moment of subscription
    /// forward (no backfill of fixes published before the
    /// subscription). [`FixSubscriber::on_closed`] is invoked
    /// exactly once when the engine is dropped.
    ///
    /// Multiple subscribers are allowed; each receives an
    /// independent stream of every fix published after its
    /// subscription begins.
    pub fn subscribe_fixes(&self, subscriber: Arc<dyn FixSubscriber>) {
        let mut subs = self.subscribers.lock().expect("subscribers mutex poisoned");
        subs.push(subscriber);
    }

    /// Look up a frame in the engine's ring buffer by its
    /// engine-assigned ID.
    ///
    /// Returns `None` when the frame has been evicted (no
    /// record currently in the body or horizon queue references
    /// it AND no sight in the active sight window references
    /// it). Foreign callers must invoke this *promptly* after a
    /// fix publishes, while its
    /// [`FfiPublishedFix::contributing_frame_ids`] are still
    /// alive in the ring; once the sight window ages past
    /// those frames they are gone.
    ///
    /// The returned [`FfiFrame`] is a deep copy of the engine's
    /// internal frame: 16-bit grayscale pixels widened across
    /// the FFI boundary as little-endian bytes, exactly the
    /// inverse of the [`Engine::push_frame`] format mapping for
    /// [`FfiPixelFormat::Gray16Le`]. Foreign callers wanting
    /// to persist these frames as PGM (the regression-test
    /// format) should down-shift each `u16` to `u8` and write
    /// a P5 header.
    #[must_use]
    pub fn frame_by_id(&self, id: u64) -> Option<FfiFrame> {
        self.inner.frame_by_id(id).map(frame_to_ffi)
    }

    /// Sights currently in the operational pool. In-memory
    /// only; cheap.
    pub fn pool_sights(&self) -> Vec<FfiSight> {
        self.inner
            .pool_sights()
            .into_iter()
            .map(pool_sight_to_ffi)
            .collect()
    }

    /// Most-recent N sights from the on-disk store (current.log
    /// + archive). Reads from disk.
    ///
    /// # Errors
    /// Returns [`FfiError::Engine`] on I/O failure.
    pub fn recent_sights(&self, n: u32) -> Result<Vec<FfiSight>, FfiError> {
        let store = self.inner.store();
        let recent = store
            .recent_sights_public(n as usize)
            .map_err(|e| FfiError::Engine {
                detail: format!("recent_sights: {e}"),
            })?;
        Ok(recent.into_iter().map(pool_sight_to_ffi).collect())
    }

    /// Most-recent persisted fix on disk. Returns `None` when
    /// no fix has been persisted yet.
    ///
    /// # Errors
    /// Returns [`FfiError::Engine`] on I/O failure.
    pub fn last_persisted_fix(&self) -> Result<Option<FfiPublishedFix>, FfiError> {
        let store = self.inner.store();
        #[allow(clippy::cast_precision_loss)]
        let now = bris_core::time::Tt::from_julian_date(
            chrono::Utc::now().timestamp() as f64 / 86_400.0 + 2_440_587.5,
        );
        let fix = store
            .last_persisted_fix_public(now, f64::INFINITY)
            .map_err(|e| FfiError::Engine {
                detail: format!("last_persisted_fix: {e}"),
            })?;
        Ok(fix.as_ref().map(published_fix_to_ffi))
    }
}

/// One sight as it crosses the FFI. Mirrors the engine's
/// internal `Sight` fields.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct FfiSight {
    /// Body class: 0 = SolarSystem, 1 = Star.
    pub body_kind: u8,
    /// Body payload: solar discriminant or HR id.
    pub body_payload: u32,
    /// Body azimuth at the assumed observer, radians.
    pub azimuth_rad: f64,
    /// Per-sight altitude σ, radians.
    pub altitude_sigma_rad: f64,
    /// Intercept in nautical miles.
    pub intercept_nm: f64,
    /// 1σ intercept uncertainty in nautical miles.
    pub intercept_sigma_nm: f64,
    /// Anchor time, Julian Date (TT).
    pub anchor_tt_jd: f64,
    /// Source frame id (engine-assigned), or `u64::MAX` when
    /// the sight was hydrated from disk and the originating
    /// frame is no longer in the ring buffer.
    pub source_frame_id: u64,
}

fn pool_sight_to_ffi(s: bris_streaming::PoolSight) -> FfiSight {
    FfiSight {
        body_kind: s.body_kind,
        body_payload: s.body_payload,
        azimuth_rad: s.azimuth_rad,
        altitude_sigma_rad: s.altitude_sigma_rad,
        intercept_nm: s.intercept_nm,
        intercept_sigma_nm: s.intercept_sigma_nm,
        anchor_tt_jd: s.anchor_tt_jd,
        source_frame_id: s.source_frame_id,
    }
}

/// Convert a [`bris_vision::Frame`] back to an [`FfiFrame`].
///
/// Pixels are encoded as little-endian `u16` bytes
/// ([`FfiPixelFormat::Gray16Le`]) so no precision is lost
/// across the boundary. The intrinsics, capture time, and
/// exposure are mirrored verbatim.
///
/// Used by [`Engine::frame_by_id`] to surface a
/// `bris_streaming::StreamingEngine`-owned frame to the
/// foreign caller. Symmetric with `convert_frame` (the
/// FfiFrame → Frame direction).
fn frame_to_ffi(frame: Frame) -> FfiFrame {
    // Convert TT JD → Unix milliseconds via the same
    // approximation `format_pbris` uses (TT − UTC ≈ 69.184 s).
    // For frames recently pushed to the engine via the FFI
    // (Unix-ms in, TT out) this round-trips to within a few
    // ms.
    const TT_MINUS_UTC_APPROX_SECS: f64 = 69.184;
    let pixels_u16 = frame.pixels();
    let mut pixels = Vec::with_capacity(pixels_u16.len() * 2);
    for px in pixels_u16 {
        pixels.extend_from_slice(&px.to_le_bytes());
    }
    let intr = frame.intrinsics;
    let utc_jd = frame.capture_tt.julian_date() - TT_MINUS_UTC_APPROX_SECS / 86_400.0;
    let utc_unix_s = (utc_jd - 2_440_587.5) * 86_400.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let captured_unix_ms = (utc_unix_s * 1000.0) as i64;
    FfiFrame {
        width: frame.width(),
        height: frame.height(),
        format: FfiPixelFormat::Gray16Le,
        pixels,
        captured_unix_ms,
        exposure_us: frame.exposure_us,
        intrinsics: FfiIntrinsics {
            fx: intr.fx,
            fy: intr.fy,
            cx: intr.cx,
            cy: intr.cy,
            k1: intr.k1,
            k2: intr.k2,
            k3: intr.k3,
            p1: intr.p1,
            p2: intr.p2,
        },
        gain_e_per_adu: frame.gain.e_per_adu(),
    }
}

/// Construct a new engine.
///
/// Spawns a background thread that pumps published fixes from
/// the engine's `fix_stream` to every registered foreign
/// [`FixSubscriber`]. The thread exits when the engine is
/// dropped.
///
/// # Errors
///
/// Returns [`FfiError::InvalidArgument`] for invalid config.
#[uniffi::export]
pub fn engine_new(config: FfiEngineConfig) -> Result<Arc<Engine>, FfiError> {
    let core_cfg = config.into_core()?;
    let inner = Arc::new(StreamingEngine::new(core_cfg));
    let receiver = inner.fix_stream().map_err(|e| FfiError::Engine {
        detail: format!("engine_new: fix_stream: {e:?}"),
    })?;

    let subscribers: Arc<Mutex<Vec<Arc<dyn FixSubscriber>>>> = Arc::new(Mutex::new(Vec::new()));
    let pump_subs = Arc::clone(&subscribers);
    let pump = std::thread::Builder::new()
        .name("bris-ffi-fix-pump".to_owned())
        .spawn(move || {
            // Block on each fix; exit when the channel closes
            // (engine dropped).
            while let Some(fix) = receiver.recv() {
                let snapshot: Vec<Arc<dyn FixSubscriber>> = {
                    let guard = pump_subs.lock().expect("subscribers mutex poisoned");
                    guard.clone()
                };
                let payload = published_fix_to_ffi(&fix);
                for s in snapshot {
                    s.on_fix(payload.clone());
                }
            }
            // Channel closed: notify all subscribers exactly
            // once.
            let final_subs: Vec<Arc<dyn FixSubscriber>> = {
                let mut guard = pump_subs.lock().expect("subscribers mutex poisoned");
                std::mem::take(&mut *guard)
            };
            for s in final_subs {
                s.on_closed();
            }
        })
        .map_err(|e| FfiError::Engine {
            detail: format!("engine_new: spawn pump thread: {e}"),
        })?;

    Ok(Arc::new(Engine {
        inner,
        subscribers,
        pump: Mutex::new(Some(pump)),
    }))
}

impl Drop for Engine {
    fn drop(&mut self) {
        // The pump thread exits when the fix channel closes,
        // which happens when the underlying StreamingEngine is
        // dropped. Our `inner: Arc<StreamingEngine>` keeps it
        // alive while we hold a reference; releasing it here
        // (implicit on field drop) signals the pump.
        //
        // The pump thread also cleans up subscribers (calling
        // on_closed) before exiting, so we don't duplicate that
        // here.
        let handle = self.pump.lock().expect("pump mutex poisoned").take();
        if let Some(h) = handle {
            // Drop our Arc<StreamingEngine> first so the channel
            // closes and the pump exits.
            // (Field drop order is declaration order; this
            // explicit join races safely against drop because
            // the pump exits as soon as the channel sees the
            // last sender drop.)
            let _ = h.join();
        }
    }
}

fn published_fix_to_ffi(p: &PublishedFix) -> FfiPublishedFix {
    FfiPublishedFix {
        latitude_deg: p.fix.lat.degrees(),
        longitude_deg: p.fix.lon.degrees(),
        sigma_major_nm: p.fix.sigma_major_nm,
        sigma_minor_nm: p.fix.sigma_minor_nm,
        orientation_rad: p.fix.orientation_rad,
        n_sights: u32::try_from(p.n_sights).unwrap_or(u32::MAX),
        azimuth_spread_rad: p.azimuth_spread_rad,
        oldest_sight_age_seconds: p.oldest_sight_age_seconds,
        dominant_source: p.dominant_source.label().to_owned(),
        timestamp_tt_jd: p.timestamp.julian_date(),
        contributing_frame_ids: p.contributing_frame_ids.clone(),
        provenance: p.provenance.label().to_owned(),
    }
}

/// Convert an [`FfiFrame`] into a `bris_vision::Frame`.
fn convert_frame(frame: FfiFrame) -> Result<Frame, FfiError> {
    let w = frame.width;
    let h = frame.height;
    if w == 0 || h == 0 {
        return Err(FfiError::InvalidArgument {
            detail: format!("frame: width={w}, height={h} must be positive"),
        });
    }
    let expected_pixels =
        (w as usize)
            .checked_mul(h as usize)
            .ok_or_else(|| FfiError::InvalidArgument {
                detail: format!("frame: width={w}*height={h} overflows"),
            })?;
    let bpp = match frame.format {
        FfiPixelFormat::Gray8 => 1usize,
        FfiPixelFormat::Gray16Le => 2usize,
    };
    let expected_bytes =
        expected_pixels
            .checked_mul(bpp)
            .ok_or_else(|| FfiError::InvalidArgument {
                detail: "frame: pixel_count * bpp overflows".to_owned(),
            })?;
    if frame.pixels.len() != expected_bytes {
        return Err(FfiError::InvalidArgument {
            detail: format!(
                "frame: pixels.len()={} != width*height*bpp={}",
                frame.pixels.len(),
                expected_bytes
            ),
        });
    }

    // Widen / unpack into u16 pipeline format.
    let pixels_u16: Vec<u16> = match frame.format {
        FfiPixelFormat::Gray8 => frame
            .pixels
            .iter()
            .map(|&b| (u16::from(b) << 8) | u16::from(b))
            .collect(),
        FfiPixelFormat::Gray16Le => frame
            .pixels
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect(),
    };

    let intrinsics = frame.intrinsics.into_core()?;
    let capture_tt = unix_ms_to_tt(frame.captured_unix_ms)?;

    let mut f =
        Frame::new(w, h, pixels_u16, capture_tt, frame.exposure_us, intrinsics).map_err(|e| {
            FfiError::InvalidArgument {
                detail: format!("frame: {e:?}"),
            }
        })?;
    f.source_rotation = Rotation::Deg0;
    f.gain = SensorGain::new(frame.gain_e_per_adu);
    Ok(f)
}

/// Convert Unix milliseconds (UTC) into [`bris_core::time::Tt`].
fn unix_ms_to_tt(ms: i64) -> Result<bris_core::time::Tt, FfiError> {
    use chrono::TimeZone;
    let secs = ms.div_euclid(1000);
    let nanos = (ms.rem_euclid(1000) * 1_000_000) as u32;
    let utc = chrono::Utc
        .timestamp_opt(secs, nanos)
        .single()
        .ok_or_else(|| FfiError::InvalidArgument {
            detail: format!("captured_unix_ms={ms}: out of range for chrono::DateTime"),
        })?;
    time::utc_to_tt(utc).map_err(|e| FfiError::InvalidArgument {
        detail: format!("captured_unix_ms={ms}: {e:?}"),
    })
}

/// Format a published fix as the `$PBRIS,FIX` sentence.
///
/// Returns a single-element list today; reserved as a list so
/// future engine-level diagnostics that ride on additional
/// `$PBRIS` subtypes (UNC / TIME / SIGHT / ERR) can be appended
/// without changing the FFI signature. Consumers (the Android
/// debug-capture buffer; the future on-screen NMEA preview)
/// concatenate with `\n` to produce the rolling log.
///
/// The sentence's UTC timestamp comes from the fix's TT
/// timestamp via the embedded leap-second table. Sentences
/// produced from the same fix are stable byte-for-byte across
/// calls; the formatter has no hidden state.
/// Scale intrinsics from the resolution they were calibrated
/// against to a target runtime resolution.
///
/// Convenience wrapper around `bris_vision::Intrinsics::scaled_to`
/// surfaced over the FFI so the foreign side can derive
/// per-resolution intrinsics from a single calibrated set
/// without reimplementing the math.
///
/// Behaviour: see `bris_vision::Intrinsics::scaled_to`. Same
/// aspect-ratio constraint, same dimensionless-distortion
/// invariant, same caveats about ISP-side warps invalidating
/// the assumption.
///
/// # Errors
///
/// Returns [`FfiError::InvalidArgument`] for zero dimensions
/// or aspect-ratio mismatch. The message includes both
/// resolutions so the operator can see what was attempted.
#[uniffi::export]
pub fn scale_intrinsics(
    intrinsics: FfiIntrinsics,
    from_width: u32,
    from_height: u32,
    to_width: u32,
    to_height: u32,
) -> Result<FfiIntrinsics, FfiError> {
    let core = intrinsics.into_core()?;
    let scaled = core
        .scaled_to(from_width, from_height, to_width, to_height)
        .map_err(|e| FfiError::InvalidArgument {
            detail: format!("scale_intrinsics: {e}"),
        })?;
    Ok(FfiIntrinsics {
        fx: scaled.fx,
        fy: scaled.fy,
        cx: scaled.cx,
        cy: scaled.cy,
        k1: scaled.k1,
        k2: scaled.k2,
        k3: scaled.k3,
        p1: scaled.p1,
        p2: scaled.p2,
    })
}

/// Format a published fix as the `$PBRIS,FIX` sentence.
///
/// Returns a single-element list today; reserved as a list so
/// future engine-level diagnostics that ride on additional
/// `$PBRIS` subtypes (UNC / TIME / SIGHT / ERR) can be appended
/// without changing the FFI signature. Consumers (the Android
/// debug-capture buffer; the future on-screen NMEA preview)
/// concatenate with `\n` to produce the rolling log.
///
/// The sentence's UTC timestamp comes from the fix's TT
/// timestamp via the embedded leap-second table. Sentences
/// produced from the same fix are stable byte-for-byte across
/// calls; the formatter has no hidden state.
#[uniffi::export]
#[must_use]
pub fn format_pbris(fix: FfiPublishedFix) -> Vec<String> {
    use chrono::TimeZone;

    // Convert TT JD → approximate UTC. The conversion in the
    // other direction (UTC → TT) lives in `bris_core::time`;
    // for the diagnostic-capture path the inverse approximation
    // is good enough — `$PBRIS,FIX`'s timestamp is human-
    // readable, not load-bearing in fix math. The 32.184 s
    // TT − TAI plus the current TAI − UTC offset (37 s as of
    // 2024) total ≈ 69 s; we subtract that constant.
    //
    // A precise inverse (binary-search the leap table) is a
    // small follow-up; tracked.
    const TT_MINUS_UTC_APPROX_SECS: f64 = 69.184;
    let utc_jd = fix.timestamp_tt_jd - TT_MINUS_UTC_APPROX_SECS / 86_400.0;
    let utc_unix_s = (utc_jd - 2_440_587.5) * 86_400.0;
    #[allow(clippy::cast_possible_truncation)]
    let secs = utc_unix_s as i64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let nanos = ((utc_unix_s.fract().abs() * 1e9) as u32).min(999_999_999);
    let utc = chrono::Utc
        .timestamp_opt(secs, nanos)
        .single()
        .unwrap_or_else(chrono::Utc::now);

    // The `FixSummary::dominant_source` is `&'static str`. We
    // map the FFI label string back to the canonical static.
    // Unknown labels fall through to "none".
    let dominant_static: &'static str = match fix.dominant_source.as_str() {
        "centroid" => "centroid",
        "horizon" => "horizon",
        "calibration" => "calibration",
        "stitching" => "stitching",
        "refraction" => "refraction",
        "dip" => "dip",
        "timing" => "timing",
        _ => "none",
    };
    let summary = bris_nmea::FixSummary {
        n_sights: fix.n_sights,
        azimuth_spread_rad: fix.azimuth_spread_rad,
        oldest_sight_age_s: u32::try_from(fix.oldest_sight_age_seconds.max(0.0) as i64)
            .unwrap_or(u32::MAX),
        dominant_source: dominant_static,
    };
    vec![bris_nmea::pbris_fix(utc, &summary)]
}

/// Severity of a single diagnostic finding (or the overall
/// diagnosis), mirroring [`bris_calibrate::DiagnosisLevel`].
///
/// Foreign code uses this to drive the post-solve UI's
/// colour scheme (green / amber / red) without having to
/// parse free-text messages.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum FfiDiagnosisLevel {
    /// Healthy; no concern.
    Ok,
    /// Usable but worth a closer look.
    Warn,
    /// Calibration is unlikely to be trustworthy.
    Error,
}

impl From<bris_calibrate::DiagnosisLevel> for FfiDiagnosisLevel {
    fn from(level: bris_calibrate::DiagnosisLevel) -> Self {
        match level {
            bris_calibrate::DiagnosisLevel::Ok => Self::Ok,
            bris_calibrate::DiagnosisLevel::Warn => Self::Warn,
            bris_calibrate::DiagnosisLevel::Error => Self::Error,
        }
    }
}

/// One issue surfaced by the calibration diagnostic.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiDiagnosisIssue {
    /// Severity.
    pub level: FfiDiagnosisLevel,
    /// Stable short identifier (`"reproj_error_high"` etc.).
    pub code: String,
    /// Human-readable description of what was found.
    pub message: String,
    /// Operator-actionable remediation advice.
    pub remediation: String,
}

/// Per-frame detection statistics from a calibration solve.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiDetectionStats {
    /// Total candidate frames examined.
    pub tried: u32,
    /// Frames where no chessboard was found.
    pub skipped_no_board: u32,
    /// Frames where the detected grid didn't match the
    /// configured target.
    pub skipped_wrong_size: u32,
    /// Frames that couldn't be opened or decoded.
    pub skipped_io: u32,
}

/// Per-view residual stats extracted from the solve.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiViewResidual {
    /// Source frame name (last path component); empty for
    /// in-memory inputs.
    pub source: String,
    /// RMS reprojection residual over this view's corners,
    /// in pixels. `NaN` if the view had no projectable
    /// corners.
    pub rms_px: f64,
    /// Maximum per-corner residual, in pixels.
    pub max_px: f64,
    /// Number of corner observations contributing.
    pub n_corners: u32,
}

/// Calibration result returned across the FFI.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiCalibrationResult {
    /// Solved intrinsics. The same struct that
    /// [`Engine::push_frame`] takes for frames captured by this
    /// camera at this resolution.
    pub intrinsics: FfiIntrinsics,
    /// Image width the calibration is valid for.
    pub width: u32,
    /// Image height the calibration is valid for.
    pub height: u32,
    /// Final reprojection RMS, in pixels.
    pub rms_px: f64,
    /// Number of input frames used in the solve.
    pub n_frames_used: u32,
    /// Number of input frames examined (including those
    /// silently skipped because no checkerboard was detected).
    pub n_frames_total: u32,
    /// Per-frame detection breakdown (which frames were
    /// skipped and why).
    pub detection_stats: FfiDetectionStats,
    /// Overall diagnosis severity (worst of `issues`, or
    /// `Ok` if empty).
    pub diagnosis_overall: FfiDiagnosisLevel,
    /// Operator-actionable diagnostic findings. Empty when
    /// the calibration is healthy.
    pub diagnosis_issues: Vec<FfiDiagnosisIssue>,
    /// Per-view residual statistics in input order. Empty
    /// if extraction failed (the aggregate `rms_px` is
    /// still trustworthy).
    pub per_view_residuals: Vec<FfiViewResidual>,
}

/// Outcome of attempting chessboard detection on a single
/// captured frame.
///
/// Mirrors [`bris_calibrate::FrameOutcome`] so foreign code
/// (Android) can render an actionable per-capture chip
/// instead of waiting for the aggregate solve to discover
/// that two-thirds of frames were unusable.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum FfiFrameOutcome {
    /// Chessboard found and grid matches the expected
    /// target.
    Detected {
        /// Number of inner corners labelled.
        n_corners: u32,
        /// Bounding box of labelled corners, in pixels.
        bbox: FfiBoundingBox,
        /// Laplacian-variance sharpness over the bbox.
        /// Higher = sharper. NaN if the bbox was too small
        /// to compute (degenerate; should not occur for
        /// real captures).
        sharpness: f64,
    },
    /// Detector ran but found nothing chessboard-shaped.
    /// Most common cause is motion blur, severe defocus,
    /// or the board outside the FOV.
    NoBoardFound,
    /// Detector found a chessboard but its grid dimensions
    /// don't match the configured target.
    WrongGridSize {
        /// Grid rows the detector recovered.
        found_rows: u32,
        /// Grid cols the detector recovered.
        found_cols: u32,
        /// Rows the operator configured.
        expected_rows: u32,
        /// Cols the operator configured.
        expected_cols: u32,
    },
    /// Image decode failed.
    DecodeFailed {
        /// Underlying decoder error message.
        reason: String,
    },
}

/// Pixel-space axis-aligned bounding box.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct FfiBoundingBox {
    /// Smallest X (column).
    pub min_x: f64,
    /// Smallest Y (row).
    pub min_y: f64,
    /// Largest X (column).
    pub max_x: f64,
    /// Largest Y (row).
    pub max_y: f64,
}

impl From<bris_calibrate::BoundingBox> for FfiBoundingBox {
    fn from(b: bris_calibrate::BoundingBox) -> Self {
        Self {
            min_x: b.min_x,
            min_y: b.min_y,
            max_x: b.max_x,
            max_y: b.max_y,
        }
    }
}

impl From<bris_calibrate::FrameOutcome> for FfiFrameOutcome {
    fn from(o: bris_calibrate::FrameOutcome) -> Self {
        match o {
            bris_calibrate::FrameOutcome::Detected {
                n_corners,
                bbox_px,
                sharpness,
                ..
            } => Self::Detected {
                n_corners,
                bbox: bbox_px.into(),
                sharpness,
            },
            bris_calibrate::FrameOutcome::NoBoardFound => Self::NoBoardFound,
            bris_calibrate::FrameOutcome::WrongGridSize {
                found_rows,
                found_cols,
                expected_rows,
                expected_cols,
            } => Self::WrongGridSize {
                found_rows,
                found_cols,
                expected_rows,
                expected_cols,
            },
            bris_calibrate::FrameOutcome::DecodeFailed { reason } => Self::DecodeFailed { reason },
        }
    }
}

/// Detect a chessboard in a single JPEG buffer.
///
/// The interactive-calibration entry point: the Android
/// shell calls this for every captured frame and renders
/// the result as a per-capture chip (green = detected,
/// amber = wrong grid, red = no board / decode failed) so
/// the operator can immediately see which frames will be
/// usable when the solve runs.
///
/// Cheap by FFI standards (decode + corner detection;
/// hundreds of ms on a phone-class device for a 1920×1080
/// JPEG). The foreign caller should still invoke this from
/// a background thread.
///
/// # Errors
///
/// - [`FfiError::InvalidArgument`] for invalid target
///   dimensions (zero rows/cols, non-positive square size).
///
/// Detection failures are reported via the
/// [`FfiFrameOutcome`] variants, not as `Err`.
#[uniffi::export]
pub fn detect_calibration_frame(
    jpeg_bytes: Vec<u8>,
    rows: u32,
    cols: u32,
    square_size_mm: f64,
) -> Result<FfiFrameOutcome, FfiError> {
    if rows == 0 || cols == 0 {
        return Err(FfiError::InvalidArgument {
            detail: format!("calibration: rows={rows} cols={cols} must be positive"),
        });
    }
    if !square_size_mm.is_finite() || square_size_mm <= 0.0 {
        return Err(FfiError::InvalidArgument {
            detail: format!("calibration: square_size_mm={square_size_mm} must be positive"),
        });
    }
    let target = bris_calibrate::CheckerboardTarget::new(rows, cols, square_size_mm / 1000.0)
        .map_err(|e| FfiError::InvalidArgument {
            detail: format!("calibration target: {e:?}"),
        })?;
    Ok(bris_calibrate::detect_corners_in_jpeg(&jpeg_bytes, target).into())
}

/// Run a one-shot calibration over a directory of checkerboard
/// frames.
///
/// Equivalent to `bris-cli calibrate --frames <dir> --rows
/// <rows> --cols <cols> --square-size-mm <sz>`. Blocks the
/// calling thread for the duration of the solve (seconds to
/// tens of seconds on a phone-class device); the foreign
/// caller should invoke this from a background thread or
/// coroutine.
///
/// # Errors
///
/// - [`FfiError::InvalidArgument`] for malformed
///   target dimensions (zero rows/cols, non-positive square
///   size).
/// - [`FfiError::Engine`] for downstream failures: no images
///   in the directory, fewer than 3 detected views,
///   inconsistent dimensions, or solver non-convergence. The
///   message names the failure mode.
#[uniffi::export]
pub fn run_calibration(
    frames_dir: String,
    rows: u32,
    cols: u32,
    square_size_mm: f64,
) -> Result<FfiCalibrationResult, FfiError> {
    if rows == 0 || cols == 0 {
        return Err(FfiError::InvalidArgument {
            detail: format!("calibration: rows={rows} cols={cols} must be positive"),
        });
    }
    if !square_size_mm.is_finite() || square_size_mm <= 0.0 {
        return Err(FfiError::InvalidArgument {
            detail: format!("calibration: square_size_mm={square_size_mm} must be positive"),
        });
    }
    let target = bris_calibrate::CheckerboardTarget::new(rows, cols, square_size_mm / 1000.0)
        .map_err(|e| FfiError::InvalidArgument {
            detail: format!("calibration target: {e:?}"),
        })?;
    let path = std::path::Path::new(&frames_dir);
    let detection = bris_calibrate::detect_corners_in_directory(path, target).map_err(|e| {
        FfiError::Engine {
            detail: format!("calibration detect: {e:?}"),
        }
    })?;
    let stats = detection.stats;
    let result = bris_calibrate::calibrate(&detection.views).map_err(|e| FfiError::Engine {
        detail: format!("calibration solve: {e:?}"),
    })?;
    let diagnosis = bris_calibrate::diagnose(&result);
    let detection_stats = FfiDetectionStats {
        tried: u32::try_from(stats.tried).unwrap_or(u32::MAX),
        skipped_no_board: u32::try_from(stats.skipped_no_board).unwrap_or(u32::MAX),
        skipped_wrong_size: u32::try_from(stats.skipped_wrong_size).unwrap_or(u32::MAX),
        skipped_io: u32::try_from(stats.skipped_io).unwrap_or(u32::MAX),
    };
    let diagnosis_issues = diagnosis
        .issues
        .iter()
        .map(|i| FfiDiagnosisIssue {
            level: i.level.into(),
            code: i.code.to_string(),
            message: i.message.clone(),
            remediation: i.remediation.to_string(),
        })
        .collect();
    let per_view_residuals = result
        .per_view
        .iter()
        .map(|v| FfiViewResidual {
            source: v
                .source
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            rms_px: v.rms_px,
            max_px: v.max_px,
            n_corners: u32::try_from(v.n_corners).unwrap_or(u32::MAX),
        })
        .collect();
    Ok(FfiCalibrationResult {
        intrinsics: FfiIntrinsics {
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
        detection_stats,
        diagnosis_overall: diagnosis.overall.into(),
        diagnosis_issues,
        per_view_residuals,
    })
}

/// Image-plane coverage of a session's accumulated
/// detected views, for the live "where to point next"
/// indicator.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiCoverageReport {
    /// Image width the report was computed against.
    pub image_width: u32,
    /// Image height the report was computed against.
    pub image_height: u32,
    /// Number of grid columns.
    pub grid_cols: u32,
    /// Number of grid rows.
    pub grid_rows: u32,
    /// Row-major counts: `cell_counts[r * grid_cols + c]` is
    /// the number of views overlapping cell `(r, c)`.
    pub cell_counts: Vec<u32>,
    /// Fraction of cells with at least one view, 0..=1.
    pub covered_fraction: f64,
    /// Cells with zero views.
    pub empty_cells: u32,
    /// Standard deviation of per-view bounding-box aspect
    /// ratios — a coarse pose-tilt diversity proxy.
    pub aspect_ratio_stddev: f64,
}

/// Compute coverage for a session whose successful
/// detections are JPEGs in `frames_dir`.
///
/// Decodes and re-detects every frame in the directory.
/// Cheap relative to the solve but still O(seconds) for a
/// full session; foreign callers should invoke from a
/// background thread.
///
/// Returns `None` (mapped to `Err(InvalidArgument)`) when
/// the directory contains no successful detections at all
/// (typical at session start before the first capture).
///
/// # Errors
///
/// - [`FfiError::InvalidArgument`] for invalid target
///   dimensions or when the directory has no usable
///   detections yet.
/// - [`FfiError::Engine`] when the directory itself can't
///   be read.
#[uniffi::export]
pub fn calibration_coverage(
    frames_dir: String,
    rows: u32,
    cols: u32,
    square_size_mm: f64,
) -> Result<FfiCoverageReport, FfiError> {
    if rows == 0 || cols == 0 {
        return Err(FfiError::InvalidArgument {
            detail: format!("calibration: rows={rows} cols={cols} must be positive"),
        });
    }
    if !square_size_mm.is_finite() || square_size_mm <= 0.0 {
        return Err(FfiError::InvalidArgument {
            detail: format!("calibration: square_size_mm={square_size_mm} must be positive"),
        });
    }
    let target = bris_calibrate::CheckerboardTarget::new(rows, cols, square_size_mm / 1000.0)
        .map_err(|e| FfiError::InvalidArgument {
            detail: format!("calibration target: {e:?}"),
        })?;
    let path = std::path::Path::new(&frames_dir);
    let detection = match bris_calibrate::detect_corners_in_directory(path, target) {
        Ok(d) => d,
        // TooFewViews / NoImages map to an empty-coverage
        // request, which the Android side renders as "0/16
        // cells covered" rather than a hard error.
        Err(
            bris_calibrate::DetectError::TooFewViews { .. }
            | bris_calibrate::DetectError::NoImages(_),
        ) => {
            return Err(FfiError::InvalidArgument {
                detail: "no successful detections yet".to_string(),
            });
        }
        Err(e) => {
            return Err(FfiError::Engine {
                detail: format!("calibration detect: {e:?}"),
            })
        }
    };
    let cov = bris_calibrate::coverage(&detection.views, bris_calibrate::CoverageConfig::default())
        .ok_or_else(|| FfiError::InvalidArgument {
            detail: "coverage: no usable views".to_string(),
        })?;
    Ok(FfiCoverageReport {
        image_width: cov.image_width,
        image_height: cov.image_height,
        grid_cols: cov.config.grid_cols,
        grid_rows: cov.config.grid_rows,
        cell_counts: cov.cell_counts,
        covered_fraction: cov.covered_fraction,
        empty_cells: cov.empty_cells,
        aspect_ratio_stddev: cov.aspect_ratio_stddev,
    })
}

/// BLAKE3 hex digest of `bytes`.
///
/// Exposed across the FFI so the Android debug-bundle writer
/// can compute the first-frame checksum recorded in
/// `BundleManifest::capture::first_frame_blake3` without
/// pulling a Kotlin BLAKE3 dependency. The checksum is over
/// the raw bytes the caller supplies; matches what
/// [`bris_bundle::verify_first_frame_checksum`] computes for
/// the on-disk PGM.
#[uniffi::export]
#[must_use]
pub fn blake3_hex(bytes: Vec<u8>) -> String {
    blake3::hash(&bytes).to_hex().to_string()
}

/// Write a `bundle.json` manifest at `bundle_dir`.
///
/// The Kotlin / Swift caller serialises a
/// [`bris_bundle::BundleManifest`] as JSON and passes the
/// string here; this function round-trips it through
/// `serde_json` against the canonical Rust types so a Kotlin
/// typo or a schema drift surfaces at write time rather than
/// at replay time. On success the manifest is written
/// pretty-printed to `<bundle_dir>/bundle.json` (creating the
/// directory if necessary).
///
/// # Errors
///
/// - [`FfiError::InvalidArgument`] if `manifest_json` does not
///   deserialise into a [`bris_bundle::BundleManifest`] or
///   declares an unsupported `schema_version`.
/// - [`FfiError::Engine`] for filesystem failures while
///   writing the manifest.
#[uniffi::export]
pub fn write_bundle_manifest(bundle_dir: String, manifest_json: String) -> Result<(), FfiError> {
    let manifest: bris_bundle::BundleManifest =
        serde_json::from_str(&manifest_json).map_err(|e| FfiError::InvalidArgument {
            detail: format!("bundle manifest parse: {e}"),
        })?;
    if manifest.schema_version != bris_bundle::SCHEMA_VERSION {
        return Err(FfiError::InvalidArgument {
            detail: format!(
                "bundle manifest schema_version={} but this build supports {}",
                manifest.schema_version,
                bris_bundle::SCHEMA_VERSION
            ),
        });
    }
    manifest
        .save_to_dir(std::path::Path::new(&bundle_dir))
        .map_err(|e| FfiError::Engine {
            detail: format!("bundle manifest write: {e}"),
        })
}

#[cfg(test)]
mod bundle_writer_tests {
    use super::*;

    #[test]
    fn blake3_hex_matches_bundle_verifier() {
        // The Android writer computes the manifest's
        // `first_frame_blake3` by calling `blake3_hex` on the
        // on-disk PGM bytes; `bris_bundle::verify_first_frame_
        // checksum` computes the same. Round-trip via the FFI
        // entry point to lock that contract.
        let bytes = b"P5\n2 2\n255\nabcd".to_vec();
        let from_ffi = blake3_hex(bytes.clone());
        let expected = blake3::hash(&bytes).to_hex().to_string();
        assert_eq!(from_ffi, expected);
    }

    #[test]
    fn write_bundle_manifest_round_trips_minimum_schema() {
        let dir = tempfile::tempdir().unwrap();
        // Minimum required fields per `BundleManifest` /
        // `CaptureInfo` / `IntrinsicsRecord`. Mirrors what the
        // Android writer composes when no calibration matches.
        let json = serde_json::json!({
            "schema_version": 1,
            "bundle_id": "test",
            "device": { "model": "TestPhone" },
            "capture": {
                "source_rotation_deg": 0,
                "frame_count": 1,
                "started_unix_ms": 1_700_000_000_000_i64,
                "ended_unix_ms": 1_700_000_000_000_i64
            },
            "intrinsics": {
                "source": { "kind": "placeholder" },
                "width": 1280, "height": 720,
                "fx": 1000.0, "fy": 1000.0, "cx": 640.0, "cy": 360.0,
                "distortion": { "model": "none" }
            }
        });
        write_bundle_manifest(dir.path().to_string_lossy().into_owned(), json.to_string()).unwrap();
        let loaded = bris_bundle::BundleManifest::load_from_dir(dir.path()).unwrap();
        assert_eq!(loaded.bundle_id, "test");
        assert_eq!(loaded.capture.frame_count, 1);
    }

    #[test]
    fn write_bundle_manifest_rejects_unknown_schema() {
        let dir = tempfile::tempdir().unwrap();
        let json = serde_json::json!({
            "schema_version": 999,
            "bundle_id": "x",
            "device": { "model": "x" },
            "capture": {
                "source_rotation_deg": 0, "frame_count": 0,
                "started_unix_ms": 0, "ended_unix_ms": 0
            },
            "intrinsics": {
                "source": { "kind": "placeholder" },
                "width": 1, "height": 1,
                "fx": 1.0, "fy": 1.0, "cx": 0.0, "cy": 0.0,
                "distortion": { "model": "none" }
            }
        });
        let err =
            write_bundle_manifest(dir.path().to_string_lossy().into_owned(), json.to_string())
                .unwrap_err();
        assert!(matches!(err, FfiError::InvalidArgument { .. }));
    }

    #[test]
    fn write_bundle_manifest_rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let err = write_bundle_manifest(
            dir.path().to_string_lossy().into_owned(),
            "{ not json".to_string(),
        )
        .unwrap_err();
        assert!(matches!(err, FfiError::InvalidArgument { .. }));
    }
}
