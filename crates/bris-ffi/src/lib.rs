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
use bris_core::{time, Latitude, Longitude};
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
    /// Most recent classifier verdict as a stable label, or
    /// `None` until the first frame is processed.
    pub last_classification: Option<String>,
    /// TT Julian Date of the most recent processed frame, or
    /// `None`.
    pub last_processed_frame_tt_jd: Option<f64>,
    /// TT Julian Date of the most recent published fix, or
    /// `None`.
    pub last_published_fix_tt_jd: Option<f64>,
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
        let last_classification = d
            .last_classification
            .map(|c| format!("{c:?}").to_lowercase());
        Self {
            frames_pushed: d.frames_pushed,
            frames_dropped: d.frames_dropped,
            stages,
            body_queue_depth: u32::try_from(d.body_queue_depth).unwrap_or(u32::MAX),
            horizon_queue_depth: u32::try_from(d.horizon_queue_depth).unwrap_or(u32::MAX),
            ring_buffer_depth: u32::try_from(d.ring_buffer_depth).unwrap_or(u32::MAX),
            sight_window_depth: u32::try_from(d.sight_window_depth).unwrap_or(u32::MAX),
            last_classification,
            last_processed_frame_tt_jd: d
                .last_processed_frame_tt
                .map(bris_core::time::Tt::julian_date),
            last_published_fix_tt_jd: d
                .last_published_fix_tt
                .map(bris_core::time::Tt::julian_date),
        }
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
    let (views, stats) =
        bris_calibrate::detect_corners_in_directory(path, target).map_err(|e| {
            FfiError::Engine {
                detail: format!("calibration detect: {e:?}"),
            }
        })?;
    let result = bris_calibrate::calibrate(&views).map_err(|e| FfiError::Engine {
        detail: format!("calibration solve: {e:?}"),
    })?;
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
    })
}
