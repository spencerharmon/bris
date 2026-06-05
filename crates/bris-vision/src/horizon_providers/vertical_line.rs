//! Auto-detected horizon from a near-vertical line in frame.
//!
//! Operator hangs a weighted string (or any near-vertical edge:
//! door frame, lamp post, building corner) in the camera's
//! field of view. The string is, by construction, parallel to
//! local gravity. Detect it → camera-frame gravity vector →
//! synthesize a `HorizonLine` via `ℓ = K⁻ᵀ g_cam` (see
//! `docs/design/horizon_brainstorm.md` §0 and §B3).
//!
//! Phase 1 scope (this module):
//! - Single-line detection (multi-line statistical fusion is
//!   the sibling vanishing-point provider's job).
//! - Minimal Hough-style detector restricted to near-vertical
//!   orientations. The operator's plumb line is high-contrast
//!   and obvious; aggressive sub-pixel refinement is overkill
//!   here.
//! - Intra-frame, fires in all conditions (Day, Night,
//!   Twilight).
//!
//! # Disabled by default in the streaming engine
//!
//! As of the disable-by-default change, the
//! [`bris_streaming::EngineConfig`] no longer dispatches this
//! provider in Stage C unless its
//! `enable_vertical_line_provider` flag is flipped to `true`.
//! The provider's gravity inference reduces to
//! `gravity ≈ r_bot - r_top` (image-space endpoint
//! difference projected through `K⁻¹`); that small-angle
//! approximation is only valid for *short* lines *centered on
//! the principal point*. For full-height lines on tilted
//! cameras — the common hand-held capture geometry — the
//! inferred gravity is wrong by 20–40°, and the synthesised
//! horizon is confidently wrong (operator + agent diagnosed
//! this on the bedroom-moon corpus).
//!
//! The module's detector + unit tests remain authoritative
//! for the short-line / plumb-string regime where the math
//! *does* hold; the change is purely about whether Stage C
//! invokes the provider. See `docs/design/ml_gravity.md` for
//! the planned replacement (an ML-based per-frame gravity
//! estimator that does not depend on a visible vertical
//! reference).

// Pedantic casts are pervasive in the Hough detector below
// (pixel grids are `usize`/`u32`; arithmetic is `f64`).
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use bris_core::Sigma;

use crate::frame::Frame;
use crate::ray::{horizon_line_from_normal, CameraRay};

use super::{
    HorizonHypothesis, HorizonProvenance, HorizonProvider, HorizonProviderContext, TemporalScope,
};

/// Configuration for [`VerticalLineProvider`].
#[derive(Debug, Clone, Copy)]
pub struct VerticalLineConfig {
    /// Maximum angle from image-vertical (radians) a detected
    /// line may have before it is rejected as "not vertical".
    /// Default ≈ 0.35 rad (~20°).
    pub max_angle_from_vertical_rad: f64,
    /// Minimum line length in pixels. Short lines carry too
    /// much direction uncertainty. Default 50.
    pub min_line_length_px: u32,
    /// Floor on the synthesized horizon altitude σ (radians).
    /// Default 1e-3 rad ≈ 3.4'. Documents the irreducible
    /// uncertainty in this provider — plumb-line pendulum
    /// sway, string thickness, single-frame detector noise.
    pub sigma_floor_rad: f64,
    /// Edge-magnitude threshold (Sobel-x absolute value, u16
    /// scale) above which a pixel is considered an edge
    /// candidate for Hough voting. Default 2000 — well above
    /// per-pixel sensor noise but well below a saturated
    /// high-contrast string edge.
    pub edge_threshold: u32,
    /// Per-endpoint pixel σ used in the angular σ derivation
    /// below. Default 1.0 px (a conservative one-pixel
    /// endpoint localisation under the minimal Hough
    /// front-end).
    pub endpoint_sigma_px: f64,
}

impl Default for VerticalLineConfig {
    fn default() -> Self {
        Self {
            max_angle_from_vertical_rad: 0.35,
            min_line_length_px: 50,
            sigma_floor_rad: 1e-3,
            edge_threshold: 2000,
            endpoint_sigma_px: 1.0,
        }
    }
}

/// Provider that infers gravity from a single near-vertical
/// line in the image.
#[derive(Debug, Clone, Copy, Default)]
pub struct VerticalLineProvider {
    /// Detector tunables.
    pub config: VerticalLineConfig,
}

/// Per-invocation outcome counters.
///
/// `hypothesized` and `rejected_no_lines` are mutually
/// exclusive within a single call: either at least one line
/// passed all filters (then `hypothesized = true`), or none
/// did (then `rejected_no_lines = true`).
#[derive(Debug, Clone, Copy, Default)]
pub struct VerticalLineStats {
    /// Provider produced a hypothesis this call.
    pub hypothesized: u64,
    /// No near-vertical line above `min_line_length_px`
    /// survived the detector.
    pub rejected_no_lines: u64,
}

/// A detected near-vertical line in image space, with its two
/// endpoints (pixel coordinates) and length.
#[derive(Debug, Clone, Copy)]
struct DetectedLine {
    /// Upper endpoint (smaller `pixel.y`).
    top: (f64, f64),
    /// Lower endpoint (larger `pixel.y`).
    bot: (f64, f64),
    /// Euclidean length in pixels.
    length_px: f64,
}

impl HorizonProvider for VerticalLineProvider {
    fn name(&self) -> &'static str {
        "vertical_line"
    }

    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }

    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        let mut stats = VerticalLineStats::default();
        self.detect_with_stats(ctx, &mut stats)
    }
}

impl VerticalLineProvider {
    /// Same as [`HorizonProvider::detect`] but populates a
    /// per-invocation [`VerticalLineStats`] so the streaming
    /// engine can fold the counters into its long-running
    /// `EngineDiagnostics`.
    #[must_use]
    pub fn detect_with_stats(
        &self,
        ctx: &HorizonProviderContext<'_>,
        stats: &mut VerticalLineStats,
    ) -> Option<HorizonHypothesis> {
        let lines = detect_near_vertical_lines(ctx.frame, &self.config);
        if lines.is_empty() {
            stats.rejected_no_lines += 1;
            return None;
        }

        // Average gravity direction across all detected lines
        // (single-line case is one entry; the spec calls for
        // averaging when multiple lines pass — kept simple
        // here, no clustering).
        let mut g_sum = CameraRay {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let mut total_length_px = 0.0_f64;
        for line in &lines {
            let r_top = CameraRay::from_pixel(ctx.intrinsics, line.top.0, line.top.1);
            let r_bot = CameraRay::from_pixel(ctx.intrinsics, line.bot.0, line.bot.1);
            // Gravity points image-down: from the upper
            // endpoint's ray toward the lower endpoint's ray.
            // For a 3D plumb line viewed from a non-degenerate
            // angle, `r_bot - r_top` is approximately parallel
            // to the 3D line direction in camera frame; that
            // direction is local gravity by construction of the
            // plumb line.
            let dx = r_bot.x - r_top.x;
            let dy = r_bot.y - r_top.y;
            let dz = r_bot.z - r_top.z;
            let Some(g) = (CameraRay {
                x: dx,
                y: dy,
                z: dz,
            })
            .normalize() else {
                continue;
            };
            // Length-weight: longer lines have tighter
            // direction estimates; averaging weighted by line
            // length minimises overall σ. See σ derivation
            // below.
            g_sum.x += g.x * line.length_px;
            g_sum.y += g.y * line.length_px;
            g_sum.z += g.z * line.length_px;
            total_length_px += line.length_px;
        }
        let Some(gravity) = g_sum.normalize() else {
            stats.rejected_no_lines += 1;
            return None;
        };
        if total_length_px <= 0.0 {
            stats.rejected_no_lines += 1;
            return None;
        }

        // σ derivation.
        //
        // For a single line of length L_px and per-endpoint
        // pixel σ of σ_px, the σ on the *line direction* in
        // image space is approximately
        //
        //     σ_dir_img ≈ σ_px · sqrt(2) / L_px       (rad-equivalent)
        //
        // (each endpoint contributes independently to the
        // angle of the chord; small-angle approx). Converting
        // image-space direction σ to camera-frame angular σ
        // costs a factor of 1/f_eff in pixels, but since the
        // chord σ is already in pixel/length units, the
        // camera-frame angular σ on the gravity direction is
        //
        //     σ_g ≈ σ_px · sqrt(2) / L_px
        //
        // directly (a small-angle perturbation of the line
        // chord in pixel space rotates the inferred 3D
        // direction by the same small angle — `f_eff` cancels
        // because both endpoint rays share it).
        //
        // For N lines combined with length-weighting,
        //
        //     σ_g_combined ≈ σ_px · sqrt(2) / sqrt(sum L_i²),
        //
        // i.e. the effective length is the RMS-summed length
        // of the contributors. We use the simpler bound
        //     σ_g_combined = σ_px · sqrt(2) / total_length_px
        // which is the limit when all lines have similar
        // length (so sum L_i² ≈ N · L̄² and sqrt(...) ≈
        // sqrt(N)·L̄ ≈ total/sqrt(N) — the formula below is
        // slightly tighter by a sqrt(N) factor, an honest
        // pre-floor estimate that the σ_floor then dominates
        // for short / single lines).
        let sigma_from_geometry = self.config.endpoint_sigma_px * std::f64::consts::SQRT_2
            / total_length_px.max(f64::EPSILON);
        let sigma_value = sigma_from_geometry.max(self.config.sigma_floor_rad);
        let altitude_sigma = Sigma::new(sigma_value).unwrap_or(Sigma::ZERO);

        // Sky-pointing normal = -gravity.
        let sky_normal = CameraRay {
            x: -gravity.x,
            y: -gravity.y,
            z: -gravity.z,
        };
        let line = horizon_line_from_normal(&sky_normal, ctx.intrinsics, altitude_sigma)?;

        stats.hypothesized += 1;
        Some(HorizonHypothesis {
            line,
            provenance: HorizonProvenance::VerticalLine {
                line_count: lines.len(),
            },
            direct_sight: None,
        })
    }
}

/// Minimal Hough-style near-vertical line detector.
///
/// Limits (deliberate, Phase 1):
///   - Sobel-x edge response only (vertical edges have strong
///     horizontal gradient).
///   - Hough parameter grid over (θ ∈ near-vertical,
///     ρ = `x_at_y_mid`).
///   - One peak per θ bin (the strongest); endpoints by
///     walking edge pixels along the line.
///   - No NMS across θ bins; collisions accepted (the
///     averaging step in the caller mitigates).
///   - No sub-pixel refinement.
#[allow(clippy::too_many_lines)]
fn detect_near_vertical_lines(frame: &Frame, cfg: &VerticalLineConfig) -> Vec<DetectedLine> {
    let w = frame.width() as usize;
    let h = frame.height() as usize;
    if w < 3 || h < 3 {
        return Vec::new();
    }
    let pixels = frame.pixels();

    // Sobel-x absolute magnitude per pixel (skipping borders).
    // Stored as u32 to avoid overflow on the 3×3 sum.
    let mut edge_mag = vec![0u32; w * h];
    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            // Sobel x:  [-1 0 +1; -2 0 +2; -1 0 +1]
            let p = |xi: usize, yi: usize| -> i32 { i32::from(pixels[yi * w + xi]) };
            let gx = -p(x - 1, y - 1) + p(x + 1, y - 1) - 2 * p(x - 1, y) + 2 * p(x + 1, y)
                - p(x - 1, y + 1)
                + p(x + 1, y + 1);
            edge_mag[y * w + x] = gx.unsigned_abs();
        }
    }

    // Hough over (θ, ρ) where the line is x = ρ + tan(θ)·(y - cy).
    // Parameterising as x(y) (rather than the standard
    // ρ = x cos θ + y sin θ) keeps "vertical" trivial and the
    // angle bound (max_angle_from_vertical_rad) cleanly
    // expressible.
    let theta_step = (1.0_f64).to_radians(); // 1° bins
    let max_theta = cfg.max_angle_from_vertical_rad;
    let n_theta_each = (max_theta / theta_step).floor() as i32;
    let n_theta = 2 * n_theta_each + 1;
    if n_theta <= 0 {
        return Vec::new();
    }
    let rho_min = 0i32;
    let rho_max = w as i32 - 1;
    let n_rho = (rho_max - rho_min + 1) as usize;
    let cy = h as f64 / 2.0;

    // accumulator[theta_bin * n_rho + rho_bin]
    let mut acc: Vec<u32> = vec![0; (n_theta as usize) * n_rho];

    for y in 1..(h - 1) {
        let dy = y as f64 - cy;
        for x in 1..(w - 1) {
            if edge_mag[y * w + x] < cfg.edge_threshold {
                continue;
            }
            for ti in -n_theta_each..=n_theta_each {
                let theta = f64::from(ti) * theta_step;
                let rho_f = x as f64 - theta.tan() * dy;
                if !rho_f.is_finite() {
                    continue;
                }
                let rho = rho_f.round() as i32;
                if rho < rho_min || rho > rho_max {
                    continue;
                }
                let tbin = (ti + n_theta_each) as usize;
                let rbin = (rho - rho_min) as usize;
                acc[tbin * n_rho + rbin] += 1;
            }
        }
    }

    // Pick local maxima: one strongest peak per θ bin, then
    // global filter by length. With 1° θ bins and ~hundreds
    // of pixels of plumb edge, the expected peak vote count
    // is on the order of the line length.
    let mut lines: Vec<DetectedLine> = Vec::new();
    let min_len = f64::from(cfg.min_line_length_px);
    for ti in 0..(n_theta as usize) {
        // Strongest ρ in this θ bin.
        let mut best_votes = 0u32;
        let mut best_rho = 0i32;
        for ri in 0..n_rho {
            let v = acc[ti * n_rho + ri];
            if v > best_votes {
                best_votes = v;
                best_rho = ri as i32 + rho_min;
            }
        }
        // Cheap pre-filter: vote count must at least equal
        // the min line length (each pixel on the line votes
        // once for its θ).
        if f64::from(best_votes) < min_len {
            continue;
        }

        let theta = f64::from(ti as i32 - n_theta_each) * theta_step;
        let tan_t = theta.tan();
        // Walk all y's; record min/max y of edge pixels whose
        // x is within ±1 px of the line. Endpoints determine
        // the line's actual extent in the image.
        let mut y_min: Option<usize> = None;
        let mut y_max: Option<usize> = None;
        for y in 1..(h - 1) {
            let dy = y as f64 - cy;
            let x_line = f64::from(best_rho) + tan_t * dy;
            let x_round = x_line.round() as i32;
            // ±1 px tolerance.
            for dx in -1i32..=1 {
                let xc = x_round + dx;
                if xc < 1 || xc >= w as i32 - 1 {
                    continue;
                }
                if edge_mag[y * w + (xc as usize)] >= cfg.edge_threshold {
                    if y_min.is_none() {
                        y_min = Some(y);
                    }
                    y_max = Some(y);
                    break;
                }
            }
        }
        let (Some(yt), Some(yb)) = (y_min, y_max) else {
            continue;
        };
        let dy_total = (yb as f64) - (yt as f64);
        if dy_total < min_len {
            continue;
        }
        let x_top = f64::from(best_rho) + tan_t * (yt as f64 - cy);
        let x_bot = f64::from(best_rho) + tan_t * (yb as f64 - cy);
        let length_px = (dy_total * dy_total + (x_bot - x_top).powi(2)).sqrt();
        if length_px < min_len {
            continue;
        }
        lines.push(DetectedLine {
            top: (x_top, yt as f64),
            bot: (x_bot, yb as f64),
            length_px,
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, Intrinsics};
    use bris_core::time::{Tt, JD_J2000};

    fn intr() -> Intrinsics {
        Intrinsics::placeholder(128, 96)
    }

    /// Build a synthetic frame with a near-vertical bright
    /// stripe on a dark background.
    ///
    /// `tilt_rad`: tilt from image-vertical, positive = top
    /// leans right. Line spans full image height.
    fn frame_with_line(tilt_rad: f64, center_x: f64) -> Frame {
        let i = intr();
        let w = 128_u32;
        let h = 96_u32;
        let mut pixels = vec![0_u16; (w * h) as usize];
        // Dark background; bright line.
        let cy = f64::from(h) / 2.0;
        for y in 0..h {
            let dy = f64::from(y) - cy;
            let x_line = center_x + tilt_rad.tan() * dy;
            for x in 0..w {
                // Thick stripe: ±1 px so Sobel-x produces a
                // strong edge on either side.
                if ((f64::from(x) - x_line).abs()) < 1.5 {
                    pixels[(y * w + x) as usize] = 60_000;
                }
            }
        }
        Frame::new(w, h, pixels, Tt::from_julian_date(JD_J2000), 1000, i).unwrap()
    }

    fn empty_frame() -> Frame {
        let i = intr();
        Frame::new(
            128,
            96,
            vec![0_u16; 128 * 96],
            Tt::from_julian_date(JD_J2000),
            1000,
            i,
        )
        .unwrap()
    }

    fn ctx_for<'a>(f: &'a Frame, i: &'a Intrinsics) -> HorizonProviderContext<'a> {
        HorizonProviderContext {
            frame: f,
            intrinsics: i,
            body_candidates: &[],
            position_prior: None,
            timestamp: Tt::from_julian_date(JD_J2000),
        }
    }

    #[test]
    fn vertical_line_yields_horizontal_horizon() {
        // Perfectly vertical line, centered → gravity ≈ +y,
        // horizon ≈ horizontal at cy.
        let f = frame_with_line(0.0, 64.0);
        let i = intr();
        let provider = VerticalLineProvider::default();
        let ctx = ctx_for(&f, &i);
        let hyp = provider.detect(&ctx).expect("vertical line must detect");
        assert!(
            hyp.line.slope.abs() < 0.05,
            "expected near-horizontal horizon, slope={}",
            hyp.line.slope,
        );
        assert!(
            (hyp.line.intercept - i.cy).abs() < 2.0,
            "intercept {} far from cy {}",
            hyp.line.intercept,
            i.cy,
        );
    }

    #[test]
    fn empty_frame_returns_none() {
        let f = empty_frame();
        let i = intr();
        let provider = VerticalLineProvider::default();
        let ctx = ctx_for(&f, &i);
        let mut stats = VerticalLineStats::default();
        assert!(provider.detect_with_stats(&ctx, &mut stats).is_none());
        assert_eq!(stats.rejected_no_lines, 1);
        assert_eq!(stats.hypothesized, 0);
    }

    #[test]
    fn strongly_tilted_line_rejected() {
        // 30° from vertical → outside the default 20° window.
        let tilt = 30.0_f64.to_radians();
        let f = frame_with_line(tilt, 64.0);
        let i = intr();
        let provider = VerticalLineProvider::default();
        let ctx = ctx_for(&f, &i);
        assert!(
            provider.detect(&ctx).is_none(),
            "30° tilt must be rejected (default window is ±20°)",
        );
    }

    #[test]
    fn short_line_gives_larger_sigma_than_long_line() {
        // Compare a full-height line vs a short half-height
        // line. The long-line σ must be ≤ short-line σ — both
        // floor at `sigma_floor_rad`, so configure a very
        // small floor for the comparison to be meaningful.
        let cfg = VerticalLineConfig {
            sigma_floor_rad: 1e-9,
            min_line_length_px: 20,
            ..VerticalLineConfig::default()
        };

        // Long line: full image height.
        let f_long = frame_with_line(0.0, 64.0);

        // Short line: 25 px tall, centered.
        let i = intr();
        let w = 128_u32;
        let h = 96_u32;
        let mut short_pixels = vec![0_u16; (w * h) as usize];
        for y in 36..61_u32 {
            for x in 0..w {
                if (f64::from(x) - 64.0).abs() < 1.5 {
                    short_pixels[(y * w + x) as usize] = 60_000;
                }
            }
        }
        let f_short =
            Frame::new(w, h, short_pixels, Tt::from_julian_date(JD_J2000), 1000, i).unwrap();

        let provider = VerticalLineProvider { config: cfg };
        let hyp_long = provider.detect(&ctx_for(&f_long, &i)).expect("long");
        let hyp_short = provider.detect(&ctx_for(&f_short, &i)).expect("short");
        assert!(
            hyp_long.line.altitude_sigma.value() < hyp_short.line.altitude_sigma.value(),
            "long-line σ {} should be < short-line σ {}",
            hyp_long.line.altitude_sigma.value(),
            hyp_short.line.altitude_sigma.value(),
        );
    }
}
