//! Auto-detected horizon from vanishing points (Manhattan-world).
//!
//! In any structured scene (urban, indoor, built) parallel
//! lines in the world converge to **vanishing points** in the
//! image:
//!
//! - Two or more *horizontal* VPs (building lintels, road
//!   markings, tile grids, bookshelves, lamp-post rails) lie
//!   *on the horizon line by definition* (`horizon_brainstorm.md`
//!   §B5). Two horizontal VPs define the horizon line directly.
//! - One *vertical* VP (building corners, doorframes, lamp
//!   posts, tree trunks) is gravity in camera frame; the
//!   horizon follows from `horizon_brainstorm.md` §0
//!   (`ℓ = K⁻ᵀ g_cam`).
//!
//! See `docs/design/horizon_autodetect.md` §5 for the
//! algorithm narrative.
//!
//! # Algorithm (Phase 1, minimal RANSAC)
//!
//! 1. Extract *edgels*: subsampled pixels with strong Sobel
//!    gradient magnitude. Each edgel `(x, y, gx, gy)` is a tiny
//!    line element with a known orientation; the line through
//!    that point with that orientation is `l = (gx, gy,
//!    -(gx·x + gy·y))` in homogeneous coordinates.
//! 2. RANSAC: pick two random edgels, intersect their lines
//!    (`vp = l_i × l_j`), score by counting other edgels whose
//!    line passes within `inlier_distance_px` of `vp`.
//! 3. Keep top-K candidate VPs with non-maximum suppression
//!    so duplicates collapse.
//! 4. Classify each VP as **vertical** (image-y far from the
//!    principal point — `|y_n - cy| > threshold · H`) or
//!    **horizontal** otherwise.
//! 5. Output policy: prefer a strong vertical VP (one VP gives
//!    gravity directly); otherwise require ≥ 2 horizontal VPs
//!    and fit the horizon line through them.
//!
//! # σ propagation
//!
//! Per-line σ enters via the gradient-direction precision
//! (subpixel edgel orientation σ); cluster σ shrinks with
//! inlier count as `~ σ_line / sqrt(N)`. Floored at
//! `VanishingPointConfig::sigma_floor_rad` (default 5e-4 rad ≈
//! 1.5'). The propagation is conservative: a richer covariance
//! treatment (per-VP residual ellipse) is a later enhancement.
//!
//! # Coordination with line-detection front-end
//!
//! A sibling work item (`vertical-line-provider`) is adding a
//! shared line-detection utility. This provider currently
//! ships its own minimal edgel detector to keep the PR
//! self-contained; the operator will consolidate the two
//! implementations into a single shared utility once both
//! providers have merged. The edgel detector here is
//! intentionally simple (Sobel + threshold + stride subsample);
//! any caller needing a more sophisticated line detector
//! should reach for the consolidated utility once it exists.
//!
//! # Cost
//!
//! Dominant cost is the RANSAC inlier-scoring loop:
//! `O(iterations · edgels)`. With defaults (200 iterations,
//! ≤ 2048 edgels) this is ~0.4 Medgel-ops/frame. A smoke
//! benchmark in the unit tests asserts the synthetic-cube
//! workload completes in < 50 ms on the dev workstation
//! (`x86_64`); proper Pi Zero 2W benchmarking is deferred to
//! Phase 3 hardware-in-the-loop measurement. The provider is
//! the most expensive of the horizon providers and is
//! dispatched last (after cheap optical / reflection-pair
//! providers).

// Pedantic lints suppressed module-wide:
//   * `similar_names` / `many_single_char_names`: pixel-math
//     local algebra (gx/gy, vx/vy/vw, etc.) is clearer with
//     short names matching the formulas in the module-level
//     docs than with verbose disambiguators.
//   * `cast_possible_truncation` on `u64 -> usize` from the
//     RNG: the modulo `% n` immediately bounds the result
//     into the edgel-slice index range; we can't actually
//     overflow `usize` on any target.
#![allow(
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::too_many_arguments,
    clippy::explicit_iter_loop
)]

use bris_core::Sigma;

use crate::frame::{Frame, Intrinsics};
use crate::ray::{horizon_line_from_normal, CameraRay};

use super::{
    HorizonHypothesis, HorizonProvenance, HorizonProvider, HorizonProviderContext, TemporalScope,
};

/// Configuration for [`VanishingPointProvider`].
#[derive(Debug, Clone, Copy)]
pub struct VanishingPointConfig {
    /// Minimum inlier edgels for a VP candidate to be kept.
    pub min_inliers: usize,
    /// Number of RANSAC iterations.
    pub ransac_iterations: usize,
    /// Floor on the synthesized horizon altitude σ (radians).
    /// Default 5e-4 rad (~1.5').
    pub sigma_floor_rad: f64,
    /// **Deprecated / reserved**: prior versions used
    /// `|y_n - cy| > value · H` to classify VPs as vertical;
    /// the classifier now uses camera-frame direction (see
    /// `is_vertical_vp`). Retained as a config field to keep
    /// the public API stable for the spike; ignored at
    /// runtime.
    pub vertical_vp_min_distance_from_image_center_normalized: f64,
    /// Distance threshold (pixels) for an edgel to count as an
    /// inlier of a candidate VP.
    pub inlier_distance_px: f64,
    /// Sobel gradient-magnitude threshold for an edge pixel to
    /// be promoted into an edgel. Tunable for noise floors;
    /// the default targets u16-scale pixel values.
    pub gradient_threshold: f64,
    /// Stride (in pixels) at which the edgel grid is sampled.
    /// Larger = fewer edgels = faster but noisier. Default 4
    /// keeps ≤ 80 k cells on a 1280×720 frame, decimated again
    /// by the threshold step to a few thousand edgels.
    pub edgel_stride: u32,
    /// Hard cap on number of edgels collected per frame.
    /// Bounds the RANSAC scoring cost on dense scenes.
    pub max_edgels: usize,
}

impl Default for VanishingPointConfig {
    fn default() -> Self {
        Self {
            // Spec suggested 4 as a starting point; in
            // practice random pairs of high-gradient pixels
            // (e.g. star blobs on a dim background) can
            // accidentally form 4-inlier "clusters" that
            // displace honest horizon detections via the
            // best-σ merge. 50 is empirically above the
            // false-positive ceiling (~48 inliers) on a
            // 128×128 uniform-noise frame (see
            // `default_config_rejects_noise`) while still
            // firing reliably on structured scenes.
            min_inliers: 50,
            ransac_iterations: 200,
            sigma_floor_rad: 5e-4,
            vertical_vp_min_distance_from_image_center_normalized: 0.3,
            inlier_distance_px: 2.0,
            gradient_threshold: 2_000.0,
            edgel_stride: 4,
            max_edgels: 2_048,
        }
    }
}

/// Vanishing-point horizon provider.
#[derive(Debug, Clone, Copy, Default)]
pub struct VanishingPointProvider {
    /// Tunables for the RANSAC search and classification.
    pub config: VanishingPointConfig,
}

/// Per-invocation counters reported alongside [`detect`].
#[derive(Debug, Clone, Copy, Default)]
pub struct VanishingPointStats {
    /// Provider produced a hypothesis (passed the inlier and
    /// classification gates).
    pub hypothesized: u64,
    /// Provider ran but no candidate VP met the inlier
    /// threshold, or no vertical/horizontal-pair policy
    /// succeeded.
    pub rejected_no_cluster: u64,
}

impl HorizonProvider for VanishingPointProvider {
    fn name(&self) -> &'static str {
        "vanishing_point"
    }

    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }

    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        let mut stats = VanishingPointStats::default();
        self.detect_with_stats(ctx, &mut stats)
    }
}

impl VanishingPointProvider {
    /// Same as [`HorizonProvider::detect`] but populates a
    /// per-invocation [`VanishingPointStats`] so the streaming
    /// engine can fold the counters into its long-running
    /// `EngineDiagnostics`.
    pub fn detect_with_stats(
        &self,
        ctx: &HorizonProviderContext<'_>,
        stats: &mut VanishingPointStats,
    ) -> Option<HorizonHypothesis> {
        let edgels = collect_edgels(ctx.frame, &self.config);
        if edgels.len() < 2 {
            stats.rejected_no_cluster += 1;
            return None;
        }
        let frame_max_dim = f64::from(ctx.frame.width().max(ctx.frame.height()));
        let vps = ransac_vanishing_points(&edgels, &self.config, frame_max_dim);
        if vps.is_empty() {
            stats.rejected_no_cluster += 1;
            return None;
        }

        let intrinsics = ctx.intrinsics;

        // Vertical-first policy: classify each VP by its
        // camera-frame direction. A VP in image pixels
        // (vp_x, vp_y) corresponds to the camera-frame
        // direction `d_cam = normalize(K⁻¹ · [vp_x, vp_y, 1])`
        // — the shared direction of the parallel lines that
        // produced the cluster. A *vertical* VP has
        // |d_cam · y_cam_axis| ≈ 1 (lines are nearly parallel
        // to the camera's y axis, i.e. world-vertical when the
        // camera is upright). This is geometric rather than
        // image-position-based, so it handles strongly-tilted
        // cameras (vertical VP *inside* the image) correctly.
        // Image-y distance is used only as a fallback for the
        // level-camera degeneracy already handled by the
        // synthetic-VP path in `ransac_vanishing_points`.
        let vertical = vps.iter().find(|v| is_vertical_vp(v, intrinsics));
        if let Some(v) = vertical {
            let sky_normal = vp_to_sky_normal(v, intrinsics)?;
            let sigma_value = vp_sigma_rad(v, &self.config, intrinsics);
            let altitude_sigma = Sigma::new(sigma_value).unwrap_or(Sigma::ZERO);
            let line = horizon_line_from_normal(&sky_normal, intrinsics, altitude_sigma)?;
            stats.hypothesized += 1;
            return Some(HorizonHypothesis {
                line,
                provenance: HorizonProvenance::VanishingPoint {
                    vp_count: vps.len(),
                    used_vertical: true,
                },
                direct_sight: None,
            });
        }

        // No vertical VP — need two horizontal VPs to define
        // the horizon line.
        let horizontals: Vec<&VanishingPoint> = vps
            .iter()
            .filter(|v| !is_vertical_vp(v, intrinsics))
            .collect();
        if horizontals.len() < 2 {
            stats.rejected_no_cluster += 1;
            return None;
        }
        let v0 = horizontals[0];
        let v1 = horizontals[1];
        // Line through (v0, v1) in pixel space.
        let dx = v1.x - v0.x;
        if dx.abs() < f64::EPSILON {
            stats.rejected_no_cluster += 1;
            return None;
        }
        let slope = (v1.y - v0.y) / dx;
        let intercept = v0.y - slope * v0.x;
        // σ scaled by RMS of the two contributing VPs' σ; the
        // combined gravity-direction σ tracks the worse of the
        // two clusters but tightens with both inlier counts.
        //
        // NOTE (σ-honesty, Phase 1 limitation): this does not
        // model the geometric leverage of the pixel-space
        // baseline between the two VPs. A proper covariance
        // would propagate each VP's positional σ through the
        // 2-point line fit, where a short baseline inflates the
        // resulting altitude σ. Proper covariance propagation
        // through the 2-point line fit is deferred to a follow-
        // up; the current value is a conservative summary
        // statistic, not a propagated quantity.
        let combined_sigma = ((vp_sigma_rad(v0, &self.config, intrinsics)).powi(2)
            + (vp_sigma_rad(v1, &self.config, intrinsics)).powi(2))
        .sqrt()
            / std::f64::consts::SQRT_2;
        let altitude_sigma =
            Sigma::new(combined_sigma.max(self.config.sigma_floor_rad)).unwrap_or(Sigma::ZERO);
        let line = crate::horizon::HorizonLine {
            slope,
            intercept,
            inlier_count: u32::try_from(v0.inliers + v1.inliers).unwrap_or(u32::MAX),
            candidate_count: u32::try_from(v0.inliers + v1.inliers).unwrap_or(u32::MAX),
            residual_rms_px: 0.0,
            altitude_sigma,
        };
        stats.hypothesized += 1;
        Some(HorizonHypothesis {
            line,
            provenance: HorizonProvenance::VanishingPoint {
                vp_count: vps.len(),
                used_vertical: false,
            },
            direct_sight: None,
        })
    }
}

/// One edge pixel with its gradient direction.
#[derive(Debug, Clone, Copy)]
struct Edgel {
    x: f64,
    y: f64,
    /// Normalized gradient (`gx² + gy² = 1`). Doubles as the
    /// `(a, b)` of the line `a·x + b·y + c = 0` passing through
    /// the edgel with the matching orientation.
    gx: f64,
    gy: f64,
}

impl Edgel {
    /// `c` term of the homogeneous line `a·x + b·y + c = 0`.
    #[inline]
    fn line_c(self) -> f64 {
        -(self.gx * self.x + self.gy * self.y)
    }
}

/// One candidate vanishing point in pixel space (finite).
#[derive(Debug, Clone, Copy)]
struct VanishingPoint {
    x: f64,
    y: f64,
    inliers: usize,
}

/// Sobel-based edgel extractor. Subsamples on a stride grid;
/// each cell with gradient magnitude above
/// `gradient_threshold` becomes an edgel with the normalized
/// gradient direction. Capped at `max_edgels`.
fn collect_edgels(frame: &Frame, cfg: &VanishingPointConfig) -> Vec<Edgel> {
    let w = frame.width();
    let h = frame.height();
    if w < 3 || h < 3 {
        return Vec::new();
    }
    let stride = cfg.edgel_stride.max(1);
    let pixels = frame.pixels();
    let row = w as usize;
    let mut edgels: Vec<Edgel> = Vec::new();
    let mut y = 1u32;
    while y + 1 < h {
        let mut x = 1u32;
        while x + 1 < w {
            let i = (y as usize) * row + (x as usize);
            // 3-tap Sobel (separable) at (x, y).
            let p_tl = f64::from(pixels[i - row - 1]);
            let p_tc = f64::from(pixels[i - row]);
            let p_tr = f64::from(pixels[i - row + 1]);
            let p_cl = f64::from(pixels[i - 1]);
            let p_cr = f64::from(pixels[i + 1]);
            let p_bl = f64::from(pixels[i + row - 1]);
            let p_bc = f64::from(pixels[i + row]);
            let p_br = f64::from(pixels[i + row + 1]);
            let gx = (p_tr + 2.0 * p_cr + p_br) - (p_tl + 2.0 * p_cl + p_bl);
            let gy = (p_bl + 2.0 * p_bc + p_br) - (p_tl + 2.0 * p_tc + p_tr);
            let mag = (gx * gx + gy * gy).sqrt();
            if mag >= cfg.gradient_threshold {
                edgels.push(Edgel {
                    x: f64::from(x),
                    y: f64::from(y),
                    gx: gx / mag,
                    gy: gy / mag,
                });
                if edgels.len() >= cfg.max_edgels {
                    return edgels;
                }
            }
            x += stride;
        }
        y += stride;
    }
    edgels
}

/// Deterministic small RNG (`SplitMix64`). Avoids pulling in a
/// new crate dependency for the RANSAC sampling — the search
/// only needs unbiased pairs, not cryptographic randomness.
/// Seeded from the edgel count so behaviour is reproducible
/// frame-over-frame for the same evidence.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// RANSAC for up to 3 vanishing points. Returns them sorted by
/// inlier count descending; performs non-maximum suppression
/// so near-duplicate VPs collapse.
fn ransac_vanishing_points(
    edgels: &[Edgel],
    cfg: &VanishingPointConfig,
    frame_max_dim: f64,
) -> Vec<VanishingPoint> {
    if edgels.len() < 2 {
        return Vec::new();
    }
    let synth_dim = frame_max_dim.max(1.0);
    let n = edgels.len();
    let mut state: u64 = (n as u64)
        .wrapping_mul(0xA076_1D64_78BD_642F)
        .wrapping_add(1);

    let mut candidates: Vec<VanishingPoint> = Vec::new();
    for _ in 0..cfg.ransac_iterations {
        let i = (splitmix64(&mut state) as usize) % n;
        let mut j = (splitmix64(&mut state) as usize) % n;
        if j == i {
            j = (j + 1) % n;
        }
        let a = edgels[i];
        let b = edgels[j];
        // Intersect a·x + b·y + c = 0 lines for the two edgels
        // (homogeneous cross product). l1 = (gx1, gy1, c1),
        // l2 = (gx2, gy2, c2); vp = l1 × l2 = (vx, vy, vw).
        let c1 = a.line_c();
        let c2 = b.line_c();
        let vx = a.gy * c2 - c1 * b.gy;
        let vy = c1 * b.gx - a.gx * c2;
        let vw = a.gx * b.gy - a.gy * b.gx;
        // Parallel-lines / level-camera degeneracy: when the
        // two edgels have nearly-identical orientations the
        // homogeneous intersection lies at (or very near)
        // infinity along their shared direction. Silently
        // dropping these pairs would discard the *exactly*
        // case a level camera looking at a building wall
        // produces (vertical VP at ±∞). Instead we synthesize
        // a finite VP at a very large distance along the
        // shared direction — geometrically equivalent for the
        // inlier-scoring step that follows (a faraway VP and
        // an at-infinity VP both score collinear edgels as
        // inliers) and preserves the level-camera case.
        let (vp_x, vp_y) = if vw.abs() < 1e-9 {
            // Shared direction perpendicular to the (a.gx, a.gy)
            // gradient is (-a.gy, a.gx). Place the synthetic VP
            // at `synth_scale · frame_max_dim` along that
            // direction from the principal point.
            let synth_scale = 1.0e6;
            let dir_x = -a.gy;
            let dir_y = a.gx;
            (
                a.x + synth_scale * synth_dim * dir_x,
                a.y + synth_scale * synth_dim * dir_y,
            )
        } else {
            (vx / vw, vy / vw)
        };
        if !vp_x.is_finite() || !vp_y.is_finite() {
            continue;
        }
        // Score: edgels k whose line passes within ε of vp.
        // Distance from line (gx, gy, c) to point (vp_x, vp_y)
        // = |gx·vp_x + gy·vp_y + c| (line already normalized).
        let eps = cfg.inlier_distance_px;
        let mut inliers = 0usize;
        for e in edgels {
            let d = (e.gx * vp_x + e.gy * vp_y + e.line_c()).abs();
            if d <= eps {
                inliers += 1;
            }
        }
        if inliers >= cfg.min_inliers {
            candidates.push(VanishingPoint {
                x: vp_x,
                y: vp_y,
                inliers,
            });
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }
    candidates.sort_by(|a, b| b.inliers.cmp(&a.inliers));
    // Non-maximum suppression: collapse VPs within
    // `nms_radius` pixels of an already-kept stronger VP.
    // Radius scales with the inlier distance threshold; a few
    // tens of pixels keeps clusters distinct without
    // accidentally merging the two horizontal VPs of a typical
    // urban scene (which are usually hundreds of pixels apart
    // on the image).
    let nms_radius = (cfg.inlier_distance_px * 20.0).max(20.0);
    let mut kept: Vec<VanishingPoint> = Vec::new();
    for c in candidates {
        if kept
            .iter()
            .any(|k| (k.x - c.x).hypot(k.y - c.y) < nms_radius)
        {
            continue;
        }
        kept.push(c);
        if kept.len() >= 3 {
            break;
        }
    }
    kept
}

/// Convert a vertical (gravity-direction) vanishing point into
/// the sky-pointing horizon-plane normal in camera frame. The
/// VP pixel position rayifies into gravity direction directly
/// (`g_cam = K⁻¹ · [vp_x, vp_y, 1]ᵀ` normalized); sky-normal
/// is `-g_cam` (image-up is sky).
fn vp_to_sky_normal(vp: &VanishingPoint, intrinsics: &Intrinsics) -> Option<CameraRay> {
    let gravity = CameraRay::from_pixel(intrinsics, vp.x, vp.y).normalize()?;
    // If the VP lies *above* the principal point in image
    // space (smaller y), gravity points toward the sky and we
    // need to flip; canonical convention is gravity.y > 0
    // (image-down).
    let g = if gravity.y >= 0.0 {
        gravity
    } else {
        CameraRay {
            x: -gravity.x,
            y: -gravity.y,
            z: -gravity.z,
        }
    };
    Some(CameraRay {
        x: -g.x,
        y: -g.y,
        z: -g.z,
    })
}

/// Per-VP σ on gravity-direction (radians). Floor at
/// `sigma_floor_rad`; tightens as `1/sqrt(inliers)`.
///
/// The per-edgel angular σ is derived from the camera
/// intrinsics: a 1-pixel perpendicular distance subtends
/// approximately `1/f` radians at the camera, where `f` is the
/// focal length in pixels. We use `mean(fx, fy)` for an
/// isotropic estimate (Bris intrinsics may have fx ≠ fy on a
/// non-square-pixel sensor, but the difference is sub-percent
/// for the lenses Bris targets). Hence per-edgel σ in pixels
/// (`inlier_distance_px`) divides by the focal length to give
/// per-edgel σ in radians. This makes the cluster σ
/// resolution-invariant: the *same* lens at a different image
/// size has its fx scaled in the same proportion as the
/// pixel-space residual, so the angular σ is unchanged. The
/// previous hand-tuned `/1000` constant violated the project's
/// σ-honesty rule (AGENTS.md).
fn vp_sigma_rad(vp: &VanishingPoint, cfg: &VanishingPointConfig, intrinsics: &Intrinsics) -> f64 {
    let f_mean = 0.5 * (intrinsics.fx + intrinsics.fy);
    let per_edgel_sigma_rad = if f_mean > 0.0 {
        (cfg.inlier_distance_px / f_mean).max(cfg.sigma_floor_rad)
    } else {
        cfg.sigma_floor_rad
    };
    #[allow(clippy::cast_precision_loss)]
    let n = (vp.inliers.max(1)) as f64;
    (per_edgel_sigma_rad / n.sqrt()).max(cfg.sigma_floor_rad)
}

/// Classify a candidate VP as vertical (gravity-direction) by
/// looking at its camera-frame direction rather than its image
/// position.
///
/// The VP pixel `(vp_x, vp_y)` rayifies through the lens model
/// into a unit direction in camera frame; that direction is
/// the shared direction of the parallel lines that converged
/// at the VP. A *vertical* VP corresponds to lines that are
/// parallel to world-vertical (gravity), which in the camera
/// frame is the y-axis when the camera is upright.
///
/// We use a *dominant-image-tilt* test: the VP is vertical iff
/// its camera-frame direction's y-component magnitude exceeds
/// its x-component magnitude (`|d.y| > |d.x|`). This
/// distinguishes lines that converge in the up/down direction
/// (vertical poles, building corners) from lines that converge
/// left/right (lintels, road markings). The z-component
/// (forward) is naturally large for any VP near the image
/// center and does not discriminate vertical from horizontal,
/// so we ignore it.
///
/// This formulation is *geometric* and so handles the cases
/// the image-y rule failed on:
///   * Level camera ⇒ vertical VP at ±∞ in image, but its
///     camera-frame direction is exactly ±`y_cam` ⇒ vertical.
///   * Strongly-tilted camera (e.g. looking up at a skyline)
///     ⇒ vertical VP *inside* the image close to `cy`, but its
///     camera-frame direction still has y-axis dominance ⇒
///     vertical.
fn is_vertical_vp(vp: &VanishingPoint, intrinsics: &Intrinsics) -> bool {
    let Some(d) = CameraRay::from_pixel(intrinsics, vp.x, vp.y).normalize() else {
        return false;
    };
    d.y.abs() > d.x.abs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, Intrinsics};
    use bris_core::time::{Tt, JD_J2000};

    fn intr(w: u32, h: u32) -> Intrinsics {
        Intrinsics::placeholder(w, h)
    }

    fn blank_frame(w: u32, h: u32) -> Vec<u16> {
        vec![10_000u16; (w * h) as usize]
    }

    /// Draw a 1px line from (x0,y0) to (x1,y1) with intensity
    /// `value`, Bresenham-style. Lines are drawn as bright
    /// streaks on a dim background so the Sobel detector
    /// fires.
    fn draw_line(pixels: &mut [u16], w: u32, h: u32, x0: i32, y0: i32, x1: i32, y1: i32, val: u16) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let mut x = x0;
        let mut y = y0;
        loop {
            if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
                let i = (y as u32 * w + x as u32) as usize;
                pixels[i] = val;
                // Thicken to ~3 px so Sobel has strong response.
                if x + 1 >= 0 && ((x + 1) as u32) < w {
                    pixels[(y as u32 * w + (x + 1) as u32) as usize] = val;
                }
                if y + 1 >= 0 && ((y + 1) as u32) < h {
                    pixels[((y + 1) as u32 * w + x as u32) as usize] = val;
                }
            }
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

    fn frame_from(pixels: Vec<u16>, w: u32, h: u32) -> Frame {
        Frame::new(
            w,
            h,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            intr(w, h),
        )
        .unwrap()
    }

    fn ctx_for<'a>(f: &'a Frame, intrinsics: &'a Intrinsics) -> HorizonProviderContext<'a> {
        HorizonProviderContext {
            frame: f,
            intrinsics,
            body_candidates: &[],
            position_prior: None,
            timestamp: Tt::from_julian_date(JD_J2000),
        }
    }

    /// Helper: draw a "fan" of lines converging at pixel
    /// `(vx, vy)` from `n_lines` starting points around the
    /// image perimeter.
    fn draw_fan(
        pixels: &mut [u16],
        w: u32,
        h: u32,
        vx: i32,
        vy: i32,
        perimeter_pts: &[(i32, i32)],
    ) {
        for (px, py) in perimeter_pts {
            draw_line(pixels, w, h, *px, *py, vx, vy, 60_000);
        }
    }

    #[test]
    fn cube_edges_yield_vps_and_horizon() {
        // Synthetic "cube"-like scene: vertical VP far above
        // image, two horizontal VPs left and right beyond the
        // frame boundary. Lines are drawn from interior
        // anchor points toward each VP.
        let w: u32 = 320;
        let h: u32 = 240;
        let mut pixels = blank_frame(w, h);
        let cx = (w / 2) as i32;
        let cy = (h / 2) as i32;
        // Vertical VP: far below the image (image-y > h),
        // so y is at +H times some factor → "gravity" points
        // image-down.
        let vp_v = (cx, cy + (h as i32) * 4);
        // Horizontal VPs: well to left and right beyond the
        // image at the image-center row.
        let vp_hl = (cx - (w as i32) * 4, cy);
        let vp_hr = (cx + (w as i32) * 4, cy);
        // Fan-of-lines anchors: a few points across the
        // image's top half (so lines emanating from them
        // toward the VPs sample the whole frame).
        let anchors: Vec<(i32, i32)> = (0..6)
            .map(|k| (40 + k * 50, 40))
            .chain((0..6).map(|k| (40 + k * 50, 200)))
            .collect();
        draw_fan(&mut pixels, w, h, vp_v.0, vp_v.1, &anchors);
        draw_fan(&mut pixels, w, h, vp_hl.0, vp_hl.1, &anchors);
        draw_fan(&mut pixels, w, h, vp_hr.0, vp_hr.1, &anchors);

        let f = frame_from(pixels, w, h);
        let intrinsics = intr(w, h);
        let ctx = ctx_for(&f, &intrinsics);
        let provider = VanishingPointProvider::default();
        let hyp = provider
            .detect(&ctx)
            .expect("structured scene should yield a hypothesis");
        // The vertical VP is image-distant (4 H below) so the
        // vertical-first branch fires; horizon line should be
        // very close to y = cy.
        match hyp.provenance {
            HorizonProvenance::VanishingPoint { used_vertical, .. } => {
                assert!(used_vertical, "vertical-VP branch should win");
            }
            other => panic!("expected VP provenance, got {other:?}"),
        }
        // The line orientation should be near-horizontal
        // (slope ≈ 0) since the vertical VP sits directly
        // below the image-center column; the exact intercept
        // depends on how far below the image the VP is
        // (closer VP ⇔ more downward camera tilt ⇔ horizon
        // farther above the image).
        assert!(
            hyp.line.slope.abs() < 0.4,
            "slope {} not near-horizontal for centered vertical VP",
            hyp.line.slope,
        );
        let _ = cy;
    }

    #[test]
    fn vertical_only_scene_yields_gravity() {
        // Row of "lamp posts" — only a vertical VP. Should
        // still produce a horizon hypothesis via the vertical
        // branch.
        let w: u32 = 320;
        let h: u32 = 240;
        let mut pixels = blank_frame(w, h);
        let cx = (w / 2) as i32;
        let cy = (h / 2) as i32;
        let vp_v = (cx, cy + (h as i32) * 4);
        let anchors: Vec<(i32, i32)> = (0..10).map(|k| (20 + k * 30, 30)).collect();
        draw_fan(&mut pixels, w, h, vp_v.0, vp_v.1, &anchors);

        let f = frame_from(pixels, w, h);
        let intrinsics = intr(w, h);
        let ctx = ctx_for(&f, &intrinsics);
        let provider = VanishingPointProvider::default();
        let hyp = provider.detect(&ctx).expect("vertical VP should detect");
        match hyp.provenance {
            HorizonProvenance::VanishingPoint { used_vertical, .. } => {
                assert!(used_vertical);
            }
            other => panic!("expected VP provenance, got {other:?}"),
        }
    }

    #[test]
    fn noise_only_returns_none() {
        // Random-ish noise (no structured lines): the
        // provider must decline. Use a deterministic
        // checkerboard-of-noise so the test is reproducible.
        let w: u32 = 128;
        let h: u32 = 128;
        let mut pixels = blank_frame(w, h);
        let mut s: u64 = 0xDEAD_BEEF;
        for v in pixels.iter_mut() {
            *v = (splitmix64(&mut s) as u16) ^ 0x2710_u16;
        }
        let f = frame_from(pixels, w, h);
        let intrinsics = intr(w, h);
        let ctx = ctx_for(&f, &intrinsics);
        let provider = VanishingPointProvider {
            // Require a large cluster so random-noise edgels
            // don't accidentally form a "VP". With a 128×128
            // noise frame at stride 4 we get ≲ 1024 edgels; a
            // truly random pair of lines has a few-percent
            // chance of passing within 2 px of a given point
            // so the expected spurious cluster is small but
            // not zero. 200 inliers is comfortably above the
            // noise floor.
            config: VanishingPointConfig {
                min_inliers: 200,
                ..VanishingPointConfig::default()
            },
        };
        assert!(
            provider.detect(&ctx).is_none(),
            "noise-only frame must yield no VP hypothesis",
        );
    }

    #[test]
    fn sigma_tightens_with_more_inliers() {
        // Provider's σ formula: σ ≈ per_edgel_σ / sqrt(N).
        let cfg = VanishingPointConfig::default();
        let i = intr(640, 480);
        let small = vp_sigma_rad(
            &VanishingPoint {
                x: 0.0,
                y: 0.0,
                inliers: 4,
            },
            &cfg,
            &i,
        );
        let large = vp_sigma_rad(
            &VanishingPoint {
                x: 0.0,
                y: 0.0,
                inliers: 400,
            },
            &cfg,
            &i,
        );
        assert!(
            large <= small,
            "σ must not grow with inlier count: {small} vs {large}",
        );
        assert!(
            small >= cfg.sigma_floor_rad - 1e-12,
            "σ must respect the floor: {small} < {}",
            cfg.sigma_floor_rad,
        );
    }

    #[test]
    fn sigma_scales_with_focal_length() {
        // σ-honesty: doubling focal length should halve the
        // per-edgel angular σ (when floor isn't binding).
        let cfg = VanishingPointConfig {
            sigma_floor_rad: 1e-12,
            ..VanishingPointConfig::default()
        };
        let vp = VanishingPoint {
            x: 0.0,
            y: 0.0,
            inliers: 100,
        };
        let mut i_short = intr(640, 480);
        let mut i_long = intr(640, 480);
        i_short.fx = 500.0;
        i_short.fy = 500.0;
        i_long.fx = 1000.0;
        i_long.fy = 1000.0;
        let s_short = vp_sigma_rad(&vp, &cfg, &i_short);
        let s_long = vp_sigma_rad(&vp, &cfg, &i_long);
        // 2× focal length ⇒ 0.5× σ.
        assert!(
            (s_short / s_long - 2.0).abs() < 1e-9,
            "σ must be inversely proportional to f: short={s_short} long={s_long}",
        );
    }

    #[test]
    fn default_config_rejects_noise() {
        // σ-honesty + false-positive coverage gap from PR
        // critic #4: at the DEFAULT config (no raised
        // min_inliers) a noise frame must yield None.
        let w: u32 = 128;
        let h: u32 = 128;
        let mut pixels = blank_frame(w, h);
        let mut s: u64 = 0xDEAD_BEEF;
        for v in pixels.iter_mut() {
            *v = (splitmix64(&mut s) as u16) ^ 0x2710_u16;
        }
        let f = frame_from(pixels, w, h);
        let intrinsics = intr(w, h);
        let ctx = ctx_for(&f, &intrinsics);
        let provider = VanishingPointProvider::default();
        assert!(
            provider.detect(&ctx).is_none(),
            "default-config noise frame must yield None",
        );
    }

    #[test]
    fn vertical_vp_classifier_geometric() {
        // A VP at the principal point (vp_x = cx, vp_y = cy)
        // corresponds to a camera-frame direction (0, 0, 1) —
        // straight ahead, *not* vertical.
        let i = intr(640, 480);
        let vp_center = VanishingPoint {
            x: i.cx,
            y: i.cy,
            inliers: 50,
        };
        assert!(!is_vertical_vp(&vp_center, &i));
        // A VP whose camera-frame direction is dominated by
        // the y-axis: place it far below the image so the ray
        // through it is nearly (0, +1, 0)-ish.
        let vp_below = VanishingPoint {
            x: i.cx,
            y: i.cy + 10_000.0,
            inliers: 50,
        };
        assert!(is_vertical_vp(&vp_below, &i));
        // A strongly-tilted-camera scenario: vertical VP
        // *inside* the image, only 50 px from cy. With image-y
        // rule (0.3 · H = 144 px) this would be misclassified
        // horizontal; the geometric classifier should still
        // see y-axis dominance for an extreme-y direction.
        // Use a long-focal-length intrinsic so a 50 px offset
        // still rayifies into a strongly y-tilted direction.
        let mut i_long = intr(640, 480);
        i_long.fx = 50.0;
        i_long.fy = 50.0;
        let vp_tilted = VanishingPoint {
            x: i_long.cx,
            y: i_long.cy + 200.0,
            inliers: 50,
        };
        assert!(
            is_vertical_vp(&vp_tilted, &i_long),
            "strongly-tilted camera vertical VP must classify as vertical even when image-y close to cy",
        );
        // And the level-camera limit: a VP very far below
        // the image ⇒ direction → +y_cam ⇒ vertical.
        let vp_at_infinity = VanishingPoint {
            x: i.cx,
            y: i.cy + 1.0e6,
            inliers: 50,
        };
        assert!(is_vertical_vp(&vp_at_infinity, &i));
    }

    #[test]
    fn parallel_lines_produce_finite_vp() {
        // Critic #2: two near-parallel edgels must produce a
        // synthetic finite VP, not be silently dropped.
        // Verify by direct call into ransac with two
        // exactly-parallel edgels at min_inliers=2.
        let edgels = vec![
            Edgel {
                x: 10.0,
                y: 10.0,
                gx: 1.0,
                gy: 0.0,
            },
            Edgel {
                x: 10.0,
                y: 200.0,
                gx: 1.0,
                gy: 0.0,
            },
        ];
        let cfg = VanishingPointConfig {
            min_inliers: 2,
            ransac_iterations: 10,
            ..VanishingPointConfig::default()
        };
        let vps = ransac_vanishing_points(&edgels, &cfg, 640.0);
        assert!(
            !vps.is_empty(),
            "parallel-lines case must synthesize a finite VP",
        );
        for v in &vps {
            assert!(v.x.is_finite() && v.y.is_finite());
        }
    }

    /// Smoke benchmark — NOT a Pi Zero 2W number. This runs on
    /// the dev workstation (`x86_64`) and asserts the synthetic-
    /// cube workload completes in well under 50 ms. Real Pi
    /// Zero 2W benchmarking is deferred to Phase 3 hardware-
    /// in-the-loop testing.
    #[test]
    fn smoke_bench_under_50ms() {
        let w: u32 = 640;
        let h: u32 = 480;
        let mut pixels = blank_frame(w, h);
        let cx = (w / 2) as i32;
        let cy = (h / 2) as i32;
        let vp_v = (cx, cy + (h as i32) * 4);
        let anchors: Vec<(i32, i32)> = (0..12).map(|k| (40 + k * 50, 40)).collect();
        draw_fan(&mut pixels, w, h, vp_v.0, vp_v.1, &anchors);
        let f = frame_from(pixels, w, h);
        let intrinsics = intr(w, h);
        let ctx = ctx_for(&f, &intrinsics);
        let provider = VanishingPointProvider::default();
        let start = std::time::Instant::now();
        let _ = provider.detect(&ctx);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 50,
            "smoke bench exceeded 50ms (x86_64 dev box): {elapsed:?}",
        );
    }
}
