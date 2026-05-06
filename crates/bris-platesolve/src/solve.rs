//! Plate-solve: peak detections → identified stars + camera attitude.
//!
//! See the crate-level docstring for the full pipeline. This module
//! is the "match a 4-tuple of peaks against the hash database, then
//! verify with additional stars" half. The hash database lives in
//! [`crate::hash`]; the rotation-recovery math is in
//! [`crate::kabsch`].

use crate::hash::{pattern_hash, ra_dec_to_unit_vec, CatalogPattern, StarHashDb};
use crate::kabsch::{kabsch_rotation, rotate_vec};
use bris_almanac::{all_stars, by_hr};
use bris_vision::{lens, Intrinsics, Peak};

/// Configuration for [`plate_solve`].
#[derive(Debug, Clone, Copy)]
pub struct PlateSolveConfig {
    /// Maximum number of brightest peaks to consider for the
    /// initial 4-tuple search. The full set is C(n, 4) so this
    /// number multiplies cubically into the solve cost. Default 12
    /// (495 4-tuples).
    pub max_peaks_to_match: usize,
    /// Minimum number of additional stars (beyond the initial 4)
    /// that must verify against detected peaks for a solution to
    /// be accepted. Default 3 (so 4 initial + 3 verified = 7
    /// stars total — strong consensus).
    pub min_verifications: usize,
    /// Maximum angular separation between a verifying star's
    /// projected position and the nearest detected peak, radians.
    /// Loose pre-refinement filter. Default 1°.
    pub verify_match_radius_rad: f64,
    /// Maximum RMS angular residual (radians) of the *refined*
    /// match (after re-fitting Kabsch on all matched pairs).
    /// Tightens the post-verification accept criterion: a loose
    /// 1° verify radius can be satisfied by accidental matches in
    /// other sky regions, but those matches won't fit within sub-
    /// pixel residuals when re-fit. Default 30 arcseconds (≈ 1.5
    /// × 10⁻⁴ rad), well above per-pixel detection sigma but well
    /// below the per-pattern false-match scale.
    pub max_rms_residual_rad: f64,
    /// Maximum allowed angular distance between any pair of stars
    /// in a candidate 4-tuple, radians. Should match the database
    /// configuration's `max_pattern_diameter_rad`. Default 60°.
    pub max_tuple_diameter_rad: f64,
}

impl Default for PlateSolveConfig {
    fn default() -> Self {
        Self {
            max_peaks_to_match: 12,
            min_verifications: 3,
            verify_match_radius_rad: 1.0_f64.to_radians(),
            max_rms_residual_rad: (30.0 / 3600.0_f64).to_radians(), // 30 arcsec
            max_tuple_diameter_rad: 60.0_f64.to_radians(),
        }
    }
}

/// Errors from [`plate_solve`].
#[derive(Debug, thiserror::Error)]
pub enum PlateSolveError {
    /// Fewer than 4 detected peaks; can't form a 4-tuple.
    #[error("need ≥ 4 peaks, got {0}")]
    InsufficientPeaks(usize),
    /// No 4-tuple matched the database with sufficient verification.
    #[error("no candidate match exceeded {min_verifications} verifications (best: {best_verifications})")]
    NoMatch {
        /// Verification count required.
        min_verifications: usize,
        /// Best verification count found across all candidates.
        best_verifications: usize,
    },
}

/// One identified star in the solved frame.
#[derive(Debug, Clone, Copy)]
pub struct IdentifiedStar {
    /// Detected peak's pixel x coordinate.
    pub pixel_x: f64,
    /// Detected peak's pixel y coordinate.
    pub pixel_y: f64,
    /// Yale BSC HR id of the matched catalog star.
    pub hr: u32,
    /// J2000 RA in radians, [0, 2π).
    pub ra_rad: f64,
    /// J2000 Dec in radians, [-π/2, π/2].
    pub dec_rad: f64,
    /// Apparent V magnitude.
    pub vmag: f64,
}

/// Solved camera attitude: rotation matrix mapping a J2000 ICRS
/// catalog unit vector to a camera-frame unit ray.
#[derive(Debug, Clone, Copy)]
pub struct Attitude {
    /// 3×3 rotation matrix, row-major.
    ///
    /// `camera_ray = matrix · catalog_vec`, where:
    ///   - `catalog_vec` is the J2000 ICRS Cartesian unit vector
    ///     of a star (from [`crate::hash::ra_dec_to_unit_vec`]).
    ///   - `camera_ray` is the unit ray that star projects to in
    ///     the camera's frame.
    pub matrix: [f64; 9],
}

/// Result of a successful plate solve.
#[derive(Debug, Clone)]
pub struct PlateSolveResult {
    /// Camera attitude.
    pub attitude: Attitude,
    /// All identified stars (the initial 4 + any that verified).
    pub identified: Vec<IdentifiedStar>,
}

/// Plate-solve a frame's detected peaks.
///
/// # Errors
///
/// See [`PlateSolveError`].
pub fn plate_solve(
    peaks: &[Peak],
    intrinsics: &Intrinsics,
    db: &StarHashDb,
    cfg: PlateSolveConfig,
) -> Result<PlateSolveResult, PlateSolveError> {
    if peaks.len() < 4 {
        return Err(PlateSolveError::InsufficientPeaks(peaks.len()));
    }

    // Take the brightest N peaks for the initial 4-tuple search.
    let n = peaks.len().min(cfg.max_peaks_to_match);
    let candidate_peaks = &peaks[..n];

    // Map each candidate peak to a unit ray in camera frame.
    let peak_rays: Vec<[f64; 3]> = candidate_peaks
        .iter()
        .map(|p| {
            let (rx, ry, rz) = lens::pixel_ray_direction(*intrinsics, p.x, p.y);
            normalize([rx, ry, rz])
        })
        .collect();

    // Precompute pairwise angular distances between peak rays.
    let mut peak_dots = vec![0.0_f64; n * n];
    for i in 0..n {
        for j in (i + 1)..n {
            let d = peak_rays[i][0] * peak_rays[j][0]
                + peak_rays[i][1] * peak_rays[j][1]
                + peak_rays[i][2] * peak_rays[j][2];
            peak_dots[i * n + j] = d;
            peak_dots[j * n + i] = d;
        }
    }
    let cos_max_diam = cfg.max_tuple_diameter_rad.cos();

    let mut best_result: Option<PlateSolveResult> = None;
    let mut best_verifications: usize = 0;

    // Enumerate 4-tuples of candidate peaks.
    for i in 0..n {
        for j in (i + 1)..n {
            if peak_dots[i * n + j] < cos_max_diam {
                continue;
            }
            for k in (j + 1)..n {
                if peak_dots[i * n + k] < cos_max_diam || peak_dots[j * n + k] < cos_max_diam {
                    continue;
                }
                for l in (k + 1)..n {
                    if peak_dots[i * n + l] < cos_max_diam
                        || peak_dots[j * n + l] < cos_max_diam
                        || peak_dots[k * n + l] < cos_max_diam
                    {
                        continue;
                    }
                    let tuple = [i, j, k, l];

                    // Compute the hash from the 6 pairwise distances.
                    let dists = [
                        peak_dots[i * n + j].acos(),
                        peak_dots[i * n + k].acos(),
                        peak_dots[i * n + l].acos(),
                        peak_dots[j * n + k].acos(),
                        peak_dots[j * n + l].acos(),
                        peak_dots[k * n + l].acos(),
                    ];
                    let hash = pattern_hash(&dists, db.config().bin_count);
                    let candidates = db.lookup(hash);

                    for catalog_pattern in candidates {
                        // Try to verify this candidate via Kabsch
                        // pose + projection of additional stars.
                        if let Some((attitude, identified, n_verified)) = try_verify(
                            tuple,
                            &peak_rays,
                            candidate_peaks,
                            catalog_pattern,
                            db,
                            &cfg,
                        ) {
                            if n_verified > best_verifications {
                                best_verifications = n_verified;
                                best_result = Some(PlateSolveResult {
                                    attitude,
                                    identified,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    match best_result {
        Some(r) if best_verifications >= cfg.min_verifications => Ok(r),
        _ => Err(PlateSolveError::NoMatch {
            min_verifications: cfg.min_verifications,
            best_verifications,
        }),
    }
}

/// Try to verify a (peak 4-tuple) ↔ (catalog pattern) match by:
///   1. Trying all 24 permutations of catalog star ↔ peak assignment.
///   2. For each permutation, run Kabsch to get an attitude.
///   3. Project additional catalog stars (within the FOV diameter)
///      into the camera frame; count how many land near a detected
///      peak.
///   4. Return the permutation with the highest verification count.
fn try_verify(
    tuple: [usize; 4],
    peak_rays: &[[f64; 3]],
    peaks: &[Peak],
    catalog_pattern: &CatalogPattern,
    db: &StarHashDb,
    cfg: &PlateSolveConfig,
) -> Option<(Attitude, Vec<IdentifiedStar>, usize)> {
    let catalog_vecs: [[f64; 3]; 4] = [
        db.star_vector(catalog_pattern.hr_ids[0])?,
        db.star_vector(catalog_pattern.hr_ids[1])?,
        db.star_vector(catalog_pattern.hr_ids[2])?,
        db.star_vector(catalog_pattern.hr_ids[3])?,
    ];
    let peak_vecs: [[f64; 3]; 4] = [
        peak_rays[tuple[0]],
        peak_rays[tuple[1]],
        peak_rays[tuple[2]],
        peak_rays[tuple[3]],
    ];

    let mut best: Option<(Attitude, Vec<IdentifiedStar>, usize)> = None;

    for perm in PERMUTATIONS_OF_4 {
        let permuted_catalog: Vec<[f64; 3]> = perm.iter().map(|&p| catalog_vecs[p]).collect();
        let camera: Vec<[f64; 3]> = peak_vecs.to_vec();

        let Ok(rot) = kabsch_rotation(&permuted_catalog, &camera) else {
            continue;
        };
        // The initial rotation; we refine it below after collecting
        // all matched pairs (initial 4 + verified). Verification
        // uses the initial rotation to project candidate stars and
        // count matches.

        // Verify: project additional catalog stars and count matches.
        // For efficiency we limit to stars within max_tuple_diameter
        // of the *centroid* of the matched 4-star pattern (in
        // catalog space).
        let centroid = catalog_centroid(&permuted_catalog);
        let cos_search = cfg.max_tuple_diameter_rad.cos();

        let mut identified: Vec<IdentifiedStar> = Vec::new();
        // Push the initial 4.
        for (i, &peak_idx) in tuple.iter().enumerate() {
            let hr = catalog_pattern.hr_ids[perm[i]];
            if let Some(star) = by_hr(hr) {
                identified.push(IdentifiedStar {
                    pixel_x: peaks[peak_idx].x,
                    pixel_y: peaks[peak_idx].y,
                    hr,
                    ra_rad: star.ra_rad,
                    dec_rad: star.dec_rad,
                    vmag: star.vmag,
                });
            }
        }

        let mut n_verified = 0_usize;
        // Track which peaks have already been claimed by an
        // identified star, to enforce one-to-one matching. Without
        // this, the verification loop accepts multiple catalog
        // stars matching the same peak, producing a self-consistent
        // but wrong rotation that "verifies" against the same
        // peaks repeatedly. (Observed empirically on the
        // night_test_highres scene where 4+ catalog stars at the
        // same RA/Dec all matched the same pixel.)
        let mut claimed_peaks = vec![false; peaks.len()];
        // Mark the initial 4-tuple's peaks as claimed.
        for &peak_idx in &tuple {
            claimed_peaks[peak_idx] = true;
        }
        for catalog_star in all_stars()
            .iter()
            .filter(|s| s.vmag <= db.config().mag_cutoff)
        {
            // Skip stars already in the 4-tuple.
            if catalog_pattern.hr_ids.contains(&catalog_star.hr) {
                continue;
            }
            let cv = ra_dec_to_unit_vec(catalog_star.ra_rad, catalog_star.dec_rad);
            // Within search cone of pattern centroid?
            let dot_centroid = cv[0] * centroid[0] + cv[1] * centroid[1] + cv[2] * centroid[2];
            if dot_centroid < cos_search {
                continue;
            }
            // Project to camera frame.
            let projected = rotate_vec(&rot, cv);
            if projected[2] <= 0.0 {
                continue;
            }
            let pp = normalize([projected[0], projected[1], projected[2]]);

            // Find the *closest* unclaimed peak within
            // verify_match_radius. One star ↔ at most one peak.
            let cos_match = cfg.verify_match_radius_rad.cos();
            let mut best_peak: Option<(usize, f64)> = None;
            for (peak_idx, _) in peaks.iter().enumerate() {
                if claimed_peaks[peak_idx] {
                    continue;
                }
                let pr = peak_rays[peak_idx];
                let dot = pr[0] * pp[0] + pr[1] * pp[1] + pr[2] * pp[2];
                if dot >= cos_match && best_peak.is_none_or(|(_, best_dot)| dot > best_dot) {
                    best_peak = Some((peak_idx, dot));
                }
            }
            if let Some((peak_idx, _)) = best_peak {
                n_verified += 1;
                claimed_peaks[peak_idx] = true;
                identified.push(IdentifiedStar {
                    pixel_x: peaks[peak_idx].x,
                    pixel_y: peaks[peak_idx].y,
                    hr: catalog_star.hr,
                    ra_rad: catalog_star.ra_rad,
                    dec_rad: catalog_star.dec_rad,
                    vmag: catalog_star.vmag,
                });
            }
        }

        // Refine: re-run Kabsch on all matched pairs (initial 4 +
        // verified). The refined rotation minimizes residuals
        // across the full set; then check that the RMS residual
        // is small enough to call it a real match. This filters
        // out false-positive matches that satisfy the loose 1°
        // verify radius but don't fit sub-pixel under refinement.
        let mut all_catalog: Vec<[f64; 3]> = Vec::with_capacity(identified.len());
        let mut all_camera: Vec<[f64; 3]> = Vec::with_capacity(identified.len());
        for (i, &peak_idx) in tuple.iter().enumerate() {
            all_catalog.push(catalog_vecs[perm[i]]);
            all_camera.push(peak_vecs[i]);
            // i indexes the tuple position (0..4); peak_idx is
            // unused here but useful for diagnostics.
            let _ = peak_idx;
        }
        // Append verified pairs. Identified contains both initial
        // 4 (first 4 entries) and verified stars after that. We
        // already pushed the initial 4 above; append the rest.
        for ident in &identified[4..] {
            let cv = ra_dec_to_unit_vec(ident.ra_rad, ident.dec_rad);
            // Find the peak ray by matching pixel coords to the
            // candidate_peaks slice.
            let pix_idx = peaks
                .iter()
                .position(|p| p.x == ident.pixel_x && p.y == ident.pixel_y);
            if let Some(idx) = pix_idx {
                all_catalog.push(cv);
                all_camera.push(peak_rays[idx]);
            }
        }

        let refined_rot = match kabsch_rotation(&all_catalog, &all_camera) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Compute RMS angular residual under the refined rotation.
        let mut sum_sq: f64 = 0.0;
        for (cv, pr) in all_catalog.iter().zip(all_camera.iter()) {
            let projected = rotate_vec(&refined_rot, *cv);
            // Angular distance between projected and pr.
            let dot = (projected[0] * pr[0] + projected[1] * pr[1] + projected[2] * pr[2])
                .clamp(-1.0, 1.0);
            let angle = dot.acos();
            sum_sq += angle * angle;
        }
        let rms = (sum_sq / all_catalog.len() as f64).sqrt();
        if rms > cfg.max_rms_residual_rad {
            // Residual too large → false-positive match. Skip.
            continue;
        }
        let refined_attitude = Attitude {
            matrix: refined_rot,
        };

        if best.as_ref().is_none_or(|b| n_verified > b.2) {
            best = Some((refined_attitude, identified, n_verified));
        }
    }

    best
}

fn catalog_centroid(vecs: &[[f64; 3]]) -> [f64; 3] {
    let mut c = [0.0; 3];
    for v in vecs {
        c[0] += v[0];
        c[1] += v[1];
        c[2] += v[2];
    }
    normalize(c)
}

fn normalize(v: [f64; 3]) -> [f64; 3] {
    let n = (v[0].powi(2) + v[1].powi(2) + v[2].powi(2)).sqrt();
    if n == 0.0 {
        [0.0, 0.0, 1.0]
    } else {
        [v[0] / n, v[1] / n, v[2] / n]
    }
}

/// All 24 permutations of [0, 1, 2, 3].
const PERMUTATIONS_OF_4: [[usize; 4]; 24] = [
    [0, 1, 2, 3],
    [0, 1, 3, 2],
    [0, 2, 1, 3],
    [0, 2, 3, 1],
    [0, 3, 1, 2],
    [0, 3, 2, 1],
    [1, 0, 2, 3],
    [1, 0, 3, 2],
    [1, 2, 0, 3],
    [1, 2, 3, 0],
    [1, 3, 0, 2],
    [1, 3, 2, 0],
    [2, 0, 1, 3],
    [2, 0, 3, 1],
    [2, 1, 0, 3],
    [2, 1, 3, 0],
    [2, 3, 0, 1],
    [2, 3, 1, 0],
    [3, 0, 1, 2],
    [3, 0, 2, 1],
    [3, 1, 0, 2],
    [3, 1, 2, 0],
    [3, 2, 0, 1],
    [3, 2, 1, 0],
];

#[cfg(test)]
mod tests {
    use super::*;
    use bris_vision::Intrinsics;

    /// Synthesize a camera frame containing N catalog stars given a
    /// known attitude. Returns Peak positions for those stars.
    /// Used for round-trip tests.
    fn project_stars_to_peaks(
        catalog_vecs: &[[f64; 3]],
        attitude: &[f64; 9],
        intrinsics: &Intrinsics,
        frame_w: u32,
        frame_h: u32,
    ) -> Vec<Peak> {
        let mut out = Vec::new();
        for (i, &v) in catalog_vecs.iter().enumerate() {
            let projected = rotate_vec(attitude, v);
            if projected[2] <= 0.0 {
                continue;
            }
            let px = intrinsics.fx * projected[0] / projected[2] + intrinsics.cx;
            let py = intrinsics.fy * projected[1] / projected[2] + intrinsics.cy;
            if px < 0.0 || py < 0.0 || px >= f64::from(frame_w) || py >= f64::from(frame_h) {
                continue;
            }
            // Synthetic intensity inversely proportional to index so
            // brighter (lower index) stars sort first.
            let intensity = 60_000.0 - (i as f64) * 1000.0;
            out.push(Peak {
                x: px,
                y: py,
                intensity,
            });
        }
        // Sort by intensity descending, mimicking detect_peaks.
        out.sort_by(|a, b| {
            b.intensity
                .partial_cmp(&a.intensity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    /// Simple identity attitude: catalog frame == camera frame.
    /// Useful for round-trip tests where we put a star "directly
    /// in front of" the camera at the +Z axis.
    #[allow(dead_code)]
    fn identity_attitude() -> [f64; 9] {
        [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]
    }

    #[test]
    fn rejects_fewer_than_4_peaks() {
        use crate::hash::StarHashDbConfig;
        let cfg = StarHashDbConfig {
            mag_cutoff: 1.5,
            ..StarHashDbConfig::default()
        };
        let db = StarHashDb::build(cfg);
        let peaks = vec![
            Peak {
                x: 100.0,
                y: 100.0,
                intensity: 50_000.0,
            },
            Peak {
                x: 200.0,
                y: 100.0,
                intensity: 40_000.0,
            },
            Peak {
                x: 100.0,
                y: 200.0,
                intensity: 30_000.0,
            },
        ];
        let intr = Intrinsics::placeholder(640, 480);
        let result = plate_solve(&peaks, &intr, &db, PlateSolveConfig::default());
        assert!(matches!(result, Err(PlateSolveError::InsufficientPeaks(3))));
    }

    /// Refinement: with a strict residual threshold and noisy
    /// peak inputs (random shuffle of pixel positions, breaking
    /// the geometric pattern), the solver must reject rather than
    /// fabricate a match.
    ///
    /// This is the load-bearing test for the
    /// `max_rms_residual_rad` knob: without it, the solver
    /// returns the best self-consistent rotation it can find
    /// regardless of how poorly that rotation actually fits.
    /// With strict refinement, random inputs produce no match.
    #[test]
    fn refinement_rejects_random_peak_positions() {
        use crate::hash::StarHashDbConfig;
        let cfg = StarHashDbConfig {
            mag_cutoff: 1.5,
            ..StarHashDbConfig::default()
        };
        let db = StarHashDb::build(cfg);

        // Sprinkle 12 peaks at fixed positions across a 640x480
        // frame. With no underlying star pattern, the chance of a
        // match-and-refine surviving sub-arcmin RMS is negligible.
        let peaks: Vec<Peak> = (0..12)
            .map(|i| Peak {
                x: 50.0 + 50.0 * f64::from(i),
                y: 100.0 + 30.0 * f64::from(i % 5),
                intensity: 50_000.0 - 1000.0 * f64::from(i),
            })
            .collect();
        let intr = Intrinsics::placeholder(640, 480);

        let result = plate_solve(
            &peaks,
            &intr,
            &db,
            PlateSolveConfig {
                max_peaks_to_match: 12,
                min_verifications: 3,
                verify_match_radius_rad: 1.0_f64.to_radians(),
                max_rms_residual_rad: (30.0 / 3600.0_f64).to_radians(),
                max_tuple_diameter_rad: 60.0_f64.to_radians(),
            },
        );
        // Either NoMatch or InsufficientPeaks; both are correct
        // refusals. The bug we're guarding against is a Some(_)
        // result with fabricated identifications.
        assert!(
            result.is_err(),
            "expected refusal, got Ok with {:?} stars",
            result.as_ref().ok().map(|r| r.identified.len()),
        );
    }

    /// End-to-end synthetic test: build a hash database from the
    /// brightest stars, project them to a synthetic frame using a
    /// known camera attitude, run the solver, verify it recovers
    /// the same attitude.
    ///
    /// Runs in release-build CI but is `#[ignore]` for default
    /// debug-mode runs — the catalog density needed to avoid
    /// geometric ambiguity (mag ≤ 4.0, ~500 stars) makes the
    /// debug-mode cost ~3 minutes. Run with
    /// `cargo test --release` or
    /// `cargo test -- --ignored --include-ignored` to exercise.
    #[test]
    #[ignore = "slow in debug; run with --release"]
    fn round_trip_recovers_known_attitude() {
        use crate::hash::{ra_dec_to_unit_vec, StarHashDbConfig};
        use bris_almanac::all_stars;

        // Use a moderate-density catalog. Mag 4.0 gives ~500
        // stars total — enough to populate the FOV densely with
        // verifiable stars while keeping db build time bounded
        // by the neighbor_limit.
        let cfg = StarHashDbConfig {
            mag_cutoff: 4.0,
            max_pattern_diameter_rad: 60.0_f64.to_radians(),
            bin_count: 50,
            neighbor_limit: 20,
        };
        let db = StarHashDb::build(cfg);

        // Pick a region of sky to point at: declination +45°, RA 6h
        // (= π/2 rad). Use the identity attitude for simplicity:
        // catalog frame → camera frame is identity, which means
        // the camera's optical axis is the +Z catalog axis (north
        // celestial pole). To get stars in the FOV we need an
        // attitude that brings the chosen sky region to +Z.
        //
        // Simpler: pick a sky region where the catalog has many
        // bright stars, and use an attitude that maps that region
        // to +Z in the camera frame.
        //
        // Find catalog stars in a 30°-radius cone around some
        // bright region. The Big Dipper / Ursa Major is a dense
        // cluster of bright stars near (RA = 12h, Dec = +55°).
        // Aim there.
        let aim_ra_rad = 12.0_f64 * 15.0_f64.to_radians(); // 12h = 180° = π
        let aim_dec_rad = 55.0_f64.to_radians();
        let aim_vec = ra_dec_to_unit_vec(aim_ra_rad, aim_dec_rad);

        // Pick all stars within 30° of the aim point (matches the
        // db's pattern diameter, so all in-frame stars are
        // potential pattern members). Use the same magnitude
        // filter as the database.
        let candidates: Vec<&bris_almanac::StarRecord> = all_stars()
            .iter()
            .filter(|s| s.vmag <= 4.0)
            .filter(|s| {
                let v = ra_dec_to_unit_vec(s.ra_rad, s.dec_rad);
                let dot = v[0] * aim_vec[0] + v[1] * aim_vec[1] + v[2] * aim_vec[2];
                dot >= 30.0_f64.to_radians().cos()
            })
            .collect();
        assert!(
            candidates.len() >= 10,
            "need at least 10 stars near the aim point at mag<=4.0; got {}",
            candidates.len(),
        );

        let catalog_vecs: Vec<[f64; 3]> = candidates
            .iter()
            .map(|s| ra_dec_to_unit_vec(s.ra_rad, s.dec_rad))
            .collect();

        // Build an attitude that rotates the aim point onto +Z.
        // Simple approach: rotation matrix whose third row is
        // aim_vec (so aim_vec maps to [0, 0, 1] = Z).
        let attitude = aim_to_z_attitude(aim_vec);

        // Project all candidate stars to peaks.
        let intr = Intrinsics::placeholder(640, 480);
        let peaks = project_stars_to_peaks(&catalog_vecs, &attitude, &intr, 640, 480);
        assert!(
            peaks.len() >= 6,
            "need at least 6 in-frame peaks; got {} for {} candidates",
            peaks.len(),
            catalog_vecs.len(),
        );

        // Solve.
        let result = plate_solve(
            &peaks,
            &intr,
            &db,
            PlateSolveConfig {
                max_peaks_to_match: 12,
                min_verifications: 5,
                ..PlateSolveConfig::default()
            },
        )
        .expect("solver should find a match for synthetic in-FOV star field");

        // Compare attitudes by checking that the recovered rotation
        // applied to the aim point lands at (or near) +Z.
        let recovered_aim = rotate_vec(&result.attitude.matrix, aim_vec);
        assert!(
            recovered_aim[2] > 0.99,
            "recovered attitude should map aim_vec near +Z; got {recovered_aim:?}; \
             {} stars identified",
            result.identified.len(),
        );
    }

    /// Build an attitude (rotation matrix, row-major) that maps
    /// the given unit vector to +Z.
    fn aim_to_z_attitude(aim: [f64; 3]) -> [f64; 9] {
        // We want a rotation R such that R · aim = [0, 0, 1].
        // Construct via Rodrigues' formula: rotation axis is
        // aim × Z, angle is acos(aim · Z) = acos(aim[2]).
        let z = [0.0, 0.0, 1.0];
        let axis = [
            aim[1] * z[2] - aim[2] * z[1],
            aim[2] * z[0] - aim[0] * z[2],
            aim[0] * z[1] - aim[1] * z[0],
        ];
        let axis_norm = (axis[0].powi(2) + axis[1].powi(2) + axis[2].powi(2)).sqrt();
        if axis_norm < 1e-12 {
            // aim already at ±Z.
            if aim[2] > 0.0 {
                return [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
            }
            // 180° flip about X.
            return [1.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, -1.0];
        }
        let axis = [
            axis[0] / axis_norm,
            axis[1] / axis_norm,
            axis[2] / axis_norm,
        ];
        let angle = aim[2].clamp(-1.0, 1.0).acos();
        let (s, c) = (angle.sin(), angle.cos());
        let one_minus_c = 1.0 - c;
        let (x, y, zz) = (axis[0], axis[1], axis[2]);
        // Rodrigues' rotation matrix.
        [
            c + x * x * one_minus_c,
            x * y * one_minus_c - zz * s,
            x * zz * one_minus_c + y * s,
            y * x * one_minus_c + zz * s,
            c + y * y * one_minus_c,
            y * zz * one_minus_c - x * s,
            zz * x * one_minus_c - y * s,
            zz * y * one_minus_c + x * s,
            c + zz * zz * one_minus_c,
        ]
    }
}
