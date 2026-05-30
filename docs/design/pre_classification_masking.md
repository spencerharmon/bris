# Pre-classification masking & pipeline reordering

Status: **design draft** (operator review pending). No code
changes proposed here are implemented yet.

Related docs:
- `pipeline.md` — current per-frame pipeline ordering.
- `horizon_autodetect.md` — segmentation + horizon-provider
  family selection (the work this proposes reordering against).
- `frame_scheduling.md` — current per-frame cost model.

## Problem

In the Austin pond corpus (`bris-debug-0019e5dd…`) the day/night
classifier reports **Twilight** on visually-pitch-black frames.
Diagnosed in detail in the session log; reproducible facts:

- Mid-band mean luma settles at ≈ 0.048 after CameraX
  auto-exposure ramps up (frame 5 onward); first frame reads
  0.0043.
- `ConditionConfig::default().night_max_luma = 0.05`, so 0.048
  classifies as Twilight.
- Twilight dispatches *both* the day path (saturated-body
  detector) and the night path (peak detector). The day path
  picks up sky-glow / reflection / lens-artifact regions as
  spurious "body candidates."
- Those spurious candidates flow into Stage E, where the
  resulting position ellipse has `axis_ratio ≈ 10⁵` and
  `sigma_major_nm` in the 10⁵ nm range — publication-gated.

The classifier's input is biased by exactly the bright pixels
classification is *not trying to measure*. Mean luma is meant
to estimate ambient sky brightness; including a saturated moon
disk or its specular reflection in the mean overstates ambient
brightness by an amount comparable to the night/twilight
threshold gap on this scene.

## Goals

1. **Make luma classification measure what it claims to
   measure**: ambient sky brightness, excluding the bodies and
   the artificial / non-sky pixels.
2. **Don't pay full body-detect cost on every frame** to
   accomplish (1). Pi Zero 2W is the floor; the per-frame
   budget is small.
3. **Reuse intermediate masks** where they're already being
   computed. No new full-frame passes for one stage to consume
   in isolation.
4. **No new ML dependency.** The existing segmentation model
   (used by horizon detection) is the only ML surface in scope.

## Non-goals

- Replacing the threshold values (`night_max_luma`,
  `day_min_luma`). The thresholds may still want tuning, but
  not until the *input* to the threshold is the right
  measurement. Tuning a biased estimator is misdirected effort.
- Adding IMU integration. IMU is Phase 5/7 in `plan.org`
  (lines ~1076, 1690); this doc explicitly does not assume it.
- Adding a separate "skyglow / urban light pollution"
  classifier. Light pollution is genuinely ambient sky
  brightness and the luma classifier is *correct* to read it
  as Twilight; the right disambiguation is the astronomical
  prior (sun-altitude), already partially implemented in
  `condition.rs::combine`.

## Proposal

Reorder the per-frame pipeline so that:

1. A **cheap bright-blob mask** is computed first.
2. The **segmentation pass** (already mandatory for horizon
   providers) runs next, gated by cheap fast-path rejection.
3. **Classification** consumes both the seg sky-mask and the
   cheap bright-blob mask.
4. **Full body detection** runs after classification (unchanged
   ordering) but uses the cheap bright-blob mask as a
   region-of-interest prior so it doesn't redo the threshold
   pass.

Sketched ordering:

```
0. cheap stats pass (downsampled): mean, saturation %,
   frame-diff motion delta vs last frame
1. cheap bright-blob mask: threshold + small dilation,
   O(N), no CC labeling
2. fast-path gates:
   - all-dark + no bright blobs → emit Unusable, skip the rest
   - all-bright (no dark pixels) → emit Day, skip seg, run
     body-only path
   - motion < ε AND prior seg fresh → reuse cached seg mask
3. segmentation (only if not cached)
4. classify(
     sky_mask = seg,
     body_exclude_mask = cheap_blob_mask,
   )
5. dispatch horizon providers (seg cached, regime-aware)
6. full body detect (using cheap_blob_mask as ROI prior,
   not as gate)
   → emits bodies with σ, photometry, identity
7. reflection-pair + sight assembly + stage E
```

## Why each piece is shaped this way

### Cheap bright-blob mask, not full body detect, for masking

Full body detect is regime-dependent: it needs classification
to decide whether to run the day path, the night path, or
both. Moving it earlier requires running *all* detectors
unconditionally, paying the full cost on every frame just to
mask the classifier. The cheap blob mask gets us 95% of the
masking benefit at 1% of the cost.

What "cheap" means:
- One threshold pass over a downsampled copy of the frame,
  picking pixels above `max(global_p99, k · median)`.
- One small (3–5 px) morphological dilation to absorb halo
  and flare skirts.
- No connected-components labeling, no centroid moments, no
  photometric tests, no catalog lookup.

What this mask is good for:
- Excluding bright compact regions from luma-classification
  averages.
- Telling the full body detector "here are the candidate
  regions; you don't need to threshold the whole frame again."
- Serving as input to reflection-pair's bright-blob enumeration
  (same data it would otherwise compute).

What this mask is **not** good for:
- Deciding which pixels are bodies. (Clouds, lens flare,
  deck lights all survive.)
- Producing body candidates with σ for Stage E.

It's *suppressive*, not *constructive*. The full body detector
keeps its gating role; it just gets a cheaper starting point.

### Seg-before-classify is approximately free

Horizon detection runs on every frame today and is the
dominant consumer of the segmentation model. Currently
segmentation runs inside the dispatched horizon-provider
family, *after* classification picks the family. But it runs
either way — neither family is seg-free. Moving seg to before
classification and caching its output for the providers that
need it is the same total work, in a different order. Net
cost delta ≈ zero.

The reordering also lets the classifier do
"sky-mean-luma" instead of "middle-band-mean-luma", which is
the more principled measurement.

### Middle-band heuristic, not removed

`compute_image_evidence` currently averages over the middle
third of the frame as a no-information spatial prior for "where
sky is most likely to be." It stays as a fallback for two cases:

- The fast-path "all-dark" / "all-bright" gates skip seg
  entirely. The classifier still needs to produce a verdict;
  middle-band stats are the cheap fallback.
- The seg model has not been loaded (off-by-config, embedded
  build that omits it, etc.). Classification must still work.

When seg *is* available, sky-mask supersedes middle-band — they
don't compose, the sky mask is strictly more informative.
Body-blob exclusion applies in both cases.

### Fast-path gates before segmentation

ML segmentation is the dominant per-frame cost on Pi Zero 2W.
Cases where seg adds no information:

- **All-dark frame** (mean luma below noise floor, no bright
  blobs anywhere): no horizon to find, no body to extract.
  Emit `Unusable`, advance, done.
- **All-bright / oversaturated frame** (no dark pixels): seg
  will produce a sky-everywhere mask that tells the horizon
  providers nothing useful. Skip seg, fall through to
  body-only handling.
- **Motion-quiescent + recent seg cached**: a camera on a
  tripod taking a corpus pass produces dozens of frames per
  pose. Re-running seg per frame is wasted work if the scene
  hasn't moved.

Motion detection here is a cheap frame-diff on the same
downsampled buffer used for stats, not IMU (IMU is a Phase 5+
concern per `plan.org`). Cache invalidation rule: any
downsampled-frame-diff above threshold invalidates the cached
seg mask. Conservative; the cost of a missed invalidation is
"horizon provider sees a stale sky mask" which produces
detectable downstream errors.

### Body-mask reuse downstream

The cheap blob mask computed at step 1 has three consumers:

1. **Classifier** (step 4): exclude from ambient-luma average.
2. **Full body detector** (step 6): ROI prior, not a gate.
   The detector still runs its own CC + moments + photometry
   on the masked regions.
3. **Reflection-pair provider**: bright-blob enumeration is
   exactly what reflection-pair starts with. Sharing the
   enumeration is a small win.

Three consumers for the cost of one pass.

## Open questions

These are the things that need operator sign-off before any
code lands.

1. **Threshold for the cheap bright-blob mask.** `p99` is a
   starting guess; the right value depends on the dynamic
   range of the post-AE frame. Likely needs a corpus sweep
   once we have more than one dark-scene capture.
2. **Dilation radius.** 3–5 px is hand-wavy; lens-flare skirts
   on the S62 main at f/1.8 may extend further. Calibration
   needed.
3. **Motion threshold for seg cache invalidation.** A wrong
   threshold here is silently bad. Bias toward invalidating
   too often (re-running seg) is the safe direction.
4. **Fast-path "all-bright" definition.** "No dark pixels" is
   ill-defined; needs a percentile (e.g. "p1 > 0.9") and a
   corpus check.
5. **Sensor-aware `night_max_luma`.** Not in scope for this
   doc, but: once `SensorGain` carries a read-noise model, the
   threshold should be `max(baseline, k · read_noise_σ)`. A
   better low-light sensor's true black sits well below the
   current 0.05; the threshold should track that.
6. **What to do when seg disagrees with body-blob.** If seg
   says "sky" but the cheap blob mask flags the same pixels as
   bright, that's almost certainly a body in the sky (good —
   exclude from luma). But if seg says "not-sky" and blob mask
   flags it (a streetlight on a building), the right answer
   is also exclude. So the rule is: exclude `seg_sky ∧
   ¬blob_mask` from luma. Any disagreement → conservative
   exclusion. Worth verifying.
7. **Where does this doc's work live in `plan.org`?** No
   existing phase entry covers "classifier consumes seg
   output." Candidates: extend Phase 3.5 (streaming engine
   hardening), or open a new bullet under Phase 5/6 (embedded
   perf). Operator decides.

## What this doc does not change

- The classifier's threshold values stay where they are.
- The horizon-provider families and their internal logic
  stay as designed in `horizon_autodetect.md`.
- The Stage E publication gate stays as designed in
  `pipeline.md`. The σ improvements come from feeding it
  cleaner sights, not from changing its gate.
- The astronomical-prior path in `condition.rs::combine`
  stays as-is. Sun-altitude disagreement handling is
  orthogonal to this reordering.
- No IMU dependency is introduced.

## Implementation order (when approved)

Granular enough that each step is independently testable and
reviewable, in the spirit of `CONTRIBUTING.md`'s "one logical
change per PR":

1. **Add `compute_bright_blob_mask`** to `bris-vision`. Unit
   tests on synthetic frames (single bright blob, multiple
   blobs, dilation behavior, all-dark, all-bright). No
   pipeline wiring yet.
2. **Add `sky_mask` and `body_exclude_mask` parameters to
   `bris_vision::condition::classify`**, both optional, both
   defaulting to "use the existing middle-band path." Unit
   tests that show the masked vs unmasked mean luma diverge
   on a moon-in-frame synthetic.
3. **Wire seg-before-classify in `bris-streaming`'s pipeline.**
   Cache the seg mask for horizon providers (they currently
   recompute). Tests: assert classification on the Austin
   pond corpus flips from Twilight to Night.
4. **Wire body-blob mask** as a ROI prior into the full body
   detector. Tests: per-stage frame budget on Pi Zero 2W
   shrinks or stays equal.
5. **Add fast-path gates.** Tests: an all-dark frame skips
   seg entirely (measurable via diagnostics counter).
6. **Add motion-gated seg caching.** Tests: a stationary
   sequence runs seg once, then reuses; a moving sequence
   re-runs seg every frame.

Each step ships its own corpus regression case
(`tests/regression/<scene>/case.toml`) where applicable.
