# ML-based gravity estimation for horizon detection

Status: **handoff-ready for one-pass implementation.**
Kickoff confirmations recorded 2026-06-05 (see
§"Operator decisions at kickoff"). All blockers cleared;
the implementer proceeds without re-asking on any item
documented below.

Related docs:
- `horizon_autodetect.md` — Stage C provider family + fusion design.
- `horizon_brainstorm.md` §0 — `ℓ = K⁻ᵀ g_cam` derivation (the
  equation that synthesizes a horizon line from a known
  gravity vector).
- `pre_classification_masking.md` — adjacent reordering work
  that also benefits from a reliable gravity prior.
- `replay_modes.md` — replay-side semantics this provider must
  preserve.
- `replay_report.md` — JSON schema the new provenance variant
  flows into.

## Problem

The vertical-line provider was disabled by default in PR #72.
Of the remaining horizon providers, none honestly handle
tilted-camera, non-horizon-visible scenes:

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
   negotiable. The first-and-only model shipped is
   heteroscedastic; Layer 1 deterministic-σ is explicitly
   skipped.
4. **Run on Pi Zero 2W.** Model size + inference latency
   budgeted against the existing per-frame envelope.
5. **No replacement of other providers.** Strict addition.
6. **No telemetry.** Inference is local; no data leaves the
   device. AGENTS.md hard rule.

## Non-goals

- Replacing the accelerometer once it's wired through. The
  IMU is the cheap, accurate, primary gravity source. ML is
  the corroboration / fallback. See "Coexistence with IMU"
  below.
- Predicting the horizon line directly (HLW-style). Gravity
  is more useful because it's also consumed by classification
  (`pre_classification_masking.md`), AP estimation, and
  rotation-honesty validation.
- Marine-specific model training in this PR. The Phase 7.7a
  training uses GeoCalib's existing OpenPano + MegaDepth
  data only; that model may underperform on marine scenes;
  the limitation is documented and addressed via the
  deferred `bris-MLGravity-trainer` companion app + custom
  dataset work (Phase 7.7d + 7.7e).
- Body identification. The model says where down is; it does
  not pick the moon over a streetlight.
- Body-conditioned gravity refinement (e.g. "you have a body
  at pixel (x,y) with known altitude from ephemeris, refine
  gravity"). Future enhancement; out of scope.

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

What the model does NOT solve (restated for clarity):

- Body identification.
- Absolute heading.
- Degenerate-input cases (low-contrast / saturated / zero-
  information frames produce honestly-high σ, not magic).

## Coordinate conventions (load-bearing — write down before coding)

The engine's `CameraRay` convention (per `crates/bris-vision/
src/ray.rs`):
- **+x** points right in the image (increasing pixel x)
- **+y** points down in the image (increasing pixel y; image
  origin top-left)
- **+z** points forward through the lens (out of the screen)
- Right-handed.

A point at pixel `(u, v)` has camera ray
`((u-cx)/fx, (v-cy)/fy, 1) / norm`.

**Gravity in this convention** points in the direction of the
physical down-vector in the camera frame:
- Camera held upright facing horizon: `g_cam ≈ (0, +1, 0)`
  (gravity is down in image = +y).
- Camera tilted up by θ to look at sky: `g_cam ≈ (0, +cos θ,
  −sin θ)` (gravity acquires a backward/+z-negative
  component as the camera tilts up).
- Camera held upside down: `g_cam ≈ (0, −1, 0)`.

**Sky-pointing normal** = `−g_cam`. This is what
`horizon_line_from_normal` expects.

**GeoCalib outputs roll and pitch** in the OpenPano convention:
- **Roll**: rotation about the camera optical axis (camera z),
  positive = clockwise as seen by the camera, in radians,
  range (-π, π].
- **Pitch**: angle between the camera optical axis and the
  horizontal plane, positive = camera looking up, in radians,
  range (-π/2, π/2).
- The camera y-axis ("camera-up" before tilt) points opposite
  to gravity when roll=0, pitch=0.

**Conversion from (roll, pitch) to `g_cam` in our convention:**

```
// roll φ rotates about camera +z; pitch θ rotates about camera +x.
// Camera-up (image-down's opposite) starts at -y (since image +y is down).
// In OpenPano (image +y is up, opposite of ours), camera-up starts at +y.
// We use OUR convention end-to-end: image +y is down.
//
// In our convention with roll φ, pitch θ:
//   g_cam.x =  sin(φ) · cos(θ)
//   g_cam.y =  cos(φ) · cos(θ)
//   g_cam.z = -sin(θ)
//
// Sanity: φ=0, θ=0 → g=(0,1,0) — image-down is gravity-down (camera upright facing horizon). ✓
// Sanity: φ=0, θ=+π/4 (looking up 45°) → g=(0, 0.707, -0.707) — gravity tilts backward toward -z. ✓
// Sanity: φ=π/2, θ=0 (rolled 90°) → g=(1, 0, 0) — gravity points to image-right. ✓
```

If GeoCalib's output convention differs (OpenPano uses image
+y=up, opposite of ours), the y-component is negated:
`g_cam.y = -cos(φ)·cos(θ)`. **Verify empirically** by
synthesizing a known-orientation panorama, running GeoCalib
ONNX, and checking the sign of `g_cam.y`. Test fixture
`bris-vision/tests/ml_gravity_convention.rs` asserts the
sign convention.

## Image preprocessing pipeline

GeoCalib expects:
- 8-bit RGB input (3 channels).
- Resized to a model-specific input resolution (256×256 or
  384×384 — confirm at export time).
- ImageNet-style per-channel normalization
  `(pixel/255 - mean) / std` with
  `mean = (0.485, 0.456, 0.406)`,
  `std = (0.229, 0.224, 0.225)`.

The engine's frames are **16-bit grayscale** (luma only). The
preprocessing chain is:

1. **Downsample** to model input resolution (whichever
   long-edge max the model uses). Use `bris-vision`'s
   existing `Frame::scaled_to` (which also rotates intrinsics
   — important if we want to map model-output coordinates back
   to source frame).
2. **8-bit conversion**: `pix8 = (pix16 >> 8) as u8`. Lossy
   but standard. Alternative: percentile normalization
   (1st–99th percentile to 0–255). Pick **percentile
   normalization** — the model is trained on normally-
   exposed natural images; bit-shift produces poorly-
   contrasted inputs on dim or saturated frames.
3. **Gray-to-RGB**: replicate the single channel to 3
   channels: `R = G = B = gray`. Standard approach for
   grayscale-input-to-RGB-model. Models trained on natural
   images encode color statistics in early layers; replicated
   gray loses that information but the model still typically
   produces usable outputs (validated empirically in pose-
   estimation literature).
4. **ImageNet normalization** as above.
5. **NHWC vs NCHW**: tract-onnx is layout-agnostic; pass
   whatever GeoCalib's ONNX expects (verify at export).

Document the preprocessing as a self-contained function
`preprocess_frame_for_geocalib(&Frame) -> Tensor` in
`crates/bris-vision/src/ml_gravity/preprocess.rs`. Unit-test
with known-fixture frames against expected tensor values.

## Intrinsics handling

GeoCalib outputs both intrinsics and roll/pitch. Two questions:

1. **Do we feed intrinsics to the model?**
   - **No.** GeoCalib runs without intrinsics input and
     estimates them internally. We use only its roll/pitch
     output. The intrinsics output is ignored.
2. **Do we use the model's intrinsics output for anything?**
   - **Not in scope.** Optional future enhancement: emit
     a `BundleWarning` if the model's estimated intrinsics
     disagree with the bundle's recorded intrinsics by more
     than k×. Additional rotation-honesty signal. Out of
     scope for this design.

## Marine vs land-based — honest expectation

GeoCalib was trained on OpenPano + MegaDepth, which are heavily
indoor/urban. Marine scenes — especially open-water captures
with no boat structure in frame — are out-of-distribution.

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
1. Phase 7.7a ships the pretrained-and-retrained-head model.
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

**Pick: GeoCalib.**

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

### tract-onnx compatibility

tract-onnx (already in workspace via `crates/bris-vision`'s
segmentation feature) supports ONNX opset 13+ and a substantial
subset of operators. **Before exporting:** verify the GeoCalib
PyTorch model uses only tract-supported ops. Likely-trouble
operators: custom CUDA kernels, `Loop`, dynamic shape
operations. If GeoCalib uses any, the workaround is one of:

1. **Op replacement at export.** Replace the unsupported op
   with a tract-supported equivalent in the PyTorch model
   before export.
2. **ONNX simplifier.** Run `onnxsim` on the exported model
   to fold constants and eliminate unsupported control flow.
3. **Switch to PerspectiveFields** as a fallback model.

This needs to be the **first thing** verified in Phase 7.7a
(separate exploration commit before training begins). If
GeoCalib's ops are tract-incompatible AND PerspectiveFields
is too, Phase 7.7a is blocked and we revert to the design-
level question of switching ML runtimes (ort vs onnxruntime
vs torch via bindings).

## Can we retrain GeoCalib to output σ using existing datasets?

**Yes**, and the answer is more nuanced than "wait for marine
data." Three layers:

### Layer 1: deterministic σ (NOT SHIPPING — historical context)

**Skipped per operator handoff 2026-06-05.** Documented here
only to explain what was rejected and why. The pretrained
GeoCalib outputs deterministic predictions; one could compute
a σ_global by running it on a held-out validation set and
reporting that single value for every prediction. This is
rejected because:

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
  uncalibrated outside. Document explicitly via a per-frame
  diagnostic `ml_gravity_ood_warning` when input distribution
  deviates from training distribution by k× (mean luma /
  contrast / sky-region-fraction heuristics).
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

### What we ship

Layer 2 (heteroscedastic σ) is the first and only model
shipped. The provider's loader rejects any model that
doesn't have the 4-scalar `(roll, pitch, σ_roll, σ_pitch)`
output shape. There is no deterministic-σ fallback path.

The σ output field is `Sigma` in the provider, computed from
the model's per-prediction `σ_pred` output via the Jacobian
in §"σ propagation through the lens model."

Rationale for skipping Layer 1: a Layer-1 fallback would have
shipped wiring with a placeholder σ that violates AGENTS.md
rule zero in an uncomfortably visible way (single σ for every
scene is honest only as a calibration constant, never as a
per-prediction value). Training Layer 2 directly is the same
total work — the head retrain is GPU-hours not days — and
ships an honest σ from day one.

## σ propagation through the lens model (full math)

The model outputs σ as a per-axis triple in camera-frame
gravity components: `σ_g = (σ_gx, σ_gy, σ_gz)` (Layer 2+;
Layer 1 collapses to a single value applied to all three).
Stage C consumes σ in *altitude radians*. Conversion via
Jacobian:

For a body at camera-frame ray `r = (rx, ry, rz)` (unit
vector) and gravity `g = (gx, gy, gz)` (unit vector),
altitude is `α = asin(r · (-g)) = asin(-(rx·gx + ry·gy +
rz·gz))`.

The Jacobian of α with respect to g:

```
∂α/∂g_i = -r_i / sqrt(1 - (r·g)²)
```

Combined variance (independent components):

```
σ_α² = Σ_i (∂α/∂g_i)² · σ_g_i²
     = [rx²·σ_gx² + ry²·σ_gy² + rz²·σ_gz²] / (1 - (r·g)²)
```

For the isotropic case where σ_gx = σ_gy = σ_gz = σ_g,
this simplifies (using rx²+ry²+rz² = 1):
`σ_α² = σ_g² / (1 - (r·g)²) = σ_g² / cos²(α)`.

**Edge cases:**
- Body at zenith from camera (r ≈ -g): `cos(α) → 0`, σ_α
  blows up. **Clamp `cos(α) ≥ 0.05`** (≈3° from zenith)
  before the division; cite a `MeasurementError::AltitudeNearZenith`
  variant when the clamp fires. Honest failure mode.
- Body below horizon: existing `BelowHorizon` check fires
  before this math; no extra handling needed.

The σ_α computed above is the contribution from the **gravity
uncertainty**. The total altitude σ is the quadrature sum
with the body-centroid σ (already handled by
`measure_altitude_from_ray`):
`σ_α_total² = σ_α_gravity² + σ_α_body²`.

**For the horizon hypothesis itself** (not a specific body
sight), we report a "representative" σ that downstream
fusion uses. The convention: evaluate the Jacobian at a
representative ray `r_ref = (0, 0, 1)` (camera optical axis
points to the body / sky region of interest). Then
`σ_α_ref² = σ_gz² / cos²(α_ref)`. Document this as a
simplification that's exact only when the body is on the
optical axis.

Existing `horizon_line_from_normal` accepts an
`altitude_sigma: Sigma` and propagates it into the
`HorizonLine.altitude_sigma` field. The new provider
computes σ_altitude (via the Jacobian above at r_ref) before
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

**Agreement threshold:** "agree within k·σ_combined" means
the angular distance between IMU gravity and ML gravity is
below `k * sqrt(σ_imu² + σ_ml²)`. Default `k = 3` (3-σ test).
σ_imu defaults to **0.5°** (Android accelerometer typical;
real value comes from device profile when available). σ_ml
is the provider's per-prediction (Layer 2) or global (Layer 1)
σ. **Document the σ_imu default in
`bris-streaming::EngineConfig` with a TODO(operator-approved)
noting it's a placeholder until device-profiled IMU σ
arrives.**

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

Existing providers stay in. Only vertical-line is disabled
(PR #72). When the model is the silent winner of fusion,
its provenance shows up in the per-fix
`HorizonProvenance::MlGravity { model_id, sigma_rad }`
(new variant — `model_id` is the ONNX file's BLAKE3 hash
truncated to 12 chars, so different model versions are
distinguishable; `sigma_rad` is the per-prediction σ that
went into fusion).

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
trivial. **Default cache N = 10** (≈ 1 Hz at 10 fps capture);
σ inflation per skipped frame is `σ_t = σ_0 + α·Δt` where
α is a "gravity drift rate" — for a hand-held camera, α =
0.5°/sec is generous (camera pose changes are slow); for a
boat-mounted camera, α = 5°/sec accounts for swell. Default
α = 1°/sec; operator override via `EngineConfig`.

## Threading model

ONNX inference is CPU-bound and blocks the calling thread.
The streaming engine is single-threaded by design (per
AGENTS.md note about not introducing parallelism without
empirical justification on Pi Zero 2W). The ML gravity
provider blocks Stage C dispatch for the inference duration.

Two design choices:

1. **Block in-line** (chosen for Phase 7.7b): inference runs
   synchronously in Stage C. Per-frame budget is the per-
   frame time + ML inference time. Frame-skip / cache
   mechanism keeps total below budget.
2. **Worker thread + most-recent-result** (deferred): spawn
   a single inference worker that always processes the most
   recent frame, returning whatever's ready when Stage C
   asks. Adds a thread to the engine; deferred until perf
   measurement shows in-line blocking exceeds the per-frame
   budget consistently.

The choice between (1) and (2) is operator-decidable later;
ship (1) and measure.

## Failure modes

Explicit behavior for every failure mode (per AGENTS.md
"never silently fail"):

| failure | provider behavior | diagnostic |
|---|---|---|
| Model file missing at startup | provider initialization fails; `EngineConfig` either rejects (if `enable_ml_gravity = true`) or silently disables (if `enable_ml_gravity = false`); operator chooses via the flag | `ml_gravity_load_failed = true` |
| Model file BLAKE3 mismatch (corruption) | initialization fails as above | `ml_gravity_load_failed = true` with reason "checksum mismatch" |
| Inference returns NaN or non-finite | provider returns `None` for this frame (no hypothesis); does NOT crash the engine | `ml_gravity_nan_outputs` counter |
| Inference exceeds budget (e.g. 2× expected) | hypothesis returned with timestamp recorded; engine logs slow-frame warning at trace level | `ml_gravity_inference_ms_p99` gauge in diagnostics |
| Pre-processing fails (e.g. zero-byte frame) | provider returns `None`; logged | `ml_gravity_preprocess_failed` counter |
| Model output convention mismatch (sign flip discovered post-deploy) | initialization-time test in the loader runs a known fixture through the model and asserts the output sign; failure means the loader refuses to construct the provider | `ml_gravity_load_failed = true` with reason "convention test failed" |

The convention-self-test at load time is load-bearing for
catching sign-flip regressions after model re-export.

## Build and deployment

### Model file bundling

**Decision: vendor the ONNX file in the repo at
`data/ml-gravity/geocalib-v1.onnx`** (subject to operator
sign-off on the open question below). Rationale:
- Reproducible builds, no fetch dependency in CI.
- Embedded targets (Pi Zero 2W) don't need network during
  install.
- Precedent: the segmentation model is bundled similarly
  (path to be confirmed during implementation).

Trade-off: 30 MB increase in repo size. Use Git LFS for the
ONNX file to keep clone-time reasonable. Existing repo has
no LFS dependency; **this design adds it.** Operator must
sign off on adopting LFS.

If LFS adoption is rejected: fall back to fetch-at-build with
a vendored checksum + a CI cache. Documented in a separate
ADR if needed.

### Cargo feature gating

The provider is behind a new `ml-gravity` cargo feature in
`crates/bris-vision/Cargo.toml`, mirroring the existing
`segmentation` feature. Default: **off**. Rationale: the
30 MB model adds binary bloat for users who don't need
gravity assistance (e.g. operators with reliable IMU); making
it opt-in respects that.

`bris-streaming` and `bris-cli` propagate the feature.
`bris-ffi` similarly: a `ml-gravity` feature that, when off,
compiles `MlGravityProvider` calls to no-ops.

### Android APK bundling

When `ml-gravity` is enabled in the bris-ffi build for
Android:
- The ONNX file is included as an Android `assets/` resource.
- `MlGravityProvider::load_from_assets(context)` extracts to
  the app's internal cache on first use.
- APK size impact: +30 MB. Document in the APK release
  notes.

When the feature is off: the ONNX file is NOT included; APK
size unchanged.

### Test infrastructure

Three test scopes:

1. **Unit tests in `crates/bris-vision/src/ml_gravity/`**:
   - Coordinate conversion (roll/pitch → g_cam) with known
     pairs.
   - Preprocessing pipeline (deterministic tensor output for
     a fixture frame).
   - σ Jacobian math (numeric verification vs analytic).
   - Convention self-test (mock model output → expected
     g_cam direction).
   - Gated by `#[cfg(feature = "ml-gravity")]`.
2. **Integration test against a real (tiny) ONNX model**:
   - A minimal "always returns roll=0, pitch=0" ONNX model
     (10 KB, shipped under `tests/fixtures/`) for testing the
     load + inference + post-process pipeline without the
     30 MB GeoCalib weights.
   - Validates the wiring end-to-end.
3. **Corpus regression**:
   - The bedroom-moon corpus replay runs `--ml-gravity` and
     emits the explorer artifacts. Acceptance: ≥ 1 sight is
     emitted from at least one previously-stuck capture, OR
     all captures fail Stage E for documentable geometric
     reasons (`BelowHorizon` / `Apparent` / `Stitch` not
     `NoHorizonHypothesis`).
   - This is manually verified; not part of CI (the model
     would inflate CI build time).

CI gates `ml-gravity` feature builds behind a job named
`test-ml-gravity` that runs only on `main` push (not on PR)
to keep PR feedback fast.

## Implementation roadmap

Each step independently testable, in the spirit of
CONTRIBUTING.md "one logical change per PR." Per the
operator handoff (2026-06-05), the work splits into
**phases**, not just PRs:

- **Phase 7.7a** — train the heteroscedastic model; produces
  an ONNX file + training-results doc, no workspace code.
- **Phase 7.7b** — build the provider that consumes the
  trained model; wires into Stage C; smoke-tests against the
  bedroom corpus.
- **Phase 7.7c** — IMU coexistence (blocks on Phase 7.5 #5).
- **Phase 7.7d** — marine fine-tune (blocks on the trainer
  APK).
- **Phase 7.7e** — trainer APK (companion workstream).

Detailed checklist for each phase below.

### Phase 7.7a: Train heteroscedastic GeoCalib (no workspace code)

**Goal:** produce `data/ml-gravity/geocalib-heteroscedastic-
v1.onnx` plus training documentation.

1. **Verify tract-onnx compatibility upfront.** Export a
   stock GeoCalib checkpoint to ONNX, load in tract, run on
   a fixture tensor. If unsupported ops emerge: either
   replace at export time (PyTorch model edit), simplify
   with `onnxsim`, or pivot to PerspectiveFields. Document
   the outcome in `scripts/ml-gravity/tract_compat_notes.md`.
   This is the **first thing** done; if it blocks, the
   whole phase blocks pending operator decision on runtime
   swap.
2. **Reproducible training environment.** `Dockerfile` +
   `requirements.txt` under `scripts/ml-gravity/` pinning
   PyTorch, GeoCalib commit, onnx, onnxsim, training data
   checksums. CI-runnable; no network at train time after
   the dataset is cached.
3. **Dataset preparation.** OpenPano + MegaDepth synthesized-
   tilt pairs, the GeoCalib upstream defaults. Held-out
   validation subset for σ calibration. Script logs
   per-split sample counts and aspect-ratio distributions.
4. **Head retrain with heteroscedastic loss.** Backbone
   frozen. Loss: `L = (g_pred − g_true)² / (2σ_pred²) +
   ½ log(σ_pred²)`. Hyperparameters logged. Expected:
   GPU-hours, single node.
5. **σ calibration validation.** On the held-out validation
   set, compute per-prediction `(σ_pred, residual)` pairs.
   Plot calibration curve (binned mean residual vs binned
   σ). The curve should be monotonic and close to the
   y=x line. Save the plot as a deliverable.
6. **ONNX export with 4-scalar output** (roll, pitch,
   σ_roll, σ_pitch). Run convention self-test fixture:
   render a known-orientation panorama, invoke the ONNX,
   confirm the output `g_cam` sign convention matches the
   design doc §"Coordinate conventions."
7. **Vendor the ONNX** at `data/ml-gravity/geocalib-
   heteroscedastic-v1.onnx` (Git LFS pending operator
   sign-off; otherwise fetch-at-build with checksum).
8. **`docs/design/ml_gravity_training.md`** (new file)
   documents the dataset splits, hyperparameters,
   calibration plot, validation residuals, the convention
   self-test results, and the expected σ floor on the
   training distribution.

**Deliverable:** the ONNX file + scripts + training-results
doc. No bris-vision / bris-streaming code touched. Operator
reviews the calibration plot before green-lighting Phase
7.7b.

### Phase 7.7b: Provider + Stage C wiring

**Goal:** new provider consumes the trained model and feeds
Stage C.

9. **`crates/bris-vision/src/ml_gravity/mod.rs`**:
   - `MlGravityProvider` struct (model handle, config).
   - `MlGravityConfig` with model path, enable flag,
     frame-cache N, drift rate α, σ_imu, agree threshold k.
   - `load_model(path)` runs the convention self-test at
     load; refuses to construct if the test fails.
   - `detect_with_stats(ctx, &mut stats)` implementing
     `HorizonProvider` trait.
   - Preprocessing module per the design doc §"Image
     preprocessing pipeline."
   - σ Jacobian helper per the design doc §"σ propagation
     through the lens model."
10. **`bris-streaming::pipeline::horizon` dispatch update**:
    - Add `MlGravityProvider` invocation last in the
      dispatch order.
    - Gated by `EngineConfig::enable_ml_gravity` (defaults
      false) AND the `ml-gravity` cargo feature.
    - When invoked, populate `EngineDiagnostics` counters
      (see below).
11. **`HorizonProvenance::MlGravity { model_id, sigma_rad }`**
    variant in `crates/bris-vision/src/horizon.rs`.
    Public-API addition; serialized in the replay-report
    JSON (extends `docs/design/replay_report.md`).
12. **`EngineDiagnostics` counter additions** (additive;
    AGENTS.md-approved):
    - `ml_gravity_invoked: u64`
    - `ml_gravity_hypothesized: u64`
    - `ml_gravity_corroborated: u64` (zero until Phase 7.7c
      lands; counter shape established here)
    - `ml_gravity_imu_disagreement: u64` (same)
    - `ml_gravity_nan_outputs: u64`
    - `ml_gravity_preprocess_failed: u64`
    - `ml_gravity_load_failed: bool`
    - `ml_gravity_inference_ms_p99: f64` (gauge)
13. **`bris-cli replay --ml-gravity`** flag that flips
    `enable_ml_gravity = true` in the engine config and
    logs the provider's per-frame outputs in the report.
14. **`bris-ffi`**: additive `enable_ml_gravity: Option<bool>`
    in `FfiEngineConfig`; default `None` means use the core's
    default (false).
15. **Tests**: unit tests for the provider (coordinate
    conversion, preprocessing, σ Jacobian, convention
    self-test); integration test against a tiny fixture
    ONNX model (10 KB "returns roll=0, pitch=0" stub) under
    `tests/fixtures/`.
16. **Corpus smoke test**: re-run replay against the bedroom
    corpus with `--ml-gravity` enabled. Document outcome in
    `docs/design/ml_gravity_results.md` (new file): per-
    capture sight count, fix count, σ statistics, before/
    after comparison.

**Acceptance:** ≥1 sight emerges from a previously-stuck
capture, OR Stage E honestly fails for a documented
geometric reason (NOT "no horizon hypothesis"); per-frame
budget within +200 ms on Pi Zero 2W; corpus explorer shows
model-derived horizons with `HorizonProvenance::MlGravity`
in the badge.

### Phase 7.7c: IMU coexistence (blocks on Phase 7.5 #5)

Follows the Android writer landing per-frame
`gravity_camera_frame` in sidecars. Without that, the
coexistence policy has nothing to corroborate.

17. Implement the agreement check per §"Coexistence with
    IMU" above. All four IMU×ML present/absent combinations
    tested.
18. Diagnostic counters that were stubbed in 7.7b
    (`ml_gravity_corroborated`, `ml_gravity_imu_disagreement`)
    now populated.
19. Document the live behavior in `docs/design/
    ml_gravity.md` §"Coexistence with IMU" (flip from
    "design" to "live").

### Phase 7.7d: Marine fine-tune (blocks on trainer APK)

20. Reuse Phase 7.7a's training scripts. Combine OpenPano +
    MegaDepth + operator marine corpus.
21. Recalibrate σ on marine subset; document expected σ
    floor for marine scenes in `docs/design/
    ml_gravity_training.md`.
22. Re-export to `data/ml-gravity/geocalib-heteroscedastic-
    marine-v1.onnx`. Loader detects which model is loaded
    via the embedded model id and reports in diagnostics.
23. Marine validation: fix-σ contributions from the gravity
    provider within Phase-8 documented accuracy budget on
    marine corpus.

### Phase 7.7e: bris-MLGravity-trainer companion APK (companion)

Separate Android APK that captures frames + IMU gravity at
controlled poses, optimized for training-data efficiency
(deduplication, pose diversity, exposure spread).

- Reuses `bris-bundle` debug-bundle schema for on-disk
  format; `gravity_camera_frame` populated per frame from
  IMU.
- Separate APK so training-data workflow doesn't pollute
  the user-facing capture UI.
- Operator can install/uninstall independently of the
  main app.
- Out of scope for 7.7a–c; feeds 7.7d when it lands.

## Open questions

Two concrete blockers require operator input before the
implementer starts. Everything else has a default decided
below.

### Operator decisions at kickoff (2026-06-05)

Both blockers are cleared. The implementer proceeds without
re-asking.

- **B1. GeoCalib weights license**: operator confirmed BSD-3-
  compatible. Vendor without double-checking. The training-
  results doc records the license confirmation note.
- **B2. Vendoring strategy**: **fetch-at-build with checksum**.
  No Git LFS. The implementer adds a fetch script (or
  `build.rs` hook, or Makefile target — pick the most
  idiomatic for the workspace and document the choice in the
  PR) that downloads the ONNX from a stable URL into
  `data/ml-gravity/` and verifies the BLAKE3 checksum
  recorded in `data/ml-gravity/SHA256SUMS`. The release URL
  is whatever the implementer uploads to as part of Phase
  7.7a; create a new GitHub release tagged `ml-gravity-v1`
  on the bris repo, attach the ONNX, and reference its
  download URL in the fetch script.
- **GPU access**: confirmed at kickoff. NVIDIA RTX 3080,
  driver 595.71.05, CUDA 13.2. Host has
  `nvidia-container-toolkit` 1.19.1 installed and verified
  with `podman run --device nvidia.com/gpu=all`. The
  implementer's subprocess should be able to use the GPU
  the same way; if it can't, that's a real blocker and the
  session stops there.
- **Containerization**: **podman, not docker.** The host has
  podman 5.8.2; no docker is installed. The doc says
  "Dockerfile" — the file format is identical; just invoke
  `podman build` / `podman run` everywhere. The fetched
  base image must support GPU passthrough via the
  `nvidia.com/gpu=all` device spec (verified at kickoff
  using `docker.io/nvidia/cuda:13.0.0-base-ubuntu22.04`).

### Concrete blockers (historical — cleared before kickoff)

**B1. GeoCalib weights license.** The GeoCalib *code* is
BSD-3 (compatible with the workspace's GPL-3.0-or-later).
The *weights* may be licensed differently (some ML projects
ship weights under CC-NC or "research only" while keeping
the training code permissive). The repo's `cargo deny check`
does not catch this; weights aren't a cargo dependency.
The operator must personally check
`https://github.com/cvg/GeoCalib`'s LICENSE + the model
release notes and confirm the weights are redistributable
in a GPL-3.0-or-later repo + included in a binary released
under the same. If not, fall back to PerspectiveFields
(Apache 2.0 code; check weights similarly) or train from
scratch on permissively-licensed data.
*Resolved 2026-06-05: operator confirmed.*

**B2. Git LFS adoption.** The model file is ~30 MB. Vendoring
in the repo requires Git LFS, which affects every clone, CI
run, and contributor workflow. The alternatives are
(a) fetch-at-build with checksum (no LFS; adds network
requirement at build time), or (b) split the model into a
separate release artifact downloaded on first use
(complicates embedded deployment).
Operator must pick one of:
  - LFS adoption (recommended for reproducibility);
  - fetch-at-build (recommended if LFS is rejected);
  - separate release artifact (only if both above are
    rejected).
*Resolved 2026-06-05: fetch-at-build with checksum.*

### Decided defaults (no operator action needed)

If the operator does not actively reject any of these before
kickoff, the implementer treats them as approved per AGENTS.md
§"Stopping is also a shortcut" — these are tradeoffs, not
user-visible-contract questions.

- **Cargo feature default**: `ml-gravity` is OFF by default.
  Operator opts in. Revisit after Phase 7.7b ships and we
  measure the binary-size impact.
- **σ_imu default**: `0.5°` (`8.7e-3` rad) as a placeholder
  for Android accelerometer typical noise. Replaced by
  per-device profile when one exists. Implementer adds a
  `TODO(operator-approved 2026-06-05):` comment in code
  citing this default.
- **Per-frame cache lifetime N**: `10` (≈1 Hz at 10 fps).
  Retune on Pi Zero 2W timing measurements in a follow-up.
- **Drift rate α for cached gravity**: `1°/sec` for hand-held.
  Boat-mounted profiles override via `Session.profile` once
  that path exists.
- **Agreement threshold k**: `3` (3-σ test for IMU×ML
  agreement). Standard outlier-rejection convention.
- **Failed convention self-test behavior**: provider
  refuses to initialize and reports `ml_gravity_load_failed
  = true` with reason `"convention test failed"`. No
  auto-flip-and-continue.
- **Layer 2 retrain compute**: operator's choice; doesn't
  affect the design. Implementer documents whichever path
  is used (Dockerfile + dataset checksums regardless).
- **plan.org placement**: Phase 7.7 (already added). Five
  sub-phases 7.7a–e.
- **`σ_global` (Layer 1 deterministic-σ placeholder)**: not
  applicable. Layer 1 is skipped per operator handoff;
  there is no global-σ fallback in the shipped provider.
  The first model loaded must be heteroscedastic; the
  loader's convention self-test rejects a 2-scalar output.
- **Segmentation-model bundling precedent**: the existing
  segmentation model is fetched / packaged via the
  segmentation cargo feature. Implementer mirrors that
  pattern for `ml-gravity`. If the segmentation precedent
  is itself unclear, implementer documents the chosen
  pattern explicitly in the PR and applies it to both.

## What this doc does not change

- Vertical-line provider stays disabled (per PR #72).
- Other Stage C providers unchanged.
- Stage C fusion math unchanged.
- Stage E unchanged.
- The accelerometer path (Phase 7.5 #5 follow-up) is
  independent. When it lands, the ML provider becomes the
  corroborator per the policy in "Coexistence with IMU."
- No replacement of existing segmentation / horizon / body /
  classification providers.
- No new telemetry. All inference is local; all training
  data collection is operator-initiated.

## Implementer instructions (one-pass session)

This section governs the implementer agent. Read it before
touching anything.

### Scope

**Implement Phases 7.7a + 7.7b in one pass.** Phase 7.7a
produces the trained ONNX file + training-results doc + the
reproducible-training scripts. Phase 7.7b consumes that ONNX,
adds the `MlGravityProvider`, wires it into Stage C, and
validates on the bedroom-moon corpus. Both ship together as
a single coherent change.

Phases 7.7c (IMU coexistence), 7.7d (marine fine-tune), and
7.7e (trainer APK) are out of scope. They have explicit
upstream blockers and ship separately.

### Forbidden shortcuts (per AGENTS.md rule zero)

The one-pass implementer must NOT:

- Ship Layer 1 (deterministic-σ global constant) as a
  fallback or option. The operator handoff explicitly skips
  Layer 1. The first-and-only model the provider accepts is
  heteroscedastic (4-scalar output).
- Stub the heteroscedastic training with placeholder σ values
  and ship the provider against the stub. The training
  pipeline must actually run and produce a real ONNX file.
- Vendor the model file without confirming Git LFS adoption
  (blocker B2 above).
- Vendor the model file without confirming the weights
  license (blocker B1 above).
- Ship a single σ for all predictions "because we don't
  have time to retrain." If you don't have time, you have a
  concrete blocker; report it and stop.
- Skip the convention self-test in the loader. The self-test
  is the load-bearing defense against sign-flip regressions.
- Use `Sigma::ZERO` or `unwrap_or(Sigma::ZERO)` anywhere in
  the provider. Non-finite or unreasonable model output is a
  real failure; emit `None` and increment the
  `ml_gravity_nan_outputs` counter.
- Stub the corpus validation. The bedroom-moon corpus replay
  is part of the deliverable and must actually run; the
  outcome must be documented in `ml_gravity_results.md`.
- Disable any existing horizon provider as a side effect.
  Vertical-line is already disabled (PR #72); no others move.
- Skip writing failing tests before implementation. AGENTS.md
  test-first discipline applies to every test the spec
  enumerates (preprocessing, σ Jacobian, convention self-
  test, fixture-ONNX integration, corpus smoke).
- Merge with `--admin` to bypass branch protection. Wait for
  green CI.

### Required behavior under each blocker

If blocker B1 (weights license) is unclear after the operator
has confirmed kickoff: STOP and ask. Do not vendor weights of
uncertain license.

If blocker B2 (LFS) is unclear after the operator has confirmed
kickoff: assume LFS-vendoring is approved and proceed. If LFS
setup itself fails (CI-side), fall back to fetch-at-build and
document the change in the PR.

If tract-onnx compatibility check fails (no supported ops for
the full GeoCalib forward pass): try the documented mitigations
(op replacement at export, onnxsim), and if all fail, STOP
and ask the operator whether to pivot to PerspectiveFields or
swap runtimes.

For every other ambiguity, use the decided defaults in
§"Open questions" above without stopping.

### Deliverables checklist

The PR is not done until all of the following exist on disk:

1. `scripts/ml-gravity/Dockerfile` and pinned
   `requirements.txt`. Image builds successfully.
2. `scripts/ml-gravity/export_geocalib.py` + training
   driver script. Reproduces the model end-to-end from a
   clean container.
3. `scripts/ml-gravity/tract_compat_check.py` (or
   equivalent Rust test). Demonstrably runs against the
   exported ONNX.
4. `data/ml-gravity/geocalib-heteroscedastic-v1.onnx` (via
   LFS if B2 = LFS) or build-time fetch infrastructure if
   B2 = fetch.
5. `data/ml-gravity/SHA256SUMS` containing the model file
   checksum.
6. `docs/design/ml_gravity_training.md` with dataset splits,
   hyperparameters, validation residuals, calibration plot
   image (vendored as PNG under the same dir), expected σ
   floor on training distribution, license confirmation for
   the weights (the answer to blocker B1).
7. `crates/bris-vision/src/ml_gravity/` with `mod.rs`,
   `preprocess.rs`, `sigma.rs` (Jacobian helper),
   `convention.rs` (self-test), and a `model.rs` (ONNX
   loader). All gated behind `#[cfg(feature = "ml-gravity")]`.
8. `crates/bris-vision/tests/ml_gravity_*.rs` integration
   tests covering preprocessing, σ Jacobian, convention
   self-test, and a tiny-fixture ONNX inference round-trip.
9. `crates/bris-streaming/src/pipeline/horizon.rs`
   dispatching `MlGravityProvider` last in the chain, gated
   by `EngineConfig::enable_ml_gravity` (default false) AND
   the cargo feature.
10. `crates/bris-vision/src/horizon.rs` with the new
    `HorizonProvenance::MlGravity { model_id, sigma_rad }`
    variant + serde support.
11. `crates/bris-streaming/src/diagnostics.rs` with the
    eight new ML-gravity counters (additive; existing
    consumers compile unchanged).
12. `crates/bris-ffi/src/lib.rs` with the additive
    `enable_ml_gravity: Option<bool>` on `FfiEngineConfig`.
13. `crates/bris-cli/src/main.rs` with the new
    `--ml-gravity` replay flag.
14. `docs/design/replay_report.md` updated for the new
    provenance variant in the JSON schema.
15. `docs/design/ml_gravity_results.md` (new) with the
    bedroom-moon corpus replay outcome: per-capture sight
    count, fix count, σ statistics, before/after.
16. `docs/design/ml_gravity.md` (this file) status line
    flipped to "Status: live as of <commit>."
17. `plan.org` Phase 7.7a + 7.7b flipped from TODO to DONE
    with audit annotations citing the merge commit.

No deliverable may be stubbed, mocked, or marked "follow-up."

### Concrete blockers the implementer may legitimately hit

These are the *only* legitimate reasons for the one-pass
session to stop short of all 17 deliverables:

- Blocker B1 unresolved (weights license).
- Blocker B2 unresolved AND LFS setup fails in CI AND
  fetch-at-build also fails.
- tract-onnx incompatibility with GeoCalib AND with
  PerspectiveFields (would require a runtime swap, which
  is out of scope for one PR).
- Convention self-test discovers that GeoCalib's output
  sign conventions cannot be reconciled with the engine's
  `CameraRay` convention via the documented formulas
  (would require a redesign of the conversion math).
- Training run produces a calibration plot that's wildly
  non-monotonic (model is fundamentally not learnable with
  the chosen head + loss). Operator decides next move.
- The corpus smoke test produces no sights AND no
  documentable Stage E failure (provider produces no
  hypothesis at all; deeper investigation needed).

Anything else — tedium, ambiguity, design tradeoff,
refactor temptation, perf optimization opportunity — is
NOT a blocker per AGENTS.md §"Stopping is also a shortcut."
Note tradeoffs in the PR description, continue.

## Validation criteria

Phase 7.7a (training) is "done" when:

- The ONNX file at
  `data/ml-gravity/geocalib-heteroscedastic-v1.onnx` loads in
  tract-onnx and produces finite outputs on a fixture
  tensor.
- Convention self-test passes: a known-orientation panorama
  fixture produces a `g_cam` matching the design doc §
  "Coordinate conventions" sign convention.
- Per-prediction calibration plot in
  `docs/design/ml_gravity_training.md` is monotonic and
  close to the y=x line on the held-out validation set.
- Training-results doc lists dataset splits, hyperparameters,
  validation residuals, and the expected σ floor on the
  training distribution.
- The training environment is reproducible from
  `scripts/ml-gravity/Dockerfile`.

Phase 7.7b (provider) is "done" when:

- ML gravity provider produces a hypothesis on every frame
  of the bedroom-moon corpus (currently zero providers do
  on those captures).
- The hypothesis is geometrically consistent with the
  visible scene (verifiable via corpus explorer renders —
  the red horizon line should land at a plausible position
  given how the camera was held).
- The σ is reported and honestly large (per-prediction
  heteroscedastic).
- Replay produces ≥ 1 sight from at least one bedroom-moon
  capture, OR honestly fails Stage E with `Apparent` /
  `BelowHorizon` / `Stitch` for documentable geometric
  reasons (NOT `NoHorizonHypothesis`).
- Pi Zero 2W per-frame budget unchanged or within +200 ms.
- The corpus explorer shows model-derived horizons with
  `HorizonProvenance::MlGravity` in the tooltip/badge.

Phase 7.7c (IMU coexistence) is "done" when:

- The `ml_gravity_corroborated` and
  `ml_gravity_imu_disagreement` counters populate correctly
  on a corpus with per-frame IMU gravity.
- All four IMU×ML present/absent combinations are tested.
- `docs/design/ml_gravity.md` §"Coexistence with IMU" is
  flipped from "design" to "live" status.

Phase 7.7d (marine) is "done" when:

- σ on marine validation corpus is calibrated to actual
  residual.
- Fix σ contributions from the gravity provider are within
  spec for the documented accuracy budget (Phase 8).
- The marine corpus replays without `ml_gravity_ood_warning`
  in normal marine conditions.

## References

- GeoCalib paper: Veicht, Lopez-Antequera, Lindenberger,
  Sattler, Pollefeys, "GeoCalib: Learning Single-image
  Calibration with Geometric Optimization," ECCV 2024.
  https://github.com/cvg/GeoCalib
- PerspectiveFields paper: Jin, Park, Jampani, Zhou, Sun,
  Sun, "Perspective Fields for Single Image Camera
  Calibration," CVPR 2023.
  https://github.com/jinlinyi/PerspectiveFields
- UprightNet paper: Xian, Zhang, Snavely, "UprightNet:
  Geometry-Aware Camera Orientation Estimation from Single
  Images," ICCV 2019.
- Heteroscedastic regression: Kendall, Gal, "What
  Uncertainties Do We Need in Bayesian Deep Learning for
  Computer Vision?," NeurIPS 2017.
- tract-onnx supported operators:
  https://github.com/sonos/tract/blob/main/doc/operators.md
- ONNX simplifier:
  https://github.com/daquexian/onnx-simplifier
