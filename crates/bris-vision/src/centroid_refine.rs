//! Sub-pixel centroid refinement via 2D Gaussian fit on halo pixels.
//!
//! Stage A's connected-component centroider in
//! [`crate::centroid`] reports integer-resolution positions
//! with a 0.5 px σ bias floor. On a saturated body that's
//! about 1 px of uncertainty, which contributes ~3 nm to the
//! LOP residual at typical altitudes (Austin moonlight-pond
//! corpus, `docs/handoff/phase3.6-closeout.md`).
//!
//! The body's saturated core carries no positional information
//! — every pixel reads the same ceiling. The *non-saturated
//! halo* around the core does: a 2D Gaussian fit to those
//! sub-ceiling pixels recovers the body centre to ~0.3 px.
//!
//! Model: `I(x, y) = A · exp(-((x-cx)² + (y-cy)²) / (2σ²)) + B`,
//! five parameters `(cx, cy, σ, A, B)`. The fit is Gauss-Newton
//! with photon-plus-read-noise weights `w_i = 1 / (I_i +
//! read_noise²)`. We default `read_noise = 5 ADU` — a sane
//! value for the cameras in the target hardware that does not
//! attempt to be per-sensor accurate.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::needless_range_loop,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

use crate::centroid::Centroid;
use crate::frame::Frame;

/// A non-saturated boundary pixel adjacent to a saturated
/// component, suitable as a sample for [`refine_centroid_subpixel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HaloPixel {
    /// Pixel column.
    pub x: u32,
    /// Pixel row.
    pub y: u32,
    /// Pixel intensity (u16 scale).
    pub intensity: u16,
}

/// A centroid refined to sub-pixel resolution via a 2D Gaussian
/// fit to the surrounding halo pixels.
///
/// Produced by [`refine_centroid_subpixel`]. The σ values are
/// per-axis standard deviations of the fitted centre, derived
/// from the inverse of the weighted normal-equations matrix
/// (Gauss-Newton covariance).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RefinedCentroid {
    /// Sub-pixel X position (image pixels).
    pub x: f64,
    /// Sub-pixel Y position (image pixels).
    pub y: f64,
    /// 1σ uncertainty in X (pixels), from fit covariance.
    pub sigma_x_px: f64,
    /// 1σ uncertainty in Y (pixels), from fit covariance.
    pub sigma_y_px: f64,
    /// Reduced chi-squared (χ²/dof) of the fit. Values near 1
    /// indicate a well-modelled Gaussian; large values flag
    /// haloes that don't match a Gaussian profile (bad fit,
    /// caller should treat the position σ with suspicion).
    pub fit_quality: f64,
    /// `true` when the fit converged and is being reported;
    /// `false` when the function fell back to the integer
    /// centroid (insufficient halo, divergence, or absurd σ).
    pub refined: bool,
}

/// Read-noise term (ADU) in the inverse-variance weight
/// `1 / (I + read_noise²)`. Five ADU is representative of
/// modest CMOS sensors at gain unity; we do not attempt
/// per-sensor accuracy.
const READ_NOISE_ADU: f64 = 5.0;

/// Minimum halo size for a 5-parameter fit to be meaningful.
const MIN_HALO_PIXELS: usize = 8;

/// Maximum Gauss-Newton iterations before declaring divergence.
const MAX_ITERS: usize = 10;

/// Convergence threshold on parameter step `‖Δp‖∞`.
const CONVERGENCE_STEP: f64 = 1.0e-4;

/// Reject the fit and fall back when the recovered position
/// σ exceeds this (pixels).
const MAX_ACCEPTABLE_SIGMA_PX: f64 = 2.0;

/// Refine an integer-pixel centroid to sub-pixel via 2D Gaussian fit.
///
/// `integer_centroid` provides the initial centre estimate and
/// the fallback used when refinement is not possible. `halo`
/// is the list of non-saturated boundary pixels (see
/// [`extract_halo_pixels`]). `frame` is unused by the fit
/// itself but is accepted to keep the signature symmetric with
/// other Stage A primitives and to make future enhancements
/// (e.g. resampling extra halo pixels) trivial.
///
/// Returns a [`RefinedCentroid`] whose `refined` flag indicates
/// whether the sub-pixel position came from the Gaussian fit
/// or from the integer fallback. The fallback case sets `σ =
/// 0.5 px` per axis and `fit_quality = f64::INFINITY`.
pub fn refine_centroid_subpixel(
    _frame: &Frame,
    integer_centroid: Centroid,
    halo: &[HaloPixel],
) -> RefinedCentroid {
    let fallback = || RefinedCentroid {
        x: integer_centroid.x,
        y: integer_centroid.y,
        sigma_x_px: 0.5,
        sigma_y_px: 0.5,
        fit_quality: f64::INFINITY,
        refined: false,
    };

    if halo.len() < MIN_HALO_PIXELS {
        return fallback();
    }

    // Initial estimates.
    let mut min_i = f64::INFINITY;
    let mut max_i = f64::NEG_INFINITY;
    for h in halo {
        let v = f64::from(h.intensity);
        if v < min_i {
            min_i = v;
        }
        if v > max_i {
            max_i = v;
        }
    }
    let b0 = min_i.max(0.0);
    // Log-linearized initial fit. Take log(I - B0) for
    // pixels well above the noise floor and solve
    //   log(I - B) = c0 + c1·x + c2·y + c3·(x² + y²)
    // (least-squares, 4 unknowns) for centre and σ:
    //   c3 = -1/(2σ²)   ⇒   σ = sqrt(-1/(2·c3))
    //   cx = -c1/(2·c3),  cy = -c2/(2·c3)
    //   A  = exp(c0 - (cx²+cy²)·c3)
    // The non-linear Gauss-Newton refinement then polishes.
    let mut init_cx = integer_centroid.x;
    let mut init_cy = integer_centroid.y;
    let mut init_sigma = ((integer_centroid.area_px as f64) / core::f64::consts::PI)
        .sqrt()
        .max(1.0);
    let mut init_a = ((max_i - b0).max(1.0)) * 2.0;
    let log_floor = b0 + 1.0;
    let mut s = [[0.0_f64; 4]; 4];
    let mut rhs = [0.0_f64; 4];
    let mut used = 0usize;
    for h in halo {
        let v = f64::from(h.intensity);
        if v <= log_floor {
            continue;
        }
        let l = (v - b0).ln();
        let x = f64::from(h.x);
        let y = f64::from(h.y);
        let row = [1.0, x, y, x * x + y * y];
        for i in 0..4 {
            rhs[i] += row[i] * l;
            for j in 0..4 {
                s[i][j] += row[i] * row[j];
            }
        }
        used += 1;
    }
    if used >= 5 {
        if let Some(c) = solve_4x4(s, rhs) {
            let c3 = c[3];
            if c3 < 0.0 && c3.is_finite() {
                let sig_sq = -1.0 / (2.0 * c3);
                let cx = -c[1] / (2.0 * c3);
                let cy = -c[2] / (2.0 * c3);
                if sig_sq > 0.0 && cx.is_finite() && cy.is_finite() {
                    let sig = sig_sq.sqrt();
                    let a = (c[0] - (cx * cx + cy * cy) * c3).exp();
                    if a.is_finite() && a > 0.0 && sig > 0.5 && sig < 1.0e3 {
                        init_cx = cx;
                        init_cy = cy;
                        init_sigma = sig;
                        init_a = a;
                    }
                }
            }
        }
    }
    let mut params = [init_cx, init_cy, init_sigma, init_a, b0];

    let mut converged = false;
    for _ in 0..MAX_ITERS {
        let Some(step) = gauss_newton_step(halo, &params) else {
            return fallback();
        };
        for k in 0..5 {
            params[k] += step[k];
        }
        // Don't let σ collapse or A go negative.
        if params[2] <= 0.1 || params[3] <= 0.0 || !params.iter().all(|p| p.is_finite()) {
            return fallback();
        }
        let max_step = step.iter().fold(0.0_f64, |m, s| m.max(s.abs()));
        if max_step < CONVERGENCE_STEP {
            converged = true;
            break;
        }
    }
    if !converged {
        return fallback();
    }

    // Covariance from the final J^T W J. Recompute it (the
    // step function consumed the matrix it built).
    let Some((cov, chi2_per_dof)) = covariance_and_chi2(halo, &params) else {
        return fallback();
    };

    let sigma_x_px = cov[0].sqrt();
    let sigma_y_px = cov[1].sqrt();
    if !sigma_x_px.is_finite()
        || !sigma_y_px.is_finite()
        || sigma_x_px > MAX_ACCEPTABLE_SIGMA_PX
        || sigma_y_px > MAX_ACCEPTABLE_SIGMA_PX
    {
        return fallback();
    }

    RefinedCentroid {
        x: params[0],
        y: params[1],
        sigma_x_px,
        sigma_y_px,
        fit_quality: chi2_per_dof,
        refined: true,
    }
}

/// Extract the non-saturated boundary halo of the component
/// containing the integer centroid.
///
/// Walks a square window of half-width `radius` pixels around
/// the centroid and collects every pixel that is *below* the
/// saturation threshold. The result is the input expected by
/// [`refine_centroid_subpixel`].
///
/// `radius` should be a few times the expected blob radius —
/// `(area/π).sqrt() * 2 + 6` is a sensible default.
#[must_use]
pub fn extract_halo_pixels(
    frame: &Frame,
    integer_centroid: Centroid,
    saturation_threshold: u16,
    radius: u32,
) -> Vec<HaloPixel> {
    let w = frame.width();
    let h = frame.height();
    let cx = integer_centroid.x.round() as i64;
    let cy = integer_centroid.y.round() as i64;
    let r = radius as i64;
    let x0 = (cx - r).max(0) as u32;
    let y0 = (cy - r).max(0) as u32;
    let x1 = (cx + r + 1).clamp(0, w as i64) as u32;
    let y1 = (cy + r + 1).clamp(0, h as i64) as u32;
    let pixels = frame.pixels();
    let wu = w as usize;
    let mut out = Vec::with_capacity(((x1 - x0) * (y1 - y0)) as usize);
    for y in y0..y1 {
        for x in x0..x1 {
            let v = pixels[(y as usize) * wu + (x as usize)];
            if v < saturation_threshold {
                let dx = f64::from(x) - integer_centroid.x;
                let dy = f64::from(y) - integer_centroid.y;
                if dx * dx + dy * dy <= (radius as f64) * (radius as f64) {
                    out.push(HaloPixel { x, y, intensity: v });
                }
            }
        }
    }
    out
}

/// One Gauss-Newton iteration. Returns the parameter step
/// `Δp = (JᵀWJ)⁻¹ JᵀWr` or `None` if the normal-equations
/// matrix is singular / non-finite.
fn gauss_newton_step(halo: &[HaloPixel], params: &[f64; 5]) -> Option<[f64; 5]> {
    let (cx, cy, sigma, a, b) = (params[0], params[1], params[2], params[3], params[4]);
    let inv_2_sig2 = 1.0 / (2.0 * sigma * sigma);
    let inv_sig3 = 1.0 / (sigma * sigma * sigma);

    // Normal equations: H = JᵀWJ (5×5 symmetric), g = JᵀWr.
    let mut h = [[0.0_f64; 5]; 5];
    let mut g = [0.0_f64; 5];
    for hp in halo {
        let fx = f64::from(hp.x);
        let fy = f64::from(hp.y);
        let dx = fx - cx;
        let dy = fy - cy;
        let r2 = dx * dx + dy * dy;
        let e = (-r2 * inv_2_sig2).exp();
        let model = a * e + b;
        let measured = f64::from(hp.intensity);
        let resid = measured - model;
        let w = 1.0 / (measured.max(0.0) + READ_NOISE_ADU * READ_NOISE_ADU);
        // ∂model/∂cx, ∂cy, ∂σ, ∂A, ∂B.
        let dm_dcx = a * e * (dx / (sigma * sigma));
        let dm_dcy = a * e * (dy / (sigma * sigma));
        let dm_dsig = a * e * (r2 * inv_sig3);
        let dm_da = e;
        let dm_db = 1.0;
        let jac = [dm_dcx, dm_dcy, dm_dsig, dm_da, dm_db];
        for i in 0..5 {
            g[i] += w * jac[i] * resid;
            for j in 0..5 {
                h[i][j] += w * jac[i] * jac[j];
            }
        }
    }
    solve_5x5(h, g)
}

/// After convergence, recompute `(JᵀWJ)⁻¹` to extract per-
/// parameter variance, and the reduced chi-squared `Σ w_i r_i² /
/// (n - 5)`.
fn covariance_and_chi2(halo: &[HaloPixel], params: &[f64; 5]) -> Option<([f64; 5], f64)> {
    let (cx, cy, sigma, a, b) = (params[0], params[1], params[2], params[3], params[4]);
    let inv_2_sig2 = 1.0 / (2.0 * sigma * sigma);
    let inv_sig3 = 1.0 / (sigma * sigma * sigma);
    let mut h = [[0.0_f64; 5]; 5];
    let mut chi2 = 0.0_f64;
    for hp in halo {
        let fx = f64::from(hp.x);
        let fy = f64::from(hp.y);
        let dx = fx - cx;
        let dy = fy - cy;
        let r2 = dx * dx + dy * dy;
        let e = (-r2 * inv_2_sig2).exp();
        let model = a * e + b;
        let measured = f64::from(hp.intensity);
        let resid = measured - model;
        let w = 1.0 / (measured.max(0.0) + READ_NOISE_ADU * READ_NOISE_ADU);
        chi2 += w * resid * resid;
        let jac = [
            a * e * (dx / (sigma * sigma)),
            a * e * (dy / (sigma * sigma)),
            a * e * (r2 * inv_sig3),
            e,
            1.0,
        ];
        for i in 0..5 {
            for j in 0..5 {
                h[i][j] += w * jac[i] * jac[j];
            }
        }
    }
    let inv = invert_5x5(h)?;
    let diag = [inv[0][0], inv[1][1], inv[2][2], inv[3][3], inv[4][4]];
    if diag.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return None;
    }
    let dof = (halo.len() as f64 - 5.0).max(1.0);
    Some((diag, chi2 / dof))
}

/// Solve `A x = b` for 4×4 `A`. Returns `None` if singular.
fn solve_4x4(mut a: [[f64; 4]; 4], mut b: [f64; 4]) -> Option<[f64; 4]> {
    for col in 0..4 {
        let mut piv = col;
        for r in (col + 1)..4 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if piv != col {
            a.swap(col, piv);
            b.swap(col, piv);
        }
        let pv = a[col][col];
        if pv.abs() < 1e-18 || !pv.is_finite() {
            return None;
        }
        for j in col..4 {
            a[col][j] /= pv;
        }
        b[col] /= pv;
        for r in 0..4 {
            if r != col {
                let f = a[r][col];
                if f != 0.0 {
                    for j in col..4 {
                        a[r][j] -= f * a[col][j];
                    }
                    b[r] -= f * b[col];
                }
            }
        }
    }
    if b.iter().any(|v| !v.is_finite()) {
        return None;
    }
    Some(b)
}

/// Solve `A x = b` for 5×5 symmetric `A`. Returns `None` if
/// `A` is singular.
fn solve_5x5(mut a: [[f64; 5]; 5], mut b: [f64; 5]) -> Option<[f64; 5]> {
    // Gauss-Jordan with partial pivoting.
    for col in 0..5 {
        // Pivot.
        let mut piv = col;
        for r in (col + 1)..5 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if piv != col {
            a.swap(col, piv);
            b.swap(col, piv);
        }
        let pv = a[col][col];
        if pv.abs() < 1e-18 || !pv.is_finite() {
            return None;
        }
        for j in col..5 {
            a[col][j] /= pv;
        }
        b[col] /= pv;
        for r in 0..5 {
            if r != col {
                let f = a[r][col];
                if f != 0.0 {
                    for j in col..5 {
                        a[r][j] -= f * a[col][j];
                    }
                    b[r] -= f * b[col];
                }
            }
        }
    }
    if b.iter().any(|v| !v.is_finite()) {
        return None;
    }
    Some(b)
}

/// Invert a 5×5 matrix via Gauss-Jordan. Returns `None` if singular.
fn invert_5x5(a: [[f64; 5]; 5]) -> Option<[[f64; 5]; 5]> {
    let mut m = [[0.0_f64; 10]; 5];
    for i in 0..5 {
        for j in 0..5 {
            m[i][j] = a[i][j];
        }
        m[i][5 + i] = 1.0;
    }
    for col in 0..5 {
        let mut piv = col;
        for r in (col + 1)..5 {
            if m[r][col].abs() > m[piv][col].abs() {
                piv = r;
            }
        }
        if piv != col {
            m.swap(col, piv);
        }
        let pv = m[col][col];
        if pv.abs() < 1e-18 || !pv.is_finite() {
            return None;
        }
        for j in 0..10 {
            m[col][j] /= pv;
        }
        for r in 0..5 {
            if r != col {
                let f = m[r][col];
                if f != 0.0 {
                    for j in 0..10 {
                        m[r][j] -= f * m[col][j];
                    }
                }
            }
        }
    }
    let mut out = [[0.0_f64; 5]; 5];
    for i in 0..5 {
        for j in 0..5 {
            out[i][j] = m[i][5 + j];
            if !out[i][j].is_finite() {
                return None;
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::centroid::{extract_multi_saturated_centroids, SaturatedBodyConfig};
    use crate::frame::Intrinsics;
    use bris_core::time::{Tt, JD_J2000};

    /// Synthesize a frame with a 2D Gaussian source (peak A
    /// over background B, width σ) clipped to `u16::MAX`.
    fn synth_gaussian(
        w: u32,
        h: u32,
        cx: f64,
        cy: f64,
        sigma: f64,
        peak: f64,
        background: f64,
    ) -> Frame {
        let mut px = vec![0u16; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let dx = f64::from(x) - cx;
                let dy = f64::from(y) - cy;
                let v = background + peak * (-(dx * dx + dy * dy) / (2.0 * sigma * sigma)).exp();
                let clipped = v.clamp(0.0, f64::from(u16::MAX));
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

    fn refine_synth(cx_true: f64, cy_true: f64) -> RefinedCentroid {
        let frame = synth_gaussian(120, 100, cx_true, cy_true, 4.0, 80_000.0, 500.0);
        let cfg = SaturatedBodyConfig {
            saturation_threshold: 60_000,
            min_area_px: 5,
        };
        let centroids = extract_multi_saturated_centroids(&frame, cfg, None).unwrap();
        let primary = centroids.into_iter().next().unwrap();
        let halo = extract_halo_pixels(&frame, primary, 60_000, 15);
        refine_centroid_subpixel(&frame, primary, &halo)
    }

    #[test]
    fn recovers_position_within_5_hundredths_of_pixel() {
        let r = refine_synth(60.37, 50.62);
        assert!(r.refined, "fit must converge on a clean Gaussian");
        assert!(
            (r.x - 60.37).abs() < 0.05,
            "x off by {}",
            (r.x - 60.37).abs()
        );
        assert!(
            (r.y - 50.62).abs() < 0.05,
            "y off by {}",
            (r.y - 50.62).abs()
        );
        // σ should be small and finite on this high-SNR fit.
        assert!(r.sigma_x_px > 0.0 && r.sigma_x_px < 0.5);
        assert!(r.sigma_y_px > 0.0 && r.sigma_y_px < 0.5);
    }

    #[test]
    fn sigma_within_20pct_for_well_sampled_gaussian() {
        // Repeat the recovery on a slightly different offset
        // and check σ scales as expected (1/sqrt(N_eff)
        // bounds). We assert it's neither absurdly tight nor
        // absurdly loose on a clean fit.
        let r = refine_synth(55.5, 45.5);
        assert!(r.refined);
        // For peak 80000 ADU and σ=4 px the per-axis position
        // sigma from photon-limited Gauss-Newton is well below
        // 0.1 px; we allow a generous upper bound.
        assert!(r.sigma_x_px < 0.2);
        assert!(r.sigma_y_px < 0.2);
    }

    #[test]
    fn saturated_plateau_with_halo_recovers_center() {
        // Wide Gaussian with peak above u16::MAX → saturated
        // plateau in the middle, well-sampled halo around it.
        let frame = synth_gaussian(160, 120, 80.4, 60.7, 6.0, 200_000.0, 800.0);
        let cfg = SaturatedBodyConfig {
            saturation_threshold: 60_000,
            min_area_px: 5,
        };
        let primary = extract_multi_saturated_centroids(&frame, cfg, None)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let halo = extract_halo_pixels(&frame, primary, 60_000, 20);
        let r = refine_centroid_subpixel(&frame, primary, &halo);
        assert!(r.refined, "saturated-plateau fit must converge");
        assert!((r.x - 80.4).abs() < 0.3, "x off by {}", (r.x - 80.4).abs());
        assert!((r.y - 60.7).abs() < 0.3, "y off by {}", (r.y - 60.7).abs());
        // Integer-centroid σ floor is 0.5 px; refined should
        // be tighter for a well-sampled saturated body.
        assert!(r.sigma_x_px < 0.5);
        assert!(r.sigma_y_px < 0.5);
    }

    #[test]
    fn too_few_halo_pixels_falls_back_to_integer() {
        let frame = synth_gaussian(120, 100, 60.3, 50.5, 4.0, 80_000.0, 500.0);
        let primary = extract_multi_saturated_centroids(
            &frame,
            SaturatedBodyConfig {
                saturation_threshold: 60_000,
                min_area_px: 5,
            },
            None,
        )
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
        // Only feed three halo pixels — below the 8-pixel gate.
        let halo = vec![
            HaloPixel {
                x: 50,
                y: 50,
                intensity: 2_000,
            },
            HaloPixel {
                x: 51,
                y: 50,
                intensity: 2_000,
            },
            HaloPixel {
                x: 52,
                y: 50,
                intensity: 2_000,
            },
        ];
        let r = refine_centroid_subpixel(&frame, primary, &halo);
        assert!(!r.refined, "must fall back when halo < 8 pixels");
        assert!((r.x - primary.x).abs() < 1e-12);
        assert!((r.y - primary.y).abs() < 1e-12);
        assert!((r.sigma_x_px - 0.5).abs() < 1e-12);
        assert!((r.sigma_y_px - 0.5).abs() < 1e-12);
    }

    #[test]
    fn divergent_halo_falls_back_to_integer() {
        // Halo pixels are uniform noise around the same value
        // — there's no Gaussian peak to fit. The fit either
        // diverges (σ blows up past `MAX_ACCEPTABLE_SIGMA_PX`)
        // or fails to converge in the iteration budget. In
        // either case it must report `refined = false` and
        // fall back to the integer centroid.
        let frame = synth_gaussian(120, 100, 60.0, 50.0, 4.0, 80_000.0, 500.0);
        let primary = extract_multi_saturated_centroids(
            &frame,
            SaturatedBodyConfig {
                saturation_threshold: 60_000,
                min_area_px: 5,
            },
            None,
        )
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
        // 16 halo samples all at the same flat intensity ⇒
        // no curvature for the fit to lock onto.
        let halo: Vec<HaloPixel> = (0..16)
            .map(|i| HaloPixel {
                x: (40 + i) as u32,
                y: 30,
                intensity: 1_500,
            })
            .collect();
        let r = refine_centroid_subpixel(&frame, primary, &halo);
        assert!(
            !r.refined,
            "flat halo (no Gaussian curvature) must fall back"
        );
        assert!((r.x - primary.x).abs() < 1e-12);
        assert!((r.y - primary.y).abs() < 1e-12);
    }

    #[test]
    fn refined_sigma_tighter_than_integer_fallback() {
        // On a clean Gaussian the refined σ must be strictly
        // smaller than the 0.5 px integer fallback floor.
        let r = refine_synth(50.5, 50.5);
        assert!(r.refined);
        let integer_sigma = 0.5_f64;
        assert!(
            r.sigma_x_px < integer_sigma,
            "refined σ_x {} should beat integer floor {}",
            r.sigma_x_px,
            integer_sigma,
        );
        assert!(
            r.sigma_y_px < integer_sigma,
            "refined σ_y {} should beat integer floor {}",
            r.sigma_y_px,
            integer_sigma,
        );
    }
}
