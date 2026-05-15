//! Kabsch algorithm: closed-form least-squares rotation matrix
//! mapping one set of unit vectors onto another.
//!
//! Used by the camera-space stitcher
//! (`bris_vision::track::track_rotation`) to recover the
//! rotation between two camera frames from N feature-matched
//! ray pairs.
//!
//! # Why this lives here as well as in bris-platesolve
//!
//! `bris-platesolve` has its own copy of this same algorithm
//! (it uses Kabsch to recover camera attitude from N identified-
//! star pairs). The dependency graph runs platesolve → vision,
//! so vision can't reach into platesolve to share the
//! implementation. Rather than restructure the workspace into a
//! `bris-math` crate today, we maintain two copies. They should
//! be kept in lockstep when bug-fixed; long-term cleanup is to
//! extract Kabsch (and the small 3×3 SVD it depends on) into a
//! shared lower layer.

#![allow(
    // Numerical / linear-algebra code routinely uses single-letter
    // variable names (i, j, k for indices; u, v, s for SVD).
    // Mirrors the suppression on the platesolve copy.
    clippy::many_single_char_names,
    clippy::similar_names,
    // Loop indices into arrays of paired indices are clearer with
    // the numeric form than with iter().enumerate() in this code.
    clippy::needless_range_loop,
)]
//!
//! # Why this lives here as well as in bris-platesolve
//!
//! `bris-platesolve` has its own copy of this same algorithm
//! (it uses Kabsch to recover camera attitude from N identified-
//! star pairs). The dependency graph runs platesolve → vision,
//! so vision can't reach into platesolve to share the
//! implementation. Rather than restructure the workspace into a
//! `bris-math` crate today, we maintain two copies. They should
//! be kept in lockstep when bug-fixed; long-term cleanup is to
//! extract Kabsch (and the small 3×3 SVD it depends on) into a
//! shared lower layer.
//!
//! Used by the plate solver to compute camera attitude from N
//! identified-star pairs (catalog-frame unit vector ↔ camera-frame
//! ray). With N ≥ 3 non-collinear pairs the solution is unique;
//! with N = 4 (the minimum useful for plate solving) it's
//! over-determined in the right way to be robust to small per-
//! star errors.
//!
//! # Algorithm
//!
//! Given paired unit vectors `a_i` (catalog frame) and `b_i`
//! (camera frame):
//!
//! 1. Compute the cross-correlation matrix `H = sum(a_i^T · b_i)`
//!    (3×3).
//! 2. SVD: `H = U · S · V^T`.
//! 3. Compute `d = sign(det(V · U^T))` to handle the reflection
//!    case.
//! 4. Rotation: `R = V · diag(1, 1, d) · U^T`.
//!
//! `R` is the rotation that, applied to a catalog vector, gives
//! the corresponding camera-frame vector: `b ≈ R · a`.
//!
//! # Implementation
//!
//! For 3×3 matrices we use Jacobi rotation SVD: iterate, zeroing
//! off-diagonal elements one at a time. Converges in ~10
//! iterations for 3×3 matrices; total cost dominated by the
//! constant-overhead matrix multiplications. No external linear-
//! algebra dependency.

use std::fmt;

/// Errors from [`kabsch_rotation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KabschError {
    /// Fewer than 3 input pairs supplied.
    #[error("Kabsch needs ≥ 3 paired vectors, got {0}")]
    InsufficientPairs(usize),
    /// Input vectors are degenerate (e.g. all colinear, all
    /// identical). The cross-correlation matrix has rank < 2,
    /// rotation is underdetermined.
    #[error("input vectors are degenerate (rank < 2)")]
    Degenerate,
}

/// Compute the rotation matrix mapping `catalog` vectors onto
/// `camera` vectors in a least-squares sense.
///
/// Both inputs must be the same length and contain unit vectors
/// (the function does not normalize). Returns the 3×3 rotation
/// in row-major order.
///
/// # Errors
///
/// See [`KabschError`].
pub fn kabsch_rotation(catalog: &[[f64; 3]], camera: &[[f64; 3]]) -> Result<[f64; 9], KabschError> {
    if catalog.len() < 3 || camera.len() < 3 {
        return Err(KabschError::InsufficientPairs(
            catalog.len().min(camera.len()),
        ));
    }
    if catalog.len() != camera.len() {
        return Err(KabschError::InsufficientPairs(
            catalog.len().min(camera.len()),
        ));
    }

    // H = sum over i of a_i^T · b_i (outer product). 3×3 matrix.
    let mut h = [[0.0_f64; 3]; 3];
    for (a, b) in catalog.iter().zip(camera.iter()) {
        for i in 0..3 {
            for j in 0..3 {
                h[i][j] += a[i] * b[j];
            }
        }
    }

    // SVD: H = U · S · V^T.
    let svd = jacobi_svd_3x3(h);

    // Rotation: R = V · diag(1, 1, d) · U^T,
    // where d = sign(det(V · U^T)) to handle reflections.
    let vu_t = mat3_mul(svd.v, mat3_transpose(svd.u));
    let det = mat3_det(vu_t);
    let d = if det >= 0.0 { 1.0 } else { -1.0 };

    let mut diag = [[0.0_f64; 3]; 3];
    diag[0][0] = 1.0;
    diag[1][1] = 1.0;
    diag[2][2] = d;

    let r = mat3_mul(mat3_mul(svd.v, diag), mat3_transpose(svd.u));

    // Sanity check: rotation matrix has det ≈ +1 and is orthogonal.
    let r_det = mat3_det(r);
    if (r_det - 1.0).abs() > 1e-3 {
        return Err(KabschError::Degenerate);
    }

    Ok([
        r[0][0], r[0][1], r[0][2], r[1][0], r[1][1], r[1][2], r[2][0], r[2][1], r[2][2],
    ])
}

/// Apply a rotation matrix (row-major 9-element) to a 3-vector.
#[must_use]
pub fn rotate_vec(rot: &[f64; 9], v: [f64; 3]) -> [f64; 3] {
    [
        rot[0] * v[0] + rot[1] * v[1] + rot[2] * v[2],
        rot[3] * v[0] + rot[4] * v[1] + rot[5] * v[2],
        rot[6] * v[0] + rot[7] * v[1] + rot[8] * v[2],
    ]
}

// ---------------------------------------------------------------------------
// 3×3 SVD via Jacobi rotations.
//
// Iterates: at each step, find the largest off-diagonal element
// of A^T·A (i.e. of the right-singular system), apply a Givens
// rotation to V from the right that zeroes it, accumulate U on
// the left from H · V_so_far.
//
// This is the textbook two-sided Jacobi algorithm specialized for
// 3×3 matrices. Converges quadratically; ~10 iterations of an
// inner pass over the 3 unique off-diagonal positions is more
// than enough for double-precision convergence.
// ---------------------------------------------------------------------------

struct Svd3 {
    u: [[f64; 3]; 3],
    v: [[f64; 3]; 3],
    /// Singular values, sorted descending. Unused by the Kabsch
    /// path but kept for diagnostics if needed.
    #[allow(dead_code)]
    s: [f64; 3],
}

fn jacobi_svd_3x3(a: [[f64; 3]; 3]) -> Svd3 {
    // We compute V s.t. A · V has orthogonal columns. Then U is
    // those columns normalized; S is their norms.
    let mut a_v = a; // start with A · I = A
    let mut v = identity3();

    let max_sweeps = 30;
    for _sweep in 0..max_sweeps {
        let mut max_off = 0.0_f64;
        for (p, q) in [(0, 1), (0, 2), (1, 2)] {
            // Compute the 2x2 block of (a_v)^T · a_v at columns p, q.
            let mut alpha = 0.0;
            let mut beta = 0.0;
            let mut gamma = 0.0;
            for k in 0..3 {
                alpha += a_v[k][p] * a_v[k][p];
                beta += a_v[k][q] * a_v[k][q];
                gamma += a_v[k][p] * a_v[k][q];
            }
            max_off = max_off.max(gamma.abs());
            if gamma.abs() < 1e-14 * (alpha.abs() + beta.abs() + 1e-300) {
                continue;
            }
            // Givens angle that zeros the (p, q) off-diagonal:
            // tan(2θ) = 2γ / (α - β).
            let zeta = (beta - alpha) / (2.0 * gamma);
            let t = if zeta >= 0.0 {
                1.0 / (zeta + (1.0 + zeta * zeta).sqrt())
            } else {
                1.0 / (zeta - (1.0 + zeta * zeta).sqrt())
            };
            let c = 1.0 / (1.0 + t * t).sqrt();
            let s = t * c;

            // Apply the rotation to columns p, q of a_v and v.
            for row in 0..3 {
                let app = c * a_v[row][p] - s * a_v[row][q];
                let aqq = s * a_v[row][p] + c * a_v[row][q];
                a_v[row][p] = app;
                a_v[row][q] = aqq;

                let vpp = c * v[row][p] - s * v[row][q];
                let vqq = s * v[row][p] + c * v[row][q];
                v[row][p] = vpp;
                v[row][q] = vqq;
            }
        }
        if max_off < 1e-14 {
            break;
        }
    }

    // Now A · V has orthogonal columns. U = (A · V) with columns
    // normalized; S = norms of those columns.
    let mut u = [[0.0_f64; 3]; 3];
    let mut s = [0.0_f64; 3];
    for col in 0..3 {
        let mut norm_sq = 0.0;
        for row in 0..3 {
            norm_sq += a_v[row][col] * a_v[row][col];
        }
        let norm = norm_sq.sqrt();
        s[col] = norm;
        if norm > 0.0 {
            for row in 0..3 {
                u[row][col] = a_v[row][col] / norm;
            }
        } else {
            // Degenerate column (rank-deficient input). Place a
            // standard basis vector to keep U orthonormal.
            u[col][col] = 1.0;
        }
    }

    // Sort columns by descending singular value.
    let mut perm = [0_usize, 1, 2];
    perm.sort_by(|&a, &b| s[b].partial_cmp(&s[a]).unwrap_or(std::cmp::Ordering::Equal));
    let mut u_sorted = [[0.0_f64; 3]; 3];
    let mut v_sorted = [[0.0_f64; 3]; 3];
    let mut s_sorted = [0.0_f64; 3];
    for (new_col, &old_col) in perm.iter().enumerate() {
        s_sorted[new_col] = s[old_col];
        for row in 0..3 {
            u_sorted[row][new_col] = u[row][old_col];
            v_sorted[row][new_col] = v[row][old_col];
        }
    }

    Svd3 {
        u: u_sorted,
        v: v_sorted,
        s: s_sorted,
    }
}

const fn identity3() -> [[f64; 3]; 3] {
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
}

fn mat3_mul(a: [[f64; 3]; 3], b: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut out = [[0.0_f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            for k in 0..3 {
                out[i][j] += a[i][k] * b[k][j];
            }
        }
    }
    out
}

fn mat3_transpose(a: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    [
        [a[0][0], a[1][0], a[2][0]],
        [a[0][1], a[1][1], a[2][1]],
        [a[0][2], a[1][2], a[2][2]],
    ]
}

fn mat3_det(a: [[f64; 3]; 3]) -> f64 {
    a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
}

#[allow(dead_code)] // used by future error formatting paths
struct Matrix3Display<'a>(&'a [[f64; 3]; 3]);
impl fmt::Display for Matrix3Display<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for row in self.0 {
            writeln!(f, "  [{:9.4}, {:9.4}, {:9.4}]", row[0], row[1], row[2])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    /// Apply a rotation built from a known axis + angle to a set
    /// of vectors. Used to set up Kabsch round-trip tests where
    /// we know the ground-truth rotation.
    fn rotate_axis_angle(v: [f64; 3], axis: [f64; 3], angle: f64) -> [f64; 3] {
        // Rodrigues' formula.
        let (sa, ca) = (angle.sin(), angle.cos());
        let one_minus_ca = 1.0 - ca;
        let ax = axis[0];
        let ay = axis[1];
        let az = axis[2];
        let dot = ax * v[0] + ay * v[1] + az * v[2];
        [
            v[0] * ca + (ay * v[2] - az * v[1]) * sa + ax * dot * one_minus_ca,
            v[1] * ca + (az * v[0] - ax * v[2]) * sa + ay * dot * one_minus_ca,
            v[2] * ca + (ax * v[1] - ay * v[0]) * sa + az * dot * one_minus_ca,
        ]
    }

    #[test]
    fn identity_rotation_maps_to_identity() {
        let pts: Vec<[f64; 3]> = vec![
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [
                1.0 / 3.0_f64.sqrt(),
                1.0 / 3.0_f64.sqrt(),
                1.0 / 3.0_f64.sqrt(),
            ],
        ];
        let r = kabsch_rotation(&pts, &pts).unwrap();
        // R ≈ identity.
        assert_relative_eq!(r[0], 1.0, epsilon = 1e-9);
        assert_relative_eq!(r[4], 1.0, epsilon = 1e-9);
        assert_relative_eq!(r[8], 1.0, epsilon = 1e-9);
        for i in [1usize, 2, 3, 5, 6, 7] {
            assert_relative_eq!(r[i], 0.0, epsilon = 1e-9);
        }
    }

    #[test]
    fn recovers_known_rotation_about_z() {
        let axis = [0.0, 0.0, 1.0];
        let angle = 30.0_f64.to_radians();
        let catalog: Vec<[f64; 3]> = vec![
            [1.0, 0.0, 0.0],
            [0.7, 0.7, 0.1],
            [0.0, 1.0, 0.0],
            [0.3, 0.3, 0.9],
        ];
        let camera: Vec<[f64; 3]> = catalog
            .iter()
            .map(|&v| rotate_axis_angle(v, axis, angle))
            .collect();
        let r = kabsch_rotation(&catalog, &camera).unwrap();
        // Apply r to catalog[0] = [1,0,0] → should be [cos, sin, 0].
        let mapped = rotate_vec(&r, catalog[0]);
        assert_relative_eq!(mapped[0], angle.cos(), epsilon = 1e-6);
        assert_relative_eq!(mapped[1], angle.sin(), epsilon = 1e-6);
        assert_relative_eq!(mapped[2], 0.0, epsilon = 1e-6);
    }

    #[test]
    fn recovers_known_rotation_arbitrary_axis() {
        let axis = {
            let raw = [0.3_f64, 0.7, 0.5];
            let n = (raw[0].powi(2) + raw[1].powi(2) + raw[2].powi(2)).sqrt();
            [raw[0] / n, raw[1] / n, raw[2] / n]
        };
        let angle = 47.0_f64.to_radians();
        let catalog: Vec<[f64; 3]> = vec![
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [
                1.0 / 3.0_f64.sqrt(),
                1.0 / 3.0_f64.sqrt(),
                1.0 / 3.0_f64.sqrt(),
            ],
            [-0.5, 0.5, 1.0_f64 / 2.0_f64.sqrt()],
        ];
        let camera: Vec<[f64; 3]> = catalog
            .iter()
            .map(|&v| rotate_axis_angle(v, axis, angle))
            .collect();
        let r = kabsch_rotation(&catalog, &camera).unwrap();
        // Verify each catalog vector maps to its camera vector.
        for (a, b) in catalog.iter().zip(camera.iter()) {
            let mapped = rotate_vec(&r, *a);
            assert_relative_eq!(mapped[0], b[0], epsilon = 1e-6);
            assert_relative_eq!(mapped[1], b[1], epsilon = 1e-6);
            assert_relative_eq!(mapped[2], b[2], epsilon = 1e-6);
        }
    }

    #[test]
    fn rejects_too_few_pairs() {
        let pts = vec![[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let err = kabsch_rotation(&pts, &pts).unwrap_err();
        assert!(matches!(err, KabschError::InsufficientPairs(_)));
    }

    #[test]
    fn rejects_mismatched_lengths() {
        let a = vec![[1.0, 0.0, 0.0]; 3];
        let b = vec![[0.0, 1.0, 0.0]; 4];
        let err = kabsch_rotation(&a, &b).unwrap_err();
        assert!(matches!(err, KabschError::InsufficientPairs(_)));
    }

    #[test]
    fn handles_noise_in_input() {
        // Add small per-vector noise; Kabsch should still recover
        // the rotation to within the noise level.
        let axis = [0.0, 1.0, 0.0];
        let angle = 25.0_f64.to_radians();
        let catalog: Vec<[f64; 3]> = vec![
            [1.0, 0.0, 0.0],
            [0.5, 0.5, 0.7],
            [0.0, 1.0, 0.0],
            [-0.2, 0.5, 0.84],
            [0.6, -0.6, 0.5],
        ];
        let mut camera: Vec<[f64; 3]> = catalog
            .iter()
            .map(|&v| rotate_axis_angle(v, axis, angle))
            .collect();
        // Add ~0.001 rad of noise to one star.
        camera[2][0] += 0.001;
        camera[2][1] -= 0.001;
        // Renormalize.
        let n = (camera[2][0].powi(2) + camera[2][1].powi(2) + camera[2][2].powi(2)).sqrt();
        camera[2][0] /= n;
        camera[2][1] /= n;
        camera[2][2] /= n;

        let r = kabsch_rotation(&catalog, &camera).unwrap();
        // First vector should map close to the true rotated [1,0,0].
        let true_mapped = rotate_axis_angle([1.0, 0.0, 0.0], axis, angle);
        let mapped = rotate_vec(&r, [1.0, 0.0, 0.0]);
        let err = ((mapped[0] - true_mapped[0]).powi(2)
            + (mapped[1] - true_mapped[1]).powi(2)
            + (mapped[2] - true_mapped[2]).powi(2))
        .sqrt();
        // Noise was ~0.001 rad on one star; rotation error should
        // be of the same order or smaller (averaged over 5 stars).
        assert!(
            err < 0.01,
            "rotation error {err} too large for ~0.001 noise"
        );
    }
}
