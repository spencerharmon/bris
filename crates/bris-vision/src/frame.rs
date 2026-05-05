//! Frame: the unit of image data flowing through the vision pipeline.
//!
//! A [`Frame`] bundles pixel data, capture metadata, and (when known)
//! the lens intrinsics under which it was captured. Pipeline stages
//! read frames, never mutate them, and produce typed outputs (horizon
//! lines, body centroids, etc.) carried alongside.
//!
//! # Pixel format
//!
//! Bris's classical-CV pipeline only needs *grayscale* input. Color
//! cameras feed in via demosaicing → luminance conversion at the
//! capture-shell boundary; the core never sees Bayer or RGB. This is
//! a deliberate simplification:
//!
//! - Horizon detection works on luminance gradients.
//! - Body centroiding works on luminance peaks (Sun/Moon are saturated;
//!   stars are point sources).
//! - Plate solving works on detected stars, which are themselves
//!   centroids of luminance peaks.
//!
//! Color information is irrelevant to the algorithm and only burdens
//! memory and arithmetic.
//!
//! # Bit depth
//!
//! Pixels are `u16` to accommodate 10-12 bit camera output without
//! clipping the highlights that we need for sub-pixel centroiding.
//! 8-bit input is widened on the way in.

use bris_core::time::Tt;
use core::num::NonZeroU32;

/// Camera intrinsics (lens parameters) under which a frame was captured.
///
/// Pinhole + Brown-Conrady distortion. Concrete distortion-coefficient
/// fitting lives in the `calibration` module (Phase 2 task).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Intrinsics {
    /// Focal length in pixels along the x axis.
    pub fx: f64,
    /// Focal length in pixels along the y axis.
    pub fy: f64,
    /// Principal point x coordinate (pixels from image origin).
    pub cx: f64,
    /// Principal point y coordinate.
    pub cy: f64,
    /// Brown-Conrady radial distortion coefficient k1.
    pub k1: f64,
    /// Brown-Conrady radial distortion coefficient k2.
    pub k2: f64,
    /// Brown-Conrady radial distortion coefficient k3.
    pub k3: f64,
    /// Brown-Conrady tangential distortion coefficient p1.
    pub p1: f64,
    /// Brown-Conrady tangential distortion coefficient p2.
    pub p2: f64,
}

impl Intrinsics {
    /// An identity-ish placeholder usable for tests and uncalibrated
    /// captures. fx = fy = 1000 px, principal point at image center,
    /// zero distortion. Real captures should ship with measured
    /// intrinsics from the calibration workflow.
    #[must_use]
    pub fn placeholder(width: u32, height: u32) -> Self {
        Self {
            fx: 1000.0,
            fy: 1000.0,
            cx: f64::from(width) / 2.0,
            cy: f64::from(height) / 2.0,
            k1: 0.0,
            k2: 0.0,
            k3: 0.0,
            p1: 0.0,
            p2: 0.0,
        }
    }
}

/// One captured frame as it flows through the pipeline.
///
/// `pixels` is row-major, length exactly `width × height`. Stride is
/// implicit (no padding); if a real capture path needs strided buffers
/// it should copy on the way in. Construction is fallible to enforce
/// the dimension invariants.
#[derive(Debug, Clone)]
pub struct Frame {
    width: NonZeroU32,
    height: NonZeroU32,
    pixels: Vec<u16>,
    /// TT instant of frame capture (mid-exposure). The vision pipeline
    /// only needs this for stitching alignment and downstream sight
    /// reduction; horizon detection and centroiding don't use it.
    pub capture_tt: Tt,
    /// Exposure duration, microseconds. Used to model motion blur
    /// uncertainty in centroiding.
    pub exposure_us: u32,
    /// Camera intrinsics under which the frame was captured.
    pub intrinsics: Intrinsics,
    /// Optional path to the source image file. Only used by the
    /// segmentation horizon detector, which needs to re-load the
    /// original color image from disk (the pretrained model expects
    /// RGB; replicating Bris's grayscale into three channels gives
    /// dramatically wrong predictions). `None` for frames not
    /// originating from a file (synthetic, future live-camera
    /// capture). When `None`, the segmentation detector falls back
    /// to grayscale-replicated input with the documented quality
    /// hit, or returns an error — caller's choice.
    pub source_path: Option<std::path::PathBuf>,
}

impl Frame {
    /// Construct a frame from row-major u16 pixels.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError::Empty`] if `width` or `height` is zero,
    /// [`FrameError::DimensionMismatch`] if `pixels.len() ≠ width × height`.
    pub fn new(
        width: u32,
        height: u32,
        pixels: Vec<u16>,
        capture_tt: Tt,
        exposure_us: u32,
        intrinsics: Intrinsics,
    ) -> Result<Self, FrameError> {
        let width = NonZeroU32::new(width).ok_or(FrameError::Empty)?;
        let height = NonZeroU32::new(height).ok_or(FrameError::Empty)?;
        let expected = (width.get() as usize)
            .checked_mul(height.get() as usize)
            .ok_or(FrameError::DimensionMismatch {
                expected: usize::MAX,
                actual: pixels.len(),
            })?;
        if pixels.len() != expected {
            return Err(FrameError::DimensionMismatch {
                expected,
                actual: pixels.len(),
            });
        }
        Ok(Self {
            width,
            height,
            pixels,
            capture_tt,
            exposure_us,
            intrinsics,
            source_path: None,
        })
    }

    /// Attach a source-file path to a frame. Consumed by the
    /// segmentation horizon detector to re-load the color image.
    /// Other consumers ignore the field.
    #[must_use]
    pub fn with_source_path(mut self, path: std::path::PathBuf) -> Self {
        self.source_path = Some(path);
        self
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width.get()
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height.get()
    }

    /// Number of pixels (`width × height`).
    pub fn len(&self) -> usize {
        self.pixels.len()
    }

    /// True iff there are no pixels (impossible by construction; provided
    /// to satisfy the clippy convention paired with [`Self::len`]).
    #[allow(clippy::unused_self)] // signature is fixed by the convention.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Row-major pixel buffer.
    pub fn pixels(&self) -> &[u16] {
        &self.pixels
    }

    /// Look up a pixel by `(x, y)`. Returns `None` if out of bounds.
    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> Option<u16> {
        if x >= self.width.get() || y >= self.height.get() {
            return None;
        }
        let idx = (y as usize) * (self.width.get() as usize) + (x as usize);
        Some(self.pixels[idx])
    }
}

/// Errors constructing a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// Width or height was zero.
    #[error("frame dimensions must be non-zero")]
    Empty,
    /// Pixel buffer length didn't match width × height.
    #[error("pixel buffer length {actual} doesn't match expected {expected}")]
    DimensionMismatch {
        /// Required length given the declared dimensions.
        expected: usize,
        /// Actual buffer length supplied by the caller.
        actual: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_core::time::JD_J2000;

    fn dummy_intrinsics() -> Intrinsics {
        Intrinsics::placeholder(4, 3)
    }

    #[test]
    fn frame_round_trips() {
        let pixels = vec![0u16; 12];
        let f = Frame::new(
            4,
            3,
            pixels.clone(),
            Tt::from_julian_date(JD_J2000),
            1000,
            dummy_intrinsics(),
        )
        .unwrap();
        assert_eq!(f.width(), 4);
        assert_eq!(f.height(), 3);
        assert_eq!(f.len(), 12);
        assert_eq!(f.pixels(), pixels.as_slice());
    }

    #[test]
    fn frame_rejects_empty_dims() {
        let err = Frame::new(
            0,
            10,
            vec![],
            Tt::from_julian_date(JD_J2000),
            0,
            dummy_intrinsics(),
        )
        .unwrap_err();
        assert_eq!(err, FrameError::Empty);
    }

    #[test]
    fn frame_rejects_dimension_mismatch() {
        let err = Frame::new(
            4,
            3,
            vec![0u16; 11],
            Tt::from_julian_date(JD_J2000),
            0,
            dummy_intrinsics(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            FrameError::DimensionMismatch {
                expected: 12,
                actual: 11
            }
        ));
    }

    #[test]
    fn pixel_lookup_in_bounds() {
        let mut pixels = vec![0u16; 12];
        pixels[5] = 42; // (x=1, y=1) in a 4-wide image.
        let f = Frame::new(
            4,
            3,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            dummy_intrinsics(),
        )
        .unwrap();
        assert_eq!(f.pixel(1, 1), Some(42));
        assert_eq!(f.pixel(0, 0), Some(0));
        assert_eq!(f.pixel(4, 0), None);
        assert_eq!(f.pixel(0, 3), None);
    }

    #[test]
    fn placeholder_intrinsics_centered() {
        let i = Intrinsics::placeholder(640, 480);
        assert!((i.cx - 320.0).abs() < 1e-12);
        assert!((i.cy - 240.0).abs() < 1e-12);
        assert!(i.k1.abs() < 1e-12 && i.k2.abs() < 1e-12 && i.k3.abs() < 1e-12);
    }
}
