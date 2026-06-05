# ML-based gravity estimation for horizon detection

Status: **design draft** (operator review pending). No code yet.

Related docs:
- `horizon_autodetect.md` — Stage C provider family + fusion design.
- `horizon_brainstorm.md` §0 — `ℓ = K⁻ᵀ g_cam` derivation (the
  equation that synthesizes a horizon line from a known
  gravity vector).
- `pre_classification_masking.md` — adjacent reordering work
  that also benefits from a reliable gravity prior.
- `replay_modes.md` — replay-side semantics this provider must
  preserve.

## Problem

The vertical-line provider is being disabled by default
(separate PR). Of the remaining horizon providers, none
honestly handle tilted-camera, non-horizon-visible scenes:

| provider | succeeds when | fails when |
|---|---|---|
| gradient | sharp horizon edge | indoor / no horizon |
| night, night-textured | sea/sky luma boundary | day / indoor |
| sky-region | clear sky segment | no sky in frame |
| segmentation | trained sky/not-sky boundary | scene out of training distribution |
| reflection-pair | body + reflection visible | no specular surface |
| vanishing-point | Manhattan structure with ≥2 horizontal VPs | open ocean, noisy edgels |
| vertical-line | (disabled — broken math) | — |

The bedroom-moon corpus replay surfaced this: every provider
either silently fails or produces nothing usable on tilted
indoor captures. The engine's only honest output on those
scenes is "no horizon hypothesis." That's correct per
AGENTS.md rule zero but it means a large class of captures
produces no fixes.

The underlying missing input is **camera-frame gravity**.
Given gravity, the horizon line follows from
`ℓ = K⁻ᵀ g_cam` (no additional image cues needed). The current
providers each try to *derive* gravity from a different image
proxy (horizon edge, plumb line, sky region) and inherit that
proxy's failure modes.

A direct gravity estimator removes the proxy. The accelerometer
is the right physical source but isn't currently in the per-
frame sidecar (Phase 7.5 follow-up). An ML model is the
fallback when the accelerometer reading is missing, unreliable,
or untrusted (e.g. replay of historic captures with no IMU
metadata).

## Goals

1. **Provide a `MlGravityProvider` for Stage C** that takes a
   frame and outputs a (gravity vector, σ) pair, from which
   the existing `horizon_line_from_normal` synthesizes a
   horizon hypothesis.
2. **Be one voice among many** in Stage C's inverse-variance
   fusion. Never the primary on real horizon scenes; the
   silent winner on scenes where no other provider produces a
   hypothesis.
3. **Report honest σ.** Heteroscedastic — the model knows when
   it's uncertain. Per AGENTS.md rule zero, this is non-
   negotiable: a single calibration constant is acceptable as
   a spike but not as ship-state.
4. **Run on Pi Zero 2W.** Model size + inference latency
   budgeted against the existing per-frame envelope.
5. **No replacement of other providers.** Strict addition.

## Non-goals

- Replacing the accelerometer once it's wired through. The
  IMU is the cheap, accurate, primary gravity source. ML is
  the corroboration / fallback. See "Coexistence with IMU"
  below.
- Predicting the horizon line directly (HLW-style). Gravity
  is more useful because it's also consumed by classification
  (`pre_classification_masking.md`), AP estimation, and
  rotation-honesty validation.
- Marine-specific model training in this PR. The initial spike
  uses a pretrained model that may underperform on marine
  scenes; that limitation is documented and addressed via the
  deferred `bris-MLGravity-trainer` companion app + custom
  dataset work.

## Use cases

The model unblocks or improves these distinct workflows:

1. **Horizon detection on tilted, non-horizon-visible scenes.**
   The bedroom-moon corpus is the canonical motivator.
2. **Classification sky-mask prior.** Gravity points to "down";
   above-horizon pixels are a defensible sky prior even when
   segmentation fails. Direct input into
   `pre_classification_masking.md`'s reordering.
3. **Cold-start AP estimation.** One body altitude + known
   gravity = circle of position. Today's cold-start requires
   multiple sights and the CoP-intersection path.
4. **Stitch / panorama gravity-corroboration.** Catches
   wrong-rotation stitches that Harris+NCC features alone
   accept.
5. **Replay sanity check.** Validates `gravity_camera_frame`
   when present in a sidecar; populates it when absent.
   `BundleWarning` surface for disagreements.
6. **"Bytes are upside-down" detection.** If the model predicts
   gravity pointing toward image-y=−small and the manifest
   claims `source_rotation_deg=0`, that's a load-bearing fail-
   loud signal. Direct mitigation for the regression that
   prompted AGENTS.md rule zero.

What the model does NOT solve:

- Body identification (still picks the brightest saturated
  blob; doesn't know moon vs streetlight).
- Absolute heading (gravity is up/down only; azimuth requires
  body identification + ephemeris).
- Degenerate-input cases (low-contrast / saturated / zero-
  information frames produce honestly-high σ, not magic).

## Marine vs land-based — honest expectation

GeoCalib and similar pretrained models were trained on
OpenPano + MegaDepth, which are heavily indoor/urban. Marine
scenes — especially open-water captures with no boat
structure in frame — are out-of-distribution.

The honest expectation:

- **Marine scenes with visible boat structure** (deck rail,
  mast, gunwale): probably works, similar to land-based with
  vertical structure.
- **Marine scenes with a clear horizon**: the gradient /
  night providers should win Stage C fusion anyway; ML
  provider's role is corroboration.
- **Open-ocean zenith captures** (no horizon, no structure,
  just sky and water): probably underperforms badly. Honest
  σ should reflect this; the model should report large
  uncertainty rather than confident garbage.

The path from "probably works" to "validated for marine":
1. Spike ships pretrained model. Ship as-is.
2. Operator captures marine corpus with the deferred
   `bris-MLGravity-trainer` app (records frames + IMU gravity).
3. Marine fine-tune. Re-validate σ calibration on marine
   distribution.
4. Re-release.

Bris is *for* marine navigation. The interim period where the
model works well indoors and tilted-on-land but not on open
ocean is a known limitation, documented in the per-frame
diagnostic output and surfaced in the per-fix σ.

## Off-the-shelf model survey

The candidates evaluated against three criteria: directly
outputs gravity (or trivially convertible), pretrained weights
publicly available, ONNX-exportable.

| model | inputs | outputs | size | license | marine? |
|---|---|---|---|---|---|
| **GeoCalib** (ECCV 2024, ETH) | RGB | intrinsics + roll/pitch | ~30 MB | BSD-3 | unknown; trained on OpenPano + MegaDepth |
| **PerspectiveFields** (CVPR 2023, MIT) | RGB | dense per-pixel "up" + "latitude" fields | ~50 MB | Apache 2.0 | unknown; mixed indoor/outdoor |
| **UprightNet** (ICCV 2019, Cornell/Adobe) | RGB | gravity in image frame | ~10 MB | MIT | trained on InteriorNet, indoor only |
| HLW-CNN family | RGB | horizon line directly | varies | varies | trained on HLW (outdoor, minimal marine) |

**Pick: GeoCalib for the spike.**

Reasons:
- Directly outputs the quantity we need (roll/pitch, trivially
  convertible to camera-frame gravity).
- Active maintenance, public weights, BSD license fits
  GPL-3.0-or-later workspace.
- ONNX export tooling is documented upstream.
- 30 MB fits the embedded budget (segmentation model is
  similar order).
- The model's intrinsics output is a bonus — could
  cross-check the bundle's `intrinsics` block against what
  the model thinks the lens looks like, which is another
  rotation-honesty signal.

Fallback if GeoCalib export proves painful: PerspectiveFields
(richer output, also Apache-licensed, similar size).

## Can we retrain GeoCalib to output σ using existing datasets?

**Yes**, and the answer is more nuanced than "wait for marine
data." Three layers:

### Layer 1: deterministic σ (the spike)

The pretrained GeoCalib outputs deterministic predictions. We
compute σ by running it on a held-out validation set with
known gravity, measuring residual variance:
`σ_global² = mean((g_pred − g_true)²)`. Single fixed σ for all
predictions.

- **Dataset needed: zero new collection.** GeoCalib's own
  validation set works.
- **Honesty cost: high.** Same σ for every scene, regardless
  of how hard the scene is. AGENTS.md rule zero violation
  unless we explicitly mark it as spike-grade in code +
  diagnostics.
- **Use: spike only.** Ships the wiring; not ship-state.

### Layer 2: heteroscedastic σ (the production fix)

Retrain GeoCalib's head — not the backbone, just the regression
head — with a heteroscedastic loss:
`L = (g_pred − g_true)² / (2σ_pred²) + ½ log(σ_pred²)`.

The model outputs both `g_pred` AND `σ_pred`. The loss
penalizes over-confident (small σ when residual is large) and
under-confident (large σ when residual is small) predictions.

- **Dataset needed: GeoCalib's own training data.** OpenPano
  (panoramas with synthesized tilts) gives free `(image,
  g_true)` pairs by construction — every synthesized tilt
  has known gravity. **No new collection required.**
- **Compute needed: GPU hours, not days.** Fine-tuning only
  the regression head from a pretrained backbone is cheap.
- **σ calibration: per-distribution.** The learned σ is
  calibrated for the OpenPano + MegaDepth distribution. Marine
  scenes will have *under-estimated* σ (the model is over-
  confident on out-of-distribution inputs by default).
- **Honesty cost: medium.** σ is honest within distribution;
  uncalibrated outside. Document explicitly.
- **Use: ship-state for the non-marine pipeline.**

### Layer 3: marine-calibrated σ (deferred)

Once `bris-MLGravity-trainer` produces a marine corpus
(frames + IMU gravity per frame), fine-tune again on the
combined distribution. Recalibrate σ.

- **Dataset needed: custom marine corpus.** Deferred to the
  trainer app workstream.
- **Compute needed: same as Layer 2.**
- **σ calibration: cross-distribution.** Honestly reports
  marine + non-marine.
- **Use: ship-state for the full production pipeline.**

### What we ship in this PR

The spike implements **Layer 1** (single global σ from a
calibration set). Code is structured so Layer 2 is a
re-export of model weights — no provider-side changes needed
when the heteroscedastic model arrives.

The σ output field is `Option<Sigma>` in the provider, with
"None" reserved for "model not loaded" (provider returns no
hypothesis). The deterministic-σ Layer 1 returns a fixed
`σ_global` from a config constant. Layer 2 returns the
model's per-prediction σ_pred. Same code path.

## σ propagation through the lens model

The model outputs σ in *camera-frame gravity components*:
`σ_g = (σ_gx, σ_gy, σ_gz)`. Stage C consumes σ in *altitude
radians*. Conversion via Jacobian:

```
σ_altitude ≈ |∇_g altitude| · σ_g
```

For a body at camera-frame ray `r` and gravity `g`,
`altitude = asin(r · (-g))` (sign convention: gravity points
down, sky-normal is `−g`, altitude is angle of `r` above the
horizon plane). The Jacobian `∂altitude/∂g_i` evaluates to
the components of `r / sqrt(1 − (r·g)²)`.

Existing `horizon_line_from_normal` already accepts an
`altitude_sigma: Sigma` and propagates it into the
`HorizonLine.altitude_sigma` field. The new provider
computes σ_altitude from σ_g via the Jacobian above before
calling that function.

The math is analogous to Phase 7.5 #17 (per-star σ via lens
model, PR #64) — same shape, different inputs.

## Coexistence with IMU

When the per-frame sidecar carries `gravity_camera_frame`
(Phase 7.5 #5 follow-up — currently unimplemented in the
Android writer; existing replay code already reads it when
present), the policy is:

- **IMU + ML agree (within k·σ_combined):** report IMU gravity
  with IMU σ. ML acts as silent corroborator. Diagnostic
  counter `ml_gravity_corroborated`.
- **IMU + ML disagree:** report ML gravity with inflated σ
  reflecting the disagreement. Diagnostic counter
  `ml_gravity_imu_disagreement`. This is the load-bearing
  case for catching IMU bias / saturation / drift.
- **IMU absent, ML available:** report ML gravity with ML σ.
- **IMU available, ML absent (model not loaded):** report IMU
  gravity with IMU σ. ML is optional.
- **Both absent:** existing fallback path (vertical-line is
  disabled; vanishing-point / segmentation may still
  contribute; if nothing produces a hypothesis, the engine
  honestly emits "no horizon").

The disagreement-handling is the cheap rotation-honesty
guard the AGENTS.md `first_frame_blake3` regression motivated.
If on-device IMU says gravity points one way and the model
strongly disagrees, something is wrong with one of them; the
high-σ output forces Stage E to either find corroboration
elsewhere or honestly fail.

## Fallback to other horizon providers

The model is one provider among several. Stage C runs all
applicable providers, fuses inverse-variance (per PR #63),
reports per-provider diagnostics.

Existing providers stay in. Only vertical-line is being
disabled (separate PR). When the model is the silent winner
of fusion, its provenance shows up in the per-fix
`HorizonProvenance::MlGravity { confidence }` (new variant)
so the operator can see which provider's voice dominated.

The model **never** wins fusion against a clean gradient or
night-horizon hypothesis with sub-arcminute σ, because the
model's σ floor (from the calibration set) is going to be
larger than that by an order of magnitude. That's correct:
when a real horizon edge is visible, the geometric providers
should always win.

## Pi Zero 2W performance budget

The segmentation model (existing) already establishes the
embedded ONNX-inference precedent. Per-frame budget:

- Segmentation: ~150–250 ms (measured in CI on ARM)
- New ML gravity: budget ≤ 200 ms per frame

Mitigations if over budget:

- Downsample input to 256×256 or 384×384 (most pose-
  estimation models are scale-tolerant).
- Cache: gravity changes slowly (operator pose changes on
  human timescales). Run the model every N frames; reuse
  cached gravity in between, with σ inflation per elapsed
  time. Same gating pattern as
  `pre_classification_masking.md` proposes for segmentation.
- Quantize to int8 (tract-onnx supports quantized models;
  ~3× speedup on ARM with ~10% accuracy degradation that
  bumps into σ honestly).

Per-frame caching is the obvious first lever and is design-
trivial.

## Implementation roadmap

Each step independently testable, in the spirit of
CONTRIBUTING.md "one logical change per PR."

### Spike (this design doc → first PR)

1. **`bris-vision::MlGravityProvider` skeleton.** Trait impl
   that loads an ONNX model at construction time, runs it
   per frame, returns deterministic σ from a config constant.
   Behind a new `ml-gravity` cargo feature, mirroring how
   `segmentation` is gated.
2. **GeoCalib → ONNX export.** Vendored conversion script.
   Output committed under `data/ml-gravity/geocalib-v1.onnx`
   (~30 MB; review whether to vendor vs fetch at build time).
3. **`MlGravityConfig`** with model path, σ_global, enable
   flag, frame-cache N. Defaults: disabled out of the box;
   operator opts in.
4. **Wire into Stage C dispatch** (`bris-streaming::pipeline::
   horizon`). Last position in the dispatch order (most
   expensive). Skipped entirely when feature is off or
   config-disabled.
5. **`HorizonProvenance::MlGravity { model_id, confidence }`**
   variant; `EngineDiagnostics` counters for invoked /
   corroborated / disagreement.
6. **Regression test** against a synthetic frame with known
   gravity (rotated panorama); assert hypothesis is within
   model's calibration σ of truth.
7. **Manual smoke** against the bedroom corpus. Re-run
   replay; document in the PR whether sights start emerging
   from previously-stuck captures.

### Layer 2 (separate PR, follows the spike)

8. **Retrain GeoCalib head with heteroscedastic loss.**
   Training scripts live under `scripts/ml-gravity/`.
   Validated against held-out OpenPano + MegaDepth.
9. **Re-export ONNX** with the new head.
10. **Update `MlGravityProvider`** to use the per-prediction
    σ output instead of the global constant. Behavior change
    is purely σ-honest; no API change.

### Layer 3 (deferred — follows `bris-MLGravity-trainer`)

11. **Marine fine-tune.** Combines OpenPano + MegaDepth +
    operator's marine corpus.
12. **Recalibrate σ** on marine subset; document expected
    σ floor for marine scenes.
13. **Re-export, re-ship.**

### bris-MLGravity-trainer (deferred companion workstream)

A separate Android APK that captures frames + IMU gravity at
controlled poses, optimized for training-data efficiency
(deduplication, pose diversity, exposure spread). Out of
scope for this design doc; tracked separately. The training
app feeds Layer 3.

## Open questions

These need operator sign-off before code lands.

1. **Vendor the GeoCalib ONNX (~30 MB) in the repo, or fetch
   at build time?** Vendoring keeps reproducible builds and
   embedded targets simple; fetching keeps the repo small.
   Precedent: the segmentation model lives where?
2. **`ml-gravity` cargo feature default: on or off?** On =
   everyone gets the model in their build (~30 MB binary
   bloat). Off = operator must opt in, smaller default
   binary. Recommend off for the spike, revisit after Layer
   2 ships.
3. **Where in `plan.org` does this work live?** No existing
   phase entry covers ML gravity. Candidates:
   - Extend Phase 5 (embedded perf / per-frame budget).
   - New Phase 7.7 "Gravity provider stack" alongside the
     pre-classification masking work.
   - Operator decides.
4. **Global σ_global value for Layer 1 spike.** Honest
   answer is "measure it on GeoCalib's validation set" but
   the operator may want a conservative default until the
   calibration is run. Recommend 0.05 rad (~3°) as a
   placeholder; calibration replaces it.
5. **Per-frame cache lifetime.** N=10 (≈ 1 Hz at 10 fps
   capture) is the obvious default; the right number depends
   on Pi Zero 2W timing measurements that don't exist yet.
6. **Layer 2 retrain compute.** Operator's local GPU vs
   cloud? Recommend documenting reproducibility (Dockerfile +
   exact dataset checksums) regardless of where it runs.
7. **License / IP review for GeoCalib weights.** BSD-3 on
   the code; the weights are presumably the same but worth
   confirming the redistribution terms before vendoring.

## What this doc does not change

- Vertical-line provider stays disabled.
- Other Stage C providers unchanged.
- Stage C fusion math unchanged.
- Stage E unchanged.
- The accelerometer path (Phase 7.5 #5 follow-up) is
  independent. When it lands, the ML provider becomes the
  corroborator per the policy in "Coexistence with IMU."
- No replacement of existing segmentation / horizon / body /
  classification providers.

## Validation criteria

The spike is "done" (Layer 1 ready to ship) when:

- ML gravity provider produces a hypothesis on every frame
  of the bedroom-moon corpus (currently zero providers do).
- The hypothesis is geometrically consistent with the visible
  scene (verifiable via corpus explorer renders).
- The σ is reported and honestly large (no sub-arcminute
  σ_floor lying).
- Replay produces ≥ 1 sight from at least one bedroom-moon
  capture, OR honestly fails Stage E with `Apparent` /
  `BelowHorizon` for documentable geometric reasons.
- Pi Zero 2W per-frame budget unchanged or within +200 ms.

Layer 2 (heteroscedastic σ) is "done" when:

- Predicted σ correlates with actual residual on validation
  set (per-prediction calibration plot is monotonic).
- σ on the bedroom-moon corpus is honestly large (the model
  knows it's out of distribution).

Layer 3 (marine) is "done" when:

- σ on marine validation corpus is calibrated to actual
  residual.
- Fix σ contributions from the gravity provider are within
  spec for the documented accuracy budget (Phase 8).
