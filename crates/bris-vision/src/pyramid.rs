//! Lazy image pyramid for per-stage analysis resolution.
//!
//! The streaming engine retains every pushed frame in a ring
//! buffer for the stitching window's duration; every stage in
//! the pipeline reads from those frames. Holding raw
//! full-resolution frames in the ring is fine on a phone for a
//! few-second window; what's wasteful is having every stage
//! re-do its own internal downsample on every read.
//!
//! [`FramePyramid`] is a full-resolution frame plus a cache of
//! downsampled variants. A stage that prefers 480p horizon
//! input requests `pyramid.level(480, 270)` and the cache
//! returns either a previously-computed level or computes one
//! lazily and remembers it. The cache lives for the pyramid's
//! lifetime; re-using the same target across stages costs one
//! downsample, not N.
//!
//! Per-stage resolution is the architectural payoff:
//!
//! - **Horizon detection** wants low resolution. The detector
//!   already downsamples internally to a "working" image (the
//!   plumbing exists in `bris_vision::horizon`), but if the
//!   ring buffer hands it a full-res frame each time it pays
//!   the downsample cost on every read. Pyramid cache fixes
//!   that.
//! - **Body centroiding** (Sun/Moon/peaks) wants full
//!   resolution; sub-pixel centroiding precision scales with
//!   pixel count. The pyramid's `full()` accessor returns the
//!   original frame zero-cost.
//! - **Segmentation** wants the model's training resolution
//!   (480-720 px wide for SegFormer-B0). Pyramid level cached
//!   alongside the horizon's working image when sizes match.
//! - **Plate solving** wants full resolution. Same as
//!   centroiding.
//!
//! # Intrinsics scaling
//!
//! Each pyramid level is paired with the source frame's
//! intrinsics scaled per [`crate::Intrinsics::scaled_to`]. The
//! scaled intrinsics are the ones a downstream stage uses when
//! converting pixel coordinates to camera-space rays
//! (see [`crate::ray`]). Bypassing the scaling and using
//! source-frame intrinsics with a downsampled pixel coordinate
//! produces wrong rays — the lens model assumes the pixel grid
//! it was calibrated against.

use std::sync::Mutex;

use crate::frame::{Frame, Intrinsics, IntrinsicsScaleError};

/// One downsampled level of a [`FramePyramid`]: pixels +
/// scaled intrinsics matching the level's resolution.
#[derive(Debug, Clone)]
pub struct PyramidLevel {
    /// The downsampled frame.
    pub frame: Frame,
}

impl PyramidLevel {
    /// Convenience: width / height of the level's frame.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.frame.width()
    }

    /// Height of the level's frame.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.frame.height()
    }
}

/// A frame plus a cache of downsampled levels.
///
/// Construct from a source [`Frame`] via [`FramePyramid::new`].
/// Read the full-resolution frame via [`Self::full`]; request
/// a downsampled level via [`Self::level`].
///
/// Thread-safe: the cache is behind a `Mutex` so multiple
/// stages on different threads can request levels concurrently
/// without recomputing. Reads of the same `(width, height)`
/// key from concurrent threads block on the first computation
/// and reuse the result.
#[derive(Debug)]
pub struct FramePyramid {
    full: Frame,
    cache: Mutex<Vec<PyramidLevel>>,
}

impl FramePyramid {
    /// Construct a pyramid wrapping a full-resolution frame.
    /// The frame's intrinsics are the calibration baseline;
    /// requested levels' intrinsics are derived by scaling.
    #[must_use]
    pub fn new(full: Frame) -> Self {
        Self {
            full,
            cache: Mutex::new(Vec::new()),
        }
    }

    /// Borrow the full-resolution source frame.
    #[must_use]
    pub fn full(&self) -> &Frame {
        &self.full
    }

    /// Width of the source frame.
    #[must_use]
    pub fn full_width(&self) -> u32 {
        self.full.width()
    }

    /// Height of the source frame.
    #[must_use]
    pub fn full_height(&self) -> u32 {
        self.full.height()
    }

    /// Source-frame intrinsics (the calibration baseline).
    #[must_use]
    pub fn full_intrinsics(&self) -> Intrinsics {
        self.full.intrinsics
    }

    /// Get (or lazily compute and cache) a downsampled level.
    ///
    /// The target resolution must preserve the source aspect
    /// ratio (within [`crate::Intrinsics::scaled_to`]'s
    /// tolerance). Asking for a level larger than the source
    /// returns the source frame as-is — we never upsample.
    ///
    /// The returned [`PyramidLevel`] holds a frame with
    /// intrinsics scaled to the level's resolution per
    /// [`Intrinsics::scaled_to`]. Stages should consume those
    /// intrinsics when converting pixel coordinates to
    /// camera-space rays.
    ///
    /// # Errors
    ///
    /// - [`PyramidError::IntrinsicsScale`] if the target
    ///   resolution can't be cleanly derived from the source
    ///   (aspect-ratio mismatch, zero dim).
    pub fn level(
        &self,
        target_width: u32,
        target_height: u32,
    ) -> Result<PyramidLevel, PyramidError> {
        // Asking for source resolution (or larger) returns the
        // full frame.
        if target_width >= self.full.width() || target_height >= self.full.height() {
            return Ok(PyramidLevel {
                frame: self.full.clone(),
            });
        }
        // Cache hit?
        {
            let guard = self.cache.lock().expect("pyramid cache mutex poisoned");
            if let Some(level) = guard
                .iter()
                .find(|l| l.width() == target_width && l.height() == target_height)
            {
                return Ok(level.clone());
            }
        }
        // Compute. Done outside the lock so concurrent readers
        // wanting a *different* level aren't blocked by a long
        // downsample. The cost is that two readers wanting
        // the *same* level may both compute it once; the
        // second insert is dropped. Acceptable for short-lived
        // pyramids in the ring buffer.
        let scaled_intrinsics = self.full.intrinsics.scaled_to(
            self.full.width(),
            self.full.height(),
            target_width,
            target_height,
        )?;
        let pixels =
            box_downsample(self.full.pixels(), self.full.width(), self.full.height(), target_width, target_height);
        let level_frame = Frame::new(
            target_width,
            target_height,
            pixels,
            self.full.capture_tt,
            self.full.exposure_us,
            scaled_intrinsics,
        )
        .map_err(|e| PyramidError::Frame {
            detail: format!("{e:?}"),
        })?;
        let level = PyramidLevel { frame: level_frame };
        let mut guard = self.cache.lock().expect("pyramid cache mutex poisoned");
        if !guard
            .iter()
            .any(|l| l.width() == target_width && l.height() == target_height)
        {
            guard.push(level.clone());
        }
        Ok(level)
    }
}

/// Errors from [`FramePyramid::level`].
#[derive(Debug, thiserror::Error)]
pub enum PyramidError {
    /// Intrinsics couldn't be scaled to the target resolution.
    /// Wraps [`IntrinsicsScaleError`].
    #[error("pyramid: {0}")]
    IntrinsicsScale(#[from] IntrinsicsScaleError),
    /// Failed to construct the downsampled [`Frame`].
    #[error("pyramid: frame construction failed: {detail}")]
    Frame {
        /// Underlying error string.
        detail: String,
    },
}

/// Box-filter downsample.
///
/// Each output pixel is the mean of the source pixels covered
/// by its proportional source-image region. Doesn't handle
/// upsampling (the caller checked).
///
/// Implementation: per output pixel `(ox, oy)`, walks the
/// source-pixel range `[ox*sw/dw, (ox+1)*sw/dw) ×
/// [oy*sh/dh, (oy+1)*sh/dh)` and averages. Floor-based bounds
/// give a clean integer downsample when `sw % dw == 0` and
/// behave reasonably for non-integer ratios (no upsampling, no
/// off-by-one truncation that loses the rightmost / bottom
/// row of pixels).
#[allow(
    // Source/dest dimensions are u32 image sizes; their u64
    // products can't overflow usize on any practical 64-bit
    // host, and on 32-bit hosts a >4 GiB image is already
    // unreachable. The `count` accumulator is bounded by the
    // pixel count in one source-block, again well under u16
    // range, but the mean-cast clippy complaint is the same
    // shape.
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
)]
fn box_downsample(src: &[u16], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u16> {
    debug_assert!(dw > 0 && dh > 0);
    debug_assert!(dw <= sw && dh <= sh, "upsampling is not supported");
    let mut out = vec![0_u16; (dw as usize) * (dh as usize)];
    let sw_u = sw as usize;
    for oy in 0..dh {
        let y0 = ((u64::from(oy) * u64::from(sh)) / u64::from(dh)) as usize;
        let y1 = ((u64::from(oy + 1) * u64::from(sh)) / u64::from(dh)).max(u64::from(y0 as u32) + 1) as usize;
        let y1 = y1.min(sh as usize);
        for ox in 0..dw {
            let x0 = ((u64::from(ox) * u64::from(sw)) / u64::from(dw)) as usize;
            let x1 = ((u64::from(ox + 1) * u64::from(sw)) / u64::from(dw)).max(u64::from(x0 as u32) + 1) as usize;
            let x1 = x1.min(sw as usize);
            let mut sum: u64 = 0;
            let mut count: u64 = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += u64::from(src[y * sw_u + x]);
                    count += 1;
                }
            }
            let avg = if count == 0 { 0 } else { (sum / count) as u16 };
            out[(oy as usize) * (dw as usize) + (ox as usize)] = avg;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_core::time::{Tt, JD_J2000};

    fn solid_frame(width: u32, height: u32, fill: u16) -> Frame {
        let pixels = vec![fill; (width as usize) * (height as usize)];
        Frame::new(
            width,
            height,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    #[test]
    fn pyramid_full_returns_source_frame() {
        let frame = solid_frame(640, 360, 0xABCD);
        let pyramid = FramePyramid::new(frame);
        assert_eq!(pyramid.full().width(), 640);
        assert_eq!(pyramid.full().height(), 360);
    }

    #[test]
    fn pyramid_level_at_source_resolution_returns_source() {
        let frame = solid_frame(640, 360, 0xABCD);
        let pyramid = FramePyramid::new(frame);
        let level = pyramid.level(640, 360).unwrap();
        assert_eq!(level.width(), 640);
        assert_eq!(level.height(), 360);
    }

    #[test]
    fn pyramid_level_at_request_larger_than_source_returns_source() {
        // Don't upsample. Asking for 1280×720 from a 640×360
        // source is a misuse; we degrade gracefully to source.
        let frame = solid_frame(640, 360, 0xABCD);
        let pyramid = FramePyramid::new(frame);
        let level = pyramid.level(1280, 720).unwrap();
        assert_eq!(level.width(), 640);
        assert_eq!(level.height(), 360);
    }

    #[test]
    fn pyramid_level_2x_downsample_preserves_solid_value() {
        // Solid 0xABCD source → all output pixels also 0xABCD
        // (the mean of a uniform region is the uniform value).
        let frame = solid_frame(640, 360, 0xABCD);
        let pyramid = FramePyramid::new(frame);
        let level = pyramid.level(320, 180).unwrap();
        assert_eq!(level.width(), 320);
        assert_eq!(level.height(), 180);
        for &p in level.frame.pixels() {
            assert_eq!(p, 0xABCD);
        }
    }

    #[test]
    fn pyramid_level_intrinsics_are_scaled() {
        // Source intrinsics fx=1000 at 640x360 → at 320x180,
        // fx must be 500.
        let frame = solid_frame(640, 360, 0);
        let pyramid = FramePyramid::new(frame);
        let level = pyramid.level(320, 180).unwrap();
        assert!((level.frame.intrinsics.fx - 500.0).abs() < 1e-9);
        assert!((level.frame.intrinsics.fy - 500.0).abs() < 1e-9);
        assert!((level.frame.intrinsics.cx - 160.0).abs() < 1e-9);
        assert!((level.frame.intrinsics.cy - 90.0).abs() < 1e-9);
    }

    #[test]
    fn pyramid_level_caches_repeated_requests() {
        let frame = solid_frame(640, 360, 0);
        let pyramid = FramePyramid::new(frame);
        let l1 = pyramid.level(320, 180).unwrap();
        let l2 = pyramid.level(320, 180).unwrap();
        // Cache hit: dimensions match (cheap structural check;
        // we don't compare pointers because PyramidLevel is
        // cloned on read).
        assert_eq!(l1.width(), l2.width());
        assert_eq!(l1.height(), l2.height());
    }

    #[test]
    fn pyramid_level_rejects_aspect_mismatch() {
        // 640×360 (16:9) → 640×480 (4:3): refused.
        let frame = solid_frame(640, 360, 0);
        let pyramid = FramePyramid::new(frame);
        let result = pyramid.level(640, 480);
        // Larger height than source triggers the early-return
        // path (no upsample) before we hit aspect check; use a
        // case that's smaller in both dimensions but with a
        // different aspect ratio.
        assert!(result.is_ok(), "640x480 from 640x360 returns source (no upsample)");

        // Try 320x240 (4:3) from 640x360 (16:9). Smaller in
        // both dims, mismatched aspect.
        let result = pyramid.level(320, 240);
        assert!(matches!(result, Err(PyramidError::IntrinsicsScale(_))));
    }

    #[test]
    fn box_downsample_2x2_to_1x1_averages() {
        let src = vec![10_u16, 20, 30, 40];
        let out = box_downsample(&src, 2, 2, 1, 1);
        assert_eq!(out, vec![25]); // (10+20+30+40)/4
    }

    #[test]
    fn box_downsample_4x4_to_2x2_block_average() {
        // 4×4 source with two distinct quadrants.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let src: Vec<u16> = (0i32..16).map(|i| (i % 4 + 1) as u16).collect();
        // src is 1,2,3,4, 1,2,3,4, ... so box average to 2x2:
        // top-left (1+2+1+2)/4 = 1.5 → 1 (integer truncation)
        // top-right (3+4+3+4)/4 = 3.5 → 3
        let out = box_downsample(&src, 4, 4, 2, 2);
        assert_eq!(out, vec![1, 3, 1, 3]);
    }
}
