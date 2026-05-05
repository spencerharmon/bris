//! Probe: run the vision pipeline against a frame and print results.
//!
//! Usage:
//!   cargo run -p bris-vision --example probe_scene -- <frame.png>
//!
//! Used to drive the corpus-pass workflow when promoting test_video/
//! scenes to regression cases. Not part of the shipped product.

use std::path::PathBuf;

use bris_core::time::{Tt, JD_J2000};
use bris_vision::{
    centroid_brightest_body, classify, detect_horizon, detect_horizon_via_sky_region,
    load_frame_from_path_with_rotation, CentroidConfig, ConditionConfig, HorizonConfig, Intrinsics,
    Rotation,
};

#[cfg(feature = "segmentation")]
use bris_vision::{detect_horizon_via_segmentation, load_model};

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
        } else {
            println!(
                "\n[horizon.segmentation]\n  skipped: model not present at {}",
                model_path.display()
            );
        }
    }
}
