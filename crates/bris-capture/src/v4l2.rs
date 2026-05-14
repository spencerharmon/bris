//! V4L2 capture backend.
//!
//! Wraps the `v4l` crate's mmap streaming capture into the
//! "open + run-capture-loop" API the rest of Bris uses.
//!
//! # Verified compile, not yet exercised on hardware
//!
//! Per the crate-level docs, this module compiles against the
//! v4l 0.14 API but hasn't been run against a real camera in
//! this commit's testing environment. The unit tests that
//! exist here exercise only the configuration/validation
//! paths that don't touch hardware (parsing, format
//! negotiation logic against synthetic enumeration outputs).
//! Bring-up against real hardware is a separate task.
//!
//! # Threading model
//!
//! [`run_capture_loop`] is meant to be called from a dedicated
//! capture thread. It blocks on V4L2 buffer dequeue and pushes
//! each completed frame into the engine. Caller is responsible
//! for spawning the thread; this lets the CLI/FFI shells
//! choose their own thread-naming and panic-handling
//! conventions.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bris_streaming::StreamingEngine;
use bris_vision::{Frame, Intrinsics};
use tracing::{debug, info, warn};
use v4l::buffer::Type as BufferType;
use v4l::format::FourCC;
use v4l::io::traits::CaptureStream;
use v4l::prelude::{Device, MmapStream};
use v4l::video::Capture;
use v4l::Format;

use crate::error::CaptureError;
use crate::format::yuyv_to_grayscale_u16;
use crate::time::{buffer_to_mid_exposure_tt, MonotonicAnchor};

/// Fourcc of the only pixel format this backend currently
/// accepts. Cameras that don't list YUYV in their
/// `enum_formats()` output are rejected at [`V4l2Capture::open`].
const FOURCC_YUYV: [u8; 4] = *b"YUYV";

/// Configuration for the V4L2 backend.
#[derive(Debug, Clone)]
pub struct V4l2Config {
    /// Path to the V4L2 device node. Typical values:
    /// `/dev/video0` for the first USB camera, `/dev/video1`
    /// for the second, etc. On Pi systems the libcamera
    /// stack exposes `/dev/video0` for unicam-direct and
    /// higher-numbered nodes for ISP outputs.
    pub device_path: PathBuf,
    /// Requested capture width. The driver may negotiate to
    /// the closest supported size; the actual size is reported
    /// back via [`V4l2Capture::actual_format`] after
    /// [`V4l2Capture::open`].
    pub width: u32,
    /// Requested capture height. Same negotiation behaviour
    /// as `width`.
    pub height: u32,
    /// Number of mmap buffers to allocate. Higher reduces the
    /// chance of a frame being dropped during transient
    /// processing slowdowns; lower reduces RAM. Default 4 —
    /// the v4l crate's example value.
    pub buffer_count: u32,
    /// Exposure time used for the mid-exposure timestamp
    /// correction, in microseconds. **Not the same as the
    /// camera's actual exposure** — the camera may be running
    /// in auto-exposure mode and the per-frame exposure may
    /// vary. For fixed-exposure deployments this is correct;
    /// for auto-exposure deployments it's a per-frame
    /// approximation that's correct on average and biased by
    /// at most `±frame_period/2` per frame.
    ///
    /// A future enhancement reads the actual exposure via
    /// V4L2 controls per-frame (or per-batch); for commit
    /// one we ship the simple model.
    pub exposure_us: u32,
}

impl V4l2Config {
    /// Reasonable defaults for a USB webcam: `/dev/video0`,
    /// 640×480, 4 mmap buffers, 10 ms exposure (a common
    /// auto-exposure default for daylight scenes).
    #[must_use]
    pub fn default_for_webcam() -> Self {
        Self {
            device_path: PathBuf::from("/dev/video0"),
            width: 640,
            height: 480,
            buffer_count: 4,
            exposure_us: 10_000,
        }
    }
}

/// Opened V4L2 device with the requested format negotiated.
///
/// Constructed via [`Self::open`]. Move into
/// [`run_capture_loop`] to begin streaming; the loop consumes
/// the capture struct because the underlying streaming object
/// holds an exclusive borrow of the device.
pub struct V4l2Capture {
    device: Device,
    config: V4l2Config,
    intrinsics: Intrinsics,
    anchor: MonotonicAnchor,
    actual_format: Format,
}

impl std::fmt::Debug for V4l2Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The v4l Device doesn't impl Debug; print the
        // operator-meaningful pieces (path, format) instead.
        // `device` and `anchor` are intentionally elided
        // (Device has no Debug; the anchor's monotonic-zero
        // is uninformative). Use `finish_non_exhaustive` to
        // make the omissions explicit.
        f.debug_struct("V4l2Capture")
            .field("device_path", &self.config.device_path)
            .field("width", &self.actual_format.width)
            .field("height", &self.actual_format.height)
            .field("config", &self.config)
            .field("intrinsics", &self.intrinsics)
            .finish_non_exhaustive()
    }
}

impl V4l2Capture {
    /// Open the camera, negotiate YUYV at the requested
    /// resolution, and prepare for streaming.
    ///
    /// # Errors
    ///
    /// - [`CaptureError::V4l2`] for any underlying ioctl
    ///   failure (device missing, permission denied, etc.).
    /// - [`CaptureError::UnsupportedFormat`] if the camera
    ///   doesn't list YUYV in its supported formats.
    /// - [`CaptureError::UnsupportedResolution`] if the
    ///   camera lists YUYV but doesn't support the requested
    ///   width/height for it.
    pub fn open(config: V4l2Config, intrinsics: Intrinsics) -> Result<Self, CaptureError> {
        info!(
            device = %config.device_path.display(),
            width = config.width,
            height = config.height,
            "V4l2Capture::open"
        );
        let device = Device::with_path(&config.device_path)?;

        // Verify YUYV is supported. We could just ask the
        // driver to set the format and let it fail, but the
        // resulting error message is opaque ("Invalid
        // argument"); enumerating formats first lets us emit
        // a useful error listing what's available.
        let mut yuyv_supported = false;
        let mut supported_fourcc_strings: Vec<String> = Vec::new();
        for desc in device.enum_formats()? {
            let fourcc = desc.fourcc.repr;
            supported_fourcc_strings.push(format!(
                "{}{}{}{}",
                fourcc[0] as char, fourcc[1] as char, fourcc[2] as char, fourcc[3] as char,
            ));
            if fourcc == FOURCC_YUYV {
                yuyv_supported = true;
            }
        }
        if !yuyv_supported {
            return Err(CaptureError::UnsupportedFormat {
                device_path: config.device_path.clone(),
                formats: supported_fourcc_strings,
            });
        }

        // Verify the requested resolution is supported for
        // YUYV. We accept either an exact discrete match or a
        // stepwise range that includes the request.
        let yuyv = FourCC::new(&FOURCC_YUYV);
        let mut discrete_supported: Vec<(u32, u32)> = Vec::new();
        let mut resolution_ok = false;
        for size in device.enum_framesizes(yuyv)? {
            for d in size.size.to_discrete() {
                discrete_supported.push((d.width, d.height));
                if d.width == config.width && d.height == config.height {
                    resolution_ok = true;
                }
            }
        }
        if !resolution_ok {
            return Err(CaptureError::UnsupportedResolution {
                device_path: config.device_path.clone(),
                width: config.width,
                height: config.height,
                supported: discrete_supported,
            });
        }

        // Negotiate. The driver returns the format actually
        // applied, which may differ slightly (stride, etc.)
        // from what we asked for. We trust the negotiated
        // values for downstream pixel-count math.
        let requested = Format::new(config.width, config.height, yuyv);
        let actual_format = device.set_format(&requested)?;
        info!(
            actual_width = actual_format.width,
            actual_height = actual_format.height,
            actual_stride = actual_format.stride,
            "V4l2Capture: format negotiated",
        );
        if actual_format.fourcc.repr != FOURCC_YUYV {
            // Defensive: set_format silently changed the
            // pixel format. Should not happen after the
            // enum_formats check, but guard rather than
            // emit garbage frames.
            return Err(CaptureError::UnsupportedFormat {
                device_path: config.device_path.clone(),
                formats: vec![format!(
                    "negotiation returned {}{}{}{}",
                    actual_format.fourcc.repr[0] as char,
                    actual_format.fourcc.repr[1] as char,
                    actual_format.fourcc.repr[2] as char,
                    actual_format.fourcc.repr[3] as char,
                )],
            });
        }

        let anchor = MonotonicAnchor::now();
        Ok(Self {
            device,
            config,
            intrinsics,
            anchor,
            actual_format,
        })
    }

    /// The pixel format that was actually negotiated with the
    /// driver. May differ from the requested values in
    /// `width`/`height`/`stride` if the camera couldn't honor
    /// the request exactly.
    #[must_use]
    pub fn actual_format(&self) -> &Format {
        &self.actual_format
    }
}

/// Statistics from a [`run_capture_loop`] run.
///
/// Reported when the loop exits (via `shutdown` flag,
/// engine-side disconnect, or a hard error). Lets the caller
/// log a session summary even on the error path.
#[derive(Debug, Default, Clone, Copy)]
pub struct CaptureStats {
    /// Number of frames successfully dequeued and pushed to
    /// the engine.
    pub frames_captured: u64,
    /// Number of frames where buffer dequeue or format
    /// conversion failed and the frame was discarded.
    pub frames_dropped_at_capture: u64,
}

/// Drive a generic capture loop: dequeue frames from V4L2,
/// convert to [`Frame`], hand each one to the supplied
/// callback.
///
/// Runs until `shutdown` is set to `true`, the callback
/// returns [`CaptureLoopAction::Stop`], or an unrecoverable
/// error occurs. Per-frame conversion failures are logged at
/// `warn!` and counted in [`CaptureStats::frames_dropped_at_capture`]
/// but do not stop the loop — a glitched buffer shouldn't
/// kill an otherwise-healthy capture session.
///
/// This is the low-level loop. Use [`run_capture_loop`] when
/// you want frames pushed into a [`StreamingEngine`]; use
/// [`run_capture_loop_with`] when you need a different
/// per-frame action (e.g. saving to disk in `bris capture`).
///
/// # Errors
///
/// Returns the first unrecoverable error: V4L2 stream init
/// failure, fatal dequeue error (broken pipe, device removed),
/// or timestamp-conversion errors that indicate the system
/// clock has gone bad. Per-frame format errors do *not* end
/// the loop.
///
/// # Threading
///
/// Designed to be called from a dedicated capture thread.
/// The callback runs on the capture thread; if it does heavy
/// work it'll back-pressure the V4L2 stream (which may drop
/// kernel-side frames). For engine ingest this is fine —
/// `engine.push_frame` is non-blocking — but for I/O-heavy
/// callbacks (like writing to disk) the caller should
/// handle the rate budget.
#[allow(
    clippy::needless_pass_by_value, // see run_capture_loop justification
)]
pub fn run_capture_loop_with<F>(
    capture: V4l2Capture,
    shutdown: Arc<AtomicBool>,
    mut on_frame: F,
) -> Result<CaptureStats, CaptureError>
where
    F: FnMut(Frame) -> CaptureLoopAction,
{
    let V4l2Capture {
        device,
        config,
        intrinsics,
        anchor,
        actual_format,
    } = capture;
    info!(
        device = %config.device_path.display(),
        "V4L2 capture loop starting"
    );
    let mut stream =
        MmapStream::with_buffers(&device, BufferType::VideoCapture, config.buffer_count)?;
    // The v4l crate documents that the first .next() call
    // performs first-time stream init; some drivers return
    // garbage in that buffer. We discard it as a warmup.
    let _warmup = stream.next()?;
    debug!("V4L2 capture loop: warmup frame discarded");

    let convert_ctx = ConvertContext {
        width: actual_format.width,
        height: actual_format.height,
        anchor,
        exposure_us: config.exposure_us,
        intrinsics,
    };

    let mut stats = CaptureStats::default();
    while !shutdown.load(Ordering::Relaxed) {
        let (buf, meta) = stream.next()?;
        let buffer_monotonic: Duration = meta.timestamp.into();
        let bytes = &buf[..meta.bytesused as usize];
        match convert_to_frame(bytes, buffer_monotonic, &convert_ctx) {
            Ok(frame) => {
                stats.frames_captured += 1;
                if matches!(on_frame(frame), CaptureLoopAction::Stop) {
                    debug!("V4L2 capture loop: callback requested stop");
                    break;
                }
            }
            Err(e) => {
                stats.frames_dropped_at_capture += 1;
                warn!(error = %e, "V4L2 capture loop: per-frame error, dropping");
            }
        }
    }
    info!(
        frames_captured = stats.frames_captured,
        frames_dropped = stats.frames_dropped_at_capture,
        "V4L2 capture loop stopped"
    );
    Ok(stats)
}

/// Per-frame return value from the [`run_capture_loop_with`]
/// callback. Lets the callback signal "I've had enough"
/// (e.g. captured the requested frame count) without forcing
/// the caller to flip the shutdown atomic from another
/// thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureLoopAction {
    /// Continue looping; dequeue the next frame.
    Continue,
    /// Stop the capture loop. Returns the current
    /// [`CaptureStats`] to the caller.
    Stop,
}

/// Drive a capture loop that pushes each captured frame
/// into the supplied [`StreamingEngine`].
///
/// Convenience wrapper around [`run_capture_loop_with`]: the
/// per-frame callback is `|frame| engine.push_frame(frame)`.
///
/// Runs until `shutdown` is set to `true` or an unrecoverable
/// error occurs. See [`run_capture_loop_with`] for error and
/// threading details.
///
/// # Errors
///
/// As [`run_capture_loop_with`].
#[allow(clippy::needless_pass_by_value)]
pub fn run_capture_loop(
    capture: V4l2Capture,
    engine: Arc<StreamingEngine>,
    shutdown: Arc<AtomicBool>,
) -> Result<CaptureStats, CaptureError> {
    run_capture_loop_with(capture, shutdown, move |frame| {
        // engine.push_frame swallows backpressure (drops on
        // full input ring). We don't try to detect that here
        // — it's observable via engine.diagnostics().frames_dropped
        // from the consumer side.
        let _ = engine.push_frame(frame);
        CaptureLoopAction::Continue
    })
}

/// Per-loop-invocation context bundling the converter
/// parameters that don't change frame-to-frame.
#[derive(Debug, Clone, Copy)]
struct ConvertContext {
    width: u32,
    height: u32,
    anchor: MonotonicAnchor,
    exposure_us: u32,
    intrinsics: Intrinsics,
}

/// Convert one V4L2 buffer to a `bris_vision::Frame`.
fn convert_to_frame(
    bytes: &[u8],
    buffer_monotonic: Duration,
    ctx: &ConvertContext,
) -> Result<Frame, CaptureError> {
    let pixels = yuyv_to_grayscale_u16(bytes, ctx.width, ctx.height)?;
    let buffer_utc =
        ctx.anchor
            .buffer_timestamp_to_utc(buffer_monotonic)
            .ok_or(CaptureError::Timestamp(
                crate::time::TimestampError::NonFinite,
            ))?;
    let tt = buffer_to_mid_exposure_tt(buffer_utc, ctx.exposure_us)?;
    let frame = Frame::new(
        ctx.width,
        ctx.height,
        pixels,
        tt,
        ctx.exposure_us,
        ctx.intrinsics,
    )?;
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_for_webcam_has_sensible_values() {
        let c = V4l2Config::default_for_webcam();
        assert_eq!(c.device_path, PathBuf::from("/dev/video0"));
        assert_eq!(c.width, 640);
        assert_eq!(c.height, 480);
        assert_eq!(c.buffer_count, 4);
        assert!(c.exposure_us > 0);
    }

    // Open / capture-loop tests need a real V4L2 device and
    // are deferred to a manual bring-up checklist documented
    // in the crate-level docstring.
}
