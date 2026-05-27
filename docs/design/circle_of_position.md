# Circle of Position Calculations

Status: design. Implements the no-AP cold-start solver for
multi-sight fixes. Lives in `crates/bris-nav/src/
circle_of_position.rs`; surfaced through a new entry point
`cold_start_fix` alongside the existing `multi_sight_fix`.

This document specifies the geometry, the algebra, the
ambiguity-resolution policy, and the integration points. It
is not a tutorial on celestial navigation; for the
conceptual background see `position_prior_requirement.md`.

## Inputs

The solver consumes a slice of `CircleOfPosition` records.
Each record is constructed from a single reduced sight and
carries the four quantities the geometry needs:

```rust
pub struct CircleOfPosition {
    /// Body's geographic position (sub-point) at the sight
    /// instant. Latitude is the body's declination; longitude
    /// is `−GHA` wrapped to [−π, π].
    pub gp_lat_rad: f64,
    pub gp_lon_rad: f64,
    /// Co-altitude: zenith distance = π/2 − Ho_apparent.
    /// In radians, range (0, π/2).
    pub co_altitude_rad: f64,
    /// 1σ uncertainty in the co-altitude, radians.
    pub sigma_rad: f64,
}
```

Construction is a thin adapter from the existing engine-side
`Sight`: the body's GP comes from the same `bris-almanac`
apparent-place call that produced `Hc`; `co_altitude` is
`π/2 − Ho`; `sigma_rad` is the per-sight altitude σ already
carried on `Sight`.

## Geometric model

Every sight defines a small circle on the unit sphere
(Earth's surface treated as a sphere of radius `R⊕`). The
circle is the set of all observer positions for which the
body's altitude equals the measured `Ho` at the measurement
instant. Equivalent characterization: the set of points on
the unit sphere whose angular distance from the body's GP
is exactly the co-altitude `z = π/2 − Ho`.

The observer is at one of the points where ALL the input
circles meet. With perfect measurements that intersection is
a single point (or two for exactly two circles); with real
measurements there is no exact common point and we seek the
maximum-likelihood estimate under Gaussian altitude noise.

## Two-circle case (exactly N = 2)

Two small circles on a sphere intersect in 0, 1, or 2 points.
The geometry is exact and solved analytically.

Let `g₁`, `g₂` be the unit vectors to the two GPs (cartesian
on the unit sphere, x = cos(lat)·cos(lon), y =
cos(lat)·sin(lon), z = sin(lat)). The observer's unit vector
`p` satisfies:

```
g₁ · p = cos(z₁)
g₂ · p = cos(z₂)
|p| = 1
```

The first two equations define a plane each; their
intersection is a line through the unit sphere; the two
intersection points are the solutions.

Algorithm:

1. Compute `d = g₁ × g₂` (perpendicular to both planes).
2. Solve the 2-equation linear system `g₁ · p = cos(z₁)`,
   `g₂ · p = cos(z₂)` for `p` in the plane spanned by `g₁`
   and `g₂`. Write `p = α g₁ + β g₂ + γ d`; the dot products
   give `α` and `β` directly via the 2×2 Gram matrix of
   `g₁`, `g₂`.
3. From `|p|² = 1`, solve for `γ` (quadratic; two roots).
   Each root gives one of the two candidate observer
   positions `p±`.

Failure modes:
- `g₁ × g₂ = 0` (GPs coincident or antipodal). Circles are
  either coincident (infinite solutions) or non-intersecting
  (no solution). Report `Disjoint` and let the caller request
  another sight.
- Discriminant of the quadratic for `γ` < 0. Circles do not
  intersect (measurements inconsistent beyond what σ can
  explain). Report `Inconsistent`.
- Discriminant ≈ 0 (within `tangent_tolerance_rad`, default
  `1e-6`). Tangent. Report a single candidate with inflated σ.

Output: `TwoCircleResult::Pair { primary, secondary,
separation_great_circle_rad }`. Both candidates are returned;
the caller chooses (operator hemisphere input, or a third
sight; see "Ambiguity resolution" below).

## General case (N ≥ 3)

With three or more sights the two-point ambiguity is resolved
geometrically. The algorithm:

1. Run the two-circle solver on every pair of input circles.
   With N circles this is `N(N−1)/2` pairs, each producing 0,
   1, or 2 candidates.
2. Collect all candidates into a single pool.
3. For each candidate `p`, compute the total weighted residual:
   `R(p) = Σᵢ (acos(gᵢ · p) − zᵢ)² / σᵢ²`. The candidate
   with smallest `R(p)` is the cluster centre.
4. Cluster the remaining candidates by great-circle distance
   from the centre. All candidates within
   `cluster_radius_great_circle_rad` (default: `5° = 0.087
   rad`) form the consensus cluster.
5. Within the consensus cluster, refine to the
   maximum-likelihood estimate by one Newton step:
   linearize each circle's residual around the cluster
   centre, solve the resulting 2×2 weighted least-squares
   problem in tangent-plane coordinates, project back to
   the sphere.

If exactly one cluster forms, the refined point is the
fix. The cluster size — number of sight pairs voting for
this region — is reported alongside.

If two distinct clusters of comparable size form, the
ambiguity persists at N ≥ 3 (rare; requires unusual
symmetry of sight geometry). Report both with their cluster
sizes; the caller decides as in the two-circle case.

If no cluster of size ≥ 2 forms, the sights are mutually
inconsistent. Report `Inconsistent { residuals }` carrying
the per-sight residual at the best single candidate so the
caller can identify the blunder.

## Output

```rust
pub enum ColdStartResult {
    /// Two candidates from a 2-circle solve. Caller resolves
    /// ambiguity via operator input or a further sight.
    TwoCandidates {
        primary: FixCandidate,
        secondary: FixCandidate,
        separation_great_circle_nm: f64,
    },
    /// Single best fix from N ≥ 3 sights (or N = 2 with
    /// secondary geometrically implausible — e.g. on dry land
    /// when sailing, but bris does not currently filter on
    /// land/sea, so this never fires automatically). Returns
    /// covariance from the Newton-step linearization.
    Fix(FixCandidate),
    /// All considered candidates were geometrically
    /// inconsistent with the input circles' σ. Carries the
    /// best single candidate and its per-sight residuals so
    /// the caller can identify the bad sight.
    Inconsistent {
        best_candidate: FixCandidate,
        per_sight_residuals_rad: Vec<f64>,
    },
}

pub struct FixCandidate {
    pub lat: Latitude,
    pub lon: Longitude,
    pub covariance_nm2: [[f64; 2]; 2],
    pub sigma_major_nm: Sigma,
    pub sigma_minor_nm: Sigma,
    pub orientation_rad: f64,
    pub sight_count: usize,
    /// Number of sight-pair candidates that fell in this
    /// candidate's consensus cluster. N(N−1)/2 is the max.
    /// 1 for the two-circle case.
    pub cluster_size: usize,
}
```

Errors:

```rust
pub enum ColdStartError {
    InsufficientSights,  // N < 2
    Disjoint,            // 2-circle: GPs coincident/antipodal
    NonFinite,           // any input NaN / inf
}
```

`Inconsistent` is a result variant, not an error: the caller
gets a candidate to display and the per-sight residuals to
diagnose. Errors are reserved for malformed input.

## Geometric-diversity floor

Two sights of the same body taken seconds apart produce two
nearly-coincident circles; the 2-circle solver returns two
candidates but their `separation_great_circle_nm` is small
and the cluster centre's tangent-plane covariance is
correspondingly elongated. The solver reports this honestly
through `sigma_major_nm` / `sigma_minor_nm`; it does not
refuse to produce a fix.

A separate gate at the engine level (see "Sight Persistence"
design doc) decides whether to publish a cold-start fix
based on `sigma_major_nm` relative to a configured threshold.
This separation keeps the solver's geometry pure: solver
reports what the math gives, engine decides what to surface.

## Covariance derivation

For the N ≥ 3 case the Newton-step linearization produces a
direct 2×2 covariance in tangent-plane (north, east) nm:

```
J_i = (∂/∂N residual_i, ∂/∂E residual_i)
    = (−sin(bearing_to_GP_i), −cos(bearing_to_GP_i)) / R⊕
W   = diag(1 / σ_i²)
cov_tangent_plane = (Jᵀ W J)⁻¹
```

This is the same algebra as the existing `multi_sight_fix`
solver, evaluated at the cold-start position instead of an
AP. The decomposition into ellipse semi-axes and orientation
follows the same eigendecomposition.

For the 2-circle case there is no overdetermined system to
linearize. Each candidate's covariance comes from
first-order propagation of the per-sight σ through the
2-circle intersection formula. Concretely: the candidate's
1σ uncertainty along the great-circle joining the two GPs
is `≈ σ̄ / sin(θ)`, and perpendicular to that great circle
is `≈ σ̄ / cos(θ)`, where `θ` is the half-angle the GPs
subtend at the candidate and `σ̄` is the RSS of the two
σ values. When `θ → 0` (GPs nearly coincident from the
observer) the along-circle σ blows up — correctly capturing
that two same-body sights minutes apart constrain only one
direction.

## Ambiguity resolution

Two-candidate results need disambiguation. The solver does
not embed a policy; the caller (engine + UI) chooses:

1. **Geographic prior hint** (cheapest). If a config-level
   `coarse_hemisphere: Option<Hemisphere>` or
   `coarse_region_centre: Option<(Latitude, Longitude)>` is
   set, pick the candidate closer to the hint. The hint is
   not an AP — it can be off by thousands of nm — but it
   resolves the antipodal ambiguity reliably.
2. **Operator prompt**. If no hint and no third sight, the
   UI surfaces both candidates ("you are at A or B; tap
   one") and the engine adopts the choice as the
   `PositionPrior` for subsequent reductions.
3. **Additional sight**. If a third sight arrives before
   the operator responds, re-run as N = 3; the geometric
   resolution kicks in and the prompt is dismissed.

The solver returns both candidates unordered; the caller's
`coarse_hemisphere` / prompt logic assigns "primary" /
"secondary" semantically.

## Numerical considerations

- Compute on unit-sphere cartesian throughout; only convert
  to (lat, lon) at the boundary. Avoids cosine-of-latitude
  singularities at the poles.
- All angles in radians internally; the existing
  `Latitude` / `Longitude` newtypes from `bris-core` handle
  conversion at the boundary.
- The Newton refinement step uses a fixed 1 iteration. The
  linearization is accurate to second order; multiple
  iterations would refine to machine precision but the
  per-sight σ floor (1″ on the best Moon sight, much
  larger generally) dwarfs the post-iteration residual.
- Cluster radius default (5°) is generous and may produce
  false consensus on pathological symmetric-circle inputs.
  Configurable via `ColdStartConfig::cluster_radius_rad`.

## Test corpus

Unit tests (in `crates/bris-nav/src/circle_of_position.rs`,
`#[cfg(test)] mod tests`):

1. **2-circle exact**: two sights of synthetic bodies with
   known GPs and a true observer at (40°N, 0°). Verify
   both candidates returned, the "right" one within
   floating-point of true.
2. **2-circle antipodal ambiguity**: same as above; verify
   the secondary candidate is on the other side of the
   GP-joining great circle.
3. **2-circle tangent**: same body, two times with GP just
   touching the observer's circle. Verify single candidate
   + inflated σ.
4. **2-circle disjoint**: σ-floor breach. Verify
   `Inconsistent` not `Disjoint` (Disjoint is for coincident
   GPs).
5. **3-circle convergence**: 3 sights of distinct bodies,
   well-separated azimuths, known truth. Verify single
   `Fix` candidate, position within 0.5 nm of truth, cluster
   size = 3 (all 3 pairs vote together).
6. **3-circle one blunder**: 3 sights, one with a 30′
   gross error. Verify `Inconsistent` with the blunder's
   residual standing out.
7. **N = 5 weighted**: 5 sights with mixed σ (0.5′ to 2′).
   Verify covariance scales with the weighted information
   matrix.
8. **Same-body 30-min-apart**: 2 Moon sights with realistic
   GP separation (~420 nm). Verify both candidates returned,
   primary covariance ellipse `sigma_major / sigma_minor`
   ratio ~6:1 (the geometric prediction from the
   `position_prior_requirement.md` table).
9. **Same-body 1-min-apart**: 2 Moon sights with ~14 nm GP
   separation. Verify the candidates exist but their
   `sigma_major_nm` exceeds 100 nm — the engine-level
   diversity gate would not publish.
10. **Pole-adjacent observer**: observer at 89°N, two
    sights. Verify cartesian path handles it (no
    cos-latitude division-by-zero).

Integration test in `crates/bris-streaming/tests/
cold_start_fix.rs` (env-gated like
`moonlight_pond_lop.rs`): synthetic capture of two distinct
bodies, no AP configured, verify a cold-start fix is
produced and matches the synthetic truth within tolerance.

## Engine integration

**Status: IMPLEMENTED** as of
`crates/bris-streaming/src/pipeline/stage_e.rs::try_publish`
(see also `circles_from_sights` /
`body_geographic_position` in the same module).

Stage E (`crates/bris-streaming/src/pipeline/stage_e.rs`)
gains a fallback path: when `multi_sight_fix` returns
`FixError::SingularGeometry` *and* no `PositionPrior` is
available, attempt `cold_start_fix` against the same sight
window. The cold-start result publishes via the same
`PublishedFix` channel with a new `Provenance::ColdStart`
flag for the FFI surface (operator-visible "this is a
cold-start fix, not yet AP-anchored").

Stale-prior trigger: when `multi_sight_fix` *succeeds* but
the maximum |intercept| across the window's sights exceeds
[`bris_streaming::ColdStartEngineConfig::stale_prior_intercept_threshold_nm`]
(default 60 nm ≈ 1°), the LSQ linearization around the
assumed position is suspect (operator-entered AP off by
>60 nm, or a recovered fix from hours ago after a long
drive). Stage E runs `cold_start_fix` as a comparison and
publishes the cold-start fix in place of Saint-Hilaire iff
it converges with a tighter `sigma_major_nm`. The
`cold_start_preferred_over_stale_sh` diagnostics counter
tracks each replacement.

When `cold_start_fix` returns `TwoCandidates` with a
configured `coarse_hemisphere` hint
([`bris_streaming::ColdStartEngineConfig::coarse_hemisphere`]),
the engine picks the candidate in the matching hemisphere
and publishes with
[`bris_streaming::FixProvenance::ColdStartAmbiguous`].
Without a hint, the engine logs and skips publication; the
operator-prompt FFI channel + `Engine::resolve_cold_start`
are follow-up work (tracked TODO inline in `try_publish`).
Diagnostics counters `cold_start_attempts`,
`cold_start_published`, `cold_start_ambiguous_skipped`,
`cold_start_inconsistent_count`, and
`cold_start_disjoint_count` on `EngineDiagnostics` expose
the path's per-session frequency.

## Out of scope

- Land/sea filtering of candidates. Bris has no coastline
  database; the operator may be on either.
- Iterative refinement past Newton step 1.
- Robust ψ-function (IRLS-biweight) inside the cold-start
  solver. The `Inconsistent` variant returns enough
  information for a separate IRLS pass at the engine level
  if and when that lands.
- Time-tagging of GPs across the input set. Each
  `CircleOfPosition` carries its own GP computed at its own
  instant; the geometry treats them as a static configuration
  on the unit sphere. Observer motion between sights is the
  caller's concern (see persistence doc, "stale sight
  policy").
