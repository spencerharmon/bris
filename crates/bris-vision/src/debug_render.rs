//! Annotated PNG renderer for replay debugging.
//!
//! Given a [`Frame`] plus structured overlay data (body centroid,
//! horizon line, classification, Stage E outcomes), produces a
//! downsampled RGB PNG with visible annotations. Intended for
//! `bris replay --render-frames` and the corpus explorer.
//!
//! Drawing is deliberately primitive: a 5×7 bitmap font for text
//! and direct pixel writes for points/lines, so this module has
//! no transitive dependency beyond the `image` crate already in
//! the workspace.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    clippy::similar_names,
    clippy::too_many_arguments,
    clippy::many_single_char_names,
    clippy::needless_continue
)]

use crate::frame::Frame;
use image::{ImageError, Rgb, RgbImage};
use std::path::Path;

/// Maximum side length (pixels) of the rendered PNG's long axis.
pub const RENDER_MAX_SIDE_PX: u32 = 1200;

/// Body centroid in source-frame pixel coordinates, plus
/// drawing-relevant statistics.
#[derive(Debug, Clone, Copy)]
pub struct CentroidOverlay {
    /// X in source-frame pixels (sub-pixel).
    pub x: f64,
    /// Y in source-frame pixels.
    pub y: f64,
    /// 1σ position uncertainty in source-frame pixels.
    pub sigma_px: f64,
    /// Connected-component area (source-frame pixels).
    pub area_px: u32,
    /// Number of additional saturated bodies detected ("secondaries").
    pub secondaries: u32,
}

/// Horizon line in source-frame pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct HorizonOverlay {
    /// `y = slope * x + intercept`, source-frame pixels.
    pub slope: f64,
    /// Source-frame pixel intercept (y at x=0).
    pub intercept: f64,
    /// Provider label (e.g. `"vertical-line"`, `"gradient"`).
    /// Borrowed for the lifetime of the call.
    pub provider: &'static str,
    /// Altitude-σ (radians) from the horizon fit.
    pub sigma_rad: f64,
}

/// One Stage E sight-reduction attempt.
#[derive(Debug, Clone)]
pub enum StageEOutcomeView {
    /// Reduction succeeded.
    Ok {
        /// Observed altitude (radians).
        altitude_rad: f64,
        /// Altitude 1σ (radians).
        sigma_rad: f64,
    },
    /// Reduction failed; `kind` is a short error-variant name
    /// (e.g. `"BelowHorizon"`).
    Err {
        /// Short name of the failure kind.
        kind: String,
    },
}

/// All the bits the renderer needs to annotate one frame.
#[derive(Debug, Clone)]
pub struct OverlayData<'a> {
    /// Zero-based frame index within the capture.
    pub frame_seq: u32,
    /// Capture UTC timestamp formatted as a short string
    /// (e.g. `"2026-02-14T03:21:44.123Z"`).
    pub captured_utc: String,
    /// Lighting classification label
    /// (e.g. `"Day"`, `"Twilight"`, `"Night"`, `"Unusable"`).
    pub classification: &'a str,
    /// Centroid overlay if available.
    pub centroid: Option<CentroidOverlay>,
    /// Horizon overlay if available.
    pub horizon: Option<HorizonOverlay>,
    /// Stage E outcomes, one entry per attempted reduction.
    pub stage_e_outcomes: Vec<StageEOutcomeView>,
    /// Truncated capture id (e.g. first 8 chars of a ULID).
    pub capture_id_short: String,
    /// Truncated session id (e.g. first 8 chars of a UUID).
    pub session_id_short: String,
}

/// Render an annotated PNG and write it to `path`.
///
/// The output is RGB8. The source frame is downsampled to fit
/// [`RENDER_MAX_SIDE_PX`] on the long axis, auto-leveled into the
/// 8-bit range, and converted to grayscale RGB. Overlays are
/// drawn in the source-frame coordinate space and transformed
/// onto the downsampled canvas.
///
/// # Errors
///
/// Returns `Err` on PNG encoding / I/O failure.
pub fn render_debug_overlay(
    frame: &Frame,
    overlay: &OverlayData<'_>,
    path: &Path,
) -> Result<(), ImageError> {
    let (out_w, out_h, scale) = scaled_dims(frame.width(), frame.height(), RENDER_MAX_SIDE_PX);
    let mut img = downsample_autoleveled(frame, out_w, out_h);

    // Horizon first so the centroid marker sits on top of the
    // line where they overlap.
    if let Some(h) = overlay.horizon {
        draw_horizon(&mut img, h, scale);
    }
    if let Some(c) = overlay.centroid {
        draw_centroid(&mut img, c, scale);
    }

    draw_top_left_textblock(&mut img, overlay);
    draw_bottom_right_textblock(&mut img, overlay);

    img.save(path)
}

/// Render only the base 8-bit RGB downsample of a frame to PNG.
///
/// Idempotent + cache-friendly: produces the same PNG bytes
/// for a given frame regardless of overlay state. Replay
/// tooling uses this to write the base image once per
/// capture; per-replay overlays (horizon, centroid, HUD)
/// render client-side in the corpus explorer as SVG over
/// the cached PNG, so multi-mode replays don't pay the
/// per-frame PNG encode cost more than once.
///
/// # Errors
///
/// Returns `Err` on PNG encoding / I/O failure.
pub fn render_base_image(frame: &Frame, path: &Path) -> Result<RenderMetadata, ImageError> {
    let (out_w, out_h, scale) = scaled_dims(frame.width(), frame.height(), RENDER_MAX_SIDE_PX);
    let img = downsample_autoleveled(frame, out_w, out_h);
    img.save(path)?;
    Ok(RenderMetadata {
        source_width: frame.width(),
        source_height: frame.height(),
        canvas_width: out_w,
        canvas_height: out_h,
        scale,
    })
}

/// Geometry returned by [`render_base_image`]. The explorer
/// uses `scale` to map source-frame pixel coordinates
/// (which is what `HorizonReport` / `BodyCentroidReport`
/// carry) into canvas pixels for SVG overlay.
#[derive(Debug, Clone, Copy)]
pub struct RenderMetadata {
    /// Source frame width in pixels (before downsample).
    pub source_width: u32,
    /// Source frame height in pixels (before downsample).
    pub source_height: u32,
    /// Canvas width in pixels (≤ [`RENDER_MAX_SIDE_PX`] on long edge).
    pub canvas_width: u32,
    /// Canvas height in pixels (≤ [`RENDER_MAX_SIDE_PX`] on long edge).
    pub canvas_height: u32,
    /// Multiply source coordinates by this to land in canvas
    /// pixels: `canvas_x = source_x * scale`.
    pub scale: f64,
}

/// Compute the downsampled canvas size and the source→canvas
/// scale factor.
fn scaled_dims(src_w: u32, src_h: u32, max_side: u32) -> (u32, u32, f64) {
    let long = src_w.max(src_h).max(1);
    if long <= max_side {
        return (src_w, src_h, 1.0);
    }
    let s = f64::from(max_side) / f64::from(long);
    let out_w = ((f64::from(src_w) * s).round() as u32).max(1);
    let out_h = ((f64::from(src_h) * s).round() as u32).max(1);
    (out_w, out_h, s)
}

/// Nearest-neighbour downsample of a u16 grayscale frame to
/// `(out_w, out_h)`, auto-leveled (1st / 99th percentile sample)
/// to fill the 8-bit dynamic range. The result is an RGB image
/// (grayscale replicated to all three channels) so we can paint
/// coloured annotations on top.
fn downsample_autoleveled(frame: &Frame, out_w: u32, out_h: u32) -> RgbImage {
    let src_w = frame.width();
    let src_h = frame.height();
    let pixels = frame.pixels();

    // Sample to compute robust min/max (1st / 99th percentile).
    // Step over a sparse grid to stay cheap on large frames.
    let mut samples = Vec::with_capacity(4096);
    let stride_x = (src_w / 64).max(1);
    let stride_y = (src_h / 64).max(1);
    let mut y = 0;
    while y < src_h {
        let row = (y as usize) * (src_w as usize);
        let mut x = 0;
        while x < src_w {
            samples.push(pixels[row + (x as usize)]);
            x += stride_x;
        }
        y += stride_y;
    }
    samples.sort_unstable();
    let lo_idx = samples.len() / 100;
    let hi_idx = samples.len().saturating_sub(samples.len() / 100).max(1) - 1;
    let lo = u32::from(samples[lo_idx]);
    let hi = u32::from(samples[hi_idx]).max(lo + 1);

    let mut out = RgbImage::new(out_w, out_h);
    for oy in 0..out_h {
        // Source row this output row samples from.
        let sy = ((u64::from(oy) * u64::from(src_h)) / u64::from(out_h)) as usize;
        let src_row = sy * (src_w as usize);
        for ox in 0..out_w {
            let sx = ((u64::from(ox) * u64::from(src_w)) / u64::from(out_w)) as usize;
            let v = u32::from(pixels[src_row + sx]);
            let scaled = if v <= lo {
                0
            } else if v >= hi {
                255
            } else {
                (((v - lo) * 255) / (hi - lo)) as u8
            };
            out.put_pixel(ox, oy, Rgb([scaled, scaled, scaled]));
        }
    }
    out
}

/// Paint a single pixel if in bounds.
fn put(img: &mut RgbImage, x: i32, y: i32, color: Rgb<u8>) {
    if x < 0 || y < 0 {
        return;
    }
    let (w, h) = (img.width(), img.height());
    let (xu, yu) = (x as u32, y as u32);
    if xu >= w || yu >= h {
        return;
    }
    img.put_pixel(xu, yu, color);
}

/// Filled disk (Bresenham-style).
fn fill_disk(img: &mut RgbImage, cx: i32, cy: i32, r: i32, color: Rgb<u8>) {
    let r2 = r * r;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r2 {
                put(img, cx + dx, cy + dy, color);
            }
        }
    }
}

/// Open-circle outline (1-px ring approximation).
fn circle_outline(img: &mut RgbImage, cx: i32, cy: i32, r: i32, color: Rgb<u8>) {
    let r2 = r * r;
    let inner = (r - 1).max(0);
    let inner2 = inner * inner;
    for dy in -r..=r {
        for dx in -r..=r {
            let d2 = dx * dx + dy * dy;
            if d2 <= r2 && d2 > inner2 {
                put(img, cx + dx, cy + dy, color);
            }
        }
    }
}

/// Draw a line between two points (Bresenham).
fn draw_line(img: &mut RgbImage, x0: i32, y0: i32, x1: i32, y1: i32, color: Rgb<u8>) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;
    loop {
        put(img, x, y, color);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

fn draw_horizon(img: &mut RgbImage, h: HorizonOverlay, scale: f64) {
    let w = img.width();
    let height = img.height();
    let red = Rgb([255, 60, 60]);
    // Horizon line is in source coordinates; transform endpoints
    // (0, intercept) and (W_src-1, slope*(W_src-1)+intercept) to
    // canvas pixels. We don't know source W here, but we can map
    // canvas x → source x via scale and evaluate.
    let mut prev: Option<(i32, i32)> = None;
    for ox in 0..w {
        let sx = f64::from(ox) / scale;
        let sy = h.slope * sx + h.intercept;
        let oy = (sy * scale).round() as i32;
        if let Some((px, py)) = prev {
            draw_line(img, px, py, ox as i32, oy, red);
        }
        prev = Some((ox as i32, oy));
        if oy < 0 || oy >= height as i32 {
            continue;
        }
    }
}

fn draw_centroid(img: &mut RgbImage, c: CentroidOverlay, scale: f64) {
    let yellow = Rgb([255, 220, 0]);
    let cx = (c.x * scale).round() as i32;
    let cy = (c.y * scale).round() as i32;
    // Radius proportional to sqrt(area/π); clamp to [10,40] in
    // output pixels (post-scale).
    let raw_r_src = (f64::from(c.area_px) / std::f64::consts::PI).sqrt();
    let r = ((raw_r_src * scale).round() as i32).clamp(10, 40);
    fill_disk(img, cx, cy, r, yellow);
    // Crosshair through centroid for sub-pixel reference.
    let black = Rgb([0, 0, 0]);
    let arm = r + 6;
    draw_line(img, cx - arm, cy, cx + arm, cy, black);
    draw_line(img, cx, cy - arm, cx, cy + arm, black);
    circle_outline(img, cx, cy, r + 1, black);
}

// --------------------------------------------------------
// Bitmap font + text drawing.
// --------------------------------------------------------

/// Width of one font glyph (pixels), excluding the inter-char gap.
const FONT_W: u32 = 5;
/// Height of one font glyph (pixels).
const FONT_H: u32 = 7;
/// 1-pixel gap between glyphs.
const FONT_GAP: u32 = 1;
/// Scale factor for rendered text (each font pixel becomes an
/// N×N block in the output image). Keeps text legible at 1200-px
/// canvases.
const FONT_SCALE: u32 = 2;

/// Width of one rendered character (with gap), in canvas pixels.
const CHAR_W: u32 = (FONT_W + FONT_GAP) * FONT_SCALE;
/// Height of one rendered text row, in canvas pixels.
const LINE_H: u32 = (FONT_H + 1) * FONT_SCALE;

/// Lookup a glyph; unknown codepoints render as a filled box.
#[allow(clippy::too_many_lines)]
fn glyph(c: char) -> [u8; FONT_H as usize] {
    let upper = c.to_ascii_uppercase();
    match upper {
        ' ' => [0; 7],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        '.' => [0; 7].tap_mut(|g| g[6] = 0b00100),
        ',' => [0; 7].tap_mut(|g| {
            g[5] = 0b00100;
            g[6] = 0b01000;
        }),
        ':' => [0, 0b00100, 0, 0, 0b00100, 0, 0],
        ';' => [0, 0b00100, 0, 0, 0b00100, 0b00100, 0b01000],
        '-' => [0, 0, 0, 0b01110, 0, 0, 0],
        '+' => [0, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0],
        '=' => [0, 0, 0b11111, 0, 0b11111, 0, 0],
        '/' => [
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        '(' => [
            0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
        ],
        ')' => [
            0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
        ],
        '[' => [
            0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110,
        ],
        ']' => [
            0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110,
        ],
        '°' => [0b01100, 0b10010, 0b10010, 0b01100, 0, 0, 0],
        'σ' => [0, 0, 0b01111, 0b10100, 0b10100, 0b10100, 0b01000],
        '′' => [0b00100, 0b01000, 0, 0, 0, 0, 0],
        '_' => [0, 0, 0, 0, 0, 0, 0b11111],
        '<' => [0, 0b00010, 0b00100, 0b01000, 0b00100, 0b00010, 0],
        '>' => [0, 0b01000, 0b00100, 0b00010, 0b00100, 0b01000, 0],
        '?' => [0b01110, 0b10001, 0b00010, 0b00100, 0b00100, 0, 0b00100],
        '!' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0, 0b00100],
        _ => [0b11111; 7],
    }
}

/// Tiny extension trait so we can construct glyphs from base patterns
/// concisely above; avoids needing `let mut` for one-pixel tweaks.
trait TapMut: Sized {
    fn tap_mut<F: FnOnce(&mut Self)>(self, f: F) -> Self;
}
impl<T> TapMut for T {
    fn tap_mut<F: FnOnce(&mut Self)>(mut self, f: F) -> Self {
        f(&mut self);
        self
    }
}

fn draw_text(img: &mut RgbImage, mut x: i32, y: i32, text: &str, color: Rgb<u8>) {
    for ch in text.chars() {
        let g = glyph(ch);
        for (row_idx, row) in g.iter().enumerate() {
            for col in 0..FONT_W {
                if (row >> (FONT_W - 1 - col)) & 1 == 1 {
                    let px0 = x + (col as i32) * (FONT_SCALE as i32);
                    let py0 = y + (row_idx as i32) * (FONT_SCALE as i32);
                    for dy in 0..(FONT_SCALE as i32) {
                        for dx in 0..(FONT_SCALE as i32) {
                            put(img, px0 + dx, py0 + dy, color);
                        }
                    }
                }
            }
        }
        x += CHAR_W as i32;
    }
}

/// Fill a translucent black background behind a text block by
/// halving each pixel's intensity.
fn dim_rect(img: &mut RgbImage, x0: i32, y0: i32, w: u32, h: u32) {
    for dy in 0..h as i32 {
        for dx in 0..w as i32 {
            let x = x0 + dx;
            let y = y0 + dy;
            if x < 0 || y < 0 {
                continue;
            }
            let (xu, yu) = (x as u32, y as u32);
            if xu >= img.width() || yu >= img.height() {
                continue;
            }
            let p = img.get_pixel(xu, yu);
            img.put_pixel(xu, yu, Rgb([p[0] / 3, p[1] / 3, p[2] / 3]));
        }
    }
}

fn draw_text_block(img: &mut RgbImage, x: i32, y: i32, lines: &[String], color: Rgb<u8>) {
    let max_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u32;
    let pad = 4_i32;
    let w = max_chars * CHAR_W + (pad as u32) * 2;
    let h = (lines.len() as u32) * LINE_H + (pad as u32) * 2;
    dim_rect(img, x, y, w, h);
    for (i, line) in lines.iter().enumerate() {
        draw_text(
            img,
            x + pad,
            y + pad + (i as i32) * (LINE_H as i32),
            line,
            color,
        );
    }
}

fn classify_stage_e(outcomes: &[StageEOutcomeView]) -> String {
    if outcomes.is_empty() {
        return "No body candidate".to_string();
    }
    let oks: Vec<&StageEOutcomeView> = outcomes
        .iter()
        .filter(|o| matches!(o, StageEOutcomeView::Ok { .. }))
        .collect();
    if let Some(StageEOutcomeView::Ok {
        altitude_rad,
        sigma_rad,
    }) = oks.first().copied()
    {
        let alt_deg = altitude_rad.to_degrees();
        let sig_arcmin = sigma_rad.to_degrees() * 60.0;
        return format!("Sight emitted (alt={alt_deg:.2}° σ={sig_arcmin:.2}′)");
    }
    // All errors: summarize by most common kind.
    let mut counts: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    for o in outcomes {
        if let StageEOutcomeView::Err { kind } = o {
            *counts.entry(kind.as_str()).or_insert(0) += 1;
        }
    }
    let total: u32 = counts.values().sum();
    if let Some((kind, _count)) = counts.iter().max_by_key(|(_, &c)| c) {
        return format!("Stage E: {total}/{n} rejected ({kind})", n = outcomes.len());
    }
    format!("Stage E: {} attempts", outcomes.len())
}

fn draw_top_left_textblock(img: &mut RgbImage, overlay: &OverlayData<'_>) {
    let stage_e = classify_stage_e(&overlay.stage_e_outcomes);
    let mut lines = vec![
        format!("frame {}", overlay.frame_seq),
        format!("utc {}", overlay.captured_utc),
        format!("class {}", overlay.classification),
    ];
    if let Some(c) = overlay.centroid {
        lines.push(format!(
            "centroid x={} y={} sigma={:.2}px area={}px2",
            c.x.round() as i64,
            c.y.round() as i64,
            c.sigma_px,
            c.area_px
        ));
    } else {
        lines.push("centroid: none".to_string());
    }
    if let Some(h) = overlay.horizon {
        lines.push(format!(
            "horizon intercept={:.1} slope={:.4} provider={} sigma={:.4}rad",
            h.intercept, h.slope, h.provider, h.sigma_rad
        ));
    } else {
        lines.push("horizon: none".to_string());
    }
    lines.push(stage_e);
    draw_text_block(img, 8, 8, &lines, Rgb([255, 255, 255]));
}

fn draw_bottom_right_textblock(img: &mut RgbImage, overlay: &OverlayData<'_>) {
    let lines = vec![format!(
        "cap {} sess {}",
        overlay.capture_id_short, overlay.session_id_short
    )];
    let max_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as i32;
    let pad = 4_i32;
    let w = (max_chars * (CHAR_W as i32)) + pad * 2;
    let h = (lines.len() as i32) * (LINE_H as i32) + pad * 2;
    let x = img.width() as i32 - w - 8;
    let y = img.height() as i32 - h - 8;
    draw_text_block(img, x, y, &lines, Rgb([220, 220, 220]));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Intrinsics;
    use bris_core::time::{Tt, JD_J2000};

    fn synthetic_frame(w: u32, h: u32) -> Frame {
        // Gradient frame to give the auto-leveler something to do.
        let mut pixels = vec![0u16; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = ((x + y) * 256) as u16;
                pixels[(y as usize) * (w as usize) + (x as usize)] = v;
            }
        }
        Frame::new(
            w,
            h,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(w, h),
        )
        .unwrap()
    }

    #[test]
    fn render_writes_png_of_expected_size() {
        let frame = synthetic_frame(2400, 1600);
        let overlay = OverlayData {
            frame_seq: 0,
            captured_utc: "2026-01-01T00:00:00Z".to_string(),
            classification: "Day",
            centroid: None,
            horizon: None,
            stage_e_outcomes: Vec::new(),
            capture_id_short: "abcd1234".to_string(),
            session_id_short: "deadbeef".to_string(),
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("png");
        render_debug_overlay(&frame, &overlay, &path).unwrap();
        let img = image::open(&path).unwrap();
        // 2400x1600 → 1200x800 at max 1200 on long axis.
        assert_eq!(img.width(), 1200);
        assert_eq!(img.height(), 800);
    }

    #[test]
    fn centroid_draws_a_yellow_filled_disc_at_scaled_coords() {
        // 1200x800 source (no scaling), centroid at (600, 400).
        let w = 1200;
        let h = 800;
        let frame = synthetic_frame(w, h);
        let overlay = OverlayData {
            frame_seq: 0,
            captured_utc: "x".to_string(),
            classification: "Day",
            centroid: Some(CentroidOverlay {
                x: 600.0,
                y: 400.0,
                sigma_px: 0.5,
                area_px: 314, // pi * r=10 → r=10 src px → clamped to 10
                secondaries: 0,
            }),
            horizon: None,
            stage_e_outcomes: Vec::new(),
            capture_id_short: "a".to_string(),
            session_id_short: "b".to_string(),
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("png");
        render_debug_overlay(&frame, &overlay, &path).unwrap();
        let img = image::open(&path).unwrap().to_rgb8();
        // Crosshair through dead-centre is black; sample a
        // pixel just inside the disc but off-axis.
        let pixel = img.get_pixel(605, 403);
        assert!(
            pixel[0] > 200 && pixel[1] > 150 && pixel[2] < 60,
            "off-centre disc pixel should be yellow-ish, got {pixel:?}"
        );
    }

    #[test]
    fn horizon_draws_red_pixels_along_the_line() {
        // Source 1200x800; horizon y = 0*x + 400 (flat line).
        let w = 1200;
        let h = 800;
        let frame = synthetic_frame(w, h);
        let overlay = OverlayData {
            frame_seq: 0,
            captured_utc: "x".to_string(),
            classification: "Day",
            centroid: None,
            horizon: Some(HorizonOverlay {
                slope: 0.0,
                intercept: 400.0,
                provider: "vertical-line",
                sigma_rad: 0.001,
            }),
            stage_e_outcomes: Vec::new(),
            capture_id_short: "a".to_string(),
            session_id_short: "b".to_string(),
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("png");
        render_debug_overlay(&frame, &overlay, &path).unwrap();
        let img = image::open(&path).unwrap().to_rgb8();
        // Sample a few canvas-x columns along the line. Skip
        // x < 200 to avoid the top-left text block.
        for x in (300..1100).step_by(100) {
            let pixel = img.get_pixel(x, 400);
            assert!(
                pixel[0] > 200 && pixel[1] < 120 && pixel[2] < 120,
                "expected red horizon pixel at ({x}, 400), got {pixel:?}"
            );
        }
    }
}
