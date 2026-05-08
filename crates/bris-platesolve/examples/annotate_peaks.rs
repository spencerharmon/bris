//! Annotate detected peaks on a frame and save the result, so a
//! human can visually verify whether they correspond to real
//! stars or to spurious features (JPEG blocks, vignetting,
//! etc.).
//!
//! Reads a PNG, runs the full peak-detection pipeline (with
//! horizon masking when a horizon is found), draws a small
//! red square around each peak, and saves to /tmp.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::uninlined_format_args,
    clippy::doc_markdown
)]

use bris_core::time::{Tt, JD_J2000};
use bris_vision::{
    detect_horizon_night_multi_pass, detect_peaks, detect_peaks_above_horizon,
    load_frame_from_path, Intrinsics, NightHorizonConfig, PeakConfig,
};
use image::Rgb;

fn main() {
    let path_str = std::env::args()
        .nth(1)
        .expect("usage: annotate_peaks <frame.png>");
    let path = std::path::Path::new(&path_str);
    let dims = image::image_dimensions(path).unwrap();
    let intr = Intrinsics::placeholder(dims.0, dims.1);

    let frame = load_frame_from_path(path, Tt::from_julian_date(JD_J2000), 0, intr).unwrap();

    // Optional horizon masking: try the night detector; fall
    // back to unmasked if it produces nothing.
    let mut horizons = detect_horizon_night_multi_pass(&frame, NightHorizonConfig::default(), None);
    horizons.sort_by_key(|h| std::cmp::Reverse(h.inlier_count));
    let horizon = horizons.first().copied();
    if let Some(h) = horizon {
        eprintln!(
            "horizon: slope={:.4} intercept={:.2} inliers={}",
            h.slope, h.intercept, h.inlier_count
        );
    } else {
        eprintln!("horizon: NONE (running peak detection unmasked)");
    }
    let min_intensity: u16 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let peak_cfg = PeakConfig {
        min_intensity,
        ..PeakConfig::default()
    };
    eprintln!("min_intensity: {min_intensity}");
    let peaks = match horizon {
        Some(h) => detect_peaks_above_horizon(&frame, peak_cfg, h, 5),
        None => detect_peaks(&frame, peak_cfg),
    };
    eprintln!("{} peaks detected", peaks.len());

    // Load original as RGB for annotation.
    let mut img = image::open(path).unwrap().to_rgb8();

    // Draw horizon line in green if present.
    if let Some(h) = horizon {
        let g = Rgb([0u8, 255, 0]);
        for x in 0..img.width() {
            let y = (h.slope * f64::from(x) + h.intercept).round() as i32;
            for dy in -1..=1 {
                let yy = y + dy;
                if yy >= 0 && yy < img.height() as i32 {
                    img.put_pixel(x, yy as u32, g);
                }
            }
        }
    }

    // Draw peaks in red. Top-12 (used by plate_solve) get a
    // larger box; remaining peaks get a smaller one.
    for (i, p) in peaks.iter().enumerate() {
        let r: i32 = if i < 12 { 8 } else { 4 };
        let cx = p.x.round() as i32;
        let cy = p.y.round() as i32;
        let color = if i < 12 {
            Rgb([255u8, 0, 0])
        } else {
            Rgb([255u8, 200, 0])
        };
        for dy in -r..=r {
            for dx in -r..=r {
                // Square outline only.
                if dy.abs() != r && dx.abs() != r {
                    continue;
                }
                let x = cx + dx;
                let y = cy + dy;
                if x >= 0 && x < img.width() as i32 && y >= 0 && y < img.height() as i32 {
                    img.put_pixel(x as u32, y as u32, color);
                }
            }
        }
    }

    let out = "/tmp/annotated_peaks.png";
    img.save(out).unwrap();
    eprintln!("saved {out}");
    eprintln!(
        "(red boxes: top-12 peaks fed to plate_solve; orange: other peaks; green line: horizon)"
    );
}
