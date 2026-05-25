# Handoff: Reflection-Pair Provider — Phase 1

Audience: implementer agents picking this up cold. Read this
whole document before touching any files. Then read
`AGENTS.md`, then `docs/design/horizon_autodetect.md` §3 and
§10–§11, then the files listed under "Existing surface" below.

## Context

Bris's engine currently has one horizon source per frame:
optical detection (Gradient / Sky / Night / Segmentation),
chosen by a cheap-first-best-σ policy in
`crates/bris-streaming/src/pipeline/horizon.rs`.

We are introducing a second kind of horizon source: an
**auto-detected reflection-pair provider** that, given a body
and its reflection in a horizontal surface (puddle, cup,
mirror) both visible in the same frame, infers local gravity
from the bisector of the two rays and emits both a horizon
hypothesis and a direct sight `Ho = θ/2`.

The full provider catalog (mirror, plumb line, vanishing
points, IMU) and the architectural roadmap live in
`docs/design/horizon_autodetect.md`. This handoff implements
**Phase 1 only**: the reflection-pair provider for **Night
mode**, intra-frame, with a `HorizonProvider` trait and
single-provider stub fusion.

## Branch

Create `reflection-pair-phase1` from `main`.

## Hard constraints (do not violate)

- **`unsafe_code = "forbid"` workspace-wide.** No exceptions.
- **No new dependencies without checking `[workspace.dependencies]`.**
  Reuse, don't fork versions.
- **Pi Zero 2W (aarch64) must compile.** `cargo check
  --target aarch64-unknown-linux-gnu` if you have the target
  installed; otherwise CI catches it.
- **Honest uncertainty everywhere.** Every output σ must
  flow from documented input σ. No silent smoothing.
- **No FFI / Android / collector changes.** Engine-internal
  only.
- **Day-mode Stage B is out of scope.** Day produces one
  centroid; reflection-pair detection in Day requires extending
  Stage B to expose multiple candidates per frame. That's a
  Phase 2 follow-up, explicitly deferred. Phase 1 lands
  **Night mode** (`BodyDetection::Night(Vec<Peak>)`) only.

## Locked decisions

From the planning conversation captured in
`docs/design/horizon_autodetect.md` §10:

- **Sight emission:** option (ii). On a successful pair, emit
  **both** a horizon hypothesis (`HorizonLine`) and a direct
  sight `Ho = θ/2` for the participating body. Sight
  combination in `bris-nav` already de-duplicates per-body
  sights in a window; trust it.
- **Catalog-consistency prior (Test 3):** preference order
  (1) last successful fix, (2) DR projection from last fix,
  (3) GNSS (phone only — N/A on Pi), (4) **cold start** →
  drop Test 3, require **≥3 concordant pairs** under Test 4.
- **Operator UX:** none. Provider is always-on inside the
  engine. No toggle, no FFI surface change.
- **Trait + single-provider stub fusion:** ship together.
  The trait is the seam for future providers; the stub fusion
  is just a passthrough of the single available hypothesis.
- **Temporal scope:** intra-frame only for Phase 1. Trait
  exposes a `TemporalScope` method to accommodate cross-frame
  in a later phase per `horizon_autodetect.md` §11; Phase 1
  returns `TemporalScope::IntraFrame`.

## Existing surface (read these first)

Found via the explore agent's recon pass (see conversation
context if uncertain about scope):

- `crates/bris-streaming/src/pipeline/horizon.rs:49-310` —
  `HorizonStageOutcome`, `HorizonDetector` enum, `detect()`
  dispatcher with `try_*` helpers + `update_best` +
  `early_terminate`. **This is the integration seam.** The
  `try_*` functions are what get refactored into a
  `HorizonProvider` trait.
- `crates/bris-streaming/src/pipeline/mod.rs:65-180` —
  `BodyDetection` enum (Day/Night/IdentifiedStars/None);
  `process_frame` ordering (A→C→B). **`BodyDetection` is
  `pub(crate)` — Phase 1 needs it (or a read-only view)
  visible to the trait/provider machinery in bris-vision.**
- `crates/bris-vision/src/horizon.rs:49-70` — `HorizonLine`
  type. New synthesized lines must populate every field
  honestly (notably `altitude_sigma: Sigma`).
- `crates/bris-vision/src/ray.rs:30-260` — `CameraRay`,
  `BodyRay`, `HorizonRay`, `altitude_from_rays`. **This is
  where the pair-geometry helpers belong** (new function:
  `bisector_normal(ray_a, ray_b) -> HorizonRay` or similar).
- `crates/bris-vision/src/lens.rs` — `pixel_ray_direction`
  (the canonical K⁻¹ helper with undistortion). Use this; do
  not build a `K` matrix explicitly.
- `crates/bris-vision/src/centroid.rs:40-55` — `Centroid`
  (used by Day path; reference shape, not directly consumed
  by Phase 1).
- `crates/bris-vision/src/peak.rs` — `Peak { intensity, … }`
  (Night path; **this is what Phase 1 consumes**).
- `crates/bris-streaming/src/pipeline/stage_e.rs:96-580` —
  pair selection, sight emission, `reduce_to_sight`. Phase 1
  injects the direct sight `Ho = θ/2` into this path.
- `crates/bris-streaming/src/engine.rs:60-110, 405-420` —
  `EngineState`; **only `last_published_fix_tt` is retained
  today**. Phase 1 must add `last_published_fix:
  Option<PublishedFix>` (or equivalent narrow view) so the
  catalog-consistency prior has something to read.
- `crates/bris-streaming/src/diagnostics.rs:1-105` —
  `EngineDiagnostics`. New counters extend this struct
  inline.
- `crates/bris-vision/tests/regression/marina_with_body/` —
  TOML-driven fixture example. `case.toml` + image; harness
  auto-generates `#[test]` via `build.rs`.
- `crates/bris-platesolve/examples/synth_frame_test.rs` —
  synthetic-frame construction pattern.

## Scope

### Module 1 — `bris-vision::horizon_providers` (new module)

Create `crates/bris-vision/src/horizon_providers/mod.rs` with
the trait:

```rust
/// Common interface for any source of a horizon line.
/// Implementors observe per-frame (and eventually
/// cross-frame) evidence and produce a `HorizonHypothesis`
/// or `None`.
pub trait HorizonProvider {
    fn name(&self) -> &'static str;

    /// Whether this provider needs only the current frame or
    /// a window of registered frames. Phase 1 implementations
    /// return `IntraFrame`.
    fn temporal_scope(&self) -> TemporalScope;

    /// Run the provider against the given evidence. Returns
    /// `None` when the provider declines (no evidence,
    /// failing tests, etc.) — never silent fallback.
    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis>;
}

pub enum TemporalScope { IntraFrame, Window }

pub struct HorizonProviderContext<'a> {
    pub frame: &'a Frame,
    pub intrinsics: &'a Intrinsics,
    pub body_detection: &'a BodyDetection, // re-exported from bris-streaming via a narrower trait or moved
    pub position_prior: Option<&'a PositionPrior>,
    pub timestamp: Tt,
}

pub struct HorizonHypothesis {
    pub line: HorizonLine,
    pub provenance: HorizonProvenance,
    /// Optional direct sight from the same evidence (e.g.
    /// reflection pair's Ho = θ/2). Sight-combination in
    /// `bris-nav` de-duplicates per-body sights in a window.
    pub direct_sight: Option<DirectSight>,
}

pub struct DirectSight {
    pub body_pixel: (f64, f64),     // which body candidate this sight is for
    pub observed_altitude: Uncertain<f64>,
}

pub enum HorizonProvenance {
    Optical(HorizonDetector),
    ReflectionPair { pair_count: usize, used_position_prior: bool },
    // Plumb, VanishingPoint, IMU, etc. land later
}
```

**Important:** `BodyDetection` currently lives in
`bris-streaming` and is `pub(crate)`. Two options:
1. **Promote** `BodyDetection` (and its constituent types,
   already in `bris-vision`) to `bris-vision` as public. The
   types it contains (`Centroid`, `Peak`, `PlateSolveResult`)
   are mostly already there.
2. **Pass a narrower view**, e.g. `&[BodyCandidate]` where
   `BodyCandidate { pixel: (f64, f64), brightness: f64,
   position_sigma_px: f64 }`, into the provider context.

Prefer **option 2**: keeps `BodyDetection` private to
`bris-streaming`, gives the provider exactly what it needs,
makes the trait testable without dragging streaming types into
bris-vision. The conversion happens at the `pipeline/horizon.rs`
call site.

### Module 2 — `bris-vision::horizon_providers::reflection_pair`

The actual provider. Implements `HorizonProvider`. Algorithm
per `horizon_autodetect.md` §3.2:

```rust
pub struct ReflectionPairProvider {
    pub config: ReflectionPairConfig,
}

pub struct ReflectionPairConfig {
    /// k-sigma tolerance for Test 3 (catalog consistency).
    pub catalog_tolerance_sigma: f64,           // default 4.0
    /// k-sigma tolerance for Test 4 (multi-pair agreement).
    pub multi_pair_tolerance_sigma: f64,         // default 3.0
    /// Maximum angle from vertical that bisector may deviate
    /// before pair is rejected as non-reflective.
    pub max_bisector_horizontal_rad: f64,        // default 0.05 rad (~3°)
    /// Cold-start: minimum concordant pairs required when
    /// no position prior is available (drops Test 3).
    pub cold_start_min_pairs: usize,             // default 3
    /// σ floor on the synthesized horizon altitude, rad.
    pub sigma_floor_rad: f64,                    // default 1e-4 rad (~20")
}
```

Five tests to implement, in order, short-circuiting on first
failure:

1. **Geometric (Test 1).** For each candidate pair
   `(b_up, b_dn)`:
   - Build rays `r_up`, `r_dn` via `BodyRay::from_pixel`.
   - Bisector `b = (r_up + r_dn).normalize()`. Inferred
     gravity is `-b` (points downward).
   - Reject if bisector's z-component (camera optical axis)
     is wrong sign for the camera being held forward-ish.
   - Reject if cross product `r_up × r_dn` is not roughly
     horizontal in camera frame (i.e. the pair plane is
     vertical); tolerance per `max_bisector_horizontal_rad`.
2. **Photometric (Test 2).** Reflection must be **dimmer**
   than direct (energy conservation). Reject pairs where
   `b_dn.brightness > b_up.brightness * (1 + tolerance)`.
   Use a generous tolerance (e.g. 10%) since local exposure
   gradients exist.
3. **Catalog consistency (Test 3, optional).** If
   `position_prior` is provided and a catalog body is
   identifiable (post-plate-solve `IdentifiedStars`), compute
   `Ho_pred` for the body and check `|θ/2 − Ho_pred| <
   catalog_tolerance_sigma · σ_pred`. **Skip this test
   silently in cold-start (no prior)** — but then Test 4
   threshold rises (see config).
4. **Multi-pair agreement (Test 4).** Surviving pairs must
   all yield consistent `g_cam`. Cluster by direction; the
   largest cluster wins. Singleton cluster rejected unless
   Test 3 passed (with prior) or cluster size ≥
   `cold_start_min_pairs`.
5. **Reflector region (Test 5).** Optional, skip for Phase 1
   — flagged in code with a TODO comment referencing
   `horizon_autodetect.md` §3.2.

Output: the surviving cluster's `g_cam` becomes the
`HorizonLine` via the projection `ℓ = K⁻ᵀ g_cam` (use
`pixel_ray_direction` to invert per the spec). σ is the
larger of (the cluster's empirical spread) and
(`sigma_floor_rad`). Each surviving pair also produces a
`DirectSight { body_pixel: b_up.pixel, observed_altitude:
Uncertain { value: θ/2, sigma: ... } }`. For Phase 1, **emit
only the first surviving pair's direct sight** to keep the
sight-combination semantics simple (one sight per body per
frame, as it already is for the optical path).

Implementation notes:
- The pair-geometry helpers (`bisector_normal`, etc.) go in
  `crates/bris-vision/src/ray.rs`. Keep this provider focused
  on policy (the five tests + clustering); ray math is
  reusable.
- σ propagation: each body ray has σ_px from `Peak` /
  `BodyRay::from_pixel`; the bisector's σ is the angular
  combination of the two ray σ's (RMS of small-angle
  contributions). Document the derivation in a code comment.

### Module 3 — `bris-streaming::pipeline::horizon` — trait integration

Refactor the existing `detect()` dispatcher to use the
`HorizonProvider` trait under the hood:

- Wrap each existing `try_*` function as a trivial
  `HorizonProvider` impl (`GradientProvider`, `SkyProvider`,
  `NightProvider`, `NightTexturedProvider`, `SegmentationProvider`).
  These wrappers live in `crates/bris-streaming/src/pipeline/horizon_providers.rs`.
- Add a `ReflectionPairProvider` to the dispatch list when
  body detection produced ≥2 peaks (Night mode).
- Keep the existing cheap-first / `update_best` / `early_terminate`
  policy — it's already "multi-hypothesis pick best σ". The
  trait refactor is mechanical.
- **Stub fusion** for Phase 1: still winner-takes-best-σ. The
  difference is that hypotheses now flow through the trait;
  the `HorizonStageOutcome` shape doesn't change (still
  carries one chosen line + provenance).

The `HorizonStageOutcome::Detected.detector` field grows a
new variant `HorizonDetector::ReflectionPair`. Optionally,
return type can be extended to surface `HorizonProvenance`
fully (more information for diagnostics); minimum change is
just the enum variant.

### Module 4 — Direct sight emission from reflection pair

In `stage_e.rs::reduce_to_sight` (or `select_pairs`), when the
`HorizonStageOutcome`'s provenance is `ReflectionPair` and
carries a `DirectSight`, **use the `direct_sight.observed_altitude`
directly** instead of computing one via
`measure_altitude(intr, line, centroid)`. This is the option
(ii) sight emission from the locked decisions. Document the
double-counting risk: if for some reason the optical path
*also* produced a sight for the same body in the same frame
(impossible today because providers are winner-takes-all, but
worth a defensive comment), the bris-nav sight combination
de-dupes by body identity.

### Module 5 — Position-prior plumbing

Extend `EngineState` (`engine.rs:60-110`) with:

```rust
last_published_fix: Option<PublishedFix>,
```

Update it in `engine.rs:415` alongside `last_published_fix_tt`.
Pass a `PositionPrior` view into the horizon dispatcher via
`HorizonProviderContext`. `PositionPrior` shape:

```rust
pub struct PositionPrior {
    pub lat: f64,
    pub lon: f64,
    pub sigma_position_m: f64,
    pub timestamp: Tt,  // the fix's TT, for DR projection
}
```

DR projection (preference #2) is **out of scope for Phase 1**
— if the last fix is stale beyond a threshold (e.g. 30 s),
treat as cold start. Document this as a Phase 2 followup.

### Module 6 — Diagnostics

Add to `EngineDiagnostics`:

```rust
pub reflection_pair_attempts: u64,
pub reflection_pair_succeeded: u64,
pub reflection_pair_rejected_geometric: u64,
pub reflection_pair_rejected_photometric: u64,
pub reflection_pair_rejected_catalog: u64,
pub reflection_pair_rejected_no_cluster: u64,
```

Increment inline at each test-failure site. Cheap.

### Module 7 — Tests

**Synthetic-fixture unit tests** in
`crates/bris-vision/src/horizon_providers/reflection_pair.rs`
(or a dedicated `tests/` file):

1. **Synthetic clean pair.** Construct two `Peak`s in a
   `BodyCandidate` list whose rays bisect to a known
   `g_cam` (use a unit-test helper to build pixels from a
   desired ray). Provider returns the expected `HorizonLine`
   to within numerical tolerance.
2. **Non-vertical bisector rejected.** Two peaks whose
   bisector deviates from vertical by 5° → rejected by Test 1.
3. **Reflection brighter rejected.** Test 2.
4. **Catalog-consistent pair with prior.** Provide a
   `PositionPrior` and an `IdentifiedStars` body detection
   whose predicted altitude matches `θ/2`; pair passes.
5. **Catalog-inconsistent pair with prior rejected.**
6. **Cold-start with one pair rejected; cold-start with three
   concordant pairs accepted.**
7. **σ propagation.** Two pairs with different per-pair σ;
   resulting `HorizonLine.altitude_sigma` ≥ `sigma_floor_rad`.
8. **Day-mode (single centroid) returns `None`** — explicit
   test for the Phase 1 scope boundary.

**Integration test** in
`crates/bris-streaming/tests/reflection_pair_integration.rs`
(or in-module under `#[cfg(test)] mod tests`):

- Drive a synthetic Night frame with two peaks through
  `process_frame` → assert `HorizonStageOutcome::Detected`
  with `HorizonDetector::ReflectionPair`, assert the emitted
  sight uses `θ/2`, assert diagnostic counters increment.

**Regression-corpus fixture (deferred):**
Real-data `tests/regression/reflection_pair_<scene>/case.toml`
fixture will be promoted from operator capture (moonlight on
puddle). **Phase 1 does not require a real fixture to land**;
the synthetic tests are the gate. Add a TODO comment in the
regression directory README pointing at this handoff.

### Module 8 — plan.org / progress.md updates

- **plan.org:** insert a new `* Phase 3.6: Horizon providers
  (auto-detected artificial horizons)` section between Phase
  3.5 (line 1482-ish) and Phase 4. Under it:
  - `** DONE Phase 1: ReflectionPairProvider (intra-frame, Night)`
    — list the eight test cases + the diagnostic counters as
    the acceptance evidence. Reference
    `docs/design/horizon_autodetect.md`.
  - `** TODO Phase 2: Day-mode multi-centroid support`
    — extend Stage B to expose ≥2 candidates per Day frame.
  - `** TODO Phase 3: Cross-frame registration (§11 of design doc)`
    — adds `TemporalScope::Window` provider mode.
  - `** TODO Phase 4: Plumb / vanishing-point providers`
  - `** TODO Phase 5: IMU provider` (with cross-reference to
    plan.org:1062 IMU-prior TODO; mark that one PARTIAL once
    the trait exists).
- **progress.md:** update the "Phases done" / "Not started"
  paragraph (around line 209). Add a new section
  `## Phase 3.6 Phase 1 landed` (or merge into the existing
  current-session narrative) describing the trait + the
  reflection-pair provider + the eight passing tests.

## Required local checks before pushing

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check
```

All must pass. The `clippy::pedantic` workspace lint is
warn-level but treat warnings introduced by your code as
errors per `AGENTS.md`.

Cross-target compilation:
```sh
cargo check --workspace --target aarch64-unknown-linux-gnu
```
(skip if the target isn't installed locally; CI catches it.)

## Out of scope (explicitly deferred)

- **Day-mode reflection pairs** — needs Stage B to expose
  multiple centroids; Phase 2.
- **DR projection of stale position prior** — Phase 2.
- **Cross-frame reflection pairs** (§11 of design doc) —
  Phase 3 of the horizon-providers roadmap.
- **Other providers** (plumb, vanishing point, IMU) — later
  phases.
- **PBRIS `horizon_provenance` field** — design doc §8 calls
  for it but it's a wire-format change; do it in a separate
  PR after Phase 1 lands.
- **Operator UI / toggle / FFI changes** — none.
- **Real-data regression fixture** — promoted from operator
  capture later; synthetic tests are the Phase 1 gate.
- **`HorizonProvider` trait being exposed via FFI** — purely
  internal in Phase 1.

## Acceptance criteria

Phase 1 is done when:

1. `HorizonProvider` trait exists in `bris-vision` with three
   methods (`name`, `temporal_scope`, `detect`) and the five
   existing optical detectors are refactored to impl it.
2. `ReflectionPairProvider` implements `HorizonProvider`,
   intra-frame, Night mode, with all five tests in place
   (Test 5 a TODO-comment stub).
3. The dispatch in `pipeline/horizon.rs` integrates the
   reflection-pair provider when Night peaks ≥ 2.
4. On a successful reflection-pair detection, the resulting
   sight uses `θ/2` (option (ii) emission).
5. `EngineState` carries `last_published_fix` and propagates
   a `PositionPrior` into the horizon dispatcher.
6. `EngineDiagnostics` carries the six new counters.
7. All eight synthetic unit tests pass.
8. The integration test (synthetic Night frame → detection →
   sight) passes.
9. `plan.org` has a Phase 3.6 section; Phase 1 is marked DONE
   under it. `progress.md` reflects the work.
10. The full local check suite (`fmt + clippy + test +
    deny`) passes clean.

## Followups (tag in PR description)

- Phase 2: Day-mode multi-centroid + DR projection.
- Phase 3: cross-frame registration (§11 of design doc).
- Phase 4: plumb / vanishing-point providers.
- Phase 5: IMU provider.
- Separate PR: PBRIS `horizon_provenance` field (design doc §8).
- Separate PR: real-data regression fixture once operator
  captures moonlight-on-puddle clip.

## Suggested commit structure

If you want to make review easier, the work splits cleanly
into these commits (but you may squash if you prefer):

1. **Trait extraction.** `HorizonProvider` trait +
   `HorizonProviderContext` + `BodyCandidate` view. Refactor
   the five optical detectors to impl the trait. No behavior
   change. All existing tests still pass.
2. **Position-prior plumbing.** `EngineState.last_published_fix`,
   `PositionPrior`, threaded into the horizon dispatcher.
3. **Reflection-pair provider.** `ReflectionPairProvider`
   with all five tests + synthetic unit tests.
4. **Direct sight emission.** Stage E consumes
   `HorizonHypothesis.direct_sight` when present.
5. **Diagnostics + plan.org / progress.md updates.**

Each step keeps the workspace green.

---

End of handoff. Read this whole document, then `AGENTS.md`,
then `docs/design/horizon_autodetect.md` §3, §10, §11, then
the eight files in "Existing surface". Start with commit step
1 (the mechanical trait extraction); it unblocks everything
else.
