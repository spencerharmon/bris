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

/// Per-test rejection counters returned by
/// [`ReflectionPairProvider::detect_with_stats`].
///
/// Cumulative within a single invocation: a `detect` call that
/// evaluates N pairs may increment each `rejected_*` field up
/// to N times. The streaming engine sums these per-frame
/// counters into its long-running
/// [`EngineDiagnostics`][bris_streaming_diag] surface
/// (where `bris_streaming_diag` is `bris_streaming::EngineDiagnostics`
/// — the doc link is via the prose to keep this crate free of
/// a `bris-streaming` dependency).
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_field_names)]
pub struct ReflectionPairStats {
    /// Pair rejected by Test 1 (geometric: bisector not
    /// forward, gravity not image-down, pair plane not
    /// vertical).
    pub rejected_geometric: u64,
    /// Pair rejected by Test 2 (photometric: reflection
    /// brighter than direct).
    pub rejected_photometric: u64,
    /// Pair rejected by Test 3 (catalog: predicted altitude
    /// inconsistent with θ/2).
    pub rejected_catalog: u64,
    /// Attempt produced ≥ 1 surviving pair but no cluster met
    /// the minimum-size threshold (Test 4 / cold-start gate).
    pub rejected_no_cluster: u64,
}

/// One pair that has passed Tests 1–3.
#[derive(Debug, Clone, Copy)]
struct PairOutcome {
    /// Index of the geometrically-upper (image-up: smaller
    /// `pixel.y`) candidate in the input slice. The direct
    /// body's image position is at the top in a reflection
    /// pair (sky-direct above, reflection below).
    up_idx: usize,
    /// Inferred gravity direction in camera frame (downward,
    /// unit vector).
    gravity: CameraRay,
    /// Direct sight: half the angle between the two rays.
    half_angle: Uncertain<f64>,
    /// Per-pair angular σ between the two rays (radians).
    /// Per-pair gravity-direction σ is `0.5 * sigma_rad` (a
    /// small-angle perturbation of either ray rotates the
    /// bisector by half that perturbation).
    sigma_rad: f64,
    /// Whether Test 3 (catalog consistency) was applied and
    /// passed for this pair. `false` when Test 3 was skipped
    /// (no prior or no identified body). Used to gate the
    /// cluster-size requirement: a singleton cluster is
    /// admissible only when ≥ 1 of its pairs passed Test 3.
    passed_test_3: bool,
}

impl HorizonProvider for ReflectionPairProvider {
    fn name(&self) -> &'static str {
        "reflection_pair"
    }

    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }

    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        let mut stats = ReflectionPairStats::default();
        self.detect_with_stats(ctx, &mut stats)
    }
}

impl ReflectionPairProvider {
    /// Same as [`HorizonProvider::detect`] but populates a
    /// per-invocation [`ReflectionPairStats`] so the streaming
    /// engine can fold the counters into its long-running
    /// `EngineDiagnostics`. Used by `bris-streaming`; the
    /// trait-method `detect` just wraps this with a discarded
    /// stats buffer.
    pub fn detect_with_stats(
        &self,
        ctx: &HorizonProviderContext<'_>,
        stats: &mut ReflectionPairStats,
    ) -> Option<HorizonHypothesis> {
        // Phase 1: Night-mode only. Day produces a single
        // centroid; Day-mode reflection-pair detection requires
        // Stage B to yield ≥ 2 candidates and is deferred to
        // Phase 2 per the handoff.
        if ctx.body_candidates.len() < 2 {
            return None;
        }

        // Test 1 + Test 2 + Test 3: build per-pair outcomes.
        // Ordering: `up` is the image-geometrically-upper
        // candidate (smaller `pixel.y` in top-left-origin
        // pixel space). Test 2 then asserts photometrically
        // that the upper one is at least as bright (within
        // tolerance) — flipping the order here would make
        // Test 2 unreachable.
        let mut outcomes: Vec<PairOutcome> = Vec::new();
        for (i, a) in ctx.body_candidates.iter().enumerate() {
            for (j, b) in ctx.body_candidates.iter().enumerate().skip(i + 1) {
                let (up_idx, up, dn) = if a.pixel.1 <= b.pixel.1 {
                    (i, a, b)
                } else {
                    (j, b, a)
                };
                if let Some(outcome) = self.evaluate_pair(ctx, up_idx, up, dn, stats) {
                    outcomes.push(outcome);
                }
            }
        }

        if outcomes.is_empty() {
            return None;
        }

        // Test 4: multi-pair agreement. Greedy clustering by
        // angular distance scaled by combined σ.
        let Some(cluster) = self.largest_cluster(&outcomes) else {
            stats.rejected_no_cluster += 1;
            return None;
        };
        let has_prior = ctx.position_prior.is_some();
        // A singleton cluster is admissible only when at least
        // one of its pairs actually *passed* Test 3 (catalog
        // consistency). A prior that exists but no pair could
        // apply Test 3 against (no identified body in the
        // pair) is treated like cold start — the prior alone
        // does not loosen the Test 4 threshold. See
        // `docs/design/horizon_autodetect.md` §10.
        let any_passed_test_3 = cluster.iter().any(|p| p.passed_test_3);
        let min_required = if any_passed_test_3 {
            1
        } else {
            self.config.cold_start_min_pairs
        };
        if cluster.len() < min_required {
            stats.rejected_no_cluster += 1;
            return None;
        }

        // Test 5 (reflector region): deferred for Phase 1 — see
        // `docs/design/horizon_autodetect.md` §3.2 (Test 5).

        // Aggregate the cluster: mean gravity direction,
        // empirical-spread σ on gravity (RMS small-angle
        // deviation from the mean), and a propagated
        // gravity-direction σ from per-pair inputs. The
        // synthesized horizon altitude σ floors at the
        // maximum of {empirical spread, propagated σ,
        // configured floor}: all three are σ on gravity
        // direction (radians) — *not* σ on the half-angle
        // sight θ/2. The half-angle σ lives on
        // `direct_sight.observed_altitude` separately.
        let (gravity_mean, empirical_sigma) = aggregate_cluster(&cluster);
        let propagated_sigma = propagated_gravity_sigma(&cluster);
        let sigma_value = empirical_sigma
            .max(self.config.sigma_floor_rad)
            .max(propagated_sigma);
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
    /// pass; `None` on rejection. Increments the appropriate
    /// counter in `stats` on each rejection.
    fn evaluate_pair(
        &self,
        ctx: &HorizonProviderContext<'_>,
        up_idx: usize,
        up: &BodyCandidate,
        dn: &BodyCandidate,
        stats: &mut ReflectionPairStats,
    ) -> Option<PairOutcome> {
        // Test 2 (photometric): reflection ≤ direct (within tol).
        // Run first because it's the cheapest filter.
        if dn.brightness > up.brightness * (1.0 + self.config.photometric_tolerance) {
            stats.rejected_photometric += 1;
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
        let Some(bisector) = bisector_normal(&up_ray.ray, &dn_ray.ray) else {
            stats.rejected_geometric += 1;
            return None;
        };
        if bisector.z <= 0.0 {
            stats.rejected_geometric += 1;
            return None;
        }
        let chord = CameraRay {
            x: dn_ray.ray.x - up_ray.ray.x,
            y: dn_ray.ray.y - up_ray.ray.y,
            z: dn_ray.ray.z - up_ray.ray.z,
        };
        let Some(gravity) = chord.normalize() else {
            stats.rejected_geometric += 1;
            return None;
        };
        if gravity.y <= 0.0 {
            stats.rejected_geometric += 1;
            return None;
        }
        // Pair-plane verticality.
        let cross = up_ray.ray.cross(&dn_ray.ray);
        let cross_norm = cross.norm();
        if cross_norm < f64::EPSILON {
            stats.rejected_geometric += 1;
            return None;
        }
        let horiz_offset = (cross.y / cross_norm).abs();
        if horiz_offset > self.config.max_bisector_horizontal_rad.sin() {
            stats.rejected_geometric += 1;
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
        // (predicted altitude on the upper candidate) are
        // available. Skipped silently otherwise.
        let mut passed_test_3 = false;
        if let (Some(_prior), Some(pred)) = (ctx.position_prior, up.predicted_altitude) {
            let combined_sigma =
                (pred.sigma.value().powi(2) + half_angle_sigma.value().powi(2)).sqrt();
            let diff = (half_angle_rad - pred.value).abs();
            if diff > self.config.catalog_tolerance_sigma * combined_sigma {
                stats.rejected_catalog += 1;
                return None;
            }
            passed_test_3 = true;
        }

        Some(PairOutcome {
            up_idx,
            gravity,
            half_angle,
            sigma_rad: pair_sigma,
            passed_test_3,
        })
    }

    /// Greedy clustering: pick the largest cluster of pairs
    /// whose gravity vectors all lie within
    /// `multi_pair_tolerance_sigma · σ_combined` of each
    /// other.
    fn largest_cluster(&self, outcomes: &[PairOutcome]) -> Option<Vec<PairOutcome>> {
        let mut best: Vec<PairOutcome> = Vec::new();
        for (seed_idx, seed) in outcomes.iter().enumerate() {
            let mut cluster: Vec<PairOutcome> = vec![*seed];
            for (other_idx, other) in outcomes.iter().enumerate() {
                if other_idx == seed_idx {
                    continue;
                }
                // Combined gravity-direction σ in radians:
                // per-pair σ on gravity = `sigma_rad/2` (each
                // ray's pixel σ perturbs the bisector by half
                // that), so the quadrature combination over
                // the seed and other pair is
                // `0.5 * sqrt(sᵢ² + sⱼ²)`. Dropping the 0.5
                // here was the historical behaviour; we keep
                // it for now and let `multi_pair_tolerance_sigma`
                // absorb the factor — see the unit-derivation
                // note in §3.2 of horizon_autodetect.md.
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

/// Propagated gravity-direction σ for the cluster (radians).
///
/// Each pair contributes a gravity-direction σ of
/// `0.5 * sigma_rad` (a small-angle perturbation of either ray
/// rotates the bisector by half that perturbation). Treating
/// the N pairs as independent estimates of the same gravity
/// vector, the combined σ reduces as `1/√N`:
///
/// ```text
///   sigma_grav_combined = sqrt(sum_i sigma_grav_i^2) / N
///                       = sqrt(sum_i (0.5 * sigma_rad_i)^2) / N.
/// ```
///
/// This is the floor on the synthesized horizon altitude σ
/// that the input ray uncertainties imply — the cluster can
/// never be tighter than what the per-pair propagation allows.
fn propagated_gravity_sigma(cluster: &[PairOutcome]) -> f64 {
    debug_assert!(!cluster.is_empty());
    let sum_sq: f64 = cluster.iter().map(|p| (0.5 * p.sigma_rad).powi(2)).sum();
    #[allow(clippy::cast_precision_loss)]
    let n = cluster.len() as f64;
    sum_sq.sqrt() / n
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
        // Geometrically-upper candidate (image-up, smaller
        // pixel.y) is dimmer than the image-down candidate.
        // Test 2 must reject regardless of input ordering.
        let f = frame();
        let i = intr();
        let (mut up, mut dn) = symmetric_pair(0.2, 0.5);
        up.brightness = 1.0;
        dn.brightness = 2.0; // reflection brighter — impossible
        let cands = vec![up, dn];
        let provider = ReflectionPairProvider::default();
        let ctx = ctx_for(&f, &i, &cands, None);
        let mut stats = ReflectionPairStats::default();
        assert!(provider.detect_with_stats(&ctx, &mut stats).is_none());
        assert_eq!(stats.rejected_photometric, 1);
    }

    #[test]
    fn photometric_test_unreachable_via_test1_regression() {
        // Regression for the blocker that pre-sorted pairs by
        // brightness before passing them into `evaluate_pair`,
        // which made Test 2 unreachable. Build a pair whose
        // image-up candidate is much dimmer than the image-down
        // candidate but whose geometry would otherwise pass
        // Test 1. Detection must fail via Test 2 (photometric),
        // not Test 1 (geometric).
        let f = frame();
        let i = intr();
        let (mut up, mut dn) = symmetric_pair(0.2, 0.5);
        up.brightness = 1.0;
        dn.brightness = 1.5 * up.brightness * (1.0 + 0.10); // > 1+tol
        let cands = vec![up, dn];
        let provider = ReflectionPairProvider::default();
        let ctx = ctx_for(&f, &i, &cands, None);
        let mut stats = ReflectionPairStats::default();
        assert!(provider.detect_with_stats(&ctx, &mut stats).is_none());
        assert_eq!(stats.rejected_photometric, 1, "must reject via Test 2");
        assert_eq!(stats.rejected_geometric, 0, "must not reject via Test 1");
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
        let mut stats = ReflectionPairStats::default();
        assert!(provider.detect_with_stats(&ctx, &mut stats).is_none());
        assert_eq!(stats.rejected_catalog, 1);
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

    #[test]
    fn stats_increment_on_success_and_each_rejection_path() {
        let f = frame();
        let i = intr();
        let provider = ReflectionPairProvider::default();

        // (a) successful 3-pair cold-start → no rejections.
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
        let ctx = ctx_for(&f, &i, &cands, None);
        let mut stats = ReflectionPairStats::default();
        assert!(provider.detect_with_stats(&ctx, &mut stats).is_some());
        assert_eq!(stats.rejected_no_cluster, 0);
        // The 3 same-altitude pairs cluster, but cross-pairs
        // (e.g. up_0 × dn_1) get filtered by Test 1 / 2.
        assert!(stats.rejected_geometric + stats.rejected_photometric > 0);

        // (b) photometric reject: image-up candidate dimmer
        // than image-down candidate.
        let (mut up_b, mut dn_b) = symmetric_pair(0.2, 0.5);
        up_b.brightness = 1.0;
        dn_b.brightness = 2.0;
        let cands_b = vec![up_b, dn_b];
        let ctx_b = ctx_for(&f, &i, &cands_b, None);
        let mut stats_b = ReflectionPairStats::default();
        assert!(provider.detect_with_stats(&ctx_b, &mut stats_b).is_none());
        assert_eq!(stats_b.rejected_photometric, 1);

        // (c) geometric reject: non-vertical pair-plane.
        let up_ray = CameraRay::from_unit_components(0.1, -0.2_f64.sin(), 0.2_f64.cos())
            .normalize()
            .unwrap();
        let dn_ray = CameraRay::from_unit_components(-0.1, 0.2_f64.sin(), 0.2_f64.cos())
            .normalize()
            .unwrap();
        let cands_c = vec![
            BodyCandidate {
                pixel: pixel_for_ray(&i, &up_ray),
                brightness: 2.0,
                position_sigma_px: 0.5,
                predicted_altitude: None,
            },
            BodyCandidate {
                pixel: pixel_for_ray(&i, &dn_ray),
                brightness: 1.0,
                position_sigma_px: 0.5,
                predicted_altitude: None,
            },
        ];
        let provider_strict = ReflectionPairProvider {
            config: ReflectionPairConfig {
                max_bisector_horizontal_rad: 0.01,
                ..ReflectionPairConfig::default()
            },
        };
        let ctx_c = ctx_for(&f, &i, &cands_c, None);
        let mut stats_c = ReflectionPairStats::default();
        assert!(provider_strict
            .detect_with_stats(&ctx_c, &mut stats_c)
            .is_none());
        assert_eq!(stats_c.rejected_geometric, 1);

        // (d) no-cluster reject: one valid pair, cold-start
        // needs 3.
        let (up1, dn1) = symmetric_pair(0.2, 0.5);
        let cands_d = vec![up1, dn1];
        let ctx_d = ctx_for(&f, &i, &cands_d, None);
        let mut stats_d = ReflectionPairStats::default();
        assert!(provider.detect_with_stats(&ctx_d, &mut stats_d).is_none());
        assert_eq!(stats_d.rejected_no_cluster, 1);
    }
}
