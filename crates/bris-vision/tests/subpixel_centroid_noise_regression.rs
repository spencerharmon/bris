#![allow(
    clippy::similar_names,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_arguments,
    clippy::doc_markdown
)]

//! Statistical test: over 100 noisy synthetic frames with a
//! saturated plateau, slightly asymmetric Gaussian halo, and
//! Poisson-approximated photon noise, refined-centroid mean
//! position error must beat the integer centroid's.
//!
//! Uses a seeded inline LCG + Box-Muller Gaussian — no
//! external RNG dependency (rand / rand_distr are not in the
//! workspace and the centroid-refine PR must not add deps).

use bris_core::time::{Tt, JD_J2000};
use bris_vision::{
    extract_halo_pixels, extract_multi_saturated_centroids, refine_centroid_subpixel, Frame,
    Intrinsics, SaturatedBodyConfig, DEFAULT_GAIN_E_PER_ADU,
};

/// Seeded LCG (Numerical Recipes constants) — deterministic
/// per-trial RNG without bringing in `rand`.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }
    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }
    /// Uniform in (0, 1).
    fn unit(&mut self) -> f64 {
        // Avoid 0 so log() in Box-Muller is finite.
        let v = self.next_u32();
        (f64::from(v) + 1.0) / (f64::from(u32::MAX) + 2.0)
    }
    /// Standard normal via Box-Muller (one sample per call).
    fn gauss(&mut self) -> f64 {
        let u1 = self.unit();
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (2.0 * core::f64::consts::PI * u2).cos()
    }
}

/// Poisson-noised, saturated-plateau, slightly anisotropic
/// Gaussian body with halo. Uses Gaussian approximation
/// (Poisson(μ) ≈ N(μ, μ) for μ ≳ 10), with a small read-noise
/// term. Output clipped to 1023 ADU at the centre (10-bit
/// sensor ceiling, deliberately below u16::MAX so the
/// saturation gate triggers cleanly).
fn synth_noisy_frame(
    w: u32,
    h: u32,
    cx: f64,
    cy: f64,
    sigma_x: f64,
    sigma_y: f64,
    peak: f64,
    bg: f64,
    sat_adu: u16,
    rng: &mut Lcg,
) -> Frame {
    let mut px = vec![0u16; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let dx = f64::from(x) - cx;
            let dy = f64::from(y) - cy;
            let mean = bg
                + peak
                    * (-(dx * dx / (2.0 * sigma_x * sigma_x)
                        + dy * dy / (2.0 * sigma_y * sigma_y)))
                        .exp();
            // Poisson ≈ N(mean, mean) for mean ≳ 10 — sound
            // here since bg alone is well above 10.
            let noisy = mean + mean.sqrt() * rng.gauss() + 2.0 * rng.gauss();
            let clipped = noisy.clamp(0.0, f64::from(sat_adu));
            px[(y as usize) * (w as usize) + (x as usize)] = clipped as u16;
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
fn refined_beats_integer_over_100_noisy_trials() {
    let cx_true = 60.37_f64;
    let cy_true = 50.62_f64;
    let sigma_x = 4.0_f64;
    let sigma_y = 4.8_f64; // ~20% anisotropy
    let peak = 5_000.0_f64; // well above 1023 ⇒ saturated core
    let bg = 50.0_f64;
    let sat_adu: u16 = 1023;

    let cfg = SaturatedBodyConfig {
        saturation_threshold: sat_adu,
        min_area_px: 5,
    };

    let mut sum_err_int = 0.0_f64;
    let mut sum_err_ref = 0.0_f64;
    let mut refined_count = 0_u32;
    let trials = 100_u32;

    for seed in 0..trials {
        let mut rng = Lcg::new(u64::from(seed) + 1);
        let frame = synth_noisy_frame(
            120, 100, cx_true, cy_true, sigma_x, sigma_y, peak, bg, sat_adu, &mut rng,
        );
        let Ok(cs) = extract_multi_saturated_centroids(&frame, cfg, None) else {
            continue;
        };
        let Some(primary) = cs.into_iter().next() else {
            continue;
        };
        let int_err = ((primary.x - cx_true).powi(2) + (primary.y - cy_true).powi(2)).sqrt();
        sum_err_int += int_err;

        let halo = extract_halo_pixels(&frame, primary, sat_adu, 18);
        let r = refine_centroid_subpixel(&frame, primary, &halo, DEFAULT_GAIN_E_PER_ADU);
        let ref_err = if r.refined {
            refined_count += 1;
            ((r.x - cx_true).powi(2) + (r.y - cy_true).powi(2)).sqrt()
        } else {
            int_err
        };
        sum_err_ref += ref_err;
    }

    let mean_int = sum_err_int / f64::from(trials);
    let mean_ref = sum_err_ref / f64::from(trials);
    assert!(
        refined_count >= trials * 3 / 4,
        "expected refinement to converge on most trials, got {refined_count}/{trials}"
    );
    assert!(
        mean_ref < mean_int,
        "refined mean error ({mean_ref:.4}) should beat integer ({mean_int:.4}) \
         over {trials} trials (refined on {refined_count})"
    );
}
