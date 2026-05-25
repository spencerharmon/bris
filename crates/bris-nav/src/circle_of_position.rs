//! Cold-start "no assumed position" fix via circle-of-position
//! intersection on the unit sphere.
//!
//! See `docs/design/circle_of_position.md` for the contract this
//! module implements. Briefly: each reduced sight defines a small
//! circle on the unit sphere whose centre is the body's geographic
//! position (GP) and whose angular radius is the co-altitude
//! `z = π/2 − Ho`. The observer lies at the intersection of all the
//! input circles. Two circles intersect in at most two points (the
//! classic Sumner ambiguity); three or more circles resolve the
//! ambiguity geometrically.
//!
//! The companion of this module is [`crate::fix::multi_sight_fix`],
//! which is the Saint-Hilaire intercept-method solver used once a
//! position prior exists. Cold-start is invoked by the engine when
//! no prior is available and `multi_sight_fix` reports
//! `SingularGeometry`.

// Navigation-domain names like lat/lon, lops, dn/de, sigma_major/
// sigma_minor are short and repeat across the module; suppress the
// pedantic similar-names lint at module level.
#![allow(clippy::similar_names)]

use crate::fix::{ellipse_from_covariance, multi_sight_fix, Fix, FixError};
use crate::sight::LineOfPosition;
use bris_core::{Latitude, Longitude, Sigma};

/// Nautical miles per radian of arc on Earth's surface.
///
/// By navigator's convention, 1 nm = 1 arcminute, so 1 rad =
/// `180·60/π` nm ≈ 3437.7468 nm.
const NM_PER_RAD: f64 = 180.0 * 60.0 / std::f64::consts::PI;

/// A reduced sight expressed as a circle on the unit sphere.
///
/// `gp_lat_rad` is the body's declination at the sight instant;
/// `gp_lon_rad` is the body's hour-angle longitude (i.e. `−GHA`
/// normalized to `[−π, π]`). `co_altitude_rad` is `π/2 − Ho`
/// where `Ho` is the apparent altitude (refraction/dip/parallax
/// applied). `sigma_rad` is the 1σ uncertainty in the co-altitude
/// (equivalently, in `Ho`).
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::struct_field_names)] // domain notation: every field is in radians
pub struct CircleOfPosition {
    /// Body GP latitude (declination), radians, in `[−π/2, π/2]`.
    pub gp_lat_rad: f64,
    /// Body GP longitude (`−GHA`), radians, in `(−π, π]`.
    pub gp_lon_rad: f64,
    /// Co-altitude = `π/2 − Ho`, radians, in `(0, π/2)`.
    pub co_altitude_rad: f64,
    /// 1σ uncertainty in the co-altitude, radians.
    pub sigma_rad: f64,
}

/// A single fix candidate produced by the cold-start solver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FixCandidate {
    /// Candidate observer latitude.
    pub lat: Latitude,
    /// Candidate observer longitude.
    pub lon: Longitude,
    /// 2×2 position covariance in nm² (north, east).
    pub covariance_nm2: [[f64; 2]; 2],
    /// 1σ semi-major axis of the uncertainty ellipse, nm.
    pub sigma_major_nm: Sigma,
    /// 1σ semi-minor axis of the uncertainty ellipse, nm.
    pub sigma_minor_nm: Sigma,
    /// Major-axis orientation, radians clockwise from north, in
    /// `[0, π)`.
    pub orientation_rad: f64,
    /// Number of input sights used to produce this candidate.
    pub sight_count: usize,
    /// Number of pairwise-intersection candidates that voted for
    /// this candidate's consensus cluster. Bounded by `N(N−1)/2`.
    /// Always 1 for the two-circle case.
    pub cluster_size: usize,
}

/// Top-level result of [`cold_start_fix`].
#[derive(Debug, Clone, PartialEq)]
pub enum ColdStartResult {
    /// A single best fix; either N ≥ 3 with a unique consensus
    /// cluster, or the tangent / collapsed-secondary two-circle
    /// case.
    Fix(FixCandidate),
    /// Two surviving candidates the caller must disambiguate
    /// (operator hemisphere hint or an additional sight).
    TwoCandidates {
        /// First candidate (caller treats as unordered).
        primary: FixCandidate,
        /// Second candidate (caller treats as unordered).
        secondary: FixCandidate,
        /// Great-circle distance between the candidates, nm.
        separation_great_circle_nm: f64,
    },
    /// The input circles are not mutually consistent within the
    /// per-sight σ. Returns the best single candidate so the
    /// caller can display *something* and the per-sight residuals
    /// (radians) so the caller can identify the blunder.
    Inconsistent {
        /// Best single candidate by weighted residual.
        best_candidate: FixCandidate,
        /// Per-input-circle residual at `best_candidate`, in
        /// the same order as the input slice. Each entry is
        /// `acos(g_i · best) − z_i` (radians).
        per_sight_residuals_rad: Vec<f64>,
    },
}

/// Errors from [`cold_start_fix`]. Reserved for malformed input;
/// geometric inconsistency is a [`ColdStartResult`] variant, not
/// an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ColdStartError {
    /// Fewer than 2 input circles.
    #[error("cold-start fix needs ≥ 2 circles, got {0}")]
    InsufficientSights(usize),
    /// In the strict 2-circle case the two GPs are coincident
    /// or antipodal (g₁ × g₂ ≈ 0). Geometry is degenerate;
    /// caller must supply a different sight.
    #[error("2-circle case: GPs coincident or antipodal")]
    Disjoint,
    /// An input contained NaN or infinity.
    #[error("non-finite value in input")]
    NonFinite,
}

/// Configuration knobs for [`cold_start_fix`].
#[derive(Debug, Clone, Copy)]
pub struct ColdStartConfig {
    /// Discriminant magnitude below which a 2-circle intersection
    /// is treated as tangent (single candidate, inflated σ).
    /// Default `1e-6`.
    pub tangent_tolerance_rad: f64,
    /// Cluster radius for the N ≥ 3 cluster-and-refine pass,
    /// in radians. Pair-intersection candidates within this
    /// great-circle distance of the lowest-residual candidate
    /// are members of the consensus cluster. Default
    /// `5° = 0.0872 rad`.
    pub cluster_radius_rad: f64,
}

impl Default for ColdStartConfig {
    fn default() -> Self {
        Self {
            tangent_tolerance_rad: 1e-6,
            cluster_radius_rad: 5.0_f64.to_radians(),
        }
    }
}

/// Solve a cold-start fix from a slice of circles of position.
///
/// See `docs/design/circle_of_position.md` for the geometry,
/// the ambiguity-resolution policy, and the test corpus.
///
/// # Errors
///
/// - [`ColdStartError::InsufficientSights`] if fewer than 2
///   circles are supplied.
/// - [`ColdStartError::Disjoint`] if exactly 2 circles are
///   supplied and their GPs are coincident or antipodal.
/// - [`ColdStartError::NonFinite`] if any input is NaN or
///   infinite.
pub fn cold_start_fix(
    circles: &[CircleOfPosition],
    cfg: &ColdStartConfig,
) -> Result<ColdStartResult, ColdStartError> {
    if circles.len() < 2 {
        return Err(ColdStartError::InsufficientSights(circles.len()));
    }
    for c in circles {
        if !c.gp_lat_rad.is_finite()
            || !c.gp_lon_rad.is_finite()
            || !c.co_altitude_rad.is_finite()
            || !c.sigma_rad.is_finite()
        {
            return Err(ColdStartError::NonFinite);
        }
    }

    if circles.len() == 2 {
        return two_circle(&circles[0], &circles[1], cfg);
    }

    // N >= 3: pairwise candidates, cluster, refine.
    n_circle(circles, cfg)
}

// ---------------------------------------------------------------
// Two-circle analytic intersection
// ---------------------------------------------------------------

/// Result of the raw analytic 2-circle intersection.
#[derive(Debug, Clone, Copy)]
enum TwoCircleRaw {
    /// Two real intersection points on the unit sphere.
    Two([f64; 3], [f64; 3]),
    /// Tangent: a single intersection point.
    One([f64; 3]),
    /// Discriminant negative; circles do not intersect. The
    /// "best" cartesian point is the closest point in the
    /// plane spanned by `g₁` and `g₂` projected back onto the
    /// unit sphere.
    Disjoint([f64; 3]),
}

fn two_circle(
    c1: &CircleOfPosition,
    c2: &CircleOfPosition,
    cfg: &ColdStartConfig,
) -> Result<ColdStartResult, ColdStartError> {
    let g1 = latlon_to_xyz(c1.gp_lat_rad, c1.gp_lon_rad);
    let g2 = latlon_to_xyz(c2.gp_lat_rad, c2.gp_lon_rad);
    let raw = solve_two_circle(g1, c1.co_altitude_rad, g2, c2.co_altitude_rad, cfg)?;

    match raw {
        TwoCircleRaw::Two(p_a, p_b) => {
            let cand_a = two_circle_candidate(p_a, c1, c2, 2);
            let cand_b = two_circle_candidate(p_b, c1, c2, 2);
            let sep_rad = unit_angle(p_a, p_b);
            Ok(ColdStartResult::TwoCandidates {
                primary: cand_a,
                secondary: cand_b,
                separation_great_circle_nm: sep_rad * NM_PER_RAD,
            })
        }
        TwoCircleRaw::One(p) => {
            // Tangent: a single candidate, with σ inflated along
            // the GP-joining great circle. Build covariance as if
            // half-angle θ is tiny (use tangent_tolerance as a
            // floor for sin θ).
            let mut cand = two_circle_candidate(p, c1, c2, 1);
            cand.cluster_size = 1;
            Ok(ColdStartResult::Fix(cand))
        }
        TwoCircleRaw::Disjoint(p) => {
            let cand = two_circle_candidate(p, c1, c2, 2);
            let r0 = unit_angle(g1, p) - c1.co_altitude_rad;
            let r1 = unit_angle(g2, p) - c2.co_altitude_rad;
            Ok(ColdStartResult::Inconsistent {
                best_candidate: cand,
                per_sight_residuals_rad: vec![r0, r1],
            })
        }
    }
}

fn solve_two_circle(
    g1: [f64; 3],
    z1: f64,
    g2: [f64; 3],
    z2: f64,
    cfg: &ColdStartConfig,
) -> Result<TwoCircleRaw, ColdStartError> {
    let c1 = z1.cos();
    let c2 = z2.cos();
    let gg = dot(g1, g2);
    let one_minus_gg2 = 1.0 - gg * gg;
    if one_minus_gg2.abs() < 1e-15 {
        return Err(ColdStartError::Disjoint);
    }
    let alpha = (c1 - gg * c2) / one_minus_gg2;
    let beta = (c2 - gg * c1) / one_minus_gg2;
    // p = α g1 + β g2 + γ d, where d = g1 × g2.
    // |p|² = α² + β² + 2αβ(g1·g2) + γ²|d|², and |d|² = 1 − (g1·g2)².
    let d = cross(g1, g2);
    let d_norm2 = one_minus_gg2; // = |d|²
    let p_in_plane_norm2 = alpha * alpha + beta * beta + 2.0 * alpha * beta * gg;
    let gamma2 = (1.0 - p_in_plane_norm2) / d_norm2;

    let base = add(scale(g1, alpha), scale(g2, beta));
    if gamma2.abs() < cfg.tangent_tolerance_rad {
        return Ok(TwoCircleRaw::One(normalize(base)));
    }
    if gamma2 < 0.0 {
        // No real intersection; best candidate is the projection
        // of the plane point back onto the unit sphere.
        return Ok(TwoCircleRaw::Disjoint(normalize(base)));
    }
    let gamma = gamma2.sqrt();
    let p_a = add(base, scale(d, gamma));
    let p_b = add(base, scale(d, -gamma));
    Ok(TwoCircleRaw::Two(normalize(p_a), normalize(p_b)))
}

/// Build a [`FixCandidate`] from a cartesian point for the
/// two-circle case, deriving covariance by first-order
/// propagation through the intersection geometry.
fn two_circle_candidate(
    p: [f64; 3],
    c1: &CircleOfPosition,
    c2: &CircleOfPosition,
    sight_count: usize,
) -> FixCandidate {
    let (lat_rad, lon_rad) = xyz_to_latlon(p);
    let g1 = latlon_to_xyz(c1.gp_lat_rad, c1.gp_lon_rad);
    let g2 = latlon_to_xyz(c2.gp_lat_rad, c2.gp_lon_rad);

    // Half-angle the two GPs subtend at p (spherical law of cosines
    // applied to the triangle GP1-p-GP2).
    let cos_delta = dot(g1, g2).clamp(-1.0, 1.0);
    let delta = cos_delta.acos();
    let z1 = c1.co_altitude_rad;
    let z2 = c2.co_altitude_rad;
    let denom = (z1.sin() * z2.sin()).max(1e-12);
    let cos_a = ((delta.cos() - z1.cos() * z2.cos()) / denom).clamp(-1.0, 1.0);
    let a_p = cos_a.acos();
    let theta = 0.5 * a_p;

    // RSS of the two sigmas (radians).
    let sigma_bar = (c1.sigma_rad * c1.sigma_rad + c2.sigma_rad * c2.sigma_rad).sqrt();
    let sin_t = theta.sin().abs().max(1e-9);
    let cos_t = theta.cos().abs().max(1e-9);
    let sigma_along_rad = sigma_bar / sin_t;
    let sigma_perp_rad = sigma_bar / cos_t;
    let sigma_along_nm = sigma_along_rad * NM_PER_RAD;
    let sigma_perp_nm = sigma_perp_rad * NM_PER_RAD;

    // Baseline-bearing axis at p: initial bearing from p toward GP1.
    // (Could equivalently use the midpoint; GP1 is fine — the
    // "along-baseline" direction is the great circle joining the
    // two GPs and p lies near it.)
    let bearing = initial_bearing(lat_rad, lon_rad, c1.gp_lat_rad, c1.gp_lon_rad);

    let sa2 = sigma_along_nm * sigma_along_nm;
    let sp2 = sigma_perp_nm * sigma_perp_nm;
    let cb = bearing.cos();
    let sb = bearing.sin();
    // R · diag(sa², sp²) · Rᵀ where R = [[cb, −sb], [sb, cb]].
    let cov = [
        [cb * cb * sa2 + sb * sb * sp2, cb * sb * (sa2 - sp2)],
        [cb * sb * (sa2 - sp2), sb * sb * sa2 + cb * cb * sp2],
    ];
    let (sigma_major_nm, sigma_minor_nm, orientation_rad) = ellipse_from_covariance(cov);

    FixCandidate {
        lat: Latitude::from_radians(
            lat_rad.clamp(-std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2),
        )
        .unwrap_or(Latitude::EQUATOR),
        lon: Longitude::from_radians(lon_rad).unwrap_or(Longitude::PRIME_MERIDIAN),
        covariance_nm2: cov,
        sigma_major_nm: Sigma::new(sigma_major_nm).unwrap_or(Sigma::ZERO),
        sigma_minor_nm: Sigma::new(sigma_minor_nm).unwrap_or(Sigma::ZERO),
        orientation_rad,
        sight_count,
        cluster_size: 1,
    }
}

// ---------------------------------------------------------------
// N >= 3 cluster-and-refine
// ---------------------------------------------------------------

fn n_circle(
    circles: &[CircleOfPosition],
    cfg: &ColdStartConfig,
) -> Result<ColdStartResult, ColdStartError> {
    // 1. Collect pair-intersection candidates.
    let mut candidates: Vec<[f64; 3]> = Vec::new();
    let n = circles.len();
    for i in 0..n {
        for j in (i + 1)..n {
            let g1 = latlon_to_xyz(circles[i].gp_lat_rad, circles[i].gp_lon_rad);
            let g2 = latlon_to_xyz(circles[j].gp_lat_rad, circles[j].gp_lon_rad);
            match solve_two_circle(
                g1,
                circles[i].co_altitude_rad,
                g2,
                circles[j].co_altitude_rad,
                cfg,
            ) {
                Ok(TwoCircleRaw::Two(a, b)) => {
                    candidates.push(a);
                    candidates.push(b);
                }
                Ok(TwoCircleRaw::One(p)) => candidates.push(p),
                Ok(TwoCircleRaw::Disjoint(_)) | Err(_) => {}
            }
        }
    }

    if candidates.is_empty() {
        return Err(ColdStartError::Disjoint);
    }

    // 2. Score every candidate by total weighted residual.
    let scored: Vec<(usize, f64)> = candidates
        .iter()
        .enumerate()
        .map(|(idx, p)| (idx, weighted_residual(*p, circles)))
        .collect();
    let best_idx = scored
        .iter()
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(0, |(i, _)| *i);
    let centre = candidates[best_idx];

    // 3. Cluster: all candidates within cluster_radius_rad of the
    //    centre form the consensus cluster.
    let cluster: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, p)| unit_angle(centre, **p) <= cfg.cluster_radius_rad)
        .map(|(i, _)| i)
        .collect();

    // Look for a second well-separated cluster of comparable size.
    let second: Option<(usize, usize)> = candidates
        .iter()
        .enumerate()
        .filter(|(_, p)| unit_angle(centre, **p) > cfg.cluster_radius_rad)
        .min_by(|a, b| {
            scored[a.0]
                .1
                .partial_cmp(&scored[b.0].1)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| {
            let p2 = candidates[i];
            let size = candidates
                .iter()
                .filter(|q| unit_angle(p2, **q) <= cfg.cluster_radius_rad)
                .count();
            (i, size)
        });

    if cluster.len() < 2 {
        // No consensus: report inconsistent with best single candidate.
        let cand = newton_refine(centre, circles, cluster.len().max(1))?;
        let residuals = per_circle_residuals(latlon_xyz_from_candidate(&cand), circles);
        return Ok(ColdStartResult::Inconsistent {
            best_candidate: cand,
            per_sight_residuals_rad: residuals,
        });
    }

    let primary = newton_refine(centre, circles, cluster.len())?;

    if let Some((idx2, size2)) = second {
        // Two clusters of comparable size: ambiguous.
        if size2 >= 2 && size2 * 2 >= cluster.len() {
            let secondary = newton_refine(candidates[idx2], circles, size2)?;
            let sep_rad = unit_angle(
                latlon_xyz_from_candidate(&primary),
                latlon_xyz_from_candidate(&secondary),
            );
            return Ok(ColdStartResult::TwoCandidates {
                primary,
                secondary,
                separation_great_circle_nm: sep_rad * NM_PER_RAD,
            });
        }
    }

    Ok(ColdStartResult::Fix(primary))
}

/// Newton refinement at `centre_xyz`: build `LineOfPosition` records
/// against the cluster centre as the assumed position and call
/// `multi_sight_fix`. This is one Newton step (single linearization)
/// and produces the 2×2 covariance and ellipse decomposition.
fn newton_refine(
    centre_xyz: [f64; 3],
    circles: &[CircleOfPosition],
    cluster_size: usize,
) -> Result<FixCandidate, ColdStartError> {
    let (lat0_rad, lon0_rad) = xyz_to_latlon(centre_xyz);
    let lat0 = Latitude::from_radians(
        lat0_rad.clamp(-std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2),
    )
    .map_err(|_| ColdStartError::NonFinite)?;
    let lon0 = Longitude::from_radians(lon0_rad).map_err(|_| ColdStartError::NonFinite)?;

    let lops: Vec<LineOfPosition> = circles
        .iter()
        .map(|c| {
            let g = latlon_to_xyz(c.gp_lat_rad, c.gp_lon_rad);
            let z_at_centre = unit_angle(g, centre_xyz);
            // intercept positive = observer closer to GP than centre
            // (i.e. observed co-altitude < centre's distance to GP).
            let intercept_rad = z_at_centre - c.co_altitude_rad;
            let bearing = initial_bearing(lat0_rad, lon0_rad, c.gp_lat_rad, c.gp_lon_rad);
            let sigma_nm = c.sigma_rad.abs() * NM_PER_RAD;
            LineOfPosition {
                assumed_lat: lat0,
                assumed_lon: lon0,
                azimuth_rad: bearing,
                intercept_nm: intercept_rad * NM_PER_RAD,
                intercept_sigma_nm: Sigma::new(sigma_nm.max(1e-9)).unwrap_or(Sigma::ZERO),
            }
        })
        .collect();

    let fix: Fix = multi_sight_fix(&lops).map_err(|e| match e {
        FixError::InsufficientSights(_) => ColdStartError::InsufficientSights(circles.len()),
        FixError::SingularGeometry | FixError::NonFinite => ColdStartError::Disjoint,
    })?;

    Ok(FixCandidate {
        lat: fix.lat,
        lon: fix.lon,
        covariance_nm2: fix.covariance_nm2,
        sigma_major_nm: Sigma::new(fix.sigma_major_nm).unwrap_or(Sigma::ZERO),
        sigma_minor_nm: Sigma::new(fix.sigma_minor_nm).unwrap_or(Sigma::ZERO),
        orientation_rad: fix.orientation_rad,
        sight_count: circles.len(),
        cluster_size,
    })
}

fn latlon_xyz_from_candidate(c: &FixCandidate) -> [f64; 3] {
    latlon_to_xyz(c.lat.radians(), c.lon.radians())
}

fn weighted_residual(p: [f64; 3], circles: &[CircleOfPosition]) -> f64 {
    circles
        .iter()
        .map(|c| {
            let g = latlon_to_xyz(c.gp_lat_rad, c.gp_lon_rad);
            let r = unit_angle(g, p) - c.co_altitude_rad;
            let s = c.sigma_rad.max(1e-12);
            (r * r) / (s * s)
        })
        .sum()
}

fn per_circle_residuals(p: [f64; 3], circles: &[CircleOfPosition]) -> Vec<f64> {
    circles
        .iter()
        .map(|c| {
            let g = latlon_to_xyz(c.gp_lat_rad, c.gp_lon_rad);
            unit_angle(g, p) - c.co_altitude_rad
        })
        .collect()
}

// ---------------------------------------------------------------
// Small spherical-geometry helpers
// ---------------------------------------------------------------

fn latlon_to_xyz(lat: f64, lon: f64) -> [f64; 3] {
    let cl = lat.cos();
    [cl * lon.cos(), cl * lon.sin(), lat.sin()]
}

fn xyz_to_latlon(p: [f64; 3]) -> (f64, f64) {
    let r = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt().max(1e-15);
    let lat = (p[2] / r).clamp(-1.0, 1.0).asin();
    let lon = p[1].atan2(p[0]);
    (lat, lon)
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f64; 3], s: f64) -> [f64; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn normalize(a: [f64; 3]) -> [f64; 3] {
    let n = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt().max(1e-15);
    [a[0] / n, a[1] / n, a[2] / n]
}

/// Angle between two unit vectors (radians), numerically stable
/// near 0 and π via `atan2(|a × b|, a · b)`.
fn unit_angle(a: [f64; 3], b: [f64; 3]) -> f64 {
    let c = cross(a, b);
    let s = (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt();
    s.atan2(dot(a, b))
}

/// Initial great-circle bearing from `(lat1, lon1)` to
/// `(lat2, lon2)`, radians clockwise from north, in `[0, 2π)`.
fn initial_bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let dlon = lon2 - lon1;
    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    y.atan2(x).rem_euclid(std::f64::consts::TAU)
}

// ===============================================================
// Tests
// ===============================================================

#[cfg(test)]
#[allow(
    clippy::manual_let_else,
    clippy::match_wildcard_for_single_variants,
    clippy::float_cmp
)]
mod tests {
    use super::*;

    /// Build a synthetic circle from an observer position and a GP
    /// (perfect measurement, σ given in arcminutes).
    fn synth_circle(
        obs_lat_deg: f64,
        obs_lon_deg: f64,
        gp_lat_deg: f64,
        gp_lon_deg: f64,
        sigma_arcmin: f64,
    ) -> CircleOfPosition {
        let obs = latlon_to_xyz(obs_lat_deg.to_radians(), obs_lon_deg.to_radians());
        let gp = latlon_to_xyz(gp_lat_deg.to_radians(), gp_lon_deg.to_radians());
        let z = unit_angle(obs, gp);
        CircleOfPosition {
            gp_lat_rad: gp_lat_deg.to_radians(),
            gp_lon_rad: gp_lon_deg.to_radians(),
            co_altitude_rad: z,
            sigma_rad: (sigma_arcmin / 60.0).to_radians(),
        }
    }

    fn nm_between(lat1_deg: f64, lon1_deg: f64, lat2_deg: f64, lon2_deg: f64) -> f64 {
        let a = latlon_to_xyz(lat1_deg.to_radians(), lon1_deg.to_radians());
        let b = latlon_to_xyz(lat2_deg.to_radians(), lon2_deg.to_radians());
        unit_angle(a, b) * NM_PER_RAD
    }

    #[test]
    fn test1_two_circle_exact() {
        let obs_lat = 40.0;
        let obs_lon = 0.0;
        let c1 = synth_circle(obs_lat, obs_lon, 20.0, 30.0, 0.5);
        let c2 = synth_circle(obs_lat, obs_lon, 60.0, -30.0, 0.5);
        let res = cold_start_fix(&[c1, c2], &ColdStartConfig::default()).unwrap();
        let (p, s) = match res {
            ColdStartResult::TwoCandidates {
                primary, secondary, ..
            } => (primary, secondary),
            _ => panic!("expected TwoCandidates, got {res:?}"),
        };
        let d_p = nm_between(p.lat.degrees(), p.lon.degrees(), obs_lat, obs_lon);
        let d_s = nm_between(s.lat.degrees(), s.lon.degrees(), obs_lat, obs_lon);
        let best = d_p.min(d_s);
        assert!(best < 0.01, "best candidate {best} nm from truth");
    }

    #[test]
    fn test2_two_circle_antipodal_ambiguity() {
        let c1 = synth_circle(40.0, 0.0, 20.0, 30.0, 0.5);
        let c2 = synth_circle(40.0, 0.0, 60.0, -30.0, 0.5);
        let res = cold_start_fix(&[c1, c2], &ColdStartConfig::default()).unwrap();
        let (p, s, sep) = match res {
            ColdStartResult::TwoCandidates {
                primary,
                secondary,
                separation_great_circle_nm,
            } => (primary, secondary, separation_great_circle_nm),
            _ => panic!("expected TwoCandidates"),
        };
        // Two distinct candidates; non-trivial separation.
        assert!(sep > 100.0, "candidates should be distant: sep = {sep} nm");
        assert!((p.lat.degrees() - s.lat.degrees()).abs() > 1.0);
    }

    #[test]
    fn test3_two_circle_tangent() {
        // Two GPs at angular distance exactly z1 + z2: circles touch
        // externally at a single point on the GP-joining arc.
        let gp1_lat = 0.0_f64;
        let gp1_lon = 0.0_f64;
        let z1 = 30.0_f64.to_radians();
        let z2 = 20.0_f64.to_radians();
        let delta = z1 + z2; // GP separation
                             // Place GP2 along the equator from GP1 by `delta`.
        let gp2_lat = 0.0_f64;
        let gp2_lon = delta.to_degrees();
        let c1 = CircleOfPosition {
            gp_lat_rad: gp1_lat.to_radians(),
            gp_lon_rad: gp1_lon.to_radians(),
            co_altitude_rad: z1,
            sigma_rad: (0.5_f64 / 60.0).to_radians(),
        };
        let c2 = CircleOfPosition {
            gp_lat_rad: gp2_lat.to_radians(),
            gp_lon_rad: gp2_lon.to_radians(),
            co_altitude_rad: z2,
            sigma_rad: (0.5_f64 / 60.0).to_radians(),
        };
        let cfg = ColdStartConfig {
            tangent_tolerance_rad: 1e-3,
            ..ColdStartConfig::default()
        };
        let res = cold_start_fix(&[c1, c2], &cfg).unwrap();
        match res {
            ColdStartResult::Fix(f) => {
                // Tangent: sigma_major should be inflated relative
                // to single-sight sigma.
                assert!(f.sigma_major_nm.value() > 10.0);
            }
            other => panic!("expected tangent Fix, got {other:?}"),
        }
    }

    #[test]
    fn test4_two_circle_disjoint_inconsistent() {
        // GPs are 60° apart; z1 = z2 = 10° → circles cannot
        // possibly intersect (sum < separation).
        let c1 = CircleOfPosition {
            gp_lat_rad: 0.0,
            gp_lon_rad: 0.0,
            co_altitude_rad: 10.0_f64.to_radians(),
            sigma_rad: (0.5_f64 / 60.0).to_radians(),
        };
        let c2 = CircleOfPosition {
            gp_lat_rad: 0.0,
            gp_lon_rad: 60.0_f64.to_radians(),
            co_altitude_rad: 10.0_f64.to_radians(),
            sigma_rad: (0.5_f64 / 60.0).to_radians(),
        };
        let res = cold_start_fix(&[c1, c2], &ColdStartConfig::default()).unwrap();
        match res {
            ColdStartResult::Inconsistent {
                per_sight_residuals_rad,
                ..
            } => {
                assert_eq!(per_sight_residuals_rad.len(), 2);
            }
            other => panic!("expected Inconsistent, got {other:?}"),
        }
    }

    #[test]
    fn test5_three_circle_convergence() {
        let obs_lat = 35.0;
        let obs_lon = -120.0;
        let circles = [
            synth_circle(obs_lat, obs_lon, 10.0, -90.0, 0.5),
            synth_circle(obs_lat, obs_lon, 50.0, -150.0, 0.5),
            synth_circle(obs_lat, obs_lon, -20.0, -110.0, 0.5),
        ];
        let res = cold_start_fix(&circles, &ColdStartConfig::default()).unwrap();
        match res {
            ColdStartResult::Fix(f) => {
                let d = nm_between(f.lat.degrees(), f.lon.degrees(), obs_lat, obs_lon);
                assert!(d < 0.5, "fix is {d} nm from truth");
                assert!(f.cluster_size >= 2);
            }
            other => panic!("expected single Fix, got {other:?}"),
        }
    }

    #[test]
    fn test6_three_circle_one_blunder() {
        let obs_lat = 35.0;
        let obs_lon = -120.0;
        let mut c1 = synth_circle(obs_lat, obs_lon, 10.0, -90.0, 0.5);
        let c2 = synth_circle(obs_lat, obs_lon, 50.0, -150.0, 0.5);
        let c3 = synth_circle(obs_lat, obs_lon, -20.0, -110.0, 0.5);
        // 30-arcmin gross error on c1.
        c1.co_altitude_rad += (30.0_f64 / 60.0).to_radians();
        let res = cold_start_fix(&[c1, c2, c3], &ColdStartConfig::default()).unwrap();
        match res {
            ColdStartResult::Inconsistent { .. } | ColdStartResult::Fix(_) => {
                // Either outcome is acceptable.
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn test7_n5_weighted_covariance_scales() {
        let obs_lat = 0.0;
        let obs_lon = 0.0;
        let bearings_deg = [0.0_f64, 60.0, 120.0, 180.0, 240.0];
        let sigmas = [0.5_f64, 1.0, 1.5, 2.0, 1.0];
        let mut circles = Vec::new();
        for (b, s) in bearings_deg.iter().zip(sigmas.iter()) {
            // Place each GP 45° away from observer in this bearing.
            let gp_lat = (45.0_f64 * b.to_radians().cos()).clamp(-89.0, 89.0);
            let gp_lon = 45.0_f64 * b.to_radians().sin();
            circles.push(synth_circle(obs_lat, obs_lon, gp_lat, gp_lon, *s));
        }
        let res_eq = cold_start_fix(
            &circles
                .iter()
                .map(|c| CircleOfPosition {
                    sigma_rad: (0.5_f64 / 60.0).to_radians(),
                    ..*c
                })
                .collect::<Vec<_>>(),
            &ColdStartConfig::default(),
        )
        .unwrap();
        let res_mixed = cold_start_fix(&circles, &ColdStartConfig::default()).unwrap();
        let (eq, mixed) = match (res_eq, res_mixed) {
            (ColdStartResult::Fix(a), ColdStartResult::Fix(b)) => (a, b),
            other => panic!("expected two Fix results: {other:?}"),
        };
        // Mixed sigmas (some larger) → larger fix uncertainty.
        assert!(mixed.sigma_major_nm.value() >= eq.sigma_major_nm.value());
    }

    #[test]
    fn test8_same_body_30min_apart() {
        // Moon GP travels ~14 nm/min; 30 minutes ≈ ~420 nm = ~7° of arc.
        let obs_lat = 30.0;
        let obs_lon = 0.0;
        let c1 = synth_circle(obs_lat, obs_lon, 0.0, 0.0, 0.5);
        let c2 = synth_circle(obs_lat, obs_lon, 0.5, 7.0, 0.5);
        let res = cold_start_fix(&[c1, c2], &ColdStartConfig::default()).unwrap();
        match res {
            ColdStartResult::TwoCandidates { primary, .. } => {
                let r = primary.sigma_major_nm.value() / primary.sigma_minor_nm.value().max(1e-9);
                assert!(r > 1.5, "ellipse should be elongated, got ratio {r}");
            }
            other => panic!("expected TwoCandidates: {other:?}"),
        }
    }

    #[test]
    fn test9_same_body_1min_apart() {
        // ~14 nm GP travel → ~0.23° on the unit sphere.
        let obs_lat = 30.0;
        let obs_lon = 0.0;
        let c1 = synth_circle(obs_lat, obs_lon, 0.0, 0.0, 0.5);
        let c2 = synth_circle(obs_lat, obs_lon, 0.05, 0.23, 0.5);
        let res = cold_start_fix(&[c1, c2], &ColdStartConfig::default()).unwrap();
        match res {
            ColdStartResult::TwoCandidates { primary, .. } => {
                assert!(
                    primary.sigma_major_nm.value() > 100.0,
                    "sigma_major = {} nm; engine-level diversity gate should refuse",
                    primary.sigma_major_nm.value()
                );
            }
            other => panic!("expected TwoCandidates: {other:?}"),
        }
    }

    #[test]
    fn test10_pole_adjacent_observer() {
        let obs_lat = 89.0;
        let obs_lon = 0.0;
        let c1 = synth_circle(obs_lat, obs_lon, 20.0, 0.0, 0.5);
        let c2 = synth_circle(obs_lat, obs_lon, 30.0, 90.0, 0.5);
        let res = cold_start_fix(&[c1, c2], &ColdStartConfig::default()).unwrap();
        // Just verify it ran without a NonFinite / divide-by-zero.
        match res {
            ColdStartResult::TwoCandidates {
                primary, secondary, ..
            } => {
                let d_p = nm_between(
                    primary.lat.degrees(),
                    primary.lon.degrees(),
                    obs_lat,
                    obs_lon,
                );
                let d_s = nm_between(
                    secondary.lat.degrees(),
                    secondary.lon.degrees(),
                    obs_lat,
                    obs_lon,
                );
                assert!(d_p.min(d_s) < 1.0, "pole-adjacent fix missed truth");
            }
            other => panic!("expected TwoCandidates: {other:?}"),
        }
    }

    #[test]
    fn rejects_too_few_sights() {
        let c = CircleOfPosition {
            gp_lat_rad: 0.0,
            gp_lon_rad: 0.0,
            co_altitude_rad: 0.5,
            sigma_rad: 1e-4,
        };
        assert!(matches!(
            cold_start_fix(&[c], &ColdStartConfig::default()),
            Err(ColdStartError::InsufficientSights(1))
        ));
    }

    #[test]
    fn rejects_non_finite_input() {
        let c1 = CircleOfPosition {
            gp_lat_rad: f64::NAN,
            gp_lon_rad: 0.0,
            co_altitude_rad: 0.5,
            sigma_rad: 1e-4,
        };
        let c2 = CircleOfPosition {
            gp_lat_rad: 0.1,
            gp_lon_rad: 0.0,
            co_altitude_rad: 0.5,
            sigma_rad: 1e-4,
        };
        assert!(matches!(
            cold_start_fix(&[c1, c2], &ColdStartConfig::default()),
            Err(ColdStartError::NonFinite)
        ));
    }
}
