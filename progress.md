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
  measurement. Calibration *workflow* and a streaming-engine quality
  knob remain.
- Phase 4 (sight reduction & fix) — 3 of 4 tasks; running fix
  remains.
- Phase 5 (NMEA output) — 4 of 6 tasks; transport layer and OpenCPN
  integration test remain.
- Phase 6 (CLI) — `bris replay` subcommand operational; everything
  else (capture, calibrate, fix, serve, log, update) still stubs.

**Not started:** Phase 1.5 (time integrity), Phase 3 (plate solving),
Phase 3.5 (continuous-operation engine, day/night detection), Phase 7
(mobile frontends), Phase 8 (validation), Phase 9 (stretch).

**Workspace metrics:** 25 commits, **198 tests passing**, 6 crates with
active code, zero clippy warnings, zero `cargo fmt` diffs.

**Last commit:** `3806eab` — sailing_with_distant_shore regression
case proving the obstruction-aware horizon detector adds 168
distant-shore columns to the 162 clean sky→sea columns on real
footage.

---

## What we proved this session

### Pipeline architecture is now concrete and documented

**`docs/design/pipeline.md`** has a Mermaid diagram and per-component
status table for the entire camera→fix flow. Key clarifications it
captures:

- **Horizon and body detection run per-frame**, not on a stitched
  panorama. The panorama path picks the best per-frame results, and
  uses cross-frame stitching only when the body and horizon are in
  different frames. Single-frame path is the common case and the
  fast path.
- **The stitching window is bounded by accuracy tolerance**, not
  frame count. At 30-60 fps a ~1 second window keeps body sidereal
  motion below per-sight σ for a 0.5 nm fix.
- **Three method sets:** day, night, twilight. Day is operational;
  night (peak detection + plate solving) is partially built (peak
  detector exists; plate solver doesn't); twilight is the hybrid that
  hasn't been designed.
- **Single-LOP "fix" is a hack** in `bris replay` (synthesizes a fake
  perpendicular LOP) so a single-body sight produces a position. Real
  fixes need ≥ 2 azimuth-diverse sights from the streaming engine.

### ML segmentation works on the deck-occluded shipboard case

Both classical horizon detectors fail on real shipboard footage where
the deck occupies the lower half of the frame:

- **Gradient detector** picks the deck-to-sea boundary on the left
  rather than the sea-sky horizon on the right (deck has stronger
  horizontal gradient than the small visible horizon).
- **Sky-region detector** picks the top of the mainsail (sky→sail-edge
  is a stronger linear feature than sky→sea).

The ML segmentation detector (SegFormer-B0 / ADE20K via `ort`) cleanly
identifies sky/sea/boat/ship and the per-column sky→sea transitions
yield a robust horizon. **172 of 512 columns** produce clean
candidates on the original sailing scene; **330 of 512** with
obstruction tolerance on the new footage with distant shore.

### ML-assistance items 1-3 implemented

Catalog items from `plan.org`:

1. **Vessel-mask centroid (item 1)** — `centroid_brightest_body_in_mask`
   takes an optional `&[bool]` allow-mask. Pixels outside the mask are
   excluded from both the connected-component search and the centroid
   integration. Connectivity breaks at the mask boundary.

2. **Sky-mask body search (item 2)** — `SegmentationMask::sky_mask`
   builds a `Vec<bool>` from the segmentation. A regression test
   proves a mask containing only the sky region causes the centroid
   to land on the Sun rather than on a competing bright distractor
   elsewhere in the frame.

3. **Obstruction-aware horizon RANSAC (item 3)** —
   `sky_to_sea_transitions_with_obstruction` walks past thin (≤ 25
   px in mask resolution, ~5% frame height) obstructions looking for
   sea below. The obstruction's *top* row is the horizon candidate,
   tagged with `CandidateSource::SkyToObstructionToSea` so future
   weighted-RANSAC can prefer cleaner sources. **On the new sailing
   scene this doubles the available horizon evidence** (162 clean +
   168 thin-shore-tolerant = 330 total).

### Regression test corpus established and growing

`crates/bris-vision/tests/regression/` has two cases now:

- **`sailing_sun_upper_left`** (376 KB): the original deck-occluded
  scene with sun in upper-left. Demonstrates the gradient and
  sky-region detector failures and proves segmentation finds the
  right horizon.
- **`sailing_with_distant_shore`** (172 KB): scene with sun directly
  ahead, water glare on the right, and a distant shoreline. Proves
  the obstruction-aware detector contributes ~168 additional columns
  beyond the strict version.

10 regression tests run as part of `cargo test`. The corpus is the
primary validation surface: every algorithm change touches it, and
the recorded values in `case.toml` move forward together with the
test assertions in the same commit.

---

## Honest limitations we know about

1. **Brightness-weighted centroid + sky mask biases toward the sky's
   center of mass.** On the original sailing scene, the masked
   centroid landed at (122, 64) instead of the visually-correct
   ~(99, 48) Sun position. The sky region around the saturated Sun is
   itself bright, and area-weighted centroiding pulls toward whichever
   side has more bright pixels. **The right algorithm for Sun/Moon in
   a sky mask is the peak detector (`detect_peaks`)**, not the
   connected-component centroider. Documented in the relevant test
   docstring; tracked as a follow-up.

2. **Placeholder camera intrinsics make absolute altitudes wrong.**
   Every test uses `Intrinsics::placeholder(w, h)` which sets
   `fx = fy = 1000`. For a real wide-angle camera (GoPro-style at
   640×360) `fy` is more like 350-500. Reported altitudes are off by
   a factor of ~2-3 from the actual sky-space altitude, even when the
   horizon and body pixel positions are correct. This is calibration,
   not algorithm; tracked as Phase 2 task "lens calibration workflow."

3. **Single-LOP fix needs an --assumed-position.** This isn't a bug,
   it's geometry. One sight gives a line, not a point. The streaming
   engine (Phase 3.5) accumulates ≥ 2 azimuth-diverse sights into a
   real fix; until then `bris replay` synthesizes a perpendicular
   anchor and labels the result "advisory."

4. **Night and twilight algorithms don't exist yet.** The peak
   detector handles the per-star detection; everything else (plate
   solving for star ID, horizon at night without bright sea-sky
   contrast, twilight hybrid) is unimplemented.

5. **The pretrained SegFormer / ADE20K is 14.5 MB ONNX + ~50 MB ort
   native lib.** Acceptable for embedded Linux; meaningful for mobile
   builds when we get there. The "Train a Bris-specific segmentation
   model" task in `plan.org` is the planned fix: 4-class model,
   1-5 MB target, plausibly runnable with `tract` instead of `ort`
   to drop the native lib entirely.

6. **No camera capture yet.** All testing goes through `bris replay`
   on saved frames. V4L2 (Linux) and platform-native equivalents are
   future Phase 6 work.

---

## Test footage available (in `test_video/`, gitignored)

The user has captured / collected a substantial test corpus. I've
viewed one frame from each and described what's there. Bris should be
exercised against each of these in the next session — some will work,
some will surface new problems, and some are documented up front as
beyond current capability.

| Directory | Frames | Conditions | Expected behavior |
|---|---|---|---|
| `orig_test_video/` | 603 | Sailboat POV, sun upper-left, deck-occluded horizon, day | **Working** — already in regression corpus as `sailing_sun_upper_left`. |
| `new_test_video/` | 25 | Sailboat POV, sun centered, water glare on right, distant shoreline | **Working** — already in regression corpus as `sailing_with_distant_shore`. |
| `cloudy_sun/` | 21 | Container ship deck, sun behind clouds, distant city skyline visible | Likely to work for horizon (clear sky-sea boundary on left); body centroid will pick up the brightest cloud-haze region near the sun. Worth running and adding as a regression case if it produces reasonable output. |
| `sunrise/` | 21 | Container ship deck, sun on horizon, low altitude | **Hard for the current pipeline.** Body very close to horizon; refraction model uncertainty at low altitude is documented to be large. May produce a sight with appropriately wide σ; that itself is the success criterion. |
| `ambiguous_sun/` | 152 | Ship's wake at dusk, sun glow but no defined sun disk visible, broken cloud bands | **Probably won't produce a body fix** — the centroid algorithm needs a saturated bright disk. Useful as a test that the pipeline reports "no body found" cleanly rather than fabricating one. |
| `bokeh/` | 21 | Container ship night, sun visible through optical bokeh artifacts | May surprise — the bokeh "rays" from the sun create false bright patterns. Sky mask should still work; centroid will probably land near the actual sun core. Worth a quick run. |
| `too_bright/` | 21 | Sailboat with sail glare, sunset directly into camera, water glare | **Worst-case for unmasked centroid** — the sail and water are both saturated bright. Sky mask + masked centroid is the correct test target here. |
| `marina/` | 21 | Marina at dusk, multiple sailboats, no clear horizon, lights on shore | **Beyond MVP scope** — this is a harbor scene where there's no usable horizon at all. Useful as a "negative test" that the pipeline reports failure cleanly. |
| `night_test_highres/` | 21 | Stars over ship's wake, dark sky, defined horizon | **Night case we can't yet handle.** Star peak detection should produce points; plate solving doesn't exist yet so star identification will fail. Horizon detection will fail because there's no daylight contrast. Useful test data for when we tackle Phase 3 (plate solving) and Phase 3.5 (continuous engine with night-mode). |
| `night_test_lowres/` | 234 | **Cellphone night shot, bright moon visible, faint horizon, very dark** | **Highest-priority night case.** User flagged this specifically: "I can make out the horizon and the stars, so it's a perfect test case for us." Moon centroiding *should* work (saturated bright body in the dark — the peak detector or even the connected-components centroider should find it). Horizon detection at night is the open problem. Worth pointing at the existing horizon detectors to see what they do (likely fail), then designing the night-horizon algorithm against this specific footage. Note that **frames are 9:16 portrait orientation** (1080×1920 vs landscape), which may surface aspect-ratio assumptions in the pipeline. |
| `container_ship/night/` | 21 | Container ship deck, dark night sky, dense star field | **Best plate-solving test case** when we have a plate solver. Many bright stars visible against truly dark sky. |
| `container_ship/night_lights_on_water/` | 21 | Container ship deck at night with lights illuminating the water | **Adversarial case for night horizon.** Aurora-like glow on horizon will fool simple "find dark/bright transition" approaches; deck lights on water create false features. |

---

## Next concrete steps (recommended ordering)

### Immediate: exercise new test footage

Run `bris replay` against each of the new test directories and:
- Add cases that produce reasonable output to the regression corpus.
- Document failure modes for cases that don't work, so they become
  motivating examples for the next round of algorithm work.
- For `night_test_lowres` specifically: this is the user's
  highest-priority night case. First step is to characterize what the
  current pipeline does on it (likely: horizon detectors fail,
  centroid finds the moon). Second step is to design and implement a
  night-horizon detector based on what the data actually looks like.

### Highest-leverage algorithm work

1. **Use the peak detector for Sun/Moon centroiding inside a sky
   mask.** Fixes the documented brightness-bias limitation. Likely a
   small change: detect peaks within the mask, take the brightest,
   refine sub-pixel as the existing peak detector already does. ~1
   work session.

2. **Night-horizon design.** No current detector handles dark scenes.
   Three approaches discussed in `docs/design/pipeline.md`: IMU prior
   (when we have one), low-altitude detected stars as a horizon
   proxy, sea-sky luma boundary with bright moonlight. The
   `night_test_lowres` and `container_ship/night/` cases are the
   testbeds. Prerequisite for any night functionality.

3. **Lens calibration workflow.** Without real intrinsics, every
   altitude reading is wrong by the FOV-error factor. The current
   placeholder `fy=1000` is far from real wide-angle camera values.
   `bris-vision::lens` already has the math; this is the CLI hook for
   capturing a calibration target and persisting per-camera
   intrinsics.

### Larger pieces, in priority order

1. **Plate solving** (Phase 3) — unlocks night fixes without
   `--body` selection. Requires the full BSC import (already done) and
   a Tetra3-style geometric-hash matcher (not started). Would let
   `night_test_highres` and `container_ship/night/` produce real
   sights.

2. **Streaming engine with continuous-operation logic** (Phase 3.5).
   Reads frames from a buffer, classifies day/night/twilight, picks
   method set, accumulates sights into a rolling window, publishes
   fixes when ≥ 2 azimuth-diverse sights are available. This is the
   piece that turns Bris from "run a single replay" into a real
   continuously-running system.

3. **NMEA transport layer** (Phase 5 remainder) — TCP/UDP/serial
   wrapping the existing formatters. Once in place, `bris fix` against
   live capture can actually drive a chartplotter.

4. **Live camera capture** (Phase 6) — V4L2 on Linux, then
   platform-native equivalents. Last piece before "point a camera at
   the sky" works end-to-end.

5. **Train a Bris-specific segmentation model** (Phase 2 follow-up).
   4-class model, ~1-5 MB, drops `ort` for `tract`. Not blocking but
   substantially reduces binary size and improves accuracy.

6. **Phase 1.5 time integrity** as a cleanup pass.

---

## Open questions

1. **Are wide-angle frames (most of the new test data) useful for
   validation, or should we focus on telephoto/normal-FOV captures?**
   The accuracy budget at the design target (0.5 nm) requires
   sub-arcmin pixel resolution, which means longer focal lengths.
   Wide-angle test data exercises the algorithms but won't validate
   the accuracy claim. Both are useful; worth being explicit about
   which we're using each footage for.

2. **What's the intended user experience for night?** Full plate
   solving and automatic body identification (cool but big) vs.
   "point at the moon, it's identified by name from the almanac
   matching its expected position" (simpler, works tonight).

3. **The pretrained SegFormer expects RGB input.** We currently
   re-load the source image from disk for inference and use Bris's
   grayscale `Frame` for everything else. For live-camera capture the
   capture path will need to keep the RGB version available alongside
   the grayscale one. Needs a small `Frame` API extension.

4. **`night_test_lowres` is portrait orientation (1080×1920).**
   Several pipeline assumptions (horizon is approximately horizontal
   in the frame, body is "above" in image coordinates) may not hold
   in portrait. Worth checking whether portrait frames need to be
   pre-rotated based on EXIF orientation, or whether the pipeline can
   be made orientation-agnostic.
