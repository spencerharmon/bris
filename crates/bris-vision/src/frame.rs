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

/// Rotation applied to source pixels at load time, in degrees
/// clockwise.
///
/// Frames captured in portrait orientation (phones in their natural
/// hold) violate the pipeline's "horizon is approximately
/// horizontal" assumption that's baked into the
/// `y = slope·x + intercept` parameterization in
/// [`crate::horizon::HorizonLine`] and the per-column scanning in
/// every horizon detector. Rather than refactor the entire pipeline
/// to a normal-form line representation, we rotate the pixel buffer
/// at load time so the *internal* frame is always landscape with
/// the horizon roughly horizontal.
///
/// [`Frame::source_rotation`] records which rotation was applied so
/// downstream code that needs to talk about source-image coordinates
/// (currently just the segmentation detector, which re-loads the
/// original file from disk) can map back.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// No rotation. Source pixels are the internal pixels.
    #[default]
    Deg0,
    /// 90° clockwise. A portrait source becomes a landscape internal
    /// frame; the source's top edge is the internal frame's right
    /// edge.
    Deg90,
    /// 180°.
    Deg180,
    /// 270° clockwise (equivalently 90° counter-clockwise). The
    /// source's top edge is the internal frame's left edge.
    Deg270,
}

impl Rotation {
    /// Heuristic: if the source aspect ratio is portrait by a margin
    /// (h ≥ 1.2 × w), return [`Rotation::Deg90`]. Otherwise
    /// [`Rotation::Deg0`]. Phone captures are typically 9:16 or 3:4
    /// portrait; the 1.2 margin avoids spurious rotation on
    /// near-square frames where either orientation is fine.
    ///
    /// The direction (CW vs. CCW) is chosen to match the modal
    /// phone-in-hand case where the volume buttons point up. EXIF
    /// orientation, when we read it (not yet — there's no EXIF
    /// dependency in the workspace), should override this heuristic.
    #[must_use]
    pub fn auto_for_aspect(width: u32, height: u32) -> Self {
        if u64::from(height) * 5 >= u64::from(width) * 6 {
            Self::Deg90
        } else {
            Self::Deg0
        }
    }

    /// Degrees as a `u16`, for serialization.
    #[must_use]
    pub fn degrees(self) -> u16 {
        match self {
            Self::Deg0 => 0,
            Self::Deg90 => 90,
            Self::Deg180 => 180,
            Self::Deg270 => 270,
        }
    }

    /// Parse from degrees. Only 0/90/180/270 are accepted.
    ///
    /// # Errors
    ///
    /// Returns `Err(degrees)` for any other value.
    pub fn from_degrees(degrees: u16) -> Result<Self, u16> {
        match degrees {
            0 => Ok(Self::Deg0),
            90 => Ok(Self::Deg90),
            180 => Ok(Self::Deg180),
            270 => Ok(Self::Deg270),
            other => Err(other),
        }
    }
}

/// Rotate a row-major pixel buffer. Returns the rotated buffer plus
/// its new (width, height).
///
/// `Rotation::Deg0` returns a clone of the original buffer with
/// dimensions unchanged. The other rotations allocate a new buffer;
/// the pipeline only does this once per loaded frame so the cost is
/// amortized over all subsequent processing.
///
/// We compute by walking the destination buffer and reading the
/// corresponding source pixel — easier to reason about than
/// computing forward maps that have to invert orientation.
#[must_use]
pub fn rotate_pixels(
    pixels: &[u16],
    src_w: u32,
    src_h: u32,
    rotation: Rotation,
) -> (Vec<u16>, u32, u32) {
    match rotation {
        Rotation::Deg0 => (pixels.to_vec(), src_w, src_h),
        Rotation::Deg90 => {
            let (dst_w, dst_h) = (src_h, src_w);
            let mut out = vec![0u16; pixels.len()];
            for y in 0..dst_h {
                for x in 0..dst_w {
                    // Internal (x, y) maps from source
                    // (src_x, src_y) where src_x = y and
                    // src_y = src_h - 1 - x. Derived: 90° CW takes
                    // a source row into a destination column,
                    // reading bottom-to-top.
                    let src_x = y;
                    let src_y = src_h - 1 - x;
                    let src_idx = (src_y as usize) * (src_w as usize) + (src_x as usize);
                    let dst_idx = (y as usize) * (dst_w as usize) + (x as usize);
                    out[dst_idx] = pixels[src_idx];
                }
            }
            (out, dst_w, dst_h)
        }
        Rotation::Deg180 => {
            let mut out = vec![0u16; pixels.len()];
            for y in 0..src_h {
                for x in 0..src_w {
                    let src_x = src_w - 1 - x;
                    let src_y = src_h - 1 - y;
                    let src_idx = (src_y as usize) * (src_w as usize) + (src_x as usize);
                    let dst_idx = (y as usize) * (src_w as usize) + (x as usize);
                    out[dst_idx] = pixels[src_idx];
                }
            }
            (out, src_w, src_h)
        }
        Rotation::Deg270 => {
            let (dst_w, dst_h) = (src_h, src_w);
            let mut out = vec![0u16; pixels.len()];
            for y in 0..dst_h {
                for x in 0..dst_w {
                    // 270° CW = 90° CCW. Mirror of the Deg90 mapping.
                    let src_x = src_w - 1 - y;
                    let src_y = x;
                    let src_idx = (src_y as usize) * (src_w as usize) + (src_x as usize);
                    let dst_idx = (y as usize) * (dst_w as usize) + (x as usize);
                    out[dst_idx] = pixels[src_idx];
                }
            }
            (out, dst_w, dst_h)
        }
    }
}

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
    /// Rotation that was applied to the source pixels at load time
    /// to produce the internal frame. `Deg0` for landscape captures
    /// loaded as-is; `Deg90` etc. for portrait or otherwise-rotated
    /// captures the loader rotated to match the pipeline's
    /// "horizon-roughly-horizontal" assumption. The segmentation
    /// detector consumes this so it can re-load and re-rotate the
    /// source RGB to match the internal frame.
    pub source_rotation: Rotation,
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
            source_rotation: Rotation::Deg0,
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

    /// Record the rotation that was applied to the source pixels at
    /// load time. Defaults to [`Rotation::Deg0`] (no rotation). The
    /// segmentation detector consults this to re-rotate the
    /// re-loaded source RGB to match the internal frame.
    #[must_use]
    pub fn with_source_rotation(mut self, rotation: Rotation) -> Self {
        self.source_rotation = rotation;
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

    // -----------------------------------------------------------------
    // Rotation
    // -----------------------------------------------------------------

    #[test]
    fn rotation_round_trip_via_degrees() {
        for r in [
            Rotation::Deg0,
            Rotation::Deg90,
            Rotation::Deg180,
            Rotation::Deg270,
        ] {
            assert_eq!(Rotation::from_degrees(r.degrees()), Ok(r));
        }
        assert_eq!(Rotation::from_degrees(45), Err(45));
        assert_eq!(Rotation::from_degrees(360), Err(360));
    }

    #[test]
    fn auto_for_aspect_picks_landscape_for_landscape_input() {
        assert_eq!(Rotation::auto_for_aspect(640, 360), Rotation::Deg0);
        assert_eq!(Rotation::auto_for_aspect(1920, 1080), Rotation::Deg0);
        // Square is left as-is.
        assert_eq!(Rotation::auto_for_aspect(500, 500), Rotation::Deg0);
        // Just below the 6:5 threshold.
        assert_eq!(Rotation::auto_for_aspect(500, 599), Rotation::Deg0);
    }

    #[test]
    fn auto_for_aspect_picks_rotation_for_portrait_input() {
        // 9:16 cellphone capture
        assert_eq!(Rotation::auto_for_aspect(1080, 1920), Rotation::Deg90);
        // 3:4 phone capture
        assert_eq!(Rotation::auto_for_aspect(960, 1280), Rotation::Deg90);
        // Threshold case (h:w = 6:5)
        assert_eq!(Rotation::auto_for_aspect(500, 600), Rotation::Deg90);
        // 4:5 portrait crop (h:w = 1.25, above the 1.2 threshold).
        assert_eq!(Rotation::auto_for_aspect(400, 500), Rotation::Deg90);
    }

    #[test]
    fn rotate_pixels_zero_is_identity() {
        let pixels: Vec<u16> = (0..12).collect();
        let (out, w, h) = rotate_pixels(&pixels, 4, 3, Rotation::Deg0);
        assert_eq!((w, h), (4, 3));
        assert_eq!(out, pixels);
    }

    #[test]
    fn rotate_pixels_180_is_reverse() {
        let pixels: Vec<u16> = (0..12).collect();
        let (out, w, h) = rotate_pixels(&pixels, 4, 3, Rotation::Deg180);
        assert_eq!((w, h), (4, 3));
        let expected: Vec<u16> = (0..12).rev().collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn rotate_pixels_90_then_270_is_identity() {
        // Use a non-symmetric pattern so we'd notice a transposition
        // bug: 4 wide × 3 tall, values 0..12 row-major.
        let pixels: Vec<u16> = (0..12).collect();
        let (rot90, w90, h90) = rotate_pixels(&pixels, 4, 3, Rotation::Deg90);
        assert_eq!((w90, h90), (3, 4));
        let (rot_back, wb, hb) = rotate_pixels(&rot90, w90, h90, Rotation::Deg270);
        assert_eq!((wb, hb), (4, 3));
        assert_eq!(rot_back, pixels, "90° CW then 270° CW should be identity");
    }

    #[test]
    fn rotate_pixels_90_specific_values() {
        // 2×3 image (w=2, h=3), values:
        //   0 1
        //   2 3
        //   4 5
        // After 90° CW it should be 3×2 (w=3, h=2):
        //   4 2 0
        //   5 3 1
        // (the bottom row of the source becomes the left column of
        // the destination, top-to-bottom).
        let pixels: Vec<u16> = vec![0, 1, 2, 3, 4, 5];
        let (out, w, h) = rotate_pixels(&pixels, 2, 3, Rotation::Deg90);
        assert_eq!((w, h), (3, 2));
        assert_eq!(out, vec![4, 2, 0, 5, 3, 1]);
    }

    #[test]
    fn rotate_pixels_270_specific_values() {
        // Same source as above. 270° CW = 90° CCW, so:
        //   0 1            1 3 5
        //   2 3   becomes  0 2 4
        //   4 5
        let pixels: Vec<u16> = vec![0, 1, 2, 3, 4, 5];
        let (out, w, h) = rotate_pixels(&pixels, 2, 3, Rotation::Deg270);
        assert_eq!((w, h), (3, 2));
        assert_eq!(out, vec![1, 3, 5, 0, 2, 4]);
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn rotate_pixels_four_90s_is_identity() {
        let pixels: Vec<u16> = (0..12).collect();
        let (a, w, h) = rotate_pixels(&pixels, 4, 3, Rotation::Deg90);
        let (b, w, h) = rotate_pixels(&a, w, h, Rotation::Deg90);
        let (c, w, h) = rotate_pixels(&b, w, h, Rotation::Deg90);
        let (d, w, h) = rotate_pixels(&c, w, h, Rotation::Deg90);
        assert_eq!((w, h), (4, 3));
        assert_eq!(d, pixels);
    }

    #[test]
    fn frame_records_source_rotation() {
        let f = Frame::new(
            4,
            3,
            vec![0u16; 12],
            Tt::from_julian_date(JD_J2000),
            0,
            dummy_intrinsics(),
        )
        .unwrap();
        assert_eq!(f.source_rotation, Rotation::Deg0);
        let f2 = f.with_source_rotation(Rotation::Deg90);
        assert_eq!(f2.source_rotation, Rotation::Deg90);
    }
}
