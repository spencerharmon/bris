//! Probe: run the vision pipeline against a frame and print results.
//!
//! Usage:
//!   `cargo run -p bris-vision --example probe_scene -- <frame.png>`
//!
//! Used to drive the corpus-pass workflow when promoting `test_video/`
//! scenes to regression cases. Not part of the shipped product.

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::path::PathBuf;

use bris_core::time::{Tt, JD_J2000};
use bris_vision::{
    centroid_brightest_body, centroid_saturated_body_in_mask, classify, detect_horizon,
    detect_horizon_via_sky_region, detect_peaks, load_frame_from_path_with_rotation,
    CentroidConfig, ConditionConfig, HorizonConfig, Intrinsics, PeakConfig, Rotation,
    SaturatedBodyConfig,
};

#[cfg(feature = "segmentation")]
use bris_vision::{detect_horizon_via_segmentation, load_model, segment_with_rotation};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: probe_scene <frame.png> [--rotate 0|90|180|270]");
        std::process::exit(1);
    }
    let path = PathBuf::from(&args[1]);
    let mut rotation = Rotation::Deg0;
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--rotate" && i + 1 < args.len() {
            let deg: u16 = args[i + 1].parse().expect("rotate value");
            rotation = Rotation::from_degrees(deg).expect("0|90|180|270");
            i += 2;
        } else {
            eprintln!("unknown arg: {}", args[i]);
            std::process::exit(1);
        }
    }

    let (src_w, src_h) = image::image_dimensions(&path).expect("dims");
    let (post_w, post_h) = match rotation {
        Rotation::Deg0 | Rotation::Deg180 => (src_w, src_h),
        Rotation::Deg90 | Rotation::Deg270 => (src_h, src_w),
    };
    let intr = Intrinsics::placeholder(post_w, post_h);
    let frame = load_frame_from_path_with_rotation(
        &path,
        Tt::from_julian_date(JD_J2000),
        0,
        intr,
        rotation,
    )
    .expect("load frame")
    .with_source_path(path.clone());

    println!(
        "scene = {}\nsource = {}x{} rotation = {}° -> internal = {}x{}",
        path.display(),
        src_w,
        src_h,
        rotation.degrees(),
        frame.width(),
        frame.height(),
    );

    // Pixel statistics.
    let max_pixel = frame.pixels().iter().copied().max().unwrap_or(0);
    let saturated_pixels = frame.pixels().iter().filter(|&&p| p >= 62258).count();
    println!(
        "\n[pixel stats]\n  max = {} ({:.3} of u16::MAX)\n  saturated_pixels (>= 95%) = {}",
        max_pixel,
        f64::from(max_pixel) / f64::from(u16::MAX),
        saturated_pixels
    );

    // Interactive: dump pixel values around a few coordinates of interest.
    if std::env::var("PROBE_DUMP").is_ok() {
        for &(cx, cy) in &[(99u32, 48u32), (117, 54)] {
            let v = frame.pixel(cx, cy).unwrap_or(0);
            println!(
                "  pixel ({cx}, {cy}) = {v} ({:.3} of u16::MAX)",
                f64::from(v) / f64::from(u16::MAX)
            );
            // 7×7 neighborhood max.
            let mut nmax = 0u16;
            for dy in -3i32..=3 {
                for dx in -3i32..=3 {
                    let nx = cx as i32 + dx;
                    let ny = cy as i32 + dy;
                    if nx >= 0 && ny >= 0 {
                        nmax = nmax.max(frame.pixel(nx as u32, ny as u32).unwrap_or(0));
                    }
                }
            }
            println!("  7×7 max around ({cx}, {cy}) = {nmax}");
        }
    }

    // Classifier (image-only).
    let cls = classify(&frame, None, ConditionConfig::default());
    println!(
        "\n[classifier]\n  condition = {:?}\n  confidence = {:.3}\n  mean_luma = {:.4}\n  saturated_fraction = {:.4}",
        cls.condition, cls.confidence, cls.image_evidence.mean_luma, cls.image_evidence.saturated_fraction
    );

    // Centroid (extended-disk).
    println!("\n[centroid (default config)]");
    match centroid_brightest_body(&frame, CentroidConfig::default()) {
        Ok(c) => println!(
            "  Ok: x = {:.2}, y = {:.2}, area_px = {}, mean_intensity = {:.0}",
            c.x, c.y, c.area_px, c.mean_intensity
        ),
        Err(e) => println!("  Err: {e}"),
    }

    // Saturated-body centroid, unmasked.
    println!("\n[centroid_saturated_body_in_mask (no mask)]");
    match centroid_saturated_body_in_mask(&frame, SaturatedBodyConfig::default(), None) {
        Ok(c) => println!(
            "  Ok: x = {:.2}, y = {:.2}, area_px = {}, mean_intensity = {:.0}",
            c.x, c.y, c.area_px, c.mean_intensity
        ),
        Err(e) => println!("  Err: {e}"),
    }

    // Top peaks (for body localization comparison).
    println!("\n[detect_peaks (default config) — top 5]");
    let peaks = detect_peaks(&frame, PeakConfig::default());
    if peaks.is_empty() {
        println!("  no peaks above threshold");
    } else {
        for (i, p) in peaks.iter().take(5).enumerate() {
            println!(
                "  #{}: x = {:.2}, y = {:.2}, intensity = {:.0}",
                i, p.x, p.y, p.intensity
            );
        }
    }

    // Horizon detectors.
    println!("\n[horizon.gradient]");
    match detect_horizon(&frame, HorizonConfig::default()) {
        Ok(line) => println!(
            "  Ok: slope = {:.4}, intercept = {:.2}, inliers = {} / {} candidates, RMS = {:.2} px",
            line.slope,
            line.intercept,
            line.inlier_count,
            line.candidate_count,
            line.residual_rms_px
        ),
        Err(e) => println!("  Err: {e}"),
    }

    println!("\n[horizon.sky_region]");
    match detect_horizon_via_sky_region(&frame, HorizonConfig::default()) {
        Ok(line) => println!(
            "  Ok: slope = {:.4}, intercept = {:.2}, inliers = {} / {} candidates, RMS = {:.2} px",
            line.slope,
            line.intercept,
            line.inlier_count,
            line.candidate_count,
            line.residual_rms_px
        ),
        Err(e) => println!("  Err: {e}"),
    }

    #[cfg(feature = "segmentation")]
    {
        let model_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join("segmentation.onnx");
        if model_path.exists() {
            load_model(&model_path).expect("load model");
            println!("\n[horizon.segmentation]");
            match detect_horizon_via_segmentation(&frame, HorizonConfig::default()) {
                Ok(line) => println!(
                    "  Ok: slope = {:.4}, intercept = {:.2}, inliers = {} / {} candidates, RMS = {:.2} px",
                    line.slope, line.intercept, line.inlier_count, line.candidate_count, line.residual_rms_px
                ),
                Err(e) => println!("  Err: {e}"),
            }

            // Saturated-body centroid inside sky mask.
            println!("\n[centroid_saturated_body_in_mask (sky mask via segmentation)]");
            match segment_with_rotation(&path, frame.source_rotation) {
                Ok(mask) => {
                    let sky = mask.sky_mask(frame.width(), frame.height());
                    let non_vessel = mask.non_vessel_mask(frame.width(), frame.height());
                    let sky_count: usize = sky.iter().filter(|&&b| b).count();
                    let non_vessel_count: usize = non_vessel.iter().filter(|&&b| b).count();
                    println!(
                        "  mask sizes: sky = {sky_count}, non_vessel = {non_vessel_count}, total = {}",
                        sky.len()
                    );
                    if std::env::var("PROBE_DUMP").is_ok() {
                        for &(cx, cy) in &[(99u32, 48u32), (117, 54)] {
                            let idx = (cy as usize) * (frame.width() as usize) + (cx as usize);
                            println!(
                                "    mask at ({cx}, {cy}): sky = {}, non_vessel = {}",
                                sky[idx], non_vessel[idx]
                            );
                        }
                    }
                    println!("  with sky mask:");
                    for &(label, thresh) in &[
                        ("0.95", (u32::from(u16::MAX) * 95 / 100) as u16),
                        ("0.98", (u32::from(u16::MAX) * 98 / 100) as u16),
                        ("0.99", (u32::from(u16::MAX) * 99 / 100) as u16),
                    ] {
                        let cfg = SaturatedBodyConfig {
                            saturation_threshold: thresh,
                            min_area_px: 20,
                        };
                        match centroid_saturated_body_in_mask(&frame, cfg, Some(&sky)) {
                            Ok(c) => println!(
                                "    thresh {label}: x = {:.2}, y = {:.2}, area_px = {}",
                                c.x, c.y, c.area_px
                            ),
                            Err(e) => println!("    thresh {label}: Err: {e}"),
                        }
                    }
                    println!("  with non-vessel mask:");
                    for &(label, thresh) in &[
                        ("0.95", (u32::from(u16::MAX) * 95 / 100) as u16),
                        ("0.98", (u32::from(u16::MAX) * 98 / 100) as u16),
                    ] {
                        let cfg = SaturatedBodyConfig {
                            saturation_threshold: thresh,
                            min_area_px: 20,
                        };
                        match centroid_saturated_body_in_mask(&frame, cfg, Some(&non_vessel)) {
                            Ok(c) => println!(
                                "    thresh {label}: x = {:.2}, y = {:.2}, area_px = {}",
                                c.x, c.y, c.area_px
                            ),
                            Err(e) => println!("    thresh {label}: Err: {e}"),
                        }
                    }
                }
                Err(e) => println!("  segment err: {e}"),
            }
        } else {
            println!(
                "\n[horizon.segmentation]\n  skipped: model not present at {}",
                model_path.display()
            );
        }
    }
}
