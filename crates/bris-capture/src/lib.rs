//! Camera capture for the Bris streaming engine.
//!
//! This crate is the bridge between a camera (V4L2 device on
//! Linux) and [`bris_streaming::StreamingEngine`]: it ingests
//! raw camera frames, converts the pixel format to the
//! engine's `u16` grayscale, timestamps each frame
//! mid-exposure in TT, and pushes them into the engine's
//! input ring.
//!
//! # Architecture
//!
//! The capture pipeline runs on its own thread:
//!
//! ```text
//!   ┌───────────────┐    ┌─────────────────┐    ┌─────────────┐
//!   │ V4L2 driver   │ →  │ V4l2Capture     │ →  │ StreamingEngine
//!   │ (kernel)      │    │ • dequeue       │    │ • push_frame
//!   └───────────────┘    │ • YUYV → u16    │    └─────────────┘
//!                        │ • timestamp     │
//!                        │ • Frame::new    │
//!                        └─────────────────┘
//! ```
//!
//! The capture thread is independent of the engine's worker
//! and the consumer (CLI, FFI shell) that drains
//! `engine.fix_stream()`. Backpressure is "drop on full
//! input ring" per [`bris_streaming::StreamingEngine::push_frame`]'s
//! contract.
//!
//! # Backends
//!
//! - **V4L2** ([`V4l2Capture`], [`V4l2Config`]): Linux native.
//!   The default and currently the only backend. Behind the
//!   `v4l2` feature flag (default-on).
//! - **libcamera**: Pi-specific path; not yet implemented.
//!   The Pi camera modules speak V4L2 too via the
//!   `bcm2835-codec` and `unicam` kernel drivers, but
//!   accessing exposure/gain/ISP tuning requires libcamera.
//!   Deferred until V4L2 hits real-camera issues that need it.
//! - **File replay**: not part of this crate; the existing
//!   [`bris_vision::load_frame_from_path`] path covers it.
//!   The CLI's `bris replay` already wires it.
//!
//! # Pixel formats
//!
//! Initially, **YUYV (YUV 4:2:2) only**. It's the most common
//! USB-webcam format, and the conversion to grayscale is one
//! line (the Y plane is already luminance). MJPEG, NV12, and
//! RAW Bayer are deferred to follow-ups.
//!
//! # What can't be tested without a real camera
//!
//! The [`format`] and [`time`] modules are fully covered by
//! unit and property tests; they don't touch hardware.
//!
//! The [`v4l2`] module (when the `v4l2` feature is enabled)
//! has been verified to **compile** against the v4l 0.14 API
//! but not exercised against real hardware in this commit's
//! testing environment. Concretely:
//!
//! - Buffer dequeue + timestamp extraction needs a real
//!   device.
//! - Format negotiation against a camera that doesn't support
//!   YUYV at the requested resolution needs hardware to
//!   exercise.
//! - The capture loop's drop-on-backpressure behaviour can't
//!   be measured without a real fps source.
//!
//! Operators bringing up a new camera should run
//! `bris capture --probe` (TODO: CLI subcommand) which lists
//! the device's reported formats and resolutions; that's the
//! right starting point for diagnosing format mismatches.

mod error;
pub mod format;
pub mod sensor_gain;
pub mod time;
#[cfg(feature = "v4l2")]
mod v4l2;

pub use error::CaptureError;
pub use format::{widen_u8_to_u16, yuyv_to_grayscale_u16, FormatError};
pub use sensor_gain::{
    map_for_card, Imx219LinearMap, OvGenericMap, SensorGainMap, SensorMapKind, UnknownMap,
};
pub use time::{buffer_to_mid_exposure_tt, MonotonicAnchor, TimestampError};
#[cfg(feature = "v4l2")]
pub use v4l2::{
    max_yuyv_resolution, run_capture_loop, run_capture_loop_with, CaptureLoopAction, CaptureStats,
    V4l2Capture, V4l2Config,
};
