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
  three horizon detectors (gradient / sky-region / segmentation),
  body centroiding (with optional mask), star-peak detector,
  cross-frame Harris+NCC+RANSAC stitching, end-to-end altitude
  measurement, **load-time rotation infrastructure for opt-in
  use by capture pipelines**, **day/night/twilight classifier**.
  Calibration *workflow* and a streaming-engine quality knob remain.
- Phase 4 (sight reduction & fix) — 3 of 4 tasks; running fix
  remains.
- Phase 5 (NMEA output) — 4 of 6 tasks; transport layer and OpenCPN
  integration test remain.
- Phase 6 (CLI) — `bris replay` subcommand exists but is **no
  longer the validation surface**; the regression-test harness is.
  Replay is kept as a manual smoke-test tool; not invested in.
  Capture, calibrate, fix, serve, log, update remain stubs.

**Phase 2.5 (real-data validation): 12 regression cases now in the
corpus** covering working day, working night-with-moon, working
day-with-shore-obstruction, expected-failure (sunrise, dense
star-field night, deck-light night), and clean-refusal (marina,
ambiguous sun glow). The user's full `test_video/` corpus is
exercised end-to-end.

**Not started:** Phase 1.5 (time integrity), Phase 3 (plate solving),
Phase 3.5 (continuous-operation engine, day/night detection wired
through the engine), Phase 7 (mobile frontends), Phase 8
(validation), Phase 9 (stretch).

**Workspace metrics:** 293 tests passing, 7 crates with active code,
zero clippy warnings, zero `cargo fmt` diffs. Last commit:
`7ece25a` — 10 regression cases promoted from `test_video/` corpus.

---

## What we proved this session

### TOML-driven regression test harness

`crates/bris-vision/tests/regression/*/case.toml` describes each
regression case; `build.rs` walks the directory at build time and
emits one `mod case_<name>` per case with one `#[test] fn` per
declared check. **Adding a new case is a TOML-write, not a
Rust-write.** Each generated test is named after the case + check
(`case_marina::horizon_segmentation_outcome`) so CI failures point
at the exact assertion.

The schema supports three case kinds:

- `kind = "working"` — pipeline should produce a usable result.
- `kind = "expected_failure"` — pipeline should refuse cleanly with
  a typed error. **Critical**: catches regressions where Bris
  starts fabricating output from scenes that don't have enough
  information.
- `kind = "expected_low_confidence"` — pipeline should produce
  output but flag wide σ.

Per-method horizon expectations carry an `outcome = "ok" | "err"`
plus optional `error_variant` substring matching, so `expected_failure`
cases can assert specific typed errors rather than vague "it failed."

### Portrait support, opt-in only (no aspect heuristic)

A `Rotation` enum + `rotate_pixels` primitive + `Frame::source_rotation`
field + `load_frame_from_path_with_rotation` lets capture pipelines
or fixtures explicitly declare a load-time rotation. The
`segment_with_rotation` function rotates source RGB before model
inference so segmentation masks align with rotated frames.

**Default is no rotation.** Aspect ratio cannot distinguish 4:3
landscape from 3:4 portrait, and phone JPEGs / conventional camera
images are saved in viewing orientation already (often after EXIF
orientation has been baked in). Auto-rotating based on aspect
silently broke the user's portrait `night_test_lowres` footage —
the photo is already in viewing orientation, the horizon runs
left-to-right across the bottom, and the pipeline handles it as-is
at 1080×1920. The retained rotation surface stays useful for
sensor-mounted-sideways captures (V4L2 / libcamera) and the
occasional sideways-stored fixture.

### Day/night/twilight classifier

`bris-vision::condition::classify(frame, sun_altitude_deg, cfg)`
returns a `Classification { condition, confidence, image_evidence,
astronomical_evidence, disagreement }`. Two evidence sources:

1. **Image**: mean luma over the middle horizontal third (avoids
   deck/sky bias) plus saturated-pixel fraction over the full
   frame. Day ≥ 0.30 mean luma, twilight 0.05 to 0.30, night below
   0.05; saturated-pixel fraction ≥ 0.5% forces day regardless of
   mean luma (so a saturated sun in a dim deck shot still
   classifies correctly).

2. **Astronomical prior** (optional): caller computes sun altitude
   from `bris-almanac` and passes it in. Maps to civil / nautical /
   astronomical twilight bands per Bowditch §22.

When both sources agree, confidence = max of the two underlying
confidences. When they disagree (e.g. dark image but sun should be
high — a covered camera, or bright image at midnight — a flashlight),
the classifier picks the more conservative (less-bright) condition,
sets `disagreement = true`, and clamps confidence ≤ 0.4 so callers
know not to trust the result. The classifier deliberately doesn't
depend on `bris-almanac`; the engine that has both crates as deps
does the almanac call once per batch.

### 12-case regression corpus exercising every detector

The full `test_video/` corpus has been promoted to the
regression-test surface. Each case records what the pipeline
*currently does* with explicit assertions; algorithm changes
produce visible diffs.

| Case | Kind | Highlights |
|---|---|---|
| `sailing_sun_upper_left` | working | Existing; deck-occluded horizon, sun upper-left |
| `sailing_with_distant_shore` | working | Existing; obstruction-aware horizon |
| `ambiguous_sun` | working | Diffuse glow; segmentation horizon OK; centroider-on-glow documented |
| `bokeh` | working | Bokeh defeats segmentation (zero candidates); sky-region works |
| `cloudy_sun` | working | Gradient + sky-region agree; segmentation declines on city skyline |
| `marina` | working | **Centroid correctly refuses** with NoBrightRegion; no fabricated body |
| `night_test_highres` | working | Stars; surprisingly sky-region + segmentation find dim horizon |
| `night_test_lowres` | working | **Portrait 1080×1920**; classifier says Night; **moon centroided successfully**; horizon detectors all fail (motivating night-horizon work) |
| `sunrise` | expected_failure | Sun on horizon defeats every horizon detector |
| `too_bright` | working | Gradient + segmentation agree; sky-region fails on sail glare |
| `container_ship_night` | expected_failure | Dense star field; every detector fails (canonical plate-solving target) |
| `container_ship_night_lights_on_water` | expected_failure | Adversarial: deck-light glow on water; should keep failing cleanly |

### Workflow tools

`crates/bris-vision/examples/probe_scene.rs` runs the full pipeline
+ classifier on a frame and prints structured results — used to
author each `case.toml` from real observations.

`crates/bris-vision/examples/convert_to_jpeg.rs` re-encodes
oversized PNGs to JPEG for the corpus 200 KB ceiling. Verified
empirically that quality 85 doesn't move centroids or horizon
parameters meaningfully.

---

## Honest limitations we know about

1. **The image-only classifier under-classifies night scenes as
   twilight** when there's any ambient light (deck lights, moon
   glow on sea). `night_test_highres` and both `container_ship_night*`
   cases all read as Twilight on luma alone; with the astronomical
   prior they correctly resolve to Night. This is the right
   behavior for the image-only path — overestimating brightness is
   safer than underestimating — but downstream code should consult
   the almanac before deciding which method set to invoke.

2. **`marina` makes the segmentation and sky-region detectors
   fabricate a horizon** along the shore-water boundary. The
   `correctness = "wrong"` field documents this, but the harness
   still asserts the line as `outcome = "ok"` because that's what
   the detectors return. The load-bearing assertion for `marina`
   is that **the centroid refuses** — the pipeline correctly
   declines to invent a body. A future "no usable horizon"
   classifier could promote this to a clean failure end-to-end.

3. **`sunrise` cannot produce a fix** because the sun-on-horizon
   defeats every horizon detector simultaneously (the sun itself
   blots out the sky→sea transition in the columns where it's
   visible). A body-excluding mask before horizon fitting would
   resolve this. Tracked under "exclude detected body from horizon
   candidates" in plan.org.

4. **All night scenes except `night_test_lowres` (which has a
   bright moon) currently fail end-to-end.** Centroid refuses, all
   three horizon detectors fail. This is the canonical case for
   plate solving + night-horizon detection.

5. **Brightness-weighted centroid + sky mask biases toward sky
   center of mass.** Documented before; the fix (peak detector
   inside sky mask) is a queued plan.org TODO. The corpus has the
   relevant test scene already; once the algorithm lands, the
   `sailing_sun_upper_left_sky_mask_centroids_to_sky_region` test's
   tolerance can be tightened.

6. **Placeholder camera intrinsics make absolute altitudes wrong**
   by a factor of ~2-3. Calibration workflow is unchanged.

7. **Single-LOP fix needs an `--assumed-position`.** Geometry, not
   a bug. Phase 3.5 streaming engine resolves this.

---

## Test footage available (in `test_video/`, gitignored)

The user's captured corpus has been fully exercised against the
pipeline and 10 of the 11 scenes promoted to regression cases.
`orig_test_video/` (603 frames of the same scene as
`sailing_sun_upper_left`) is the lone remaining unrepresented one,
and it's redundant with the regression case it duplicates — kept
as a source for future multi-frame stitching tests when needed.

The full `test_video/` directory remains gitignored (~10 MB of
PNG); the regression corpus carries one or two representative
frames per scene.

---

## Next concrete steps (recommended ordering)

### Highest-leverage algorithm work, motivated directly by the corpus

1. **Sun/Moon peak detection inside sky mask.** Fixes the
   documented brightness-bias on `sailing_sun_upper_left` and the
   wrong-centroid behavior on `too_bright` and `ambiguous_sun`.
   Small change: replace the connected-component centroider with
   `detect_peaks` constrained to the sky-mask region. ~1 work
   session. Updates 3 regression cases on landing.

2. **Night-horizon detection v1: sea-sky luma boundary.** Smallest
   independent change (no new infrastructure dependency). Find the
   horizontal band of maximum luma transition in the lower portion
   of the frame; should work on `night_test_lowres` (where the
   moon illuminates the sea-sky boundary) and possibly on
   `container_ship_night` (faint horizon with star-density
   transition). Will *not* work on `container_ship_night_lights_on_water`
   — that case is the regression floor: if any future detector
   produces a horizon there, it's probably wrong.

3. **Plate solving (Phase 3).** Tetra3-style 4-star geometric
   hash matcher. Unlocks night fixes from peak detections on
   `night_test_highres` and `container_ship_night`. Requires the
   star catalog import (already done).

### Larger pieces

4. **Body-excluding mask before horizon fitting.** Resolves
   `sunrise`. Probably reuses the segmentation mask but would also
   work with the body's centroid + a circular dilation.

5. **Streaming engine + continuous-operation engine** (Phase 3.5).
   Reads frames continuously, classifies day/night/twilight,
   accumulates sights, publishes fixes when ≥2 azimuth-diverse
   sights are available.

6. **NMEA transport** (Phase 5 remainder).

7. **Live camera capture** (Phase 6) — V4L2 on Linux. The
   capture-side rotation surface (`load_frame_from_path_with_rotation`
   already in place) means a sideways-mounted sensor can be
   handled without pipeline changes.

8. **Train a Bris-specific segmentation model**. Substantially
   reduces binary size and improves accuracy in the dark / bokeh /
   shore-on-horizon cases that the current ADE20K-trained model
   handles poorly.

---

## Open questions

1. **What sun-altitude lookup goes where in the eventual streaming
   engine?** The classifier takes it as a parameter; the engine
   has both `bris-vision` and `bris-almanac` and does the call
   once per batch. The exact engine API is Phase 3.5.

2. **`marina`'s shore-fabricated horizon** — should the harness
   assert `outcome = "ok"` (what currently happens) or
   `outcome = "wrong"` (what we'd prefer)? The schema doesn't
   currently distinguish "this output is technically OK but
   navigationally wrong"; the `correctness = "wrong"` field is
   documentation only. Adding a `"navigation_correct"` flag to the
   harness would let `marina` and similar cases assert the right
   thing.

3. **Should the centroider's known-wrong outputs on
   `too_bright` / `ambiguous_sun` / `marina` (when not refusing)
   be regression-asserted or removed?** Currently we assert the
   recorded values to detect drift; once peak-in-sky-mask lands,
   those positions will move and the cases will need updating.
   Worth noting that the current assertions are
   detection-of-drift, not target-of-correctness.
