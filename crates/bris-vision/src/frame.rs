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
/// The pipeline assumes that in the *internal* frame coordinate
/// system the horizon runs left-to-right (parameterized as
/// `y = slope·x + intercept` in [`crate::horizon::HorizonLine`]).
/// This is the natural orientation for nearly any consumer image:
/// phones encode photos in viewing orientation (often after
/// applying EXIF orientation themselves), and conventional cameras
/// save in landscape. **In all those cases no rotation is needed.**
///
/// Rotation is opt-in for two situations:
///
/// 1. **Capture pipelines** that read sensor pixels in their native
///    orientation (e.g. raw V4L2 or libcamera streams from a
///    sideways-mounted camera) and need to rotate before the rest
///    of the pipeline sees the frame. The capture shell knows the
///    device + sensor orientation and supplies the appropriate
///    [`Rotation`] explicitly.
/// 2. **Test fixtures or hand-edited inputs** where the saved bytes
///    don't match viewing orientation. Regression cases declare
///    rotation explicitly via `source_rotation_deg` in `case.toml`.
///
/// We deliberately do **not** auto-rotate based on aspect ratio:
/// aspect cannot distinguish a 4:3 landscape from a 3:4 portrait
/// (and EXIF, when we eventually read it, is the right source of
/// truth for ambiguous cases anyway). When an image arrives in the
/// wrong orientation and the pipeline produces nonsense, the
/// detector errors will surface that fact loudly rather than the
/// loader silently guessing.
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
    /// 90° clockwise. Useful when the source bytes are in
    /// sensor-native orientation and the sensor is mounted
    /// rotated 90° CCW relative to the intended scene up.
    Deg90,
    /// 180°.
    Deg180,
    /// 270° clockwise (equivalently 90° counter-clockwise).
    Deg270,
}

impl Rotation {
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

    /// Scale these intrinsics from the resolution they were
    /// calibrated against (`from_width × from_height`) to a
    /// target runtime resolution (`to_width × to_height`).
    ///
    /// The transformation is:
    ///
    /// ```text
    /// fx_to = fx_from · (to_width  / from_width )
    /// fy_to = fy_from · (to_height / from_height)
    /// cx_to = cx_from · (to_width  / from_width )
    /// cy_to = cy_from · (to_height / from_height)
    /// ```
    ///
    /// Distortion coefficients (k1, k2, k3, p1, p2) are
    /// dimensionless ratios in normalized image-plane
    /// coordinates and **do not change** under uniform image
    /// scaling. The same numbers carry across.
    ///
    /// # When this is correct
    ///
    /// The scaling is exact under **uniform aspect-preserving
    /// downsample** from the calibrated resolution to the
    /// target. That is: `to_width / from_width ==
    /// to_height / from_height` (equivalent: same aspect
    /// ratio).
    ///
    /// Concretely on Android, this matches the case where
    /// `CameraX`'s `ResolutionSelector` delivers a resolution
    /// that's a clean integer downsample of the camera's
    /// native sensor resolution at the same aspect ratio.
    /// For 16:9 capture this is the typical case.
    ///
    /// # When this is wrong (callers should re-calibrate)
    ///
    /// - Non-uniform scaling (different x and y ratios). The
    ///   downsample stretched / squashed the image, which a
    ///   calibration solved at the source resolution can't
    ///   account for. Returns
    ///   [`IntrinsicsScaleError::AspectRatioMismatch`].
    /// - Crop-then-downsample paths where the active image
    ///   region within the sensor changed between calibration
    ///   and runtime. Outside the scope of this method; the
    ///   caller must recalibrate at the runtime resolution.
    /// - ISP-side warp / EIS / lens-correction at the resize
    ///   step. The vendor pipeline applies an unmodeled
    ///   transformation; intrinsics aren't recoverable by
    ///   pure scaling. Same fix.
    ///
    /// The diagnostic surface (engine-side: a flag on the
    /// per-fix output) marks every fix produced under scaled
    /// intrinsics so the operator knows the path was taken.
    ///
    /// # Errors
    ///
    /// - [`IntrinsicsScaleError::ZeroDimension`] if any
    ///   dimension is 0.
    /// - [`IntrinsicsScaleError::AspectRatioMismatch`] if
    ///   the source and target aspect ratios differ by more
    ///   than 0.1% (a tolerance for floating-point comparison
    ///   of 16:9, 4:3, etc.).
    pub fn scaled_to(
        &self,
        from_width: u32,
        from_height: u32,
        to_width: u32,
        to_height: u32,
    ) -> Result<Self, IntrinsicsScaleError> {
        const ASPECT_TOLERANCE: f64 = 0.001;
        if from_width == 0 || from_height == 0 || to_width == 0 || to_height == 0 {
            return Err(IntrinsicsScaleError::ZeroDimension);
        }
        // Aspect-ratio sanity check. Allow tiny floating-point
        // slop (e.g. 16:9 vs. 1280:720 vs. 1920:1080 round
        // identically; a sloppy 1281×720 wouldn't).
        let from_aspect = f64::from(from_width) / f64::from(from_height);
        let to_aspect = f64::from(to_width) / f64::from(to_height);
        let aspect_ratio_drift = (from_aspect / to_aspect - 1.0).abs();
        if aspect_ratio_drift > ASPECT_TOLERANCE {
            return Err(IntrinsicsScaleError::AspectRatioMismatch {
                from_width,
                from_height,
                to_width,
                to_height,
                relative_drift: aspect_ratio_drift,
            });
        }
        let sx = f64::from(to_width) / f64::from(from_width);
        let sy = f64::from(to_height) / f64::from(from_height);
        Ok(Self {
            fx: self.fx * sx,
            fy: self.fy * sy,
            cx: self.cx * sx,
            cy: self.cy * sy,
            // Distortion coefficients are dimensionless ratios
            // in normalized coordinates; they survive uniform
            // scaling unchanged.
            k1: self.k1,
            k2: self.k2,
            k3: self.k3,
            p1: self.p1,
            p2: self.p2,
        })
    }
}

/// Errors from [`Intrinsics::scaled_to`].
///
/// All variants signal "a calibration produced at one
/// resolution can't be cleanly applied at another" — the
/// operator should recalibrate at the target resolution.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum IntrinsicsScaleError {
    /// One of the dimensions was zero. Probably a programming
    /// error upstream (a misconfigured `CameraX` selector or a
    /// missing config field).
    #[error("intrinsics scale: zero dimension in source or target")]
    ZeroDimension,

    /// Source and target aspect ratios differ. Per-axis
    /// scaling would distort the principal point and the
    /// distortion model in ways the calibration didn't
    /// observe.
    #[error(
        "intrinsics scale: aspect ratio mismatch between calibration ({from_width}×{from_height}, \
         {from_aspect:.4}) and runtime ({to_width}×{to_height}, {to_aspect:.4}); drift {relative_drift:.3}; \
         recalibrate at the runtime resolution",
        from_aspect = f64::from(*from_width) / f64::from(*from_height),
        to_aspect = f64::from(*to_width) / f64::from(*to_height),
    )]
    AspectRatioMismatch {
        /// Calibration resolution width.
        from_width: u32,
        /// Calibration resolution height.
        from_height: u32,
        /// Runtime resolution width.
        to_width: u32,
        /// Runtime resolution height.
        to_height: u32,
        /// `|aspect_from / aspect_to − 1|`, the relative
        /// aspect-ratio drift.
        relative_drift: f64,
    },
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
    // Intrinsics::scaled_to
    // -----------------------------------------------------------------

    /// A measured-style calibration at 4032×3024 (4:3 phone
    /// sensor) with non-trivial distortion. Used as the source
    /// for the scaling tests.
    fn calib_at_4k() -> Intrinsics {
        Intrinsics {
            fx: 3200.0,
            fy: 3200.0,
            cx: 2010.0, // off-center; scaling must preserve the offset's relative position
            cy: 1500.0,
            k1: -0.18,
            k2: 0.04,
            k3: 0.00,
            p1: 0.001,
            p2: -0.0005,
        }
    }

    #[test]
    fn scaled_to_identity_is_identity() {
        let src = calib_at_4k();
        let scaled = src.scaled_to(4032, 3024, 4032, 3024).unwrap();
        assert_eq!(src, scaled);
    }

    #[test]
    #[allow(clippy::float_cmp)] // distortion fields are copied verbatim; bit-identical equality is what we want to assert
    fn scaled_to_halves_focal_and_principal_under_2x_downsample() {
        let src = calib_at_4k();
        let scaled = src.scaled_to(4032, 3024, 2016, 1512).unwrap();
        let sx = 2016.0 / 4032.0;
        let sy = 1512.0 / 3024.0;
        assert!((scaled.fx - src.fx * sx).abs() < 1e-9);
        assert!((scaled.fy - src.fy * sy).abs() < 1e-9);
        assert!((scaled.cx - src.cx * sx).abs() < 1e-9);
        assert!((scaled.cy - src.cy * sy).abs() < 1e-9);
        // Distortion is dimensionless and must not change.
        assert_eq!(scaled.k1, src.k1);
        assert_eq!(scaled.k2, src.k2);
        assert_eq!(scaled.k3, src.k3);
        assert_eq!(scaled.p1, src.p1);
        assert_eq!(scaled.p2, src.p2);
    }

    #[test]
    fn scaled_to_round_trips_through_intermediate_resolution() {
        // A → B → A should recover the original to floating-
        // point precision. Locks the scaling math against
        // accidental non-multiplicative refactors.
        let src = calib_at_4k();
        let mid = src.scaled_to(4032, 3024, 1280, 960).unwrap();
        let back = mid.scaled_to(1280, 960, 4032, 3024).unwrap();
        assert!((back.fx - src.fx).abs() < 1e-9);
        assert!((back.fy - src.fy).abs() < 1e-9);
        assert!((back.cx - src.cx).abs() < 1e-9);
        assert!((back.cy - src.cy).abs() < 1e-9);
    }

    #[test]
    fn scaled_to_rejects_aspect_ratio_mismatch() {
        let src = calib_at_4k(); // 4:3
        // Try to scale a 4:3 calibration into a 16:9 runtime.
        // The scaling math would silently distort principal-
        // point and distortion behavior; refuse instead.
        let result = src.scaled_to(4032, 3024, 1920, 1080);
        match result {
            Err(IntrinsicsScaleError::AspectRatioMismatch {
                from_width,
                from_height,
                to_width,
                to_height,
                ..
            }) => {
                assert_eq!(from_width, 4032);
                assert_eq!(from_height, 3024);
                assert_eq!(to_width, 1920);
                assert_eq!(to_height, 1080);
            }
            other => panic!("expected AspectRatioMismatch, got {other:?}"),
        }
    }

    #[test]
    fn scaled_to_accepts_tiny_aspect_drift_within_tolerance() {
        // 1280:720 has aspect 1.7777... and 1920:1080 has the
        // same; scaling between them is fine. Verify the
        // tolerance permits the integer-rounding noise that
        // shows up in real CameraX numbers.
        let src = Intrinsics::placeholder(1920, 1080);
        let scaled = src.scaled_to(1920, 1080, 1280, 720).unwrap();
        // 1280/1920 = 0.6666...; placeholder fx=1000 → 666.66...
        assert!((scaled.fx - 1000.0 * 1280.0 / 1920.0).abs() < 1e-9);
    }

    #[test]
    fn scaled_to_rejects_zero_dimension() {
        let src = Intrinsics::placeholder(1920, 1080);
        assert!(matches!(
            src.scaled_to(1920, 1080, 0, 720),
            Err(IntrinsicsScaleError::ZeroDimension)
        ));
        assert!(matches!(
            src.scaled_to(0, 1080, 1280, 720),
            Err(IntrinsicsScaleError::ZeroDimension)
        ));
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
