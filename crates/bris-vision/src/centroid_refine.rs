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
//! five parameters `(cx, cy, σ, A, B)`. The fit is
//! Levenberg-Marquardt with photon-plus-read-noise inverse-
//! variance weights `w_i = 1 / (I_i / gain + read_noise_adu²)`
//! (variance in ADU²). Pass [`DEFAULT_GAIN_E_PER_ADU`] when the
//! caller does not have a measured sensor gain — the Pi Zero
//! 2W / Android camera plumbing has yet to surface it. TODO:
//! wire from `bris-android` camera characteristics +
//! `bris-capture` V4L2 controls.

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
    /// Position covariance cross-term `Cov(x, y)` in pixels²,
    /// the (1, 2) entry of the inverted fit Hessian. Callers
    /// projecting σ onto a non-axis direction (e.g. the
    /// altitude axis in image coordinates) need this term to
    /// rotate the covariance correctly.
    pub cov_xy_px2: f64,
    /// Reduced chi-squared (χ²/dof) of the fit. Expected near
    /// 1 for properly weighted fits; may differ on real frames
    /// if the sensor gain calibration is off (the weights
    /// assume `gain_e_per_adu`).
    pub fit_quality: f64,
    /// `true` when the fit converged and is being reported;
    /// `false` when the function fell back to the integer
    /// centroid (insufficient halo, divergence, or absurd σ).
    pub refined: bool,
}

/// Read-noise term (ADU) in the inverse-variance weight
/// `1 / (I/gain + read_noise²)`. Five ADU is representative of
/// modest CMOS sensors at gain unity; we do not attempt
/// per-sensor accuracy.
const READ_NOISE_ADU: f64 = 5.0;

/// Default sensor gain (electrons per ADU) used when no
/// measured value is available from camera characteristics.
/// TODO: plumb a measured gain from `bris-capture` V4L2
/// controls / `bris-android` `CameraCharacteristics` instead
/// of falling back to this constant.
pub const DEFAULT_GAIN_E_PER_ADU: f64 = 1.0;

/// Minimum halo size for a 5-parameter fit to be meaningful.
const MIN_HALO_PIXELS: usize = 8;

/// Maximum outer Gauss-Newton / LM iterations before declaring
/// divergence. Matches the pre-LM Gauss-Newton iteration cap;
/// LM doesn't change the convergence horizon, just makes each
/// outer step safer.
const MAX_ITERS: usize = 10;

/// Maximum LM inner step-rejection attempts per outer iteration.
const MAX_LM_INNER: usize = 5;

/// Initial LM damping factor.
const LM_LAMBDA_INIT: f64 = 1.0e-3;

/// Convergence threshold on parameter step `‖Δp‖∞`.
const CONVERGENCE_STEP: f64 = 1.0e-4;

/// Reject the refined fit and fall back when both axis σs
/// exceed the integer-centroid fallback σ supplied to
/// [`refine_centroid_subpixel`]. The integer fallback floor
/// is 0.5 px today (see [`crate::centroid`]).
const INTEGER_FALLBACK_SIGMA_PX: f64 = 0.5;

/// Refine an integer-pixel centroid to sub-pixel via 2D Gaussian fit.
///
/// `integer_centroid` provides the initial centre estimate and
/// the fallback used when refinement is not possible. `halo`
/// is the list of non-saturated boundary pixels (see
/// [`extract_halo_pixels`]). `gain_e_per_adu` is the sensor
/// conversion gain in electrons per ADU; pass
/// [`DEFAULT_GAIN_E_PER_ADU`] when no measured value is
/// available. `frame` is unused by the fit itself but is
/// accepted to keep the signature symmetric with other Stage A
/// primitives.
///
/// Returns a [`RefinedCentroid`] whose `refined` flag indicates
/// whether the sub-pixel position came from the Gaussian fit
/// or from the integer fallback. The fallback case sets `σ =
/// 0.5 px` per axis, `cov_xy = 0`, and `fit_quality =
/// f64::INFINITY`.
///
/// The refined fit is only accepted when `max(σx, σy) <
/// 0.5 px` — i.e. when it strictly beats the integer fallback
/// floor. Otherwise the integer fallback is returned.
pub fn refine_centroid_subpixel(
    _frame: &Frame,
    integer_centroid: Centroid,
    halo: &[HaloPixel],
    gain_e_per_adu: f64,
) -> RefinedCentroid {
    let fallback = || RefinedCentroid {
        x: integer_centroid.x,
        y: integer_centroid.y,
        sigma_x_px: INTEGER_FALLBACK_SIGMA_PX,
        sigma_y_px: INTEGER_FALLBACK_SIGMA_PX,
        cov_xy_px2: 0.0,
        fit_quality: f64::INFINITY,
        refined: false,
    };

    if halo.len() < MIN_HALO_PIXELS {
        return fallback();
    }

    let gain = if gain_e_per_adu.is_finite() && gain_e_per_adu > 0.0 {
        gain_e_per_adu
    } else {
        DEFAULT_GAIN_E_PER_ADU
    };

    // Initial sigma estimate from blob area; used to define
    // the "outer annulus" for the background floor.
    let init_sigma_area = ((integer_centroid.area_px as f64) / core::f64::consts::PI)
        .sqrt()
        .max(1.0);

    let mut max_i = f64::NEG_INFINITY;
    for h in halo {
        let v = f64::from(h.intensity);
        if v > max_i {
            max_i = v;
        }
    }

    // Background floor = 10th percentile of outer-annulus
    // pixels (those with r > 2·σ_init from the integer
    // centroid). Falls back to global min on too few outer
    // samples.
    let b0 = estimate_background_floor(halo, integer_centroid, init_sigma_area);

    // Log-linearized initial fit. We solve in *centred*
    // coordinates (u = x - cx0, v = y - cy0) so the design
    // matrix entries stay O(1) instead of O(1e10) on typical
    // image-frame indices.
    //   log(I - B) = c0 + c1·u + c2·v + c3·(u² + v²)
    //   c3 = -1/(2σ²)   ⇒  σ = sqrt(-1/(2·c3))
    //   du = -c1/(2·c3), dv = -c2/(2·c3)   ⇒  cx = cx0 + du
    //   A  = exp(c0 - (du² + dv²)·c3)
    let cx0 = integer_centroid.x;
    let cy0 = integer_centroid.y;
    let mut init_cx = cx0;
    let mut init_cy = cy0;
    let mut init_sigma = init_sigma_area;
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
        let u = f64::from(h.x) - cx0;
        let vv = f64::from(h.y) - cy0;
        let row = [1.0, u, vv, u * u + vv * vv];
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
                let du = -c[1] / (2.0 * c3);
                let dv = -c[2] / (2.0 * c3);
                if sig_sq > 0.0 && du.is_finite() && dv.is_finite() {
                    let sig = sig_sq.sqrt();
                    let a = (c[0] - (du * du + dv * dv) * c3).exp();
                    if a.is_finite() && a > 0.0 && sig > 0.5 && sig < 1.0e3 {
                        init_cx = cx0 + du;
                        init_cy = cy0 + dv;
                        init_sigma = sig;
                        init_a = a;
                    }
                }
            }
        }
    }
    let mut params = [init_cx, init_cy, init_sigma, init_a, b0];
    let mut chi2_old = weighted_chi2(halo, &params, gain);
    let mut lambda = LM_LAMBDA_INIT;

    let mut converged = false;
    'outer: for _ in 0..MAX_ITERS {
        // Build H, g once per outer iter.
        let Some((h, g)) = build_normal_equations(halo, &params, gain) else {
            return fallback();
        };
        let mut accepted = false;
        for _ in 0..MAX_LM_INNER {
            // Solve (H + λI) δ = g.
            let mut h_damped = h;
            for k in 0..5 {
                h_damped[k][k] *= 1.0 + lambda;
            }
            let Some(step) = solve_5x5(h_damped, g) else {
                lambda *= 10.0;
                continue;
            };
            let mut trial = params;
            for k in 0..5 {
                trial[k] += step[k];
            }
            if trial[2] <= 0.1 || trial[3] <= 0.0 || !trial.iter().all(|p| p.is_finite()) {
                lambda *= 10.0;
                continue;
            }
            let chi2_new = weighted_chi2(halo, &trial, gain);
            if chi2_new < chi2_old {
                params = trial;
                chi2_old = chi2_new;
                lambda = (lambda / 10.0).max(1.0e-12);
                accepted = true;
                let max_step = step.iter().fold(0.0_f64, |m, s| m.max(s.abs()));
                if max_step < CONVERGENCE_STEP {
                    converged = true;
                    break 'outer;
                }
                break;
            }
            lambda *= 10.0;
        }
        if !accepted {
            // Could not improve χ² this outer step. Treat as
            // converged-or-stuck; the covariance check below
            // decides if the result is acceptable.
            converged = true;
            break;
        }
    }
    if !converged {
        return fallback();
    }

    let Some((cov, cov_xy, chi2_per_dof)) = covariance_and_chi2(halo, &params, gain) else {
        return fallback();
    };

    let sigma_x_px = cov[0].sqrt();
    let sigma_y_px = cov[1].sqrt();
    let max_sigma = sigma_x_px.max(sigma_y_px);
    if !sigma_x_px.is_finite()
        || !sigma_y_px.is_finite()
        || !cov_xy.is_finite()
        || max_sigma >= INTEGER_FALLBACK_SIGMA_PX
    {
        return fallback();
    }

    RefinedCentroid {
        x: params[0],
        y: params[1],
        sigma_x_px,
        sigma_y_px,
        cov_xy_px2: cov_xy,
        fit_quality: chi2_per_dof,
        refined: true,
    }
}

/// Background floor = 10th percentile of pixels in the outer
/// annulus (radial distance > `2·σ_init` from the integer
/// centroid). Falls back to global min when too few outer
/// samples are available.
fn estimate_background_floor(halo: &[HaloPixel], c: Centroid, sigma_init: f64) -> f64 {
    let r_cut2 = (2.0 * sigma_init) * (2.0 * sigma_init);
    let mut outer: Vec<f64> = halo
        .iter()
        .filter_map(|h| {
            let dx = f64::from(h.x) - c.x;
            let dy = f64::from(h.y) - c.y;
            if dx * dx + dy * dy > r_cut2 {
                Some(f64::from(h.intensity))
            } else {
                None
            }
        })
        .collect();
    if outer.len() < 5 {
        tracing::trace!(
            outer_count = outer.len(),
            "centroid_refine: too few outer-annulus pixels, falling back to min-I background"
        );
        let mut min_i = f64::INFINITY;
        for h in halo {
            let v = f64::from(h.intensity);
            if v < min_i {
                min_i = v;
            }
        }
        return min_i.max(0.0);
    }
    outer.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let idx = ((outer.len() as f64 - 1.0) * 0.10).round() as usize;
    outer[idx].max(0.0)
}

/// Extract the non-saturated boundary halo of the component
/// containing the integer centroid.
///
/// Iterates a circle of radius `radius` (in pixels) about the
/// integer centroid row-by-row; for each row the x-extent is
/// `sqrt(radius² − dy²)` so only pixels actually inside the
/// disk are visited. Returns every pixel below
/// `saturation_threshold` within that disk, suitable as input
/// to [`refine_centroid_subpixel`].
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
    let cx_f = integer_centroid.x;
    let cy_f = integer_centroid.y;
    let cy_i = cy_f.round() as i64;
    let r = radius as i64;
    let r2 = r * r;
    let y_lo = (cy_i - r).max(0) as u32;
    let y_hi = (cy_i + r + 1).clamp(0, h as i64) as u32;
    let pixels = frame.pixels();
    let wu = w as usize;
    let mut out: Vec<HaloPixel> = Vec::new();
    for y in y_lo..y_hi {
        let dy = f64::from(y) - cy_f;
        let dy2 = dy * dy;
        let half_w2 = (radius as f64) * (radius as f64) - dy2;
        if half_w2 < 0.0 {
            continue;
        }
        let half_w = half_w2.sqrt();
        let x_lo = ((cx_f - half_w).floor() as i64).max(0) as u32;
        let x_hi = (((cx_f + half_w).ceil() as i64) + 1).clamp(0, w as i64) as u32;
        for x in x_lo..x_hi {
            let dx = f64::from(x) - cx_f;
            if dx * dx + dy2 > r2 as f64 {
                continue;
            }
            let v = pixels[(y as usize) * wu + (x as usize)];
            if v < saturation_threshold {
                out.push(HaloPixel { x, y, intensity: v });
            }
        }
    }
    out
}

/// Pixel weight in ADU². `var = I/gain + read_noise²` because
/// shot-noise variance in ADU is (counts in electrons)/gain² =
/// `I_adu` / gain.
#[inline]
fn pixel_weight(measured_adu: f64, gain_e_per_adu: f64) -> f64 {
    1.0 / (measured_adu.max(0.0) / gain_e_per_adu + READ_NOISE_ADU * READ_NOISE_ADU)
}

/// Σ wᵢ rᵢ² for a candidate parameter vector. Used by the LM
/// step-acceptance test.
fn weighted_chi2(halo: &[HaloPixel], params: &[f64; 5], gain: f64) -> f64 {
    let (cx, cy, sigma, a, b) = (params[0], params[1], params[2], params[3], params[4]);
    let inv_2_sig2 = 1.0 / (2.0 * sigma * sigma);
    let mut chi2 = 0.0_f64;
    for hp in halo {
        let dx = f64::from(hp.x) - cx;
        let dy = f64::from(hp.y) - cy;
        let r2 = dx * dx + dy * dy;
        let e = (-r2 * inv_2_sig2).exp();
        let model = a * e + b;
        let measured = f64::from(hp.intensity);
        let resid = measured - model;
        chi2 += pixel_weight(measured, gain) * resid * resid;
    }
    chi2
}

/// Build normal equations `H = JᵀWJ` (5×5 symmetric) and
/// `g = JᵀWr` (5-vector) for a parameter vector. Returns
/// `None` if anything goes non-finite.
fn build_normal_equations(
    halo: &[HaloPixel],
    params: &[f64; 5],
    gain: f64,
) -> Option<([[f64; 5]; 5], [f64; 5])> {
    let (cx, cy, sigma, a, b) = (params[0], params[1], params[2], params[3], params[4]);
    let inv_2_sig2 = 1.0 / (2.0 * sigma * sigma);
    let inv_sig3 = 1.0 / (sigma * sigma * sigma);
    let mut h = [[0.0_f64; 5]; 5];
    let mut g = [0.0_f64; 5];
    for hp in halo {
        let dx = f64::from(hp.x) - cx;
        let dy = f64::from(hp.y) - cy;
        let r2 = dx * dx + dy * dy;
        let e = (-r2 * inv_2_sig2).exp();
        let model = a * e + b;
        let measured = f64::from(hp.intensity);
        let resid = measured - model;
        let w = pixel_weight(measured, gain);
        let jac = [
            a * e * (dx / (sigma * sigma)),
            a * e * (dy / (sigma * sigma)),
            a * e * (r2 * inv_sig3),
            e,
            1.0,
        ];
        for i in 0..5 {
            g[i] += w * jac[i] * resid;
            for j in 0..5 {
                h[i][j] += w * jac[i] * jac[j];
            }
        }
    }
    if g.iter().any(|v| !v.is_finite()) || h.iter().flatten().any(|v| !v.is_finite()) {
        return None;
    }
    Some((h, g))
}

/// After convergence, recompute `(JᵀWJ)⁻¹` to extract per-
/// parameter variance and the cross-term `Cov(cx, cy)`, plus
/// the reduced chi-squared `Σ wᵢ rᵢ² / (n − 5)`.
fn covariance_and_chi2(
    halo: &[HaloPixel],
    params: &[f64; 5],
    gain: f64,
) -> Option<([f64; 5], f64, f64)> {
    let (h, _g) = build_normal_equations(halo, params, gain)?;
    let chi2 = weighted_chi2(halo, params, gain);
    let inv = invert_5x5(h)?;
    let diag = [inv[0][0], inv[1][1], inv[2][2], inv[3][3], inv[4][4]];
    if diag.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return None;
    }
    let cov_xy = inv[0][1];
    let dof = (halo.len() as f64 - 5.0).max(1.0);
    Some((diag, cov_xy, chi2 / dof))
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
    for col in 0..5 {
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
        refine_centroid_subpixel(&frame, primary, &halo, DEFAULT_GAIN_E_PER_ADU)
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
        assert!(r.sigma_x_px > 0.0 && r.sigma_x_px < 0.5);
        assert!(r.sigma_y_px > 0.0 && r.sigma_y_px < 0.5);
    }

    #[test]
    fn sigma_within_20pct_for_well_sampled_gaussian() {
        let r = refine_synth(55.5, 45.5);
        assert!(r.refined);
        assert!(r.sigma_x_px < 0.2);
        assert!(r.sigma_y_px < 0.2);
    }

    #[test]
    fn saturated_plateau_with_halo_recovers_center() {
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
        let r = refine_centroid_subpixel(&frame, primary, &halo, DEFAULT_GAIN_E_PER_ADU);
        assert!(r.refined, "saturated-plateau fit must converge");
        assert!((r.x - 80.4).abs() < 0.3, "x off by {}", (r.x - 80.4).abs());
        assert!((r.y - 60.7).abs() < 0.3, "y off by {}", (r.y - 60.7).abs());
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
        let r = refine_centroid_subpixel(&frame, primary, &halo, DEFAULT_GAIN_E_PER_ADU);
        assert!(!r.refined, "must fall back when halo < 8 pixels");
        assert!((r.x - primary.x).abs() < 1e-12);
        assert!((r.y - primary.y).abs() < 1e-12);
        assert!((r.sigma_x_px - 0.5).abs() < 1e-12);
        assert!((r.sigma_y_px - 0.5).abs() < 1e-12);
    }

    #[test]
    fn divergent_halo_falls_back_to_integer() {
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
        let halo: Vec<HaloPixel> = (0..16)
            .map(|i| HaloPixel {
                x: (40 + i) as u32,
                y: 30,
                intensity: 1_500,
            })
            .collect();
        let r = refine_centroid_subpixel(&frame, primary, &halo, DEFAULT_GAIN_E_PER_ADU);
        assert!(
            !r.refined,
            "flat halo (no Gaussian curvature) must fall back"
        );
        assert!((r.x - primary.x).abs() < 1e-12);
        assert!((r.y - primary.y).abs() < 1e-12);
    }

    #[test]
    fn refined_sigma_tighter_than_integer_fallback() {
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
