#![allow(clippy::similar_names, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

//! Integration: sub-pixel centroid refinement must produce a
//! tighter position σ than the integer-fallback floor on a
//! well-sampled saturated synthetic disk.
//!
//! The unit tests in `centroid_refine.rs` cover correctness on
//! clean Gaussians; this integration test exercises the full
//! `extract_multi_saturated_centroids` → `extract_halo_pixels`
//! → `refine_centroid_subpixel` chain that Stage A actually
//! drives, against a saturated-disk-plus-halo synth (the
//! Moon-over-water failure mode the corpus exposed).

use bris_core::time::{Tt, JD_J2000};
use bris_vision::{
    extract_halo_pixels, extract_multi_saturated_centroids, refine_centroid_subpixel, Frame,
    Intrinsics, SaturatedBodyConfig,
};

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn synth_saturated_with_halo(
    w: u32,
    h: u32,
    cx: f64,
    cy: f64,
    sat_radius: f64,
    halo_sigma: f64,
) -> Frame {
    let peak = 2.0e5_f64;
    let bg = 800.0_f64;
    let mut px = vec![0u16; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let dx = f64::from(x) - cx;
            let dy = f64::from(y) - cy;
            let r2 = dx * dx + dy * dy;
            let v = if r2 <= sat_radius * sat_radius {
                f64::from(u16::MAX)
            } else {
                let g = peak * (-r2 / (2.0 * halo_sigma * halo_sigma)).exp() + bg;
                g.clamp(0.0, f64::from(u16::MAX))
            };
            px[(y as usize) * (w as usize) + (x as usize)] = v as u16;
        }
    }
    Frame::new(
        w,
        h,
        px,
        Tt::from_julian_date(JD_J2000),
        1_000,
        Intrinsics::placeholder(w, h),
    )
    .unwrap()
}

#[test]
fn subpixel_refinement_beats_integer_sigma_on_saturated_disk() {
    let cx_true = 200.37_f64;
    let cy_true = 150.62_f64;
    let frame = synth_saturated_with_halo(400, 300, cx_true, cy_true, 8.0, 6.0);
    let cfg = SaturatedBodyConfig {
        saturation_threshold: (u32::from(u16::MAX) * 95 / 100) as u16,
        min_area_px: 50,
    };
    let primary = extract_multi_saturated_centroids(&frame, cfg, None)
        .unwrap()
        .into_iter()
        .next()
        .expect("must find the saturated disk");

    let integer_sigma = primary.position_sigma_px.value();
    assert!(
        integer_sigma >= 0.5,
        "integer centroid σ has 0.5 px bias floor; got {integer_sigma}"
    );

    let halo = extract_halo_pixels(&frame, primary, cfg.saturation_threshold, 30);
    assert!(
        halo.len() >= 50,
        "expected a generous halo, got {}",
        halo.len()
    );

    let refined = refine_centroid_subpixel(&frame, primary, &halo);
    assert!(refined.refined, "Gaussian fit must converge on this synth");

    // Position recovered to within a fraction of a pixel — far
    // better than the integer centroid.
    assert!(
        (refined.x - cx_true).abs() < 0.5,
        "x off by {} (true {cx_true}, got {})",
        (refined.x - cx_true).abs(),
        refined.x
    );
    assert!(
        (refined.y - cy_true).abs() < 0.5,
        "y off by {} (true {cy_true}, got {})",
        (refined.y - cy_true).abs(),
        refined.y
    );

    // The whole point: refined σ must be tighter than integer.
    assert!(
        refined.sigma_x_px < integer_sigma,
        "refined σ_x ({}) should be tighter than integer ({})",
        refined.sigma_x_px,
        integer_sigma
    );
    assert!(
        refined.sigma_y_px < integer_sigma,
        "refined σ_y ({}) should be tighter than integer ({})",
        refined.sigma_y_px,
        integer_sigma
    );
}
