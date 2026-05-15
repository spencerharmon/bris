//! Minimal feature tracker for cross-frame alignment.
//!
//! This is the building block for panorama-style stitching: given two
//! frames with overlap, find the rigid transformation (rotation +
//! translation in pixel coordinates) that maps one onto the other.
//!
//! # Why this and not full ORB?
//!
//! The full ORB pipeline (FAST + BRIEF + scale pyramid + orientation
//! assignment) is overkill for the marine sweep case:
//! - Adjacent frames differ by small rotation and small translation,
//!   not arbitrary rigid transforms.
//! - We don't need scale invariance — the camera doesn't zoom mid-sweep.
//! - We don't need rotation invariance — the camera roll between
//!   adjacent frames is small (boat motion or slow pan).
//!
//! What we *do* need:
//! 1. Detect strong corners in both frames.
//! 2. For each corner in frame A's overlap region, search a small
//!    window in frame B for the matching pixel patch.
//! 3. Fit a rigid transform (or similarity, if needed) from inlier
//!    matches.
//!
//! This is closer to Lucas-Kanade-style feature tracking than full ORB.
//! Simpler to implement, no large external descriptors, and matches the
//! actual image transformation between adjacent frames in a sweep.
//!
//! # When this fails
//!
//! - Featureless overlap (sea-only frame to sea-only frame). Fallback:
//!   the horizon-line direction (when both frames contain a horizon)
//!   provides a "down" anchor; pure rotation can be inferred from the
//!   horizon orientation alone, with translation indeterminate.
//! - Frames with too little overlap. Detected by inlier-count threshold;
//!   the alignment is reported as failed and the streaming engine
//!   should request more frames or surface "stitching failed" to the
//!   operator.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    clippy::similar_names
)]

use crate::frame::Frame;
use crate::lens::pixel_ray_direction;
use bris_core::Sigma;
use bris_math::kabsch;

/// A detected corner in a frame.
#[derive(Debug, Clone, Copy)]
pub struct Corner {
    /// X coordinate, pixels.
    pub x: u32,
    /// Y coordinate, pixels.
    pub y: u32,
    /// Detection strength (Harris response or equivalent).
    pub strength: f64,
}

/// A rigid 2D transformation from frame A to frame B in pixel coords.
///
/// `(x_b, y_b) = R(theta) · (x_a − cx_a, y_a − cy_a) + (tx, ty) + (cx_b, cy_b)`
///
/// Stored about each frame's center so that transforms compose
/// straightforwardly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RigidTransform {
    /// Rotation angle (radians, frame B relative to frame A).
    pub theta_rad: f64,
    /// Translation x (pixels).
    pub tx_px: f64,
    /// Translation y (pixels).
    pub ty_px: f64,
    /// Number of inlier matches.
    pub inlier_count: u32,
    /// Total candidate matches considered.
    pub candidate_count: u32,
    /// Per-inlier RMS residual, pixels.
    pub residual_rms_px: f64,
    /// 1σ uncertainty in the rotation angle, radians, derived from
    /// the residual RMS.
    pub theta_sigma: Sigma,
}

/// Camera-space rotation between two frames captured by the
/// same camera at (slightly) different orientations.
///
/// The output of [`track_rotation`]: a rotation matrix mapping
/// frame A's camera-coordinate rays onto frame B's. With this
/// in hand, a body detected in frame B at pixel `(x_b, y_b)`
/// can be projected through frame B's intrinsics to a unit
/// ray, then *rotated by the inverse of this matrix* to land
/// in frame A's camera frame, where it can be measured against
/// frame A's horizon plane.
///
/// Resolution-free at the boundary: the rotation depends only
/// on the camera's orientation in space between the two
/// captures. The pixel grids of frame A and frame B may
/// differ — that's the whole point of the per-stage-resolution
/// architecture (Phase 2 step 4: `bris-vision`'s panorama
/// stitcher composes a low-resolution horizon detection with a
/// high-resolution body centroid by lifting both into camera-
/// space rays through their respective intrinsics, then
/// composing through this rotation).
///
/// The rotation is computed by Kabsch on `n ≥ 3` matched ray
/// pairs `(ray_a, ray_b)`. The RMS residual is reported in
/// radians (great-circle distance between the rotation-mapped
/// `ray_a` and the observed `ray_b`, averaged over inliers).
#[derive(Debug, Clone, Copy)]
pub struct RotationBetweenFrames {
    /// Rotation matrix R such that `ray_b ≈ R · ray_a`.
    /// 3×3 in row-major order.
    pub matrix: [f64; 9],
    /// Number of feature-matched ray pairs that contributed to
    /// the Kabsch fit.
    pub inlier_count: u32,
    /// Number of feature matches considered (some may have
    /// been rejected as RANSAC outliers when the ransac path
    /// is added; today every match feeds Kabsch directly).
    pub candidate_count: u32,
    /// RMS great-circle residual `acos(dot(R · ray_a, ray_b))`
    /// over the inlier ray pairs, radians.
    pub rms_residual_rad: f64,
    /// 1σ rotation uncertainty in radians, surfaced for σ
    /// composition in downstream sight-reduction. Today we
    /// report `rms_residual_rad` directly; a more rigorous
    /// derivation that accounts for ray-density and
    /// distribution is a future refinement.
    pub rotation_sigma: Sigma,
}

/// Errors from feature tracking.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum TrackError {
    /// Too few corners detected in the source frame.
    #[error("only {0} corners detected (need ≥ {1})")]
    InsufficientCorners(u32, u32),
    /// Too few corner matches survived the search.
    #[error("only {0} matches found (need ≥ {1})")]
    InsufficientMatches(u32, u32),
    /// RANSAC produced too few inliers to trust the transform.
    #[error("only {0} inliers from {1} matches (need ≥ {2})")]
    LowConfidence(u32, u32, u32),
}

/// Configuration for feature tracking.
#[derive(Debug, Clone, Copy)]
pub struct TrackConfig {
    /// Number of strongest corners to retain in each frame. Default 200.
    pub max_corners: u32,
    /// Minimum corner-strength Harris-response threshold. Default
    /// 1e10 — calibrated for u16 pixel data with the 3×3 box-windowed
    /// structure tensor; rescale for other bit depths.
    pub min_corner_strength: f64,
    /// Half-size of the patch used for matching (full patch is `2k+1`).
    /// Default 5 (11×11 patch).
    pub patch_half_size: u32,
    /// Maximum search radius (pixels) when matching frame A's corners
    /// in frame B. Default 30. Should be larger than the largest
    /// expected inter-frame displacement.
    pub search_radius_px: u32,
    /// RANSAC iterations for transform fitting. Default 200.
    pub ransac_iterations: u32,
    /// RANSAC inlier threshold (pixels). Default 2.0.
    pub ransac_inlier_px: f64,
    /// Minimum inlier count to accept a transform. Default 8.
    pub min_inliers: u32,
}

impl Default for TrackConfig {
    fn default() -> Self {
        Self {
            max_corners: 200,
            min_corner_strength: 1e10,
            patch_half_size: 5,
            search_radius_px: 30,
            ransac_iterations: 200,
            ransac_inlier_px: 2.0,
            min_inliers: 8,
        }
    }
}

/// Detect corners using the Harris corner response on a grayscale frame.
///
/// Implementation: compute Sobel gradients Ix, Iy; build the structure
/// tensor M = [[Ix², `IxIy`], [`IxIy`, Iy²]] summed over a 3×3 box window;
/// the Harris response `R = det(M) − k·trace(M)²` (k = 0.04) is large
/// at corner-like patches. Returns the strongest `max_corners` above
/// `min_corner_strength`.
///
/// The 3×3 box window on the structure tensor is what distinguishes
/// "corner" from "edge": at an edge, even a strong gradient produces
/// a near-zero Harris response because the off-diagonal `Ixx · Iyy −
/// Ixy²` cancels; at a corner the windowed sum captures contributions
/// from gradients in two different directions.
#[must_use]
pub fn detect_corners(frame: &Frame, cfg: TrackConfig) -> Vec<Corner> {
    let w = frame.width() as usize;
    let h = frame.height() as usize;
    if w < 5 || h < 5 {
        return Vec::new();
    }
    let pixels = frame.pixels();

    // First pass: per-pixel Ix, Iy.
    let mut ix_buf = vec![0.0_f64; w * h];
    let mut iy_buf = vec![0.0_f64; w * h];
    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let p = |dx: isize, dy: isize| -> f64 {
                let xi = (x as isize + dx) as usize;
                let yi = (y as isize + dy) as usize;
                f64::from(pixels[yi * w + xi])
            };
            let ix = (-p(-1, -1)
                + 1.0 * p(1, -1)
                + -2.0 * p(-1, 0)
                + 2.0 * p(1, 0)
                + -p(-1, 1)
                + 1.0 * p(1, 1))
                / 8.0;
            let iy = (-p(-1, -1)
                + -2.0 * p(0, -1)
                + -p(1, -1)
                + 1.0 * p(-1, 1)
                + 2.0 * p(0, 1)
                + 1.0 * p(1, 1))
                / 8.0;
            ix_buf[y * w + x] = ix;
            iy_buf[y * w + x] = iy;
        }
    }

    // Second pass: structure tensor with 3×3 box-window summation,
    // then Harris response.
    let mut response = vec![0.0_f64; w * h];
    let k = 0.04;
    for y in 2..(h - 2) {
        for x in 2..(w - 2) {
            let mut sum_xx = 0.0;
            let mut sum_yy = 0.0;
            let mut sum_xy = 0.0;
            for dy in -1_isize..=1 {
                for dx in -1_isize..=1 {
                    let xi = (x as isize + dx) as usize;
                    let yi = (y as isize + dy) as usize;
                    let ix = ix_buf[yi * w + xi];
                    let iy = iy_buf[yi * w + xi];
                    sum_xx += ix * ix;
                    sum_yy += iy * iy;
                    sum_xy += ix * iy;
                }
            }
            let det = sum_xx * sum_yy - sum_xy * sum_xy;
            let trace = sum_xx + sum_yy;
            response[y * w + x] = det - k * trace * trace;
        }
    }

    // Non-maximum suppression in a 3×3 window + threshold + top-N.
    let mut corners: Vec<Corner> = Vec::new();
    for y in 2..(h - 2) {
        for x in 2..(w - 2) {
            let r = response[y * w + x];
            if r < cfg.min_corner_strength {
                continue;
            }
            let mut is_max = true;
            'nms: for dy in -1_isize..=1 {
                for dx in -1_isize..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let nx = (x as isize + dx) as usize;
                    let ny = (y as isize + dy) as usize;
                    if response[ny * w + nx] >= r {
                        is_max = false;
                        break 'nms;
                    }
                }
            }
            if is_max {
                corners.push(Corner {
                    x: x as u32,
                    y: y as u32,
                    strength: r,
                });
            }
        }
    }

    corners.sort_by(|a, b| {
        b.strength
            .partial_cmp(&a.strength)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    corners.truncate(cfg.max_corners as usize);
    corners
}

/// Track frame A's corners in frame B.
///
/// For each corner in A, compare the surrounding patch to candidate
/// patches in B within `search_radius_px`. The best-NCC (normalized
/// cross-correlation) candidate is the match. Then RANSAC-fit a rigid
/// transform from the (`corner_a`, `match_b`) pairs.
///
/// # Errors
///
/// Returns `Err` if too few corners or too few matches survive, or
/// if the RANSAC fit has too few inliers.
pub fn track(
    frame_a: &Frame,
    frame_b: &Frame,
    cfg: TrackConfig,
) -> Result<RigidTransform, TrackError> {
    let corners_a = detect_corners(frame_a, cfg);
    if corners_a.len() < cfg.min_inliers as usize {
        return Err(TrackError::InsufficientCorners(
            corners_a.len() as u32,
            cfg.min_inliers,
        ));
    }
    let anchors: Vec<(u32, u32)> = corners_a.iter().map(|c| (c.x, c.y)).collect();
    track_with_anchors(frame_a, frame_b, &anchors, cfg)
}

/// Track a frame's star-like peaks in another frame.
///
/// Same NCC-matching + RANSAC-rigid pipeline as [`track`], but driven
/// by [`crate::peak::detect_peaks`] instead of Harris corners. Use
/// this for night frames where stars are the primary features.
///
/// # Errors
///
/// As [`track`].
pub fn track_peaks(
    frame_a: &Frame,
    frame_b: &Frame,
    peak_cfg: crate::peak::PeakConfig,
    cfg: TrackConfig,
) -> Result<RigidTransform, TrackError> {
    let peaks_a = crate::peak::detect_peaks(frame_a, peak_cfg);
    if peaks_a.len() < cfg.min_inliers as usize {
        return Err(TrackError::InsufficientCorners(
            peaks_a.len() as u32,
            cfg.min_inliers,
        ));
    }
    let anchors: Vec<(u32, u32)> = peaks_a
        .iter()
        .map(|p| (p.x.round() as u32, p.y.round() as u32))
        .collect();
    track_with_anchors(frame_a, frame_b, &anchors, cfg)
}

/// Shared implementation: given anchor pixels in A, find their NCC
/// matches in B and RANSAC a rigid transform.
fn track_with_anchors(
    frame_a: &Frame,
    frame_b: &Frame,
    anchors: &[(u32, u32)],
    cfg: TrackConfig,
) -> Result<RigidTransform, TrackError> {
    // Match each anchor in A to its best NCC candidate in B.
    let mut matches: Vec<MatchPair> = Vec::new();
    for &(ax, ay) in anchors {
        let dummy = Corner {
            x: ax,
            y: ay,
            strength: 0.0,
        };
        if let Some((bx, by, _score)) = find_best_match(frame_a, frame_b, &dummy, &cfg) {
            matches.push(((f64::from(ax), f64::from(ay)), (bx, by)));
        }
    }
    if (matches.len() as u32) < cfg.min_inliers {
        return Err(TrackError::InsufficientMatches(
            matches.len() as u32,
            cfg.min_inliers,
        ));
    }

    let candidate_count = matches.len() as u32;
    let cx_a = f64::from(frame_a.width()) / 2.0;
    let cy_a = f64::from(frame_a.height()) / 2.0;
    let cx_b = f64::from(frame_b.width()) / 2.0;
    let cy_b = f64::from(frame_b.height()) / 2.0;

    let fit = ransac_rigid(
        &matches,
        cx_a,
        cy_a,
        cx_b,
        cy_b,
        cfg.ransac_iterations,
        cfg.ransac_inlier_px,
    );

    if fit.inlier_count < cfg.min_inliers {
        return Err(TrackError::LowConfidence(
            fit.inlier_count,
            candidate_count,
            cfg.min_inliers,
        ));
    }

    let r_typical = ((frame_a.width().pow(2) + frame_a.height().pow(2)) as f64).sqrt() / 4.0;
    let theta_sigma_rad = fit.residual_rms_px / r_typical.max(1.0);
    let theta_sigma = bris_core::Sigma::new(theta_sigma_rad).unwrap_or(bris_core::Sigma::ZERO);

    Ok(RigidTransform {
        theta_rad: fit.theta_rad,
        tx_px: fit.tx_px,
        ty_px: fit.ty_px,
        inlier_count: fit.inlier_count,
        candidate_count,
        residual_rms_px: fit.residual_rms_px,
        theta_sigma,
    })
}

/// Compute the **camera-space rotation** between two frames
/// using the same Harris+NCC feature-matching pipeline as
/// [`track`] but composing the matched pixel pairs into ray
/// pairs and Kabsch-fitting a rotation.
///
/// This is the camera-space sibling of [`track`]. Use it
/// when:
///
/// * the two frames are at different resolutions (the ray
///   conversion handles each frame's own intrinsics);
/// * downstream code wants a resolution-free composition
///   (rotation matrix in camera space) rather than a pixel-
///   space transform tied to one resolution;
/// * cross-frame body / horizon composition needs to bridge
///   measurements from differently-downsampled stages.
///
/// The rotation maps `ray_a → ray_b`: applying the result to
/// a frame-A camera-space ray gives the corresponding frame-B
/// camera-space ray. To go the other direction (project a
/// frame-B detection back into frame A's coordinate system),
/// transpose the matrix.
///
/// # Errors
///
/// Same as [`track`]: insufficient corners, insufficient
/// matches, or a degenerate ray distribution that makes
/// Kabsch refuse.
pub fn track_rotation(
    frame_a: &Frame,
    frame_b: &Frame,
    cfg: TrackConfig,
) -> Result<RotationBetweenFrames, TrackError> {
    let corners_a = detect_corners(frame_a, cfg);
    if corners_a.len() < cfg.min_inliers as usize {
        return Err(TrackError::InsufficientCorners(
            corners_a.len() as u32,
            cfg.min_inliers,
        ));
    }

    // Match each anchor in A to its best NCC candidate in B.
    let mut matched: Vec<((f64, f64), (f64, f64))> = Vec::new();
    for c in &corners_a {
        let dummy = Corner {
            x: c.x,
            y: c.y,
            strength: 0.0,
        };
        if let Some((bx, by, _score)) = find_best_match(frame_a, frame_b, &dummy, &cfg) {
            matched.push(((f64::from(c.x), f64::from(c.y)), (bx, by)));
        }
    }
    let candidate_count = matched.len() as u32;
    if candidate_count < cfg.min_inliers {
        return Err(TrackError::InsufficientMatches(
            candidate_count,
            cfg.min_inliers,
        ));
    }

    // Lift each matched pair to a unit-ray pair through each
    // frame's own intrinsics.
    let intr_a = frame_a.intrinsics;
    let intr_b = frame_b.intrinsics;
    let mut rays_a: Vec<[f64; 3]> = Vec::with_capacity(matched.len());
    let mut rays_b: Vec<[f64; 3]> = Vec::with_capacity(matched.len());
    for &((ax, ay), (bx, by)) in &matched {
        let (rx, ry, rz) = pixel_ray_direction(intr_a, ax, ay);
        rays_a.push([rx, ry, rz]);
        let (rx, ry, rz) = pixel_ray_direction(intr_b, bx, by);
        rays_b.push([rx, ry, rz]);
    }

    let matrix = kabsch::kabsch_rotation(&rays_a, &rays_b).map_err(|_| {
        // Kabsch refused; treat as low-confidence so the
        // caller's error path is consistent with the pixel-
        // rigid track().
        TrackError::LowConfidence(0, candidate_count, cfg.min_inliers)
    })?;

    // Per-pair angular residual: angle between (R · ray_a) and
    // ray_b. Sum of squares averaged → RMS.
    let mut sq_sum = 0.0_f64;
    for (a, b) in rays_a.iter().zip(rays_b.iter()) {
        let mapped = kabsch::rotate_vec(&matrix, *a);
        let dot = (mapped[0] * b[0] + mapped[1] * b[1] + mapped[2] * b[2]).clamp(-1.0, 1.0);
        let theta = dot.acos();
        sq_sum += theta * theta;
    }
    let rms_residual_rad = (sq_sum / matched.len() as f64).sqrt();

    let rotation_sigma = Sigma::new(rms_residual_rad).unwrap_or(Sigma::ZERO);

    Ok(RotationBetweenFrames {
        matrix,
        inlier_count: candidate_count,
        candidate_count,
        rms_residual_rad,
        rotation_sigma,
    })
}

fn find_best_match(
    frame_a: &Frame,
    frame_b: &Frame,
    corner: &Corner,
    cfg: &TrackConfig,
) -> Option<(f64, f64, f64)> {
    let k = cfg.patch_half_size as i32;
    let patch_a = extract_patch(frame_a, corner.x as i32, corner.y as i32, k)?;
    let r = cfg.search_radius_px as i32;
    let cx = corner.x as i32;
    let cy = corner.y as i32;
    let mut best: Option<(f64, f64, f64)> = None;
    for dy in -r..=r {
        for dx in -r..=r {
            let bx = cx + dx;
            let by = cy + dy;
            let Some(patch_b) = extract_patch(frame_b, bx, by, k) else {
                continue;
            };
            let score = ncc(&patch_a, &patch_b);
            if best.is_none_or(|(_, _, s)| score > s) {
                best = Some((bx as f64, by as f64, score));
            }
        }
    }
    // Reject low-quality matches.
    best.filter(|&(_, _, s)| s > 0.7)
}

fn extract_patch(frame: &Frame, cx: i32, cy: i32, k: i32) -> Option<Vec<f64>> {
    let w = frame.width() as i32;
    let h = frame.height() as i32;
    if cx - k < 0 || cy - k < 0 || cx + k >= w || cy + k >= h {
        return None;
    }
    let size = (2 * k + 1) as usize;
    let mut patch = Vec::with_capacity(size * size);
    for y in (cy - k)..=(cy + k) {
        for x in (cx - k)..=(cx + k) {
            let idx = (y as usize) * (frame.width() as usize) + (x as usize);
            patch.push(f64::from(frame.pixels()[idx]));
        }
    }
    Some(patch)
}

/// Normalized cross-correlation between two equal-length patches.
fn ncc(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let mean_a = a.iter().sum::<f64>() / n;
    let mean_b = b.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den_a = 0.0;
    let mut den_b = 0.0;
    for (&va, &vb) in a.iter().zip(b.iter()) {
        let da = va - mean_a;
        let db = vb - mean_b;
        num += da * db;
        den_a += da * da;
        den_b += db * db;
    }
    let denom = (den_a * den_b).sqrt();
    if denom < 1e-12 {
        return 0.0;
    }
    num / denom
}

struct RigidFit {
    theta_rad: f64,
    tx_px: f64,
    ty_px: f64,
    inlier_count: u32,
    residual_rms_px: f64,
}

/// One pair (frame-A pixel, frame-B pixel) for transform fitting.
type MatchPair = ((f64, f64), (f64, f64));

#[allow(clippy::too_many_lines)] // Procrustes + RANSAC; splitting hurts clarity.
fn ransac_rigid(
    matches: &[MatchPair],
    cx_a: f64,
    cy_a: f64,
    cx_b: f64,
    cy_b: f64,
    iterations: u32,
    inlier_px: f64,
) -> RigidFit {
    let n = matches.len();
    if n < 2 {
        return RigidFit {
            theta_rad: 0.0,
            tx_px: 0.0,
            ty_px: 0.0,
            inlier_count: 0,
            residual_rms_px: f64::INFINITY,
        };
    }

    // Seed PRNG from data for determinism.
    let mut seed: u64 = 0x5A5A_A5A5_A5A5_5A5A;
    for &((ax, ay), (bx, by)) in matches {
        seed ^= ax.to_bits().wrapping_mul(0x9E37_79B9_7F4A_7C15);
        seed = seed.rotate_left(7);
        seed ^= ay.to_bits().wrapping_mul(0xBF58_476D_1CE4_E5B9);
        seed ^= bx.to_bits().wrapping_mul(0x94D0_49BB_1331_11EB);
        seed = seed.rotate_left(11);
        seed ^= by.to_bits().wrapping_mul(0x2545_F491_4F6C_DD1D);
    }

    let mut best_inliers: Vec<usize> = Vec::new();

    for _ in 0..iterations {
        let i = next_idx(&mut seed, n);
        let mut j = next_idx(&mut seed, n);
        if j == i {
            j = (j + 1) % n;
        }
        let ((ax1, ay1), (bx1, by1)) = matches[i];
        let ((ax2, ay2), (bx2, by2)) = matches[j];

        // Centered coordinates.
        let ax1c = ax1 - cx_a;
        let ay1c = ay1 - cy_a;
        let ax2c = ax2 - cx_a;
        let ay2c = ay2 - cy_a;
        let bx1c = bx1 - cx_b;
        let by1c = by1 - cy_b;
        let bx2c = bx2 - cx_b;
        let by2c = by2 - cy_b;

        // Compute rotation that maps (a_centered) → (b_centered − t).
        // For two points, average of two angle estimates.
        let dax = ax2c - ax1c;
        let day = ay2c - ay1c;
        let dbx = bx2c - bx1c;
        let dby = by2c - by1c;
        let len_a = (dax * dax + day * day).sqrt();
        let len_b = (dbx * dbx + dby * dby).sqrt();
        if len_a < 1.0 || len_b < 1.0 {
            continue;
        }
        let theta = dby.atan2(dbx) - day.atan2(dax);

        // Translation: maps a_centered (rotated) to b_centered.
        let (sin_t, cos_t) = theta.sin_cos();
        let tx = bx1c - (cos_t * ax1c - sin_t * ay1c);
        let ty = by1c - (sin_t * ax1c + cos_t * ay1c);

        // Count inliers.
        let mut inliers = Vec::new();
        for (idx, &((ax, ay), (bx, by))) in matches.iter().enumerate() {
            let axc = ax - cx_a;
            let ayc = ay - cy_a;
            let predicted_bx = cos_t * axc - sin_t * ayc + tx + cx_b;
            let predicted_by = sin_t * axc + cos_t * ayc + ty + cy_b;
            let r = ((predicted_bx - bx).powi(2) + (predicted_by - by).powi(2)).sqrt();
            if r <= inlier_px {
                inliers.push(idx);
            }
        }
        if inliers.len() > best_inliers.len() {
            best_inliers = inliers;
        }
    }

    if best_inliers.is_empty() {
        return RigidFit {
            theta_rad: 0.0,
            tx_px: 0.0,
            ty_px: 0.0,
            inlier_count: 0,
            residual_rms_px: f64::INFINITY,
        };
    }

    // Refit by minimizing sum of squared residuals over inliers using
    // the closed-form Procrustes solution for 2D rigid alignment.
    let inlier_pts: Vec<MatchPair> = best_inliers.iter().map(|&i| matches[i]).collect();
    let n_inl = inlier_pts.len() as f64;
    let mean_a_x = inlier_pts.iter().map(|(a, _)| a.0).sum::<f64>() / n_inl;
    let mean_a_y = inlier_pts.iter().map(|(a, _)| a.1).sum::<f64>() / n_inl;
    let mean_b_x = inlier_pts.iter().map(|(_, b)| b.0).sum::<f64>() / n_inl;
    let mean_b_y = inlier_pts.iter().map(|(_, b)| b.1).sum::<f64>() / n_inl;
    let mut sum_xx = 0.0;
    let mut sum_xy = 0.0;
    for &((ax, ay), (bx, by)) in &inlier_pts {
        let dax = ax - mean_a_x;
        let day = ay - mean_a_y;
        let dbx = bx - mean_b_x;
        let dby = by - mean_b_y;
        sum_xx += dax * dbx + day * dby;
        sum_xy += dax * dby - day * dbx;
    }
    let theta_rad = sum_xy.atan2(sum_xx);
    let (sin_t, cos_t) = theta_rad.sin_cos();
    // tx, ty in centered (frame-B-center) coordinates relative to centered A.
    let tx_px = (mean_b_x - cx_b) - (cos_t * (mean_a_x - cx_a) - sin_t * (mean_a_y - cy_a));
    let ty_px = (mean_b_y - cy_b) - (sin_t * (mean_a_x - cx_a) + cos_t * (mean_a_y - cy_a));

    let mut sum_sq = 0.0;
    for &((ax, ay), (bx, by)) in &inlier_pts {
        let axc = ax - cx_a;
        let ayc = ay - cy_a;
        let predicted_bx = cos_t * axc - sin_t * ayc + tx_px + cx_b;
        let predicted_by = sin_t * axc + cos_t * ayc + ty_px + cy_b;
        sum_sq += (predicted_bx - bx).powi(2) + (predicted_by - by).powi(2);
    }
    let residual_rms_px = (sum_sq / n_inl).sqrt();

    RigidFit {
        theta_rad,
        tx_px,
        ty_px,
        inlier_count: inlier_pts.len() as u32,
        residual_rms_px,
    }
}

fn next_idx(seed: &mut u64, modulus: usize) -> usize {
    let mut x = *seed;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *seed = x;
    let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    (r as usize) % modulus
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Intrinsics;
    use approx::assert_relative_eq;
    use bris_core::time::{Tt, JD_J2000};

    /// Build a frame with an irregular bright pattern that produces
    /// strong corners.
    fn synth_corners_frame(width: u32, height: u32, offset_x: i32, offset_y: i32) -> Frame {
        let mut pixels = vec![5_000u16; (width as usize) * (height as usize)];
        // Place several distinct bright square markers — squares produce
        // strong Harris corners at their four corners.
        let centers = [
            (50, 40),
            (120, 80),
            (200, 60),
            (80, 150),
            (180, 130),
            (40, 110),
            (150, 170),
            (220, 100),
            (260, 50),
            (260, 200),
        ];
        for (cx, cy) in centers {
            let x_world = cx - offset_x;
            let y_world = cy - offset_y;
            for dy in -3_i32..=3 {
                for dx in -3_i32..=3 {
                    let xx = x_world + dx;
                    let yy = y_world + dy;
                    if xx < 0 || yy < 0 || xx >= width as i32 || yy >= height as i32 {
                        continue;
                    }
                    pixels[(yy as usize) * (width as usize) + (xx as usize)] = 60_000;
                }
            }
        }
        Frame::new(
            width,
            height,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    #[test]
    fn detects_corners_in_synthetic_frame() {
        let frame = synth_corners_frame(320, 240, 0, 0);
        let corners = detect_corners(&frame, TrackConfig::default());
        assert!(
            corners.len() >= 8,
            "expected many corners, got {}",
            corners.len()
        );
    }

    #[test]
    fn detects_pure_translation() {
        // Frame B is frame A shifted (content moves) by (10, 5)
        // pixels — i.e. content world-coordinate (50, 40) appears at
        // pixel (40, 35) in B because the camera translated by
        // (10, 5). The A → B transform that maps A's pixel to where
        // it ends up in B is therefore (tx, ty) = (-10, -5).
        let a = synth_corners_frame(320, 240, 0, 0);
        let b = synth_corners_frame(320, 240, 10, 5);
        let xform = track(&a, &b, TrackConfig::default()).unwrap();
        assert!(
            xform.theta_rad.abs() < 0.02,
            "expected ~0 rotation, got {} rad",
            xform.theta_rad
        );
        assert_relative_eq!(xform.tx_px, -10.0, epsilon = 1.0);
        assert_relative_eq!(xform.ty_px, -5.0, epsilon = 1.0);
    }

    #[test]
    fn rejects_featureless_frames() {
        let pixels = vec![10_000u16; 100 * 100];
        let a = Frame::new(
            100,
            100,
            pixels.clone(),
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(100, 100),
        )
        .unwrap();
        let b = Frame::new(
            100,
            100,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(100, 100),
        )
        .unwrap();
        let result = track(&a, &b, TrackConfig::default());
        assert!(matches!(
            result,
            Err(TrackError::InsufficientCorners(_, _) | TrackError::InsufficientMatches(_, _))
        ));
    }

    #[test]
    fn theta_sigma_finite_and_positive() {
        let a = synth_corners_frame(320, 240, 0, 0);
        let b = synth_corners_frame(320, 240, 10, 5);
        let xform = track(&a, &b, TrackConfig::default()).unwrap();
        assert!(xform.theta_sigma.value().is_finite());
        assert!(xform.theta_sigma.value() >= 0.0);
        // For noiseless synthetic data the sigma should be small.
        assert!(xform.theta_sigma.value().to_degrees() < 1.0);
    }

    /// Build a star-field frame (Gaussian blobs over a dark sky)
    /// shifted by the given pixel offset for cross-frame testing.
    fn synth_starfield_frame(width: u32, height: u32, offset_x: i32, offset_y: i32) -> Frame {
        let mut pixels = vec![100u16; (width as usize) * (height as usize)];
        let centers = [
            (60, 50),
            (130, 70),
            (200, 60),
            (250, 110),
            (90, 130),
            (160, 150),
            (220, 180),
            (40, 90),
            (290, 50),
            (180, 30),
        ];
        let sigma = 1.5_f64;
        let half = 4_i32;
        for (cx, cy) in centers {
            let cx_world = cx as f64 - offset_x as f64;
            let cy_world = cy as f64 - offset_y as f64;
            for dy in -half..=half {
                for dx in -half..=half {
                    let x = (cx_world + dx as f64).round() as i32;
                    let y = (cy_world + dy as f64).round() as i32;
                    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                        continue;
                    }
                    let r2 = (cx_world - x as f64).powi(2) + (cy_world - y as f64).powi(2);
                    let g = (-r2 / (2.0 * sigma * sigma)).exp();
                    let v = (30_000.0 * g) as u16;
                    let idx = (y as usize) * (width as usize) + (x as usize);
                    pixels[idx] = pixels[idx].saturating_add(v);
                }
            }
        }
        Frame::new(
            width,
            height,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    #[test]
    fn track_peaks_detects_pure_translation_on_starfield() {
        // Star-field equivalent of detects_pure_translation. Verifies
        // the track_peaks entry point routes peaks through the same
        // RANSAC machinery and recovers the known (-10, -5) shift.
        let a = synth_starfield_frame(320, 240, 0, 0);
        let b = synth_starfield_frame(320, 240, 10, 5);
        let xform = track_peaks(
            &a,
            &b,
            crate::peak::PeakConfig::default(),
            TrackConfig::default(),
        )
        .unwrap();
        assert!(
            xform.theta_rad.abs() < 0.02,
            "expected ~0 rotation, got {} rad",
            xform.theta_rad
        );
        assert_relative_eq!(xform.tx_px, -10.0, epsilon = 1.0);
        assert_relative_eq!(xform.ty_px, -5.0, epsilon = 1.0);
    }

    #[test]
    fn track_rotation_recovers_pure_translation_as_small_rotation() {
        // A pure pixel-translation between two frames is, in
        // camera space, a rotation about an axis perpendicular
        // to the translation direction with angle ≈ |t| / f.
        // For a 10-px x-shift on a 320×240 frame with placeholder
        // f = 1000 px, the expected rotation is ~0.01 rad about
        // the y axis.
        let a = synth_corners_frame(320, 240, 0, 0);
        let b = synth_corners_frame(320, 240, 10, 0);
        let cfg = TrackConfig {
            min_inliers: 6,
            ..TrackConfig::default()
        };
        let rot = track_rotation(&a, &b, cfg).unwrap();
        // The rotation should be small.
        // Apply R to the optical-axis ray [0, 0, 1]; it should
        // tilt mostly along the x axis (because the translation
        // shifted features in x).
        let mapped = kabsch::rotate_vec(&rot.matrix, [0.0, 0.0, 1.0]);
        assert!(
            mapped[2] > 0.99,
            "rotation should be small; got mapped z = {}",
            mapped[2],
        );
        // Residual should be small for a synthetic translation.
        // NCC matching quantizes to integer pixel coordinates,
        // contributing ~1 px ÷ f ≈ 0.001 rad per pair plus
        // accumulation across the corner set, so the empirical
        // ceiling sits around ~0.02 rad on this fixture.
        assert!(
            rot.rms_residual_rad < 0.05,
            "residual {} too large for synthetic translation",
            rot.rms_residual_rad,
        );
        assert!(rot.inlier_count >= 6);
    }

    #[test]
    fn track_rotation_rejects_too_few_corners() {
        // Empty frame → no corners → InsufficientCorners.
        let pixels = vec![5000_u16; 200 * 200];
        let blank = Frame::new(
            200,
            200,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(200, 200),
        )
        .unwrap();
        let cfg = TrackConfig {
            min_inliers: 6,
            ..TrackConfig::default()
        };
        let err = track_rotation(&blank, &blank, cfg).unwrap_err();
        assert!(matches!(err, TrackError::InsufficientCorners(_, _)));
    }
}
