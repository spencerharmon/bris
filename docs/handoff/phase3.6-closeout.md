# Phase 3.6 Close-out: Multi-Provider Horizon System

Status: complete. All planned Phase 3.6 deliverables landed on
`main` between commits `e1ba241` and `bc2f6c3`. This document
captures what shipped, what real-world capture data revealed,
and the prioritized accuracy work that follows.

For per-task narrative see `plan.org` Phase 3.6 entries
(lines ~1482–1614); for the design rationale see
`docs/design/horizon_autodetect.md`.

## What landed

| Phase | Sub-task | PR | Commit | Outcome |
|---|---|---|---|---|
| 1 | `ReflectionPairProvider` (Night/Twilight intra-frame) | #7 | `239fea0` | Trait + 5 detectors refactored; reflection-pair Tests 1–4 + 9 unit tests |
| 2 | Day-mode multi-centroid + Sun-almanac plumbing | #8 | `6997265` | `extract_multi_saturated_centroids`, halo-based `mean_intensity`, Day dispatch |
| 4a | `VerticalLineProvider` (plumb / single edge) | #9 | `52ecfe0` | Hough-restricted line detector, gravity from endpoint rays, 4 unit tests |
| 4b | `VanishingPointProvider` (Manhattan-world) | #10 | `cd28c26` | Sobel + RANSAC + NMS, vertical/two-horizontal classifier, 9 unit tests |
| HUD | Horizon provenance line + FFI sigma | #11 | `976baf5` | `last_horizon_provenance: String`, `last_horizon_sigma_arcmin: f64` on FFI |
| Corpus | Moonlight-pond LOP regression (env-gated) | #12 | `7462c38` | First end-to-end real-capture regression in the workspace |
| Bug | Reflection-pair Test 1 gravity-axis fix | #14 | `935d646` | Sensor-landscape captures now fire reflection-pair (used 0 → 1 on corpus) |
| 6 | `fuse_horizon_hypotheses` (inverse-variance) | #13 | `6094d9d` | Weighted fusion + concordance gate + `HorizonProvenance::Fused` |
| Almanac | Lunar diurnal parallax (Meeus Ch. 40) | #15 | `bc2f6c3` | Moon Hc error: 53.5′ → 0.6′; corpus intercept 61 nm → 8 nm |

Phase 3 (cross-frame registration) and Phase 5 (IMU provider)
remain `TODO` in `plan.org`. The trait surface is ready for
both — they are additive providers with no architectural
prerequisite.

## What the corpus revealed

The Austin moonlit-pond capture (`bris-debug-
0019e5dd3922b89e328521193bb6f`, frame 10, 4032×3024) is the
first capture to exercise the full pipeline against a known
operator position. Walking through the intercept history is
how Phase 3.6 actually got debugged:

| State | Intercept | LOP | Notes |
|---|---|---|---|
| Initial (pre-#14) | −61.1′ | 61.13 nm | Engine path: `reflection_pair_used=0` (rejected by Test 1 gravity-axis bug); LOP from operator-centroid path |
| After #14 (gravity fix) | −61.1′ | 61.13 nm | Engine `used=1` for the first time; LOP value unchanged (geometric reduction was always axis-independent) |
| After #15 (parallax fix) | **−8.2′** | **8.18 nm** | Hc went from geocentric to topocentric; aligned with Skyfield to 0.6′ |

**7.5× accuracy improvement on the corpus from a single
diurnal-parallax fix**, isolated by cross-checking against
Skyfield/JPL DE421.

The journey also surfaced two real bugs in code that had been
green on synthetic tests for months:

- **`ReflectionPairProvider` Test 1** hardcoded gravity along
  pixel-y (portrait sensor mount). Every landscape capture
  was geometrically rejected. Fixed in #14 by introducing
  `Frame::gravity_camera_frame: Option<(f64,f64,f64)>`.
- **Stellar-only almanac validation hid the lunar parallax
  stub.** The TODO comment had been present since the first
  almanac commit. Fixed in #15.

The lesson: synthetic tests don't catch what only real-world
captures expose. The env-gated regression pattern in
`moonlight_pond_lop.rs` is now the template — each new
capture deserves a paired test.

## Where we are at the end of Phase 3.6

Per-fix accuracy on the only real-world capture we've reduced:

- **8.18 nm intercept against a known true position from a
  single Moon sight via reflection-pair**.
- σ reported by the pipeline: **0.92 nm**.
- σ vs reality: the actual residual exceeds the reported σ by
  ~9×. The pipeline is **over-confident** by an order of
  magnitude — honest-uncertainty hard rule violated. The
  per-stage σ contributions are not yet fully populated; see
  "Next steps" item 4.

The geometry is sound. The astronomy is now mostly sound. The
remaining error is split between operator-input quality
(centroids, gravity vector, eye-height) and missing per-stage
σ accounting.

## Next steps, in priority order

Ordered by expected impact on corpus intercept residual.
Each item carries an estimated post-fix residual contribution.

### 1. Sub-pixel centroid refinement (~3 nm → ~0.3 nm) — DONE

Landed on `feat/subpixel-centroid`. A 2D Gaussian fit on the
non-saturated halo (the existing `mean_intensity` boundary
machinery from #8) recovers the body centre to sub-pixel
resolution and reports a fit-covariance position σ.

On the saturated-disk integration synth
(`crates/bris-vision/tests/subpixel_centroid_regression.rs`),
the refined per-axis σ drops well below the 0.5 px integer
floor and the recovered centre lands within 0.5 px of truth.
Projected corpus contribution: integer-centroid residual
~3 nm → refined-centroid residual ~0.3 nm (driven by
the σ reduction from ~1 px to <0.3 px on a well-sampled
halo). The Austin corpus replay needs the refined fit
threaded through the per-stage σ chain (item 4) before the
LOP-residual drop can be measured end-to-end.

Lever lived in `crates/bris-vision/src/centroid_refine.rs`;
Stage A wiring is in
`crates/bris-streaming/src/pipeline/mod.rs::detect_day_body`.

### 2. Annual aberration (~0.3 nm)

Currently a fixed 20″ σ placeholder
(`ABERRATION_PLACEHOLDER_SIGMA_RAD` in `apparent.rs`).
Implementing Meeus Ch. 23 zeroes it out. The 20″ σ would
shrink to ~0.1″ residual.

Lever: `crates/bris-almanac/src/apparent.rs` — apply the
aberration correction between nutation and parallax in
`common_apparent_place`.

### 3. Lunar oblateness-corrected parallax (~0.5 nm at mid-lat)

The implemented parallax transform uses spherical-Earth `φ′ =
φ`, `ρ = 1`. WGS-84 oblateness shifts `φ′` by up to ~11′ and
`ρ` by ±0.003. For Moon this can introduce ~10–20″ altitude
residual at mid-latitudes.

Lever: `crates/bris-almanac/src/observer.rs` — add
geocentric-latitude conversion; thread `ρ` through the
parallax call.

### 4. Per-stage σ population (no intercept change, σ honesty)

The 0.92 nm σ the pipeline reports is far too tight. The
per-stage σ TODO at `plan.org` "Phase 4: Per-stage uncertainty
propagation" needs to land:
- Centroid σ from sub-pixel fit covariance (item 1 gives us
  this for free)
- Lens calibration σ from the calibration RMS (Cat S62
  profile has 0.733 px aggregate RMS)
- Refraction σ scaled by `dh/dh_apparent` near horizon
- Eye-height σ → dip σ (already done)
- Timing σ (negligible for Moon but real for stars)

Without this, every reported σ is a lie. Honest uncertainty
is a hard rule.

### 5. Reflector-region test (Test 5) for reflection-pair

Currently a deferred-from-Phase-1 TODO. On scenes where the
"reflector" is mistaken (a wet road vs the sea, or a window
vs water), reflection-pair will happily produce a horizon
that's wrong. Test 5 cross-checks the implied reflector plane
against the rest of the scene. For the Austin pond corpus
this would not have changed the intercept (the water plane
IS what we want), but it's load-bearing for false-acceptance
hardening before reflection-pair sees broad use.

Lever: `crates/bris-vision/src/horizon_providers/
reflection_pair.rs` — add Test 5 stage.

### 6. Multi-body fix from a single capture

The current corpus has only the Moon visible (twilight). A
fix needs ≥ 2 LOPs. Options:
- Multi-body scenes (Moon + bright planet or star at full
  dark)
- Time-separated sights of the same body that has moved (≥
  20 min apart for the Moon)
- Combining Sun + Moon during civil twilight
The streaming engine's sight-window machinery already
supports multiple sights; the bottleneck is capture
opportunities and Stage B's tendency to mistake bright peaks
for stars vs. bodies on real moonlit scenes.

### 7. Stage B Moon-over-water peak-storm fix

On the corpus the Night-mode peak detector produces ~16 000
peaks from moonlit water glitter, choking the reflection-
pair O(N²) Test 1 loop. Reduced to ~13 000 after #14 but
still excessive. A Moon-specific Stage B path (single
saturated disk + reflection candidate, not star-pattern
matching) would let the engine surface the right pair without
the operator hand-providing centroids.

Lever: `crates/bris-streaming/src/pipeline/body.rs` — add
Moon-class peak path gated on classifier output.

### 8. Cross-frame registration (Phase 3.6 Phase 3)

Already planned. Lets horizon providers accumulate evidence
across the rolling window rather than per-frame. Largest
benefit is for vanishing-point and vertical-line providers
that fire on transient line evidence; reflection-pair
benefits less because the Moon's image position changes
frame-to-frame and the reflection geometry has to be redone.

### 9. IMU provider (Phase 3.6 Phase 5)

`Sensor.TYPE_GRAVITY` plus the camera-mount rotation gives
an independent horizon hypothesis. Phone IMUs are noisy
(several degrees of bias under motion), but their σ is honest
and the fusion gate from #13 already handles weighting them
correctly. Useful as a sanity-check provider when optical
methods disagree.

Lever: new `crates/bris-vision/src/horizon_providers/imu.rs`
implementing `HorizonProvider`; Android side needs to push
gravity into `Frame::gravity_camera_frame` (the field exists
from #14).

## Stop-the-bleeding items still open

- **Stage B peak-storm**. The engine doesn't currently use
  reflection-pair effectively on real moonlit scenes because
  the peak detector produces too many candidates. Item 7
  above.
- **Pipeline σ over-confidence**. Item 4. The 0.92 nm σ
  reported on a sight that's actually 8 nm off is a hard-
  rule violation we know about.
- **No fix from one body**. Item 6. Single-LOP from a single
  scene is informative but not a fix.

## Acceptance for Phase 3.6 close

- All Phase 3.6 sub-tasks `DONE` except Phase 3 (cross-frame)
  and Phase 5 (IMU), both deferred.
- First real-capture regression test in the workspace, env-
  gated and skip-passing in CI.
- First end-to-end LOP from a real capture: 8.18 nm from
  known truth, error budget understood and prioritized.
- Two latent bugs (gravity-axis, lunar parallax) found and
  fixed by exercising the corpus.

Phase 4 (sight reduction & fix) and Phase 6 (calibration UI)
are the natural next phases. The accuracy improvements in
"Next steps" 1–5 can be done in any order and slot into
either phase's PR queue.
