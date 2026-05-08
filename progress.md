# Bris progress

Status snapshot. Updated as work proceeds.

For the full design and per-task roadmap, see `plan.org`.
For the project overview, see `readme.org`.
For the end-to-end pipeline architecture and data flow, see
`docs/design/pipeline.md`.

---

## Current status

**Phases done:**
- Phase 0 (scaffolding) — complete.
- Phase 1 (almanac) — 8 of 9 tasks; only Pi Zero benchmark remains.
- Phase 2 (vision) — substantively complete: image I/O, lens model,
  three daylight horizon detectors (gradient / sky-region /
  segmentation), **night-horizon detector v1** (sea-sky luma
  boundary), body centroiding (extended-disk + saturated-body),
  star-peak detector, cross-frame Harris+NCC+RANSAC stitching,
  end-to-end altitude measurement, load-time rotation
  infrastructure, day/night/twilight classifier, **column-mask
  surface for body-excluding horizon detection**. Calibration
  *workflow* and a streaming-engine quality knob remain.
- **Phase 3 (plate solving) — complete.** Tetra3-style geometric
  hash database + Kabsch-based pose solver + sub-arcmin residual
  refinement + per-star altitude extraction. End-to-end synthetic
  round-trip works; the night plate-solving pipeline now reaches
  from peak detection through identified-stars to altitude
  observations consumable by `bris-nav::sight`.
- Phase 4 (sight reduction & fix) — 3 of 4 tasks; running fix
  remains.
- Phase 5 (NMEA output) — 4 of 6 tasks; transport layer and OpenCPN
  integration test remain.
- Phase 6 (CLI) — `bris replay` subcommand exists but is **no
  longer the validation surface**; the regression-test harness is.
  Replay is kept as a manual smoke-test tool; not invested in.

**Phase 2.5 (real-data validation): 13 regression cases** spanning
working day, working night-with-moon, working
day-with-shore-obstruction, working dusk-with-occluded-body
(marina_with_body), expected-failure (sunrise, dense star-field
night, deck-light night), and clean-refusal (marina without body
assertions, ambiguous sun glow). The user's full `test_video/`
corpus is exercised end-to-end. Algorithm work is now driven by
specific, named failure modes from the corpus.

**Not started:** Phase 1.5 (time integrity), Phase 3.5
(continuous-operation engine + day/night classifier integration),
Phase 7 (mobile frontends), Phase 8 (validation), Phase 9
(stretch). Phase 3 is complete except for two minor follow-ups
(magnitude-consistency verification check, observer-location
external prior) that are queued.

**Workspace metrics:** 335 tests passing + 3 ignored
(slow/release-only), 7 crates with active code, zero clippy
warnings, zero `cargo fmt` diffs. Last commit: `0c68910` —
per-star altitude extraction closes Phase 3.

---

## What we proved this session

This session diagnosed why real-data plate-solving fails on the
corpus and made the diagnosis sharp enough to commit
infrastructure even before calibration lands. Five concrete
deliverables:

### `detect_peaks_above_horizon` in `bris-vision::peak`

Gates the peak detector's local-max search to pixels above an
optional horizon line, with a configurable safety margin (px
above the line, default 5 in the streaming engine) to exclude
the horizon's own gradient and silhouetted shipboard structure.
The unmasked `detect_peaks` is unchanged for sky-pointed frames
that genuinely contain no horizon — the streaming engine passes
`Some(line)` when Stage C succeeded and `None` when it didn't,
preserving the no-horizon-frame path.

Three new unit tests
(`detect_peaks_above_horizon_excludes_below_line`,
`horizon_margin_excludes_pixels_just_above_line`,
`sloped_horizon_masks_per_column`) cover the contract.
**Empirical impact** on the high-res `night_test_highres`
screenshot: top-12 peaks went from "all wake glints clustered in
groups of 3 within 25 px" to "isolated stars in the upper sky
band with proper intensity falloff" (verified via the new
`annotate_peaks` example which draws peak boxes + horizon line
on the source PNG).

### `bris-streaming` Stage B/C reordering

Stage C (horizon) now runs before Stage B (body) for all frames
so the night/twilight peak detector can consume the horizon
when found. Stage B's night path takes a new
`Option<HorizonLine>` parameter; `EngineConfig` gains
`peak_horizon_margin_px` (default 5). The twilight-fallback
unit test was updated to call `detect_twilight` directly with
`horizon: None` so it doesn't fight a synthetic frame's
spurious horizon detection.

### `plate_solve` 20× speedup (42.5s → 2.13s per call on real data)

samply-profiled and attacked the three biggest hot paths:

1. **Removed `normalize()` of orthonormal-rotated unit vectors**
   in the per-star verification loop (~27% savings). The Kabsch
   rotation is orthonormal and the input `cv` is a unit vector,
   so the result is unit-length to within float epsilon — the
   downstream dot-product comparisons are tolerant of the ε
   residual.
2. **Hoisted catalog-cone filtering and `cos_match`** out of the
   per-permutation loop into perm-invariant precompute (~37%
   savings). The 4-tuple's centroid is the same regardless of
   permutation, so the `~1600 → ~50-100` cone filter runs once
   per `try_verify` instead of 24 times. Required adding
   `StarHashDb::verify_stars()` returning a flat
   `&[VerifyStar]` with cached unit vectors.
3. **Reduced 24 → 2 permutations** via geometric distance-
   footprint matching (`pick_geometric_permutations`,
   ~90% savings of the remainder). Each star's
   "footprint" — sum of dot products to the other 3 in the
   tuple — is a permutation-invariant scalar; sorting catalog
   and peak stars by footprint and matching by rank gives the
   canonical correspondence; the chiral mirror (swap middle two
   ranks) covers the other valid assignment. The other 22
   perms align stars whose pairwise distances don't even
   approximately match. Falls back to all-24 via new
   `PlateSolveConfig::exhaustive_permutations` knob; default
   fast path.

All 20 platesolve unit tests still pass, including the load-
bearing `round_trip_recovers_known_attitude` (canary that the
geom-perm heuristic correctly identifies the right
correspondence on synthetic input).

### End-to-end synthetic-frame pipeline test (`examples/synth_frame_test.rs`)

Bridges the gap between the in-memory `round_trip` unit test
(peaks computed and handed directly to `plate_solve`, no peak
detector in loop) and the real-frame regression cases (PNG →
load → detect_peaks → plate_solve). Projects catalog stars
through a known attitude into a PNG, then runs the *full* real-
frame pipeline on it. Works in 34ms with 8 identified stars,
attitude recovered to within 4 arcsec of truth.

This rules out bugs in the chain: PNG loading, frame format
conversion, peak detector output format, unit conversions, hash
db lookup, geom-perm heuristic, Kabsch math. The chain is
correct end-to-end.

### Confirmed: lens distortion is the real-data plate-solve blocker

Re-ran `synth_frame_test` with `RENDER_K1=-0.10` (camera-side
distortion) and `SOLVE_K1=0` (assumed in solver): **complete
failure** (`best: 0` — no candidate ever passed even 3
verifications). With matched `RENDER_K1=SOLVE_K1=-0.05`: works
fine. With matched `-0.10`: still fails (best: 2, just shy of
threshold) — the iterative `undistort_pixel` may not converge
well at high distortion magnitudes.

Mechanism: the plate-solver hashes 4-tuple pairwise distance
ratios. Pixel-to-ray angles depend on the lens model
(`fx` + `k1..k3` + `p1..p2`). Wrong `fx` scales all distances
by a constant — survivable in principle but our hash has fixed
bin tolerance. **Wrong distortion warps pairs differently
depending on radial position** — the 4-tuple's distance ratios
shift between bins, and no bucket lookup ever returns the right
pattern.

Real wide-angle marine cameras routinely have `k1 ∈ [-0.1, -0.3]`,
which is exactly where plate-solve breaks down without
calibration. The corpus videos (JeffHK's 30-day timelapse, the
ASMR cruise-ship channel) are YouTube re-encodes that have
stripped all camera EXIF, so we cannot recover lens parameters
from file metadata.

**Conclusion:** real-data `[plate_solve]` regression cases stay
`outcome = "err"` until the lens calibration workflow lands.
This is a *correct refusal* by the solver — not a bug, and not
something fx-tuning alone can fix.

### Three new diagnostic examples

- `examples/probe_intrinsics.rs` — fx sweep + horizon-masked
  peak detection + optional `ALL_PERMS=1` and `MIN_INTENSITY=N`
  env knobs. The empirical-tuning tool from this session.
- `examples/synth_frame_test.rs` — projects catalog stars
  through known attitude/intrinsics to a PNG, runs the full
  pipeline, supports `RENDER_K1`/`SOLVE_K1` env knobs for the
  distortion test described above.
- `examples/bench_solve.rs` — minimal samply target: load
  frame, detect peaks, time three plate_solve runs.
- `examples/annotate_peaks.rs` — draws detected peak boxes
  (red for top-12, orange for rest) and the horizon line
  (green) onto a PNG. Diagnostic visualization.

---

## What we proved last session

### Plate-solver refinement: sub-arcmin residual gate

After the initial 4+verified-N match, the solver now re-runs
Kabsch on all matched pairs and computes the RMS angular residual
under the refined rotation. If the RMS exceeds
`max_rms_residual_rad` (default 30 arcsec), the match is
rejected as a false positive. This catches the v1 failure mode
where loose 1° verify radius accepted geometrically-self-consistent
but wrong sky regions: on `night_test_highres` the v1 returned a
12-star "match" in the southern hemisphere; refinement correctly
rejects it.

A new unit test `refinement_rejects_random_peak_positions`
asserts the solver returns Err on synthetic random peak
positions. Without refinement it would have returned a self-
consistent fabricated match.

Real-data behavior post-refinement: with placeholder intrinsics
the *correct* sky region also fails refinement (residuals can't
be sub-arcmin when the pixel→ray mapping is wrong by 2-3×), so
the post-refinement behavior on real footage is "no match
returned" until calibration arrives. This is the *right* answer.

### Multi-pass night-horizon detection

`detect_horizon_night_multi_pass` enumerates the top-N
horizontal luma transitions by repeatedly: find strongest, mask
its row neighborhood, find next-strongest. Returns candidates
sorted by RANSAC inlier count (best first).

Empirical:
- `night_test_highres`: single-pass found the wake region (66
  inliers); multi-pass surfaces the actual sea-sky horizon (195
  inliers) as the top candidate.
- `container_ship_night`: single-pass found the deck top (105
  inliers); multi-pass surfaces the sea horizon (164 inliers)
  first.
- `night_test_lowres`: still hard. The moon halo's transitions
  dominate even after multi-pass exclusion; the explicit
  `search_row_range` path remains the only working approach.
- `container_ship_night_lights_on_water`: deck-light glow on
  water produces multiple candidates in the deck/water region;
  none is the actual horizon. Remains expected_failure.

Two new corpus regression tests assert the multi-pass top
candidate matches the actual horizon on the two scenes where
it works.

### Per-star altitude extraction

`bris-platesolve::altitude::star_altitudes` converts each
identified star from a `PlateSolveResult` into an altitude
observation against an independently-measured horizon line.

Math: catalog J2000 unit vector × attitude → camera-frame ray
→ altitude via the existing horizon-plane geometry. A new sister
function `bris-vision::measure_altitude_from_ray` takes a
precomputed ray instead of a centroid pixel; the centroid-driven
`measure_altitude` is a thin wrapper for backward compatibility.

Below-horizon stars are silently skipped (they don't contribute
observations but aren't an error condition for the batch). 3
unit tests cover the synthetic cases; real-data validation
requires calibrated intrinsics.

This **closes Phase 3** structurally. The night plate-solving
pipeline now reaches from peak detection through identified-stars
to altitude observations that `bris-nav::sight` can consume.

### Plate solving (Phase 3 v1)

`bris-platesolve` is no longer a stub. End-to-end pipeline:

- `StarHashDb::build` enumerates 4-star patterns from the
  embedded BSC catalog using a neighborhood approach
  (`O(N · M^3)` per anchor, where M = `neighbor_limit`),
  computes a quantized hash from the 4 normalized pairwise
  distance ratios, and stores in a `HashMap<hash,
  Vec<CatalogPattern>>`. The neighborhood approach is essential
  — naïve `O(N^4)` enumeration is intractable at the catalog
  densities Bris targets.

- `plate_solve` maps detected peaks to camera-frame unit rays,
  enumerates 4-tuples of the brightest peaks, hashes each,
  looks up matching catalog patterns, and verifies via
  Kabsch-based pose recovery (closed-form least-squares
  rotation; SVD via Jacobi rotation, no external linear-
  algebra dep) plus projection of additional catalog stars
  with one-to-one peak↔star matching.

- The synthetic round-trip test (`#[ignore]` for slow debug;
  passes in release in ~30s) builds a mag-4.0 db, picks a known
  sky region, constructs a known attitude, projects in-cone
  stars through it, and verifies the solver recovers an
  attitude that maps the aim point near +Z.

- Real-data probes (`tests/real_data.rs`, also `#[ignore]`)
  load `night_test_highres` and `container_ship_night` from the
  bris-vision corpus, run the full pipeline, and report the
  best match. The solver runs end-to-end and produces 12+
  identified stars on `night_test_highres`. Whether the matched
  sky region is the *correct* one is a separate question that
  requires tighter verification (sub-pixel residual RANSAC),
  stricter match radius, or external priors — tracked as the
  Phase 3 verification-refinement follow-up.

- An empirical finding caught during real-data probing: the
  initial verification accepted multiple catalog stars matching
  the same peak (4+ at one pixel on `night_test_highres`),
  inflating verification counts and producing self-consistent
  but meaningless matches. Fix: enforce one-to-one peak↔star
  matching in the verification loop (`a33ddea`).

### `centroid_saturated_body_in_mask` for Sun/Moon localization

A new entry point (`centroid_saturated_body_in_mask`) thresholds
at an absolute saturation level (default 95% of `u16::MAX`)
rather than a fraction of the frame's brightest pixel. This
isolates the saturated body's disk from the bright haze around
it, which the previous extended-disk centroider was confusing
into one big component (the documented (122, 64) drift on
`sailing_sun_upper_left`).

**A surprise from the corpus pass:** the ADE20K-trained
segmentation model classifies the saturated Sun as something
*other* than sky — likely "light" or one of the indoor classes.
Constraining the saturated centroider to the sky mask therefore
*excludes* the actual Sun pixels. The unmasked saturated centroider
works better in practice; saturation thresholding is itself
restrictive enough to exclude most non-body pixels. The mask is
useful when there are competing saturated regions (sail glare,
water glare) but not for the canonical Sun/Moon case.

On `sailing_sun_upper_left` the new function lands at ~(99, 45) —
sub-pixel close on x, ~3 px high in y because saturation extends
slightly into brighter sky above the disk. On `night_test_lowres`
it cleanly finds the Moon at (454.17, 349.66) (sub-pixel close to
the unmasked extended-disk centroider's output, confirming this
is the right algorithm). On `marina` it correctly refuses with
`NoBrightRegion` — the load-bearing "pipeline doesn't fabricate"
assertion.

5 unit tests + 3 regression tests cover the new function.

### Body-excluding column mask for horizon detection

All three daylight detectors and the new night detector gain an
optional `column_mask: Option<&[bool]>` parameter that skips
specified columns during candidate generation. The companion
`body_column_mask(frame_width, body_x, body_radius_px, pad_px) ->
Vec<bool>` builds the mask from a detected body centroid +
apparent radius (from `sqrt(area/π)`).

This unsticks the canonical low-altitude-body case: when a
saturated body sits on or near the horizon, it blots out the
sky→sea transition in those columns and the detectors fail.
Excluding the body's columns lets the remaining columns produce a
horizon fit. On `sunrise` with body-exclusion + a relaxed
`min_inlier_fraction = 0.3`, the gradient detector finds the
horizon at intercept ≈ 241; the sun centroid at y ≈ 226 is ~15
px above the horizon — correct geometry for sunrise. RMS 2.35 px
correctly translates to elevated altitude σ; low-altitude bodies
carry irreducible refraction-model uncertainty.

On `bokeh` and `cloudy_sun` the body-excluding mask also
meaningfully improves horizon outcomes by removing spurious
gradient votes from the body's halo.

6 new unit tests + 1 regression test
(`sunrise_horizon_findable_with_body_exclusion_and_relaxed_ransac`)
cover the new surface.

### `night_horizon` module (sea-sky luma boundary detector)

A new detector for low-light scenes where all three daylight
detectors fail. It works on the smoothed *per-row* mean luma
profile rather than per-column gradients:

1. Per-row mean luma over a horizontal center band (vignetting
   protection), optionally honoring a column mask.
2. Smooth with a small box filter.
3. Find the row of maximum vertical gradient in the configured
   `search_row_range`.
4. For each column, find the row in a window around the global
   horizon row where per-column gradient is largest.
5. RANSAC-fit through the candidates.

Critically, the column mask is honored in step 1 — excluding
masked columns from the per-row mean is essential when the mask
covers a saturated body, otherwise the body's bright pixels skew
the per-row profile and the global gradient peak lands on the
body's row instead of the horizon's.

The convenience function `detect_horizon_night_excluding_body`
builds both the column mask AND restricts `search_row_range` to
"below the body," assuming the body is above the horizon (the
usual case at night).

**Empirical results on the night corpus:**

- `night_test_lowres` (moonlit; portrait 1080×1920): default
  config catches the moon halo's edge at y ≈ 258. With manually-
  tuned `search_row_range = (0.55, 1.0)` + `min_inlier_fraction
  = 0.2`, finds the actual sea-sky horizon at y ≈ 1324 (~69% of
  frame height). The new test
  `night_test_lowres_horizon_findable_with_tuned_search_range`
  documents this.
- `night_test_highres` (stars over wake): finds y=180
  (wake-region bright transition), not the actual y=85 horizon.
  Segmentation detector handles this scene better.
- `container_ship_night`, `container_ship_night_lights_on_water`:
  finds the deck-to-sky boundary, not the sea-sky horizon. A
  future deck-excluding mask (parallel to `body_column_mask` but
  for row ranges) would resolve this.

The module docstring is honest about these limitations: the
detector finds the **strongest horizontal luma transition** in the
search range, which is sometimes the horizon and sometimes a
deck/wake/halo edge. Distinguishing them requires either manual
scene context (the `search_row_range` knob), a multi-pass
algorithm (find strongest, mask its neighborhood, find
next-strongest), or combining with the segmentation detector for
sky/sea class priors. All three are queued as follow-ups.

5 unit tests + 2 regression tests cover the new detector.

### `marina_with_body` regression case

The user pointed out that the `marina` scene captured at dusk has
a Moon visible in the upper portion of the frame, partially
obscured by the rigging of one of the moored sailboats — and the
rigging swings as the boat sways. Adding a new case demonstrates
two complementary behaviors:

1. **Peak detection finds non-saturated bodies** that the
   extended-disk centroider misses. The Moon at dusk is bright
   (peak intensity ~43000) but doesn't form a 50+ pixel connected
   component at the centroider's `0.85·frame_max` threshold. Peak
   detection finds it cleanly at (415.88, 111.77).
2. **Single-frame detection isn't enough** when the body is
   intermittently obscured. Across 21 captured frames the rigging
   swings across the Moon between frames 17 and 18, dimming the
   detected intensity from ~43000 to ~29000 (a 33% drop). The
   peak isn't gone — the rigging is partially transparent — but
   the intensity drop is the signal a temporal-tracking algorithm
   would use to know the body is being obscured.

Three frames at different points of the sway cycle:
- `frame_visible.png`: clear Moon at intensity ~43000.
- `frame_partial.png`: Moon at ~41000; rigging starting to cross.
- `frame_obscured.png`: Moon at ~29000; rigging substantially
  across.

This is the motivating case for the streaming engine's
cross-frame predictive-tracking work (Phase 3.5): the Phase 2
panorama stitching machinery is the foundation, but predictive
tracking through temporal occlusions is the missing piece.

2 new static regression tests + 4 generated tests cover the case.

---

## Honest limitations we know about

1. **The image-only classifier under-classifies night scenes as
   twilight** when there's any ambient light (deck lights, moon
   glow on sea). With the astronomical prior they correctly
   resolve to Night. The classifier reports this is the right
   image-only behavior; downstream code should consult the almanac
   before deciding which method set to invoke.

2. **`marina`'s shore-fabricated horizon** still asserts
   `outcome = "ok"` despite being navigationally wrong. The
   `correctness = "wrong"` field is documentation only. The
   harness has no current way to assert "output is technically OK
   but navigationally wrong"; this is an open schema question.

3. **`sunrise` cannot produce a fix under default config** because
   the horizon detectors' default `min_inlier_fraction = 0.5` is
   too strict for the legitimately-noisier low-altitude scene.
   With body-exclusion + a relaxed config, the horizon is
   findable; documented in the
   `sunrise_horizon_findable_with_body_exclusion_and_relaxed_ransac`
   regression test. The path forward is auto-relaxing the
   inlier-fraction threshold when low-altitude conditions are
   detected, or per-method config overrides at the case.toml
   level.

4. **The night_horizon detector finds the strongest horizontal
   luma transition, not necessarily the sea-sky horizon.** On
   shipboard footage (`container_ship_night*`) it lands on the
   deck-to-sky boundary; on wake footage (`night_test_highres`) it
   lands on the wake region. Distinguishing requires multi-pass
   detection, a deck-excluding row-range mask, or a sky/sea
   segmentation prior — all queued as follow-ups.

5. **No plate solving yet**, so star identification doesn't work.
   The peak detector finds star-like points; plate solving is the
   next major piece. The `container_ship_night` scene is the
   canonical target.

6. **Brightness-weighted centroider on `too_bright` is hopeless**
   because the sail glare, water glare, and sun all merge into one
   13000+ pixel saturated region. The case records the wrong-but-
   stable centroid as documentation. The right fix would be peak
   detection inside a vessel-excluding mask, but the segmentation
   model's vessel class doesn't perfectly capture sail-with-glare;
   a Bris-trained model would resolve this.

7. **Placeholder camera intrinsics make absolute altitudes wrong**
   by a factor of ~2-3. Calibration workflow is unchanged.

8. **Single-LOP fix needs an `--assumed-position`.** Geometry, not
   a bug. Phase 3.5 streaming engine resolves this.

---

## Test footage available (in `test_video/`, gitignored)

The user's full captured corpus has been exercised against the
pipeline:

- 11 of 12 scenes promoted to regression cases.
- The 12th (`orig_test_video/`) duplicates `sailing_sun_upper_left`
  and is reserved as a multi-frame source for future stitching
  validation.
- The `marina` scene contributed both a body-less case (original
  `marina`) and a body-with-occlusion case (`marina_with_body`,
  three frames showing the rigging-sway cycle).

Total corpus size: ~2.1 MB across 13 cases. Average ~150 KB per
case.

---

## Next concrete steps (recommended ordering)

### Plate-solver minor follow-ups (Phase 3 polish)

Phase 3 is structurally complete; two low-priority refinements
remain:

1. **Magnitude consistency check** in verification: catalog
   magnitudes for matched stars should correlate with peak
   intensities. A bright peak matched to a dim catalog star (or
   vice versa) is a flag.

2. **External prior** (Phase 7-relevant): when an observer
   location and capture time are available, only consider
   candidate matches whose sky region is above the horizon at
   that observer/time.

Both are quality-of-output improvements rather than
correctness fixes; the refinement-residual gate already filters
the most egregious false matches.

### Algorithm refinements motivated by the corpus

1. **Lens calibration workflow** — *the single highest-impact
   remaining item for real-data plate-solving*, confirmed by
   the distortion-injection synthetic test (this session). Once
   landed, the corpus `[plate_solve]` cases flip from
   `outcome = "err"` to `outcome = "ok"` and we get end-to-end
   night-fix coverage. See `plan.org` Phase 2 calibration TODO.

2. **Deck-excluding row-range for night detector** — analogous to
   `body_column_mask` but for excluding a row range below a
   detected deck top. The multi-pass detector handles
   `container_ship_night` already (the deck top is found in
   pass 1, the actual horizon in pass 2 with stronger
   consensus); deck-exclusion would be cleaner still.

3. **Combine night detector with segmentation prior** — when the
   segmentation model produces a sky/sea boundary on a night scene
   (it sometimes does, e.g. `night_test_highres`), use that
   directly; fall back to the luma-boundary detector when
   segmentation fails.

4. **Auto-relax inlier-fraction for low-altitude scenes** — when
   a body is detected near the horizon (within a configurable
   altitude threshold), relax the horizon detector's
   `min_inlier_fraction` automatically. Resolves `sunrise` under
   default config.

### Larger pieces

5. **Streaming engine** (Phase 3.5). Architecture and
   implementation order are documented in
   `docs/design/frame_scheduling.md`; plan.org's Phase 3.5 list
   tracks the per-task scoping. Key decisions already made:
   staged-pipeline-with-per-stage-early-rejection, two parallel
   detection queues (body + horizon), lazy stitching at pair
   selection, ring buffer for raw frames, sight window with
   diminishing-returns-aware cap. New `bris-streaming` crate
   will hold the engine; existing crates remain unchanged.

6. **NMEA transport** (Phase 5 remainder).

7. **Live camera capture** (Phase 6) — V4L2 on Linux. The
   capture-side rotation surface is already in place.

8. **Train a Bris-specific segmentation model**. Substantially
   reduces binary size and resolves the documented "model excludes
   saturated bodies from sky class" failure on `sailing_sun_upper_left`,
   the segmentation-zero-candidates failure on `bokeh`, and the
   sail-vs-vessel ambiguity on `too_bright`.

---

## Open questions

1. **What sun-altitude lookup goes where in the eventual streaming
   engine?** The classifier takes it as a parameter; the engine
   has both `bris-vision` and `bris-almanac` and does the call
   once per batch. The exact engine API is Phase 3.5.

2. **`marina`'s shore-fabricated horizon** — should the harness
   gain a `"navigation_correct"` flag distinct from `outcome`?
   Currently `marina` and `marina_with_body` both record the wrong
   horizon as `outcome = "ok"` with `correctness = "wrong"` for
   documentation. A typed flag would let CI catch the case where
   navigation correctness regresses without the output-shape
   regressing.

3. **Per-method config overrides in `case.toml`?** The `sunrise`
   case currently needs a custom Rust test for the
   body-exclusion + relaxed-RANSAC path. A `case.toml` mechanism
   for declaring per-method config overrides would let
   `expected_failure` cases that succeed under non-default config
   be recorded declaratively. Worth adding when the second case
   needs it.

4. **Cross-frame predictive tracking** for the `marina_with_body`
   scenario. The peak detector sees the Moon dim from 43000 to
   29000 as the rigging swings across; a Kalman-style track over
   recent frames could maintain a position estimate and a
   confidence weight that drops with intensity, then reweight when
   the body reappears clearly. This is Phase 3.5 streaming-engine
   work but worth flagging as a specific design problem motivated
   by real corpus footage.
