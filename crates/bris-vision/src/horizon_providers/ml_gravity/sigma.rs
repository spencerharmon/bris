//! σ propagation from per-axis (roll, pitch) σ through
//! gravity → altitude. See `docs/design/ml_gravity.md` §"σ
//! propagation through the lens model".

#![cfg(feature = "ml-gravity")]
#![allow(clippy::similar_names, clippy::doc_markdown)]

use crate::ray::CameraRay;

/// Convert per-axis (σ_roll, σ_pitch) into per-component
/// (σ_gx, σ_gy, σ_gz) via the Jacobian of (φ, θ) → g.
///
/// Per the design doc:
/// ```text
///   g_x =  sin φ cos θ      g_y =  cos φ cos θ      g_z = -sin θ
/// ```
/// Independent (σ_φ, σ_θ) → per-component σ via linear
/// propagation (squared-σ sum of partial-derivative
/// contributions).
#[must_use]
pub fn gravity_axis_sigmas(
    roll: f64,
    pitch: f64,
    sigma_roll: f64,
    sigma_pitch: f64,
) -> (f64, f64, f64) {
    let (sr, cr) = roll.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    // ∂g_x/∂φ = cos φ cos θ ;  ∂g_x/∂θ = -sin φ sin θ
    // ∂g_y/∂φ = -sin φ cos θ;  ∂g_y/∂θ = -cos φ sin θ
    // ∂g_z/∂φ = 0           ;  ∂g_z/∂θ = -cos θ
    let var_gx = (cr * cp * sigma_roll).powi(2) + (sr * sp * sigma_pitch).powi(2);
    let var_gy = (sr * cp * sigma_roll).powi(2) + (cr * sp * sigma_pitch).powi(2);
    let var_gz = (cp * sigma_pitch).powi(2);
    (var_gx.sqrt(), var_gy.sqrt(), var_gz.sqrt())
}

/// Altitude σ contribution at a representative ray.
///
/// α = asin(r · -g) = asin(-(r·g))
/// dα/dg_i = -r_i / sqrt(1 - (r·g)²)
/// σ_α² = (r_x²·σ_gx² + r_y²·σ_gy² + r_z²·σ_gz²) / (1 - (r·g)²)
///
/// Near-zenith clamp: `cos(α) ≥ 0.05` (≈3° from zenith).
#[must_use]
pub fn altitude_sigma_at_ray(
    r: &CameraRay,
    g: &CameraRay,
    sigma_gx: f64,
    sigma_gy: f64,
    sigma_gz: f64,
) -> f64 {
    let rdotg = r.x * g.x + r.y * g.y + r.z * g.z;
    let cos2 = (1.0 - rdotg * rdotg).max(0.05 * 0.05);
    let num = (r.x * sigma_gx).powi(2) + (r.y * sigma_gy).powi(2) + (r.z * sigma_gz).powi(2);
    (num / cos2).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jacobian_at_zero_orientation() {
        // φ=0, θ=0: g = (0, 1, 0). Jacobian elements:
        //   ∂g_x/∂φ = 1, ∂g_y/∂φ = 0, ∂g_z/∂θ = -1
        let (sx, sy, sz) = gravity_axis_sigmas(0.0, 0.0, 0.1, 0.05);
        assert!((sx - 0.1).abs() < 1e-12);
        assert!(sy.abs() < 1e-12);
        assert!((sz - 0.05).abs() < 1e-12);
    }

    #[test]
    fn altitude_sigma_on_axis() {
        // r along +z, g along +y. r·g = 0; cos²=1; σ_α² = σ_gz².
        let r = CameraRay {
            x: 0.0,
            y: 0.0,
            z: 1.0,
        };
        let g = CameraRay {
            x: 0.0,
            y: 1.0,
            z: 0.0,
        };
        let s = altitude_sigma_at_ray(&r, &g, 0.0, 0.0, 0.02);
        assert!((s - 0.02).abs() < 1e-12);
    }

    #[test]
    fn altitude_sigma_near_zenith_clamped() {
        // r ≈ -g (body at zenith): cos²→0, but clamp to 0.0025.
        // σ_α² = σ_gz² / 0.0025 = σ_gz · 20.
        let r = CameraRay {
            x: 0.0,
            y: -1.0,
            z: 0.0,
        };
        let g = CameraRay {
            x: 0.0,
            y: 1.0,
            z: 0.0,
        };
        let s = altitude_sigma_at_ray(&r, &g, 0.0, 0.01, 0.0);
        let expected = 0.01 / 0.05; // σ_gy · |r_y| / 0.05
        assert!((s - expected).abs() < 1e-9, "got {s} want {expected}");
    }
}
