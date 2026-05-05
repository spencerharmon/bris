//! Frame I/O: load PNG/JPEG/PPM into Bris's `Frame`, save raw frames
//! for offline replay.
//!
//! Bris's vision pipeline operates on grayscale `u16`. The `image`
//! crate handles decoding and color-to-luminance conversion at the
//! boundary; nothing else in the pipeline needs to know about file
//! formats.
//!
//! # Color → luminance
//!
//! ITU-R BT.709 luminance: `Y = 0.2126·R + 0.7152·G + 0.0722·B`.
//! The 8-bit input is widened to 16-bit by left-shifting 8 (multiply
//! by 257) so `0xFF` maps to `0xFFFF`. This preserves dynamic range
//! for downstream centroiding and peak detection that assume the
//! pipeline's full u16 dynamic range.
//!
//! For 16-bit input (PNG can carry 16-bit gray or 48-bit RGB) we use
//! the channels as-is.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use crate::frame::{rotate_pixels, Frame, FrameError, Intrinsics, Rotation};
use bris_core::time::Tt;
use std::path::Path;

/// Errors loading a frame from disk.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// I/O or decode failure from the `image` crate.
    #[error("image decode failed: {0}")]
    Decode(#[from] image::ImageError),
    /// Decoded image had zero width or height, or pixel buffer
    /// length didn't match.
    #[error("frame construction failed: {0}")]
    Frame(#[from] FrameError),
}

/// Load a frame from a path with no rotation. Convenience wrapper
/// over [`load_frame_from_path_with_rotation`] for callers that
/// only deal with landscape input.
///
/// Color images are converted to grayscale via BT.709 luma. The
/// loaded frame's `source_rotation` is [`Rotation::Deg0`].
///
/// `capture_tt` and `intrinsics` come from external metadata (saved
/// alongside the frame, or set by the caller). The image file itself
/// carries no astronomical metadata.
///
/// # Errors
///
/// See [`LoadError`].
pub fn load_frame_from_path<P: AsRef<Path>>(
    path: P,
    capture_tt: Tt,
    exposure_us: u32,
    intrinsics: Intrinsics,
) -> Result<Frame, LoadError> {
    load_frame_from_path_with_rotation(path, capture_tt, exposure_us, intrinsics, Rotation::Deg0)
}

/// Load a frame from a path, applying the given rotation to the
/// pixel buffer at load time.
///
/// `intrinsics` must describe the camera in the *internal*
/// (post-rotation) frame. For the default placeholder intrinsics,
/// pass dimensions in post-rotation order (i.e. for 90°/270°
/// rotation of a portrait source, swap source width and height).
/// The loaded frame records the applied rotation in
/// [`Frame::source_rotation`].
///
/// # Errors
///
/// See [`LoadError`].
pub fn load_frame_from_path_with_rotation<P: AsRef<Path>>(
    path: P,
    capture_tt: Tt,
    exposure_us: u32,
    intrinsics: Intrinsics,
    rotation: Rotation,
) -> Result<Frame, LoadError> {
    let dyn_img = image::open(path.as_ref())?;
    let (src_w, src_h) = (dyn_img.width(), dyn_img.height());
    let pixels = decode_to_luma16(dyn_img);
    let (rotated, w, h) = rotate_pixels(&pixels, src_w, src_h, rotation);
    let frame = Frame::new(w, h, rotated, capture_tt, exposure_us, intrinsics)?
        .with_source_rotation(rotation);
    Ok(frame)
}

/// Decode any [`image::DynamicImage`] variant down to row-major
/// `Vec<u16>` BT.709 luminance. Extracted from
/// [`load_frame_from_path_with_rotation`] so the rotation step has
/// a clean buffer to operate on.
fn decode_to_luma16(dyn_img: image::DynamicImage) -> Vec<u16> {
    match dyn_img {
        image::DynamicImage::ImageLuma8(buf) => widen_8_to_16(buf.into_raw()),
        image::DynamicImage::ImageLuma16(buf) => buf.into_raw(),
        image::DynamicImage::ImageRgb8(buf) => rgb8_to_luma16(&buf.into_raw()),
        image::DynamicImage::ImageRgb16(buf) => rgb16_to_luma16(&buf.into_raw()),
        image::DynamicImage::ImageLumaA8(buf) => {
            let raw = buf.into_raw();
            // Drop the alpha channel.
            widen_8_to_16(raw.iter().step_by(2).copied().collect())
        }
        image::DynamicImage::ImageLumaA16(buf) => {
            let raw = buf.into_raw();
            raw.iter().step_by(2).copied().collect()
        }
        image::DynamicImage::ImageRgba8(buf) => {
            // Drop alpha; convert to luma.
            let raw = buf.into_raw();
            let rgb: Vec<u8> = raw
                .chunks_exact(4)
                .flat_map(|c| [c[0], c[1], c[2]])
                .collect();
            rgb8_to_luma16(&rgb)
        }
        image::DynamicImage::ImageRgba16(buf) => {
            let raw = buf.into_raw();
            let rgb: Vec<u16> = raw
                .chunks_exact(4)
                .flat_map(|c| [c[0], c[1], c[2]])
                .collect();
            rgb16_to_luma16(&rgb)
        }
        // Other formats (Rgb32F, etc.) are exotic; convert via the
        // crate's helper to Luma8 then widen.
        other => widen_8_to_16(other.to_luma8().into_raw()),
    }
}

/// Widen u8 pixels to u16 by replicating the high byte to the low byte.
/// `0xFF → 0xFFFF`, `0x00 → 0x0000`, monotonic and gap-free.
fn widen_8_to_16(buf: Vec<u8>) -> Vec<u16> {
    buf.into_iter().map(|v| u16::from(v) * 257).collect()
}

/// Convert RGB8 → BT.709 luma → u16. 8-bit input widened as in
/// [`widen_8_to_16`].
fn rgb8_to_luma16(rgb: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(rgb.len() / 3);
    for chunk in rgb.chunks_exact(3) {
        let r = f64::from(chunk[0]);
        let g = f64::from(chunk[1]);
        let b = f64::from(chunk[2]);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let y_u8 = y.round().clamp(0.0, 255.0) as u16;
        out.push(y_u8 * 257);
    }
    out
}

/// Convert RGB16 → BT.709 luma → u16, preserving the 16-bit range.
fn rgb16_to_luma16(rgb: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(rgb.len() / 3);
    for chunk in rgb.chunks_exact(3) {
        let r = f64::from(chunk[0]);
        let g = f64::from(chunk[1]);
        let b = f64::from(chunk[2]);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        out.push(y.round().clamp(0.0, f64::from(u16::MAX)) as u16);
    }
    out
}

/// Save a frame as a 16-bit grayscale PNG. Useful for capturing real
/// frames from a webcam and re-running them through `bris replay`
/// later.
///
/// # Errors
///
/// Returns `Err` on I/O or PNG-encoding failure.
pub fn save_frame_as_png<P: AsRef<Path>>(frame: &Frame, path: P) -> Result<(), image::ImageError> {
    let buf = image::ImageBuffer::<image::Luma<u16>, _>::from_raw(
        frame.width(),
        frame.height(),
        frame.pixels().to_vec(),
    )
    .expect("frame dimensions match pixel buffer length by construction");
    buf.save_with_format(path.as_ref(), image::ImageFormat::Png)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_core::time::JD_J2000;

    fn placeholder_intrinsics(w: u32, h: u32) -> Intrinsics {
        Intrinsics::placeholder(w, h)
    }

    #[test]
    fn round_trip_grayscale_png() {
        let mut pixels = vec![0u16; 32 * 24];
        for (i, p) in pixels.iter_mut().enumerate() {
            // Modulo to stay in u16 range; the pattern is arbitrary,
            // we just need the data to round-trip exactly.
            *p = ((i * 100) % 65_535) as u16;
        }
        let frame = Frame::new(
            32,
            24,
            pixels.clone(),
            Tt::from_julian_date(JD_J2000),
            1000,
            placeholder_intrinsics(32, 24),
        )
        .unwrap();

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("png");
        save_frame_as_png(&frame, &path).unwrap();

        let loaded = load_frame_from_path(
            &path,
            Tt::from_julian_date(JD_J2000),
            1000,
            placeholder_intrinsics(32, 24),
        )
        .unwrap();
        assert_eq!(loaded.width(), 32);
        assert_eq!(loaded.height(), 24);
        assert_eq!(loaded.pixels(), pixels.as_slice());
    }

    #[test]
    fn rgb_png_converted_to_luma() {
        // Synthesize a small RGB image: a green horizontal stripe over
        // a red background. After luma conversion the green stripe
        // should be brighter than the red region (BT.709 weights green
        // at 0.7152 vs red at 0.2126).
        let w = 16u32;
        let h = 8u32;
        let mut buf = image::ImageBuffer::<image::Rgb<u8>, _>::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let pixel = if y < 4 {
                    image::Rgb([255u8, 0, 0]) // red
                } else {
                    image::Rgb([0u8, 255, 0]) // green
                };
                buf.put_pixel(x, y, pixel);
            }
        }
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("png");
        buf.save_with_format(&path, image::ImageFormat::Png)
            .unwrap();

        let loaded = load_frame_from_path(
            &path,
            Tt::from_julian_date(JD_J2000),
            1000,
            placeholder_intrinsics(w, h),
        )
        .unwrap();
        let red_pixel = loaded.pixel(0, 0).unwrap();
        let green_pixel = loaded.pixel(0, 5).unwrap();
        assert!(
            green_pixel > red_pixel,
            "green ({green_pixel}) should be brighter than red ({red_pixel}) under BT.709"
        );
    }

    #[test]
    fn missing_file_returns_error() {
        let r = load_frame_from_path(
            "/this/path/does/not/exist.png",
            Tt::from_julian_date(JD_J2000),
            0,
            placeholder_intrinsics(1, 1),
        );
        assert!(r.is_err());
    }

    #[test]
    fn load_with_rotation_90_swaps_dims_and_records_rotation() {
        // Synthesize a small grayscale image: 4 wide × 2 tall.
        // After 90° CW load it should be 2 wide × 4 tall, and the
        // frame should record source_rotation = Deg90.
        let buf = image::ImageBuffer::<image::Luma<u8>, _>::from_fn(4, 2, |x, y| {
            image::Luma([(x + y * 4) as u8])
        });
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("png");
        buf.save_with_format(&path, image::ImageFormat::Png)
            .unwrap();

        let loaded = load_frame_from_path_with_rotation(
            &path,
            Tt::from_julian_date(JD_J2000),
            0,
            placeholder_intrinsics(2, 4), // post-rotation dims
            Rotation::Deg90,
        )
        .unwrap();
        assert_eq!(loaded.width(), 2);
        assert_eq!(loaded.height(), 4);
        assert_eq!(loaded.source_rotation, Rotation::Deg90);

        // Verify pixel mapping. Source row 1 (y=1, values 4 5 6 7)
        // becomes destination column 0 (x=0, top-to-bottom). Each
        // source pixel was widened by ×257.
        // After 90° CW with our convention:
        //   src (0,1)=4 → dst (0,0)
        //   src (0,0)=0 → dst (0,1)? — no, our convention rotates so
        //   the bottom-left source pixel becomes the top-left
        //   destination pixel. Let's just check that the four corners
        //   land where rotate_pixels said they would.
        // Reuse rotate_pixels for the expectation so this test is
        // about the loader-vs-rotator wiring, not the rotation math
        // itself (which has its own dedicated tests in frame.rs).
        let src_pixels: Vec<u16> = (0..8u16).map(|v| v * 257).collect();
        let (expected, _, _) = rotate_pixels(&src_pixels, 4, 2, Rotation::Deg90);
        assert_eq!(loaded.pixels(), expected.as_slice());
    }

    #[test]
    fn load_with_rotation_zero_matches_unrotated_loader() {
        let buf = image::ImageBuffer::<image::Luma<u8>, _>::from_fn(4, 2, |x, y| {
            image::Luma([(x + y * 4) as u8])
        });
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("png");
        buf.save_with_format(&path, image::ImageFormat::Png)
            .unwrap();

        let a = load_frame_from_path(
            &path,
            Tt::from_julian_date(JD_J2000),
            0,
            placeholder_intrinsics(4, 2),
        )
        .unwrap();
        let b = load_frame_from_path_with_rotation(
            &path,
            Tt::from_julian_date(JD_J2000),
            0,
            placeholder_intrinsics(4, 2),
            Rotation::Deg0,
        )
        .unwrap();
        assert_eq!(a.pixels(), b.pixels());
        assert_eq!(a.width(), b.width());
        assert_eq!(a.height(), b.height());
    }
}
