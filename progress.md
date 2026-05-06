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

**Not started:** Phase 1.5 (time integrity), Phase 3 (plate solving
— next up), Phase 3.5 (continuous-operation engine + day/night
classifier integration), Phase 7 (mobile frontends), Phase 8
(validation), Phase 9 (stretch).

**Workspace metrics:** 312 tests passing, 7 crates with active
code, zero clippy warnings, zero `cargo fmt` diffs. Last commit:
`e8bed24` — `marina_with_body` case demonstrating peak-detection
of the dusk Moon behind swinging rigging.

---

## What we proved this session

This session pushed forward four algorithm pieces, all motivated
by specific corpus failure modes from the previous session's
data-collection pass.

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

### Plate solving (Phase 3) — the next major piece

Tetra3-style 4-star geometric hash matcher against the embedded
Yale BSC catalog. Unlocks night fixes from peak detections on
`night_test_highres` and `container_ship_night`. Requires:

- Build-time hash database generation (one entry per 4-star
  pattern within a configurable FOV; hashed on the 4 pairwise
  distances or distance ratios).
- Runtime matcher (for each 4-tuple of bright peaks, compute hash,
  look up matches, verify by additional star geometry, output
  camera RA/Dec/roll if confident).
- Per-star altitude extraction (using camera pose + identified
  star RA/Dec, compute the star's altitude in the frame; combine
  with the measured horizon for an altitude observation).

This is a substantial implementation (~few hundred LOC + a
build.rs database generator). Probably 4-6 commits if done in
focused chunks.

### Algorithm refinements motivated by the corpus

1. **Multi-pass night horizon detector** — find strongest
   gradient, mask its neighborhood, find next-strongest. The
   horizon is often the second-strongest when a deck or saturated
   body is in frame.

2. **Deck-excluding row-range for night detector** — analogous to
   `body_column_mask` but for excluding a row range below a
   detected deck top. Resolves `container_ship_night*`.

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

5. **Streaming engine + continuous-operation engine** (Phase 3.5).
   Reads frames continuously, classifies day/night/twilight via
   the existing classifier, accumulates sights, publishes fixes
   when ≥2 azimuth-diverse sights are available. Includes
   cross-frame body tracking for the `marina_with_body` motivation.

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
