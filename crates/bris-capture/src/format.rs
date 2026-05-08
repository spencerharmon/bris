//! Pixel-format conversion to the engine's grayscale `u16`
//! frame format.
//!
//! Bris's vision pipeline operates on 16-bit grayscale
//! ([`bris_vision::Frame`]). Cameras produce frames in a wide
//! variety of formats (YUYV, MJPEG, NV12, RAW Bayer, …); the
//! capture shell converts at ingest so the rest of the engine
//! sees a uniform format.
//!
//! # Why u16 grayscale specifically
//!
//! See `crates/bris-vision/src/frame.rs` (the `# Bit depth`
//! and `# Pixel format` sections of the module docstring).
//! Briefly:
//!
//! - Color channels carry no celestial-navigation information
//!   our algorithms can use.
//! - 16-bit headroom matters for sub-pixel centroiding on
//!   saturated bodies and for contrast headroom on low-light
//!   star fields.
//!
//! Cameras that natively produce 8-bit luminance (most USB
//! webcams via YUYV) are widened on the way in by
//! left-shifting; cameras producing 10/12-bit raw can pass
//! their values through.
//!
//! # Currently implemented
//!
//! - **YUYV (YUV 4:2:2)** — the most common USB-webcam format.
//!   The Y plane is already luminance; we widen u8→u16 by
//!   replicating the high byte into the low byte (so 0x00 →
//!   0x0000, 0xFF → 0xFFFF; preserves the relative scale
//!   exactly).
//!
//! Future work (separate commits as the need arises):
//! MJPEG (decode + Y extract), NV12 (Y plane widen), RAW
//! Bayer (demosaic + luminance).

use thiserror::Error;

/// Errors converting a camera frame to the engine's u16
/// grayscale format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FormatError {
    /// Input buffer length doesn't match what the declared
    /// dimensions and format would require. Indicates a
    /// driver bug or a misconfiguration; never user-actionable
    /// without intervention.
    #[error(
        "input buffer length {actual} doesn't match expected {expected} \
         for {format} at {width}×{height}"
    )]
    BufferSizeMismatch {
        /// The pixel format that was supposed to produce
        /// this buffer.
        format: &'static str,
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
        /// Buffer length the format requires.
        expected: usize,
        /// Actual buffer length received from the driver.
        actual: usize,
    },
    /// Format requires even width but width is odd. YUYV
    /// (and other 4:2:2 / 4:2:0 formats) pack pairs of pixels
    /// in a single byte group; odd widths are undefined.
    /// Cameras configured to YUYV always report even widths in
    /// practice.
    #[error("{format} requires even width but width is {width} (odd)")]
    OddWidth {
        /// The pixel format whose parity invariant was
        /// violated.
        format: &'static str,
        /// The offending width.
        width: u32,
    },
}

/// Convert a YUYV (YUV 4:2:2) frame to grayscale `u16`.
///
/// YUYV stores two pixels in four bytes: `Y0 U0 Y1 V0` where
/// Y0 and Y1 are the luminance values for the two pixels and
/// U0/V0 are the shared chrominance. Bris discards the
/// chrominance channels at the capture boundary; only the Y
/// values feed downstream.
///
/// The widening from 8-bit Y to 16-bit grayscale replicates
/// the high byte into the low byte (`y << 8 | y`). This
/// preserves the relative-brightness scale exactly:
/// 0x00 → 0x0000, 0xFF → 0xFFFF, 0x80 → 0x8080. A simple
/// `(y as u16) << 8` would map 0xFF → 0xFF00 instead of
/// `u16::MAX`, leaving an artificial 0xFF gap below the
/// saturation point — bad for the saturation-threshold-driven
/// body centroider.
///
/// # Errors
///
/// - [`FormatError::BufferSizeMismatch`] if `bytes.len() != 2
///   × width × height`.
/// - [`FormatError::OddWidth`] if `width` is odd. YUYV packs
///   two pixels per 4-byte group, so the format is undefined
///   for odd widths; cameras configured to YUYV always report
///   even widths in practice.
pub fn yuyv_to_grayscale_u16(
    bytes: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u16>, FormatError> {
    if !width.is_multiple_of(2) {
        return Err(FormatError::OddWidth { format: "YUYV", width });
    }
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or(FormatError::BufferSizeMismatch {
            format: "YUYV",
            width,
            height,
            expected: usize::MAX,
            actual: bytes.len(),
        })?;
    let expected = pixel_count.checked_mul(2).ok_or(FormatError::BufferSizeMismatch {
        format: "YUYV",
        width,
        height,
        expected: usize::MAX,
        actual: bytes.len(),
    })?;
    if bytes.len() != expected {
        return Err(FormatError::BufferSizeMismatch {
            format: "YUYV",
            width,
            height,
            expected,
            actual: bytes.len(),
        });
    }
    let mut out = Vec::with_capacity(pixel_count);
    // Step 4 bytes (= 2 pixels) at a time. Width's even-ness
    // (validated above) guarantees the buffer is a whole
    // number of 4-byte groups.
    for chunk in bytes.chunks_exact(4) {
        let y0 = chunk[0];
        let y1 = chunk[2];
        out.push(widen_u8_to_u16(y0));
        out.push(widen_u8_to_u16(y1));
    }
    Ok(out)
}

/// Widen an 8-bit luminance value to 16-bit by replicating
/// the high byte into the low byte.
///
/// `0x00 → 0x0000`, `0xFF → 0xFFFF`, `0x80 → 0x8080`. This
/// preserves both the zero point and the saturation point
/// exactly, which a simple `(y as u16) << 8` would not (it
/// caps at `0xFF00`, leaving a 0xFF gap below `u16::MAX`).
#[inline]
#[must_use]
pub fn widen_u8_to_u16(y: u8) -> u16 {
    let y16 = u16::from(y);
    (y16 << 8) | y16
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn widen_preserves_endpoints() {
        assert_eq!(widen_u8_to_u16(0x00), 0x0000);
        assert_eq!(widen_u8_to_u16(0xFF), 0xFFFF);
        assert_eq!(widen_u8_to_u16(0x80), 0x8080);
    }

    proptest! {
        /// Widen is monotonic — `a ≤ b ⇒ widen(a) ≤ widen(b)` —
        /// and full-range — covers `[0, u16::MAX]` exactly at
        /// the byte endpoints. Both properties matter for the
        /// downstream saturation-threshold-driven body detector.
        #[test]
        fn widen_monotonic(a in 0u8..=255, b in 0u8..=255) {
            if a <= b {
                prop_assert!(widen_u8_to_u16(a) <= widen_u8_to_u16(b));
            }
        }

        /// Widening followed by narrowing (high byte) is the
        /// identity. Important because tests and downstream
        /// consumers may want to round-trip back to 8-bit
        /// for visualization.
        #[test]
        fn widen_high_byte_round_trips(y in 0u8..=255) {
            let w = widen_u8_to_u16(y);
            // High byte recovers the original.
            #[allow(clippy::cast_possible_truncation)]
            let recovered = (w >> 8) as u8;
            prop_assert_eq!(recovered, y);
        }
    }

    #[test]
    fn yuyv_extracts_y_values_in_order() {
        // Two YUYV pixels: pixel 0 has Y=0x40, pixel 1 has
        // Y=0xC0; chrominance bytes (U0, V0) are arbitrary
        // and must not affect the output.
        let yuyv: Vec<u8> = vec![0x40, 0x12, 0xC0, 0x34];
        let g = yuyv_to_grayscale_u16(&yuyv, 2, 1).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0], widen_u8_to_u16(0x40));
        assert_eq!(g[1], widen_u8_to_u16(0xC0));
    }

    #[test]
    fn yuyv_handles_2x2_frame() {
        // 2 wide, 2 tall: 8 YUYV bytes total (2 bytes/pixel ×
        // 4 pixels). Build a frame with Y values 0x10, 0x20,
        // 0x30, 0x40 reading row-major.
        let yuyv: Vec<u8> = vec![
            0x10, 0x00, 0x20, 0x00, // row 0: Y0=0x10, Y1=0x20
            0x30, 0x00, 0x40, 0x00, // row 1: Y0=0x30, Y1=0x40
        ];
        let g = yuyv_to_grayscale_u16(&yuyv, 2, 2).unwrap();
        assert_eq!(g.len(), 4);
        assert_eq!(g[0], widen_u8_to_u16(0x10));
        assert_eq!(g[1], widen_u8_to_u16(0x20));
        assert_eq!(g[2], widen_u8_to_u16(0x30));
        assert_eq!(g[3], widen_u8_to_u16(0x40));
    }

    #[test]
    fn yuyv_rejects_size_mismatch() {
        // Declared 2×1 wants 4 bytes; we provide 6.
        let yuyv = vec![0u8; 6];
        let err = yuyv_to_grayscale_u16(&yuyv, 2, 1).unwrap_err();
        assert!(matches!(
            err,
            FormatError::BufferSizeMismatch {
                format: "YUYV",
                expected: 4,
                actual: 6,
                ..
            }
        ));
    }

    #[test]
    fn yuyv_zero_width_or_height_rejected_with_size_mismatch() {
        // 0×0 demands 0 bytes; supplying anything else is a
        // mismatch. (Zero dimensions are also rejected at the
        // Frame constructor; this test just confirms the
        // converter doesn't panic on them.)
        let err = yuyv_to_grayscale_u16(&[0; 4], 0, 0).unwrap_err();
        assert!(matches!(err, FormatError::BufferSizeMismatch { expected: 0, actual: 4, .. }));
    }

    #[test]
    fn yuyv_rejects_odd_width() {
        // Odd width: chunks_exact(4) would silently discard
        // trailing bytes and produce the wrong pixel count.
        // Reject up front instead.
        let err = yuyv_to_grayscale_u16(&[0; 6], 3, 1).unwrap_err();
        assert!(
            matches!(
                err,
                FormatError::OddWidth { format: "YUYV", width: 3 }
            ),
            "expected OddWidth, got {err:?}"
        );
    }

    proptest! {
        /// Round-trip property: an arbitrary YUYV buffer
        /// converts to a grayscale buffer of exactly
        /// width×height u16 values.
        #[test]
        fn yuyv_output_has_correct_pixel_count(
            width in 1u32..=64,
            height in 1u32..=64,
        ) {
            // YUYV requires even width (two pixels per
            // 4-byte group). Round up to the nearest even.
            let effective_width = if width.is_multiple_of(2) { width } else { width + 1 };
            let pixel_count = (effective_width * height) as usize;
            let bytes = vec![0u8; pixel_count * 2];
            let g = yuyv_to_grayscale_u16(&bytes, effective_width, height).unwrap();
            prop_assert_eq!(g.len(), pixel_count);
        }
    }
}
