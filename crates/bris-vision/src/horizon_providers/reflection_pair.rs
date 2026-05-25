//! Auto-detected horizon from reflection pairs.
//!
//! Given two body candidates that are (geometrically and
//! photometrically) consistent with one being the direct image
//! of a celestial body and the other its reflection in a
//! locally-horizontal surface, the local vertical can be
//! recovered as the bisector of the two camera rays. See
//! `docs/design/horizon_autodetect.md` §3 for the algorithm and
//! §10 for the Phase 1 decisions.
//!
//! Phase 1 scope: intra-frame only, Night mode (≥ 2 body
//! candidates required), no Day-mode multi-centroid support.

use bris_core::{Sigma, Uncertain};

use crate::ray::{bisector_normal, horizon_line_from_normal, BodyRay, CameraRay};

use super::{
    BodyCandidate, DirectSight, HorizonHypothesis, HorizonProvenance, HorizonProvider,
    HorizonProviderContext, TemporalScope,
};

/// Configuration for [`ReflectionPairProvider`].
#[derive(Debug, Clone, Copy)]
pub struct ReflectionPairConfig {
    /// k-σ tolerance for Test 3 (catalog consistency). Applied
    /// only when both a `PositionPrior` and an identified body
    /// (i.e. `BodyCandidate::predicted_altitude` populated)
    /// are available.
    pub catalog_tolerance_sigma: f64,
    /// k-σ tolerance for Test 4 (multi-pair agreement). Two
    /// pairs are clustered together if their inferred gravity
    /// vectors agree within `multi_pair_tolerance_sigma` times
    /// the quadrature-combined per-pair angular σ.
    pub multi_pair_tolerance_sigma: f64,
    /// Maximum angle (radians) by which the pair plane (the
    /// plane containing the two rays) may deviate from
    /// vertical before Test 1 rejects it. Default 0.05 rad
    /// (~3°).
    pub max_bisector_horizontal_rad: f64,
    /// Cold-start: minimum concordant pairs required when no
    /// position prior is available (drops Test 3). With a
    /// prior, two concordant pairs (or one passing Test 3)
    /// suffice.
    pub cold_start_min_pairs: usize,
    /// Floor on the synthesized horizon altitude σ (radians).
    pub sigma_floor_rad: f64,
    /// Brightness-ratio tolerance for Test 2: a reflection
    /// passes if `brightness_dn ≤ brightness_up · (1 + tol)`.
    pub photometric_tolerance: f64,
}

impl Default for ReflectionPairConfig {
    fn default() -> Self {
        Self {
            catalog_tolerance_sigma: 4.0,
            multi_pair_tolerance_sigma: 3.0,
            max_bisector_horizontal_rad: 0.05,
            cold_start_min_pairs: 3,
            sigma_floor_rad: 1e-4,
            photometric_tolerance: 0.10,
        }
    }
}

/// Reflection-pair horizon provider.
///
/// Implements the five tests of
/// `docs/design/horizon_autodetect.md` §3.2: Test 1 geometric,
/// Test 2 photometric, Test 3 catalog consistency (when a
/// position prior + identified body are available), Test 4
/// multi-pair agreement, Test 5 reflector-region (deferred,
/// flagged in code).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReflectionPairProvider {
    /// Tunables for the five tests.
    pub config: ReflectionPairConfig,
}

/// One pair that has passed Tests 1–3.
#[derive(Debug, Clone, Copy)]
struct PairOutcome {
    /// Index of the brighter (direct) candidate in the input
    /// slice.
    up_idx: usize,
    /// Inferred gravity direction in camera frame (downward,
    /// unit vector).
    gravity: CameraRay,
    /// Direct sight: half the angle between the two rays.
    half_angle: Uncertain<f64>,
    /// Per-pair angular σ on `gravity` (radians).
    sigma_rad: f64,
}

impl HorizonProvider for ReflectionPairProvider {
    fn name(&self) -> &'static str {
        "reflection_pair"
    }

    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }

    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        // Phase 1: Night-mode only. Day produces a single
        // centroid; Day-mode reflection-pair detection requires
        // Stage B to yield ≥ 2 candidates and is deferred to
        // Phase 2 per the handoff.
        if ctx.body_candidates.len() < 2 {
            return None;
        }

        // Test 1 + Test 2 + Test 3: build per-pair outcomes.
        let mut outcomes: Vec<PairOutcome> = Vec::new();
        for (i, a) in ctx.body_candidates.iter().enumerate() {
            for (j, b) in ctx.body_candidates.iter().enumerate().skip(i + 1) {
                // Order: up = brighter.
                let (up_idx, up, dn) = if a.brightness >= b.brightness {
                    (i, a, b)
                } else {
                    (j, b, a)
                };
                if let Some(outcome) = self.evaluate_pair(ctx, up_idx, up, dn) {
                    outcomes.push(outcome);
                }
            }
        }

        if outcomes.is_empty() {
            return None;
        }

        // Test 4: multi-pair agreement. Greedy clustering by
        // angular distance scaled by combined σ.
        let cluster = self.largest_cluster(&outcomes)?;
        let has_prior = ctx.position_prior.is_some();
        let min_required = if has_prior {
            // With a prior, a single pair that passed Test 3
            // (catalog) is enough; otherwise two concordant
            // pairs suffice (Test 4).
            if cluster.iter().any(|p| {
                // Re-check whether this pair had a catalog-
                // applicable identification; if so the pair
                // would have been gated by Test 3 already
                // and a singleton cluster is admissible.
                ctx.body_candidates
                    .get(p.up_idx)
                    .and_then(|c| c.predicted_altitude)
                    .is_some()
            }) {
                1
            } else {
                2
            }
        } else {
            self.config.cold_start_min_pairs
        };
        if cluster.len() < min_required {
            return None;
        }

        // Test 5 (reflector region): deferred for Phase 1 — see
        // `docs/design/horizon_autodetect.md` §3.2 (Test 5).

        // Aggregate the cluster: mean gravity direction,
        // empirical-spread σ (RMS small-angle deviation from
        // the mean), floored by config.
        let (gravity_mean, empirical_sigma) = aggregate_cluster(&cluster);
        let sigma_value = empirical_sigma
            .max(self.config.sigma_floor_rad)
            .max(min_per_pair_sigma(&cluster));
        let altitude_sigma = Sigma::new(sigma_value).unwrap_or(Sigma::ZERO);
        // Sky-pointing normal = -gravity (gravity points down,
        // sky points up).
        let sky_normal = CameraRay {
            x: -gravity_mean.x,
            y: -gravity_mean.y,
            z: -gravity_mean.z,
        };
        let line = horizon_line_from_normal(&sky_normal, ctx.intrinsics, altitude_sigma)?;

        // Direct sight: pick the first surviving pair (the
        // representative). Per the handoff, Phase 1 emits one
        // sight per frame from the reflection pair (matching
        // the existing optical path's one-sight-per-body-per-
        // frame semantics; bris-nav de-dupes per-body sights
        // in a window).
        let representative = cluster[0];
        let body = &ctx.body_candidates[representative.up_idx];
        let direct_sight = DirectSight {
            body_pixel: body.pixel,
            observed_altitude: representative.half_angle,
        };

        Some(HorizonHypothesis {
            line,
            provenance: HorizonProvenance::ReflectionPair {
                pair_count: cluster.len(),
                used_position_prior: has_prior,
            },
            direct_sight: Some(direct_sight),
        })
    }
}

impl ReflectionPairProvider {
    /// Evaluate one ordered pair `(up, dn)` against Tests 1–3.
    /// Returns `Some(PairOutcome)` when all applicable tests
    /// pass; `None` on rejection.
    fn evaluate_pair(
        &self,
        ctx: &HorizonProviderContext<'_>,
        up_idx: usize,
        up: &BodyCandidate,
        dn: &BodyCandidate,
    ) -> Option<PairOutcome> {
        // Test 2 (photometric): reflection ≤ direct (within tol).
        // Run first because it's the cheapest filter.
        if dn.brightness > up.brightness * (1.0 + self.config.photometric_tolerance) {
            return None;
        }

        // Build camera rays.
        let up_sigma = Sigma::new(up.position_sigma_px.max(f64::EPSILON)).ok()?;
        let dn_sigma = Sigma::new(dn.position_sigma_px.max(f64::EPSILON)).ok()?;
        let up_ray = BodyRay::from_pixel(ctx.intrinsics, up.pixel.0, up.pixel.1, up_sigma);
        let dn_ray = BodyRay::from_pixel(ctx.intrinsics, dn.pixel.0, dn.pixel.1, dn_sigma);

        // Test 1 (geometric):
        //  - The bisector of the two rays points toward the
        //    horizon plane (in the symmetric case, parallel
        //    to the optical axis); its z component must be
        //    positive so the camera looks forward.
        //  - Gravity is the chord `dn - up`, normalised: for
        //    a body at altitude +α above the horizon and its
        //    reflection at -α, the chord points image-down
        //    along the local vertical. Magnitude scales with
        //    2·sin(α).
        //  - The pair plane (the plane containing the two
        //    rays) must be near-vertical, i.e. its normal
        //    `r_up × r_dn` is near-horizontal in image space
        //    (small y component relative to its norm).
        let bisector = bisector_normal(&up_ray.ray, &dn_ray.ray)?;
        if bisector.z <= 0.0 {
            return None;
        }
        let chord = CameraRay {
            x: dn_ray.ray.x - up_ray.ray.x,
            y: dn_ray.ray.y - up_ray.ray.y,
            z: dn_ray.ray.z - up_ray.ray.z,
        };
        let gravity = chord.normalize()?;
        if gravity.y <= 0.0 {
            return None;
        }
        // Pair-plane verticality.
        let cross = up_ray.ray.cross(&dn_ray.ray);
        let cross_norm = cross.norm();
        if cross_norm < f64::EPSILON {
            return None;
        }
        let horiz_offset = (cross.y / cross_norm).abs();
        if horiz_offset > self.config.max_bisector_horizontal_rad.sin() {
            return None;
        }

        // Angle θ between the two rays; direct sight = θ/2.
        let cos_theta = up_ray.ray.dot(&dn_ray.ray).clamp(-1.0, 1.0);
        let theta = cos_theta.acos();
        let half_angle_rad = 0.5 * theta;
        // Per-pair angular σ: quadrature combination of the two
        // body ray angular σ values (each a small-angle
        // approximation: pixel σ / f_eff). Documented in
        // `BodyRay::from_pixel`.
        let pair_sigma = (up_ray.direction_sigma.value().powi(2)
            + dn_ray.direction_sigma.value().powi(2))
        .sqrt();
        let half_angle_sigma = Sigma::new(0.5 * pair_sigma).unwrap_or(Sigma::ZERO);
        let half_angle = Uncertain::new(half_angle_rad, half_angle_sigma);

        // Test 3 (catalog consistency, optional). Applied only
        // when both a position prior and an identified body
        // (predicted altitude on the brighter candidate) are
        // available. Skipped silently otherwise.
        if let (Some(_prior), Some(pred)) = (ctx.position_prior, up.predicted_altitude) {
            let combined_sigma =
                (pred.sigma.value().powi(2) + half_angle_sigma.value().powi(2)).sqrt();
            let diff = (half_angle_rad - pred.value).abs();
            if diff > self.config.catalog_tolerance_sigma * combined_sigma {
                return None;
            }
        }

        Some(PairOutcome {
            up_idx,
            gravity,
            half_angle,
            sigma_rad: pair_sigma,
        })
    }

    /// Greedy clustering: pick the largest cluster of pairs
    /// whose gravity vectors all lie within
    /// `multi_pair_tolerance_sigma · σ_combined` of each
    /// other.
    fn largest_cluster(&self, outcomes: &[PairOutcome]) -> Option<Vec<PairOutcome>> {
        let mut best: Vec<PairOutcome> = Vec::new();
        for seed in outcomes {
            let mut cluster: Vec<PairOutcome> = vec![*seed];
            for other in outcomes {
                if std::ptr::eq(seed, other) {
                    continue;
                }
                let combined_sigma = (seed.sigma_rad.powi(2) + other.sigma_rad.powi(2))
                    .sqrt()
                    .max(f64::EPSILON);
                let cos = seed.gravity.dot(&other.gravity).clamp(-1.0, 1.0);
                let angle = cos.acos();
                if angle <= self.config.multi_pair_tolerance_sigma * combined_sigma {
                    cluster.push(*other);
                }
            }
            if cluster.len() > best.len() {
                best = cluster;
            }
        }
        if best.is_empty() {
            None
        } else {
            Some(best)
        }
    }
}

/// Aggregate a cluster of pair outcomes into a mean gravity
/// direction and an empirical-spread σ (radians).
fn aggregate_cluster(cluster: &[PairOutcome]) -> (CameraRay, f64) {
    debug_assert!(!cluster.is_empty());
    let mut sx = 0.0_f64;
    let mut sy = 0.0_f64;
    let mut sz = 0.0_f64;
    for p in cluster {
        sx += p.gravity.x;
        sy += p.gravity.y;
        sz += p.gravity.z;
    }
    let mean = CameraRay {
        x: sx,
        y: sy,
        z: sz,
    }
    .normalize()
    .unwrap_or(cluster[0].gravity);
    if cluster.len() == 1 {
        return (mean, 0.0);
    }
    let mut sum_sq = 0.0_f64;
    for p in cluster {
        let cos = mean.dot(&p.gravity).clamp(-1.0, 1.0);
        let angle = cos.acos();
        sum_sq += angle * angle;
    }
    #[allow(clippy::cast_precision_loss)]
    let rms = (sum_sq / cluster.len() as f64).sqrt();
    (mean, rms)
}

/// Smallest per-pair σ in the cluster. The synthesized horizon
/// σ floors at this value: the horizon can never be tighter
/// than the best single pair's angular σ.
fn min_per_pair_sigma(cluster: &[PairOutcome]) -> f64 {
    cluster
        .iter()
        .map(|p| 0.5 * p.sigma_rad)
        .fold(f64::INFINITY, f64::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, Intrinsics};
    use crate::lens::project_pinhole;
    use crate::ray::CameraRay;
    use bris_core::time::{Tt, JD_J2000};

    fn intr() -> Intrinsics {
        Intrinsics::placeholder(1280, 720)
    }

    /// Build a synthetic frame stamped at J2000.
    fn frame() -> Frame {
        Frame::new(
            32,
            32,
            vec![0_u16; 32 * 32],
            Tt::from_julian_date(JD_J2000),
            1000,
            intr(),
        )
        .unwrap()
    }

    /// Project a ray direction (camera frame) to pixel coords
    /// via the pinhole model. The placeholder intrinsics have
    /// zero distortion so the inverse of
    /// `pixel_ray_direction` for a forward ray (z > 0) is the
    /// raw pinhole projection.
    fn pixel_for_ray(intrinsics: &Intrinsics, ray: &CameraRay) -> (f64, f64) {
        // project_pinhole returns (px, py) in pixels.
        let (px, py) = project_pinhole(*intrinsics, ray.x / ray.z, ray.y / ray.z);
        (px, py)
    }

    /// Construct two body candidates whose camera rays bisect
    /// to gravity = (0, 1, 0) (straight down), i.e. the
    /// horizon plane is perpendicular to image-down.
    ///
    /// The body candidates are placed symmetrically above and
    /// below the optical axis at altitude `alt` (radians).
    /// Brightness: up=2.0, dn=1.0 (passes Test 2).
    fn symmetric_pair(alt: f64, position_sigma_px: f64) -> (BodyCandidate, BodyCandidate) {
        let intrinsics = intr();
        let up_ray = CameraRay::from_unit_components(0.0, -alt.sin(), alt.cos());
        let dn_ray = CameraRay::from_unit_components(0.0, alt.sin(), alt.cos());
        let up_px = pixel_for_ray(&intrinsics, &up_ray);
        let dn_px = pixel_for_ray(&intrinsics, &dn_ray);
        (
            BodyCandidate {
                pixel: up_px,
                brightness: 2.0,
                position_sigma_px,
                predicted_altitude: None,
            },
            BodyCandidate {
                pixel: dn_px,
                brightness: 1.0,
                position_sigma_px,
                predicted_altitude: None,
            },
        )
    }

    fn ctx_for<'a>(
        f: &'a Frame,
        intrinsics: &'a Intrinsics,
        cands: &'a [BodyCandidate],
        prior: Option<crate::PositionPrior>,
    ) -> HorizonProviderContext<'a> {
        HorizonProviderContext {
            frame: f,
            intrinsics,
            body_candidates: cands,
            position_prior: prior,
            timestamp: Tt::from_julian_date(JD_J2000),
        }
    }

    #[test]
    fn synthetic_clean_pair_yields_expected_horizon() {
        // Cold-start (no prior) requires ≥ 3 concordant pairs.
        // Build three near-identical pairs by offsetting the
        // altitude very slightly so each pair survives Test 1
        // and clusters together under Test 4.
        let f = frame();
        let i = intr();
        let alts = [0.20_f64, 0.21, 0.22];
        let mut cands: Vec<BodyCandidate> = Vec::new();
        for (k, alt) in alts.iter().enumerate() {
            let (up, dn) = symmetric_pair(*alt, 0.5);
            // Stash a tiny x offset per pair so the rays
            // aren't degenerate when crossed pairwise.
            #[allow(clippy::cast_precision_loss)]
            let x_off = k as f64 * 5.0;
            cands.push(BodyCandidate {
                pixel: (up.pixel.0 + x_off, up.pixel.1),
                ..up
            });
            cands.push(BodyCandidate {
                pixel: (dn.pixel.0 + x_off, dn.pixel.1),
                ..dn
            });
        }
        // Each pair (up_k, dn_k) shares x: bisector is vertical.
        // Cross-pairs (up_j, dn_k for j ≠ k) won't bisect to
        // perfect vertical but also won't be vertical-plane;
        // they'll be discarded by Test 1.
        let provider = ReflectionPairProvider::default();
        let ctx = ctx_for(&f, &i, &cands, None);
        let hyp = provider.detect(&ctx).expect("clean pair should detect");
        // Horizon line should be near-horizontal at ≈ cy.
        assert!(hyp.line.slope.abs() < 1e-6, "slope = {}", hyp.line.slope);
        assert!(
            (hyp.line.intercept - i.cy).abs() < 1.0,
            "intercept = {}, cy = {}",
            hyp.line.intercept,
            i.cy
        );
        // Direct sight: half-angle ≈ alts[0] for the first
        // cluster member.
        let sight = hyp.direct_sight.expect("direct sight must be emitted");
        assert!(
            (sight.observed_altitude.value - 0.20).abs() < 1e-3,
            "direct sight Ho = {}, expected ≈ 0.20",
            sight.observed_altitude.value,
        );
        assert!(matches!(
            hyp.provenance,
            HorizonProvenance::ReflectionPair {
                used_position_prior: false,
                ..
            }
        ));
    }

    #[test]
    fn non_vertical_bisector_rejected() {
        // Build a pair whose two rays both lie at altitude
        // 0.2 but offset in x — the bisector lies in a plane
        // that's *not* vertical. Test 1 should reject.
        let f = frame();
        let i = intr();
        let up_ray = CameraRay::from_unit_components(0.1, -0.2_f64.sin(), 0.2_f64.cos())
            .normalize()
            .unwrap();
        let dn_ray = CameraRay::from_unit_components(-0.1, 0.2_f64.sin(), 0.2_f64.cos())
            .normalize()
            .unwrap();
        let up_px = pixel_for_ray(&i, &up_ray);
        let dn_px = pixel_for_ray(&i, &dn_ray);
        let cands = vec![
            BodyCandidate {
                pixel: up_px,
                brightness: 2.0,
                position_sigma_px: 0.5,
                predicted_altitude: None,
            },
            BodyCandidate {
                pixel: dn_px,
                brightness: 1.0,
                position_sigma_px: 0.5,
                predicted_altitude: None,
            },
        ];
        let provider = ReflectionPairProvider {
            config: ReflectionPairConfig {
                max_bisector_horizontal_rad: 0.01,
                ..ReflectionPairConfig::default()
            },
        };
        let ctx = ctx_for(&f, &i, &cands, None);
        assert!(provider.detect(&ctx).is_none());
    }

    #[test]
    fn reflection_brighter_rejected() {
        // Same geometry as a clean pair but with `dn` brighter
        // than `up`. Test 2 must reject.
        let f = frame();
        let i = intr();
        let (mut up, mut dn) = symmetric_pair(0.2, 0.5);
        up.brightness = 1.0;
        dn.brightness = 2.0; // reflection brighter — impossible
        let cands = vec![up, dn];
        let provider = ReflectionPairProvider::default();
        let ctx = ctx_for(&f, &i, &cands, None);
        assert!(provider.detect(&ctx).is_none());
    }

    #[test]
    fn catalog_consistent_pair_with_prior_accepted() {
        // One pair, predicted altitude matches θ/2 within
        // tolerance, prior present → single concordant pair
        // is enough.
        let f = frame();
        let i = intr();
        let (mut up, dn) = symmetric_pair(0.2, 0.5);
        up.predicted_altitude = Some(Uncertain::new(0.20, Sigma::new(1e-4).unwrap()));
        let cands = vec![up, dn];
        let prior = crate::PositionPrior {
            lat_rad: 0.0,
            lon_rad: 0.0,
            sigma_position_m: 1000.0,
            timestamp: Tt::from_julian_date(JD_J2000),
        };
        let provider = ReflectionPairProvider::default();
        let ctx = ctx_for(&f, &i, &cands, Some(prior));
        let hyp = provider
            .detect(&ctx)
            .expect("with prior + catalog match, one pair suffices");
        assert!(matches!(
            hyp.provenance,
            HorizonProvenance::ReflectionPair {
                used_position_prior: true,
                ..
            }
        ));
    }

    #[test]
    fn catalog_inconsistent_pair_with_prior_rejected() {
        // Predicted altitude wildly off → Test 3 rejects.
        let f = frame();
        let i = intr();
        let (mut up, dn) = symmetric_pair(0.2, 0.5);
        up.predicted_altitude = Some(Uncertain::new(0.50, Sigma::new(1e-4).unwrap()));
        let cands = vec![up, dn];
        let prior = crate::PositionPrior {
            lat_rad: 0.0,
            lon_rad: 0.0,
            sigma_position_m: 1000.0,
            timestamp: Tt::from_julian_date(JD_J2000),
        };
        let provider = ReflectionPairProvider::default();
        let ctx = ctx_for(&f, &i, &cands, Some(prior));
        assert!(provider.detect(&ctx).is_none());
    }

    #[test]
    fn cold_start_single_pair_rejected_three_concordant_accepted() {
        let f = frame();
        let i = intr();
        // One pair, no prior → reject (cold-start needs ≥ 3).
        let (up1, dn1) = symmetric_pair(0.2, 0.5);
        let cands_single = vec![up1, dn1];
        let provider = ReflectionPairProvider::default();
        let ctx1 = ctx_for(&f, &i, &cands_single, None);
        assert!(
            provider.detect(&ctx1).is_none(),
            "cold-start: 1 pair rejected"
        );

        // Three concordant pairs (slightly varied altitudes,
        // distinct x offsets so cross-pairs fail Test 1) →
        // accept. Reuses the `synthetic_clean_pair_yields_expected_horizon`
        // construction.
        let alts = [0.20_f64, 0.21, 0.22];
        let mut cands_three: Vec<BodyCandidate> = Vec::new();
        for (k, alt) in alts.iter().enumerate() {
            let (up, dn) = symmetric_pair(*alt, 0.5);
            #[allow(clippy::cast_precision_loss)]
            let x_off = k as f64 * 5.0;
            cands_three.push(BodyCandidate {
                pixel: (up.pixel.0 + x_off, up.pixel.1),
                ..up
            });
            cands_three.push(BodyCandidate {
                pixel: (dn.pixel.0 + x_off, dn.pixel.1),
                ..dn
            });
        }
        let ctx3 = ctx_for(&f, &i, &cands_three, None);
        assert!(
            provider.detect(&ctx3).is_some(),
            "cold-start: 3 pairs accepted"
        );
    }

    #[test]
    fn sigma_propagation_respects_floor() {
        // Three concordant pairs; the resulting horizon σ must
        // be at least the configured `sigma_floor_rad`.
        let f = frame();
        let i = intr();
        let alts = [0.20_f64, 0.21, 0.22];
        let mut cands: Vec<BodyCandidate> = Vec::new();
        for (k, alt) in alts.iter().enumerate() {
            let (up, dn) = symmetric_pair(*alt, 0.5);
            #[allow(clippy::cast_precision_loss)]
            let x_off = k as f64 * 5.0;
            cands.push(BodyCandidate {
                pixel: (up.pixel.0 + x_off, up.pixel.1),
                ..up
            });
            cands.push(BodyCandidate {
                pixel: (dn.pixel.0 + x_off, dn.pixel.1),
                ..dn
            });
        }
        let provider = ReflectionPairProvider {
            config: ReflectionPairConfig {
                sigma_floor_rad: 5e-3,
                ..ReflectionPairConfig::default()
            },
        };
        let ctx = ctx_for(&f, &i, &cands, None);
        let hyp = provider.detect(&ctx).expect("three pairs should detect");
        assert!(
            hyp.line.altitude_sigma.value() >= 5e-3 - 1e-12,
            "altitude_sigma {} below floor 5e-3",
            hyp.line.altitude_sigma.value()
        );
    }

    #[test]
    fn day_mode_single_candidate_returns_none() {
        // Single body candidate → provider declines (Phase 1
        // scope boundary: Day-mode requires Stage B multi-
        // centroid support, deferred to Phase 2).
        let f = frame();
        let i = intr();
        let cands = vec![BodyCandidate {
            pixel: (100.0, 100.0),
            brightness: 1.0,
            position_sigma_px: 0.5,
            predicted_altitude: None,
        }];
        let provider = ReflectionPairProvider::default();
        let ctx = ctx_for(&f, &i, &cands, None);
        assert!(provider.detect(&ctx).is_none());
    }
}
