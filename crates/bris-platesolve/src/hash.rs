//! Geometric-hash database for plate solving.
//!
//! Builds and stores a map from quantized star-pattern hash to a
//! list of catalog patterns matching that hash. The hash is
//! computed from the four pairwise-distance ratios of a 4-star
//! pattern, so it's invariant to the camera's focal length (which
//! scales all distances by the same factor) and to translation /
//! rotation in the image plane.

use bris_almanac::{all_stars, StarRecord};
use std::collections::HashMap;

/// Configuration for [`StarHashDb::build`].
#[derive(Debug, Clone, Copy)]
pub struct StarHashDbConfig {
    /// Maximum angular distance between any two stars in a pattern,
    /// radians. Patterns with diameter exceeding this aren't
    /// included. Should match the camera's expected diagonal FOV
    /// or be slightly smaller. Default 60° (~1.047 rad).
    pub max_pattern_diameter_rad: f64,
    /// Magnitude cutoff: only stars with `vmag <= mag_cutoff` are
    /// included. Default 5.5 (naked-eye limit; matches typical
    /// handheld camera at modest exposure).
    pub mag_cutoff: f64,
    /// Number of quantization bins per ratio dimension. The hash
    /// space size is `bin_count^4`. Larger = fewer collisions per
    /// bucket but smaller chance two noisy observations land in
    /// the same bin. Default 50.
    pub bin_count: u16,
    /// Maximum number of nearest-neighbor stars to consider when
    /// enumerating 4-tuple patterns from each anchor star.
    /// Bounding the neighborhood is essential: naïve O(N^4)
    /// enumeration scales catastrophically — 500 stars =
    /// ~2.6 × 10^10 4-tuples. With a neighborhood limit of M,
    /// enumeration is O(N × M^3) per anchor, manageable for
    /// M ≈ 20 even at thousands of stars. Default 20.
    pub neighbor_limit: u16,
}

impl Default for StarHashDbConfig {
    fn default() -> Self {
        Self {
            max_pattern_diameter_rad: 60.0_f64.to_radians(),
            mag_cutoff: 5.5,
            bin_count: 50,
            neighbor_limit: 20,
        }
    }
}

/// One catalog 4-star pattern. Stars are stored sorted by HR id
/// (deterministic ordering for hashing); pattern geometry is
/// orientation-invariant via the ratio-based hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogPattern {
    /// HR ids of the four stars, sorted ascending.
    pub hr_ids: [u32; 4],
}

/// Quantized pattern hash. Two patterns whose pairwise-distance
/// ratios bin to the same 4-tuple have equal hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PatternHash([u16; 4]);

/// One catalog entry as needed by the plate-solver's
/// verification loop. Built once at db construction time so the
/// solver never recomputes unit vectors or rescans the magnitude
/// filter inside its hot path.
#[derive(Debug, Clone, Copy)]
pub struct VerifyStar {
    /// Yale BSC HR id.
    pub hr: u32,
    /// Apparent V magnitude (already known to satisfy `vmag <= mag_cutoff`).
    pub vmag: f64,
    /// J2000 RA, radians.
    pub ra_rad: f64,
    /// J2000 Dec, radians.
    pub dec_rad: f64,
    /// Cached J2000 ICRS unit vector for `(ra_rad, dec_rad)`.
    pub unit_vec: [f64; 3],
}

/// The hash database.
#[derive(Debug)]
pub struct StarHashDb {
    table: HashMap<PatternHash, Vec<CatalogPattern>>,
    cfg: StarHashDbConfig,
    /// Cached unit vectors (J2000 ICRS Cartesian) for every
    /// included star, keyed by HR id. Reused at solve time for
    /// pose computation.
    pub(crate) star_vectors: HashMap<u32, [f64; 3]>,
    /// Flat list of all stars passing the magnitude cutoff,
    /// each with cached unit vector. The plate-solver's
    /// verification loop iterates this directly instead of
    /// re-filtering [`bris_almanac::all_stars`] and re-computing
    /// unit vectors from `(ra, dec)` on every call. With ~1600
    /// stars at mag 5.0 and ~2 billion inner-loop iterations per
    /// `plate_solve` call, this saves the trig overhead that
    /// otherwise dominates wall time.
    pub(crate) verify_stars: Vec<VerifyStar>,
}

impl StarHashDb {
    /// Build the hash database from the embedded star catalog.
    ///
    /// For each "anchor" star, find its `neighbor_limit` nearest
    /// neighbors within `max_pattern_diameter_rad`, then enumerate
    /// 4-tuples consisting of the anchor + 3 chosen neighbors. This
    /// gives `O(N × M^3)` patterns where N is the catalog size
    /// passing the magnitude filter and M is the neighbor limit.
    /// Naïve `O(N^4)` enumeration is intractable at the catalog
    /// densities Bris targets (mag 4.0 = ~500 stars = 2.6e10
    /// 4-tuples).
    ///
    /// The same 4-star pattern is enumerated multiple times (once
    /// per choice of anchor); we deduplicate via the sorted
    /// `hr_ids` so only one entry per pattern lands in the table.
    #[must_use]
    #[allow(clippy::too_many_lines)] // single-purpose builder; splitting hurts readability
    pub fn build(cfg: StarHashDbConfig) -> Self {
        let stars: Vec<&StarRecord> = all_stars()
            .iter()
            .filter(|s| s.vmag <= cfg.mag_cutoff)
            .collect();
        let n = stars.len();

        let mut star_vectors: HashMap<u32, [f64; 3]> = HashMap::with_capacity(n);
        for s in &stars {
            star_vectors.insert(s.hr, ra_dec_to_unit_vec(s.ra_rad, s.dec_rad));
        }

        let cos_max_dist = cfg.max_pattern_diameter_rad.cos();
        let vecs: Vec<[f64; 3]> = stars.iter().map(|s| star_vectors[&s.hr]).collect();

        // For each anchor, find the indices of stars within the
        // diameter cone, sorted ascending by distance, capped at
        // neighbor_limit.
        let mut neighborhoods: Vec<Vec<usize>> = Vec::with_capacity(n);
        for i in 0..n {
            let mut neighbors: Vec<(f64, usize)> = Vec::new();
            for j in 0..n {
                if j == i {
                    continue;
                }
                let dot =
                    vecs[i][0] * vecs[j][0] + vecs[i][1] * vecs[j][1] + vecs[i][2] * vecs[j][2];
                if dot < cos_max_dist {
                    continue;
                }
                neighbors.push((dot, j));
            }
            // Sort by *descending* dot (closest first; dot=1 means
            // colocated).
            neighbors.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            neighbors.truncate(cfg.neighbor_limit as usize);
            neighborhoods.push(neighbors.into_iter().map(|(_, idx)| idx).collect());
        }

        let mut table: HashMap<PatternHash, Vec<CatalogPattern>> = HashMap::new();
        let mut seen_patterns: std::collections::HashSet<[u32; 4]> =
            std::collections::HashSet::new();

        for i in 0..n {
            let nbrs = &neighborhoods[i];
            // Choose 3 distinct neighbors a, b, c from nbrs. Only
            // accept the 4-tuple {i, a, b, c} if all 6 pairwise
            // distances are within the diameter (which is
            // automatically true for pairs involving i since
            // they're in the neighborhood; we only need to verify
            // the 3 pairs among {a, b, c}).
            for ai in 0..nbrs.len() {
                let a = nbrs[ai];
                for bi in (ai + 1)..nbrs.len() {
                    let b = nbrs[bi];
                    let dot_ab =
                        vecs[a][0] * vecs[b][0] + vecs[a][1] * vecs[b][1] + vecs[a][2] * vecs[b][2];
                    if dot_ab < cos_max_dist {
                        continue;
                    }
                    for ci in (bi + 1)..nbrs.len() {
                        let c = nbrs[ci];
                        let dot_ac = vecs[a][0] * vecs[c][0]
                            + vecs[a][1] * vecs[c][1]
                            + vecs[a][2] * vecs[c][2];
                        if dot_ac < cos_max_dist {
                            continue;
                        }
                        let dot_bc = vecs[b][0] * vecs[c][0]
                            + vecs[b][1] * vecs[c][1]
                            + vecs[b][2] * vecs[c][2];
                        if dot_bc < cos_max_dist {
                            continue;
                        }
                        // Build the sorted hr_ids for dedup.
                        let mut hr_ids = [stars[i].hr, stars[a].hr, stars[b].hr, stars[c].hr];
                        hr_ids.sort_unstable();
                        if !seen_patterns.insert(hr_ids) {
                            continue;
                        }
                        // Compute 6 pairwise distances.
                        let dot_ia = vecs[i][0] * vecs[a][0]
                            + vecs[i][1] * vecs[a][1]
                            + vecs[i][2] * vecs[a][2];
                        let dot_ib = vecs[i][0] * vecs[b][0]
                            + vecs[i][1] * vecs[b][1]
                            + vecs[i][2] * vecs[b][2];
                        let dot_ic = vecs[i][0] * vecs[c][0]
                            + vecs[i][1] * vecs[c][1]
                            + vecs[i][2] * vecs[c][2];
                        let dists = [
                            dot_ia.acos(),
                            dot_ib.acos(),
                            dot_ic.acos(),
                            dot_ab.acos(),
                            dot_ac.acos(),
                            dot_bc.acos(),
                        ];
                        let hash = pattern_hash(&dists, cfg.bin_count);
                        table
                            .entry(hash)
                            .or_default()
                            .push(CatalogPattern { hr_ids });
                    }
                }
            }
        }

        tracing::info!(
            patterns = table.values().map(Vec::len).sum::<usize>(),
            unique_hashes = table.len(),
            stars_used = stars.len(),
            "plate solver hash database built",
        );

        Self {
            table,
            cfg,
            star_vectors,
            verify_stars: stars
                .iter()
                .map(|s| VerifyStar {
                    hr: s.hr,
                    vmag: s.vmag,
                    ra_rad: s.ra_rad,
                    dec_rad: s.dec_rad,
                    unit_vec: ra_dec_to_unit_vec(s.ra_rad, s.dec_rad),
                })
                .collect(),
        }
    }

    /// Look up a hash. Returns the (possibly empty) list of
    /// catalog patterns matching the hash, plus also probes
    /// neighboring bins to handle quantization-edge cases. The
    /// neighbor probing inflates the false-positive rate slightly
    /// in exchange for handling near-bin-boundary observations
    /// without a quantization-induced miss.
    #[must_use]
    pub fn lookup(&self, hash: PatternHash) -> Vec<&CatalogPattern> {
        let mut results = Vec::new();
        // Exact bin.
        if let Some(v) = self.table.get(&hash) {
            results.extend(v.iter());
        }
        // ±1 in any single dimension (4 dims × 2 directions = 8 neighbors).
        let mut neighbor_hash = hash;
        for dim in 0..4 {
            for delta in [-1i32, 1] {
                let v = i32::from(hash.0[dim]) + delta;
                if v < 0 || v >= i32::from(self.cfg.bin_count) {
                    continue;
                }
                neighbor_hash.0[dim] = v as u16;
                if let Some(patterns) = self.table.get(&neighbor_hash) {
                    results.extend(patterns.iter());
                }
                neighbor_hash.0[dim] = hash.0[dim];
            }
        }
        results
    }

    /// Number of (hash, pattern) entries in the database.
    #[must_use]
    pub fn pattern_count(&self) -> usize {
        self.table.values().map(Vec::len).sum()
    }

    /// Number of unique hash bins used.
    #[must_use]
    pub fn bin_count_used(&self) -> usize {
        self.table.len()
    }

    /// Configuration the database was built with.
    #[must_use]
    pub fn config(&self) -> StarHashDbConfig {
        self.cfg
    }

    /// Look up a star's J2000 unit vector by HR id.
    #[must_use]
    pub fn star_vector(&self, hr: u32) -> Option<[f64; 3]> {
        self.star_vectors.get(&hr).copied()
    }

    /// All catalog stars passing the magnitude cutoff, each with
    /// cached unit vector. Used by [`crate::plate_solve`]'s
    /// verification loop.
    #[must_use]
    pub fn verify_stars(&self) -> &[VerifyStar] {
        &self.verify_stars
    }
}

/// Compute the quantized hash for an observed (or catalog)
/// 4-star pattern from its 6 pairwise distances.
///
/// The distances are sorted ascending; the largest is used to
/// normalize the other 5. The 4 ratios after normalization
/// (5 distances / largest, dropping the trailing 1.0) become the
/// hash key after quantization to `bin_count` bins per dimension.
#[must_use]
pub fn pattern_hash(distances_in: &[f64; 6], bin_count: u16) -> PatternHash {
    let mut dists = *distances_in;
    dists.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let max_d = dists[5];
    if max_d <= 0.0 || !max_d.is_finite() {
        return PatternHash([0; 4]);
    }
    let scale = f64::from(bin_count);
    let mut bins = [0u16; 4];
    for (i, &d) in dists[0..4].iter().enumerate() {
        let ratio = (d / max_d).clamp(0.0, 1.0);
        let bin = (ratio * scale).floor() as u32;
        bins[i] = bin.min(u32::from(bin_count) - 1) as u16;
    }
    PatternHash(bins)
}

impl PatternHash {
    /// Internal accessor for tests + neighbor enumeration.
    #[must_use]
    pub fn bins(self) -> [u16; 4] {
        self.0
    }
}

/// J2000 RA/Dec to unit Cartesian in the ICRS frame.
#[must_use]
pub fn ra_dec_to_unit_vec(ra_rad: f64, dec_rad: f64) -> [f64; 3] {
    let cd = dec_rad.cos();
    [cd * ra_rad.cos(), cd * ra_rad.sin(), dec_rad.sin()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn unit_vec_at_pole_is_z() {
        let v = ra_dec_to_unit_vec(0.0, std::f64::consts::FRAC_PI_2);
        assert_relative_eq!(v[0], 0.0, epsilon = 1e-12);
        assert_relative_eq!(v[1], 0.0, epsilon = 1e-12);
        assert_relative_eq!(v[2], 1.0, epsilon = 1e-12);
    }

    #[test]
    fn unit_vec_at_equator_zero_ra_is_x() {
        let v = ra_dec_to_unit_vec(0.0, 0.0);
        assert_relative_eq!(v[0], 1.0, epsilon = 1e-12);
        assert_relative_eq!(v[1], 0.0, epsilon = 1e-12);
        assert_relative_eq!(v[2], 0.0, epsilon = 1e-12);
    }

    #[test]
    fn pattern_hash_invariant_under_uniform_scale() {
        // Same geometry, different overall scale -> same hash.
        let d1 = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let d2 = [0.2, 0.4, 0.6, 0.8, 1.0, 1.2];
        let h1 = pattern_hash(&d1, 50);
        let h2 = pattern_hash(&d2, 50);
        assert_eq!(h1, h2);
    }

    #[test]
    fn pattern_hash_invariant_under_permutation() {
        let d1 = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let d2 = [0.6, 0.5, 0.4, 0.3, 0.2, 0.1];
        assert_eq!(pattern_hash(&d1, 50), pattern_hash(&d2, 50));
    }

    #[test]
    fn pattern_hash_distinguishes_different_geometry() {
        let d1 = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        // Different ratio profile.
        let d2 = [0.1, 0.1, 0.1, 0.5, 0.5, 0.6];
        assert_ne!(pattern_hash(&d1, 50), pattern_hash(&d2, 50));
    }

    #[test]
    fn pattern_hash_handles_degenerate() {
        let d = [0.0; 6];
        let h = pattern_hash(&d, 50);
        // All-zero distances → hash bins all 0; documented behavior.
        assert_eq!(h.bins(), [0, 0, 0, 0]);
    }

    #[test]
    fn build_db_with_high_mag_cutoff_yields_few_stars() {
        // Bright stars only — very small N keeps the test fast.
        let cfg = StarHashDbConfig {
            mag_cutoff: 1.5,
            ..StarHashDbConfig::default()
        };
        let db = StarHashDb::build(cfg);
        // At mag ≤ 1.5 there are ~22 stars; some 4-tuples will fit.
        // We don't pin the exact count (catalog-version-dependent)
        // but assert the db is non-empty and self-consistent.
        assert!(db.pattern_count() > 0, "expected some patterns");
        assert_relative_eq!(db.config().mag_cutoff, 1.5);
    }

    #[test]
    fn lookup_returns_inserted_pattern() {
        let cfg = StarHashDbConfig {
            mag_cutoff: 1.5,
            ..StarHashDbConfig::default()
        };
        let db = StarHashDb::build(cfg);
        // Sample one entry, look it up, expect to find it.
        let (hash, patterns) = db.table.iter().next().expect("nonempty db");
        let found = db.lookup(*hash);
        assert!(
            found.contains(&&patterns[0]),
            "lookup should return the pattern stored at its hash"
        );
    }
}
