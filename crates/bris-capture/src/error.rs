//! Top-level error type for the capture crate.
//!
//! Bundles errors from the V4L2 backend, format conversion,
//! timestamping, and the underlying `bris_vision::Frame`
//! constructor into a single `Result` type for the public
//! API. Internal modules use their narrower error types
//! (`FormatError`, `TimestampError`); they convert into
//! [`CaptureError`] at the crate boundary via `From` impls.

use thiserror::Error;

use crate::format::FormatError;
use crate::time::TimestampError;

/// Errors from the camera capture pipeline.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// V4L2 driver error: opening the device, querying
    /// formats, configuring, dequeuing buffers. Carries the
    /// underlying [`std::io::Error`] because the v4l crate
    /// reports almost everything as I/O.
    #[cfg(feature = "v4l2")]
    #[error("V4L2: {0}")]
    V4l2(#[from] std::io::Error),
    /// The camera doesn't support a format the engine can
    /// consume. Currently the engine only ingests YUYV; this
    /// fires when the camera's available format list contains
    /// no compatible entry.
    #[cfg(feature = "v4l2")]
    #[error(
        "camera at {device_path:?} doesn't support YUYV at any resolution; \
         supported formats: {formats:?}"
    )]
    UnsupportedFormat {
        /// The device path passed to [`crate::V4l2Capture::open`].
        device_path: std::path::PathBuf,
        /// Stringified list of `FourCC` codes the device
        /// reported as supported.
        formats: Vec<String>,
    },
    /// The camera doesn't support the requested
    /// (width, height) at the chosen format. Lists the
    /// supported sizes so the operator can pick a workable
    /// one.
    #[cfg(feature = "v4l2")]
    #[error(
        "camera at {device_path:?} doesn't support {width}×{height} for YUYV; \
         supported sizes: {supported:?}"
    )]
    UnsupportedResolution {
        /// The device path passed to [`crate::V4l2Capture::open`].
        device_path: std::path::PathBuf,
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
        /// `(width, height)` pairs the device reported as
        /// supported.
        supported: Vec<(u32, u32)>,
    },
    /// Pixel-format conversion failed on a captured frame.
    /// Almost always indicates a driver-vs-format mismatch
    /// (camera reported YUYV but delivered something else).
    #[error(transparent)]
    Format(#[from] FormatError),
    /// Timestamp conversion failed.
    #[error(transparent)]
    Timestamp(#[from] TimestampError),
    /// `bris_vision::Frame` constructor rejected the frame.
    /// Should be impossible if format conversion succeeded
    /// (the buffer length matches by construction); guarded
    /// here because the constructor's invariants are part of
    /// the public surface.
    #[error("Frame constructor rejected the captured frame: {0}")]
    FrameConstruct(#[from] bris_vision::FrameError),
}
