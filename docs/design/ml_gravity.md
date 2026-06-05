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
   negotiable: a single calibration constant is acceptable as
   a spike but not as ship-state.
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
- Marine-specific model training in this PR. The initial spike
  uses a pretrained model that may underperform on marine
  scenes; that limitation is documented and addressed via the
  deferred `bris-MLGravity-trainer` companion app + custom
  dataset work.
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
   - **Not in the spike.** Optional future enhancement: emit
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

This needs to be the **first thing** verified in the spike
(separate exploration commit before the implementation work
begins). If GeoCalib's ops are tract-incompatible AND
PerspectiveFields is too, the spike is blocked and we revert
to the design-level question of switching ML runtimes (ort vs
onnxruntime vs torch via bindings).

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
  validation set (held-out OpenPano + MegaDepth subset) works.
- **Honesty cost: high.** Same σ for every scene, regardless
  of how hard the scene is. AGENTS.md rule zero violation
  unless we explicitly mark it as spike-grade in code +
  diagnostics. **Spike marker:** the config field is named
  `sigma_global_rad_spike_only` so the lie is visible at
  every call site.
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

### What we ship in this design's spike

The spike implements **Layer 1** (single global σ from a
calibration set). Code is structured so Layer 2 is a
re-export of model weights — no provider-side changes needed
when the heteroscedastic model arrives.

The σ output field is `Sigma` in the provider, computed from
either the global constant (Layer 1) or the model's per-
prediction σ_pred (Layer 2+). Same code path; provider
detects which mode by inspecting model output tensor shape
(deterministic = 2 scalars, heteroscedastic = 4 scalars).

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

For the spike (Layer 1) where σ_gx = σ_gy = σ_gz = σ_g,
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

1. **Block in-line** (chosen for the spike): inference runs
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
CONTRIBUTING.md "one logical change per PR." The spike is
**three PRs**, not one, to keep each reviewable.

### Spike PR 1: Model export + tract compatibility verification

**Goal:** prove the ONNX file works in tract-onnx before
investing in the wiring.

1. **Add `scripts/ml-gravity/export_geocalib.py`** that
   clones GeoCalib at a pinned commit, exports to ONNX,
   optionally runs `onnxsim` to fold/strip unsupported ops.
   Records the exact PyTorch + GeoCalib + onnx + onnxsim
   versions in a `manifest.txt` next to the output.
2. **Add `crates/bris-vision/tests/geocalib_ops_supported.
   rs`** — a test that loads the exported ONNX file with
   tract and asserts inference runs on a fixture tensor.
   Test is gated by `#[cfg(feature = "ml-gravity")]` and
   ignored unless the model file is present.
3. **Vendor the model** at `data/ml-gravity/geocalib-v1.
   onnx` (via LFS pending operator sign-off; otherwise
   fetch-at-build with checksum).
4. **CI updates**: add LFS support if vendoring chosen; add
   the `test-ml-gravity` CI job that runs the ops-supported
   test.

PR-1 acceptance: GeoCalib ONNX runs in tract on a fixture
tensor, returning finite outputs.

If tract incompatibility is discovered: PR-1 is the place
to either fix it (op replacement at export) or pivot to
PerspectiveFields. Documented in the PR.

### Spike PR 2: Provider wiring

**Goal:** new provider that hands a horizon hypothesis to
Stage C, gated by config.

5. **`crates/bris-vision/src/ml_gravity/mod.rs`** with:
   - `MlGravityProvider` struct (model handle, config).
   - `MlGravityConfig` with model path, σ_global_rad,
     enable flag, frame-cache N, drift rate α, σ_imu, agree
     threshold k.
   - `load_model(path)` with convention self-test at load.
   - `detect_with_stats(ctx, &mut stats)` implementing
     `HorizonProvider` trait.
   - Preprocessing module per the pipeline above.
   - σ Jacobian helper.
6. **`bris-streaming::pipeline::horizon` dispatch update**:
   - Add `MlGravityProvider` invocation last in the
     dispatch order.
   - Gated by `EngineConfig::enable_ml_gravity` (defaults
     false) AND the `ml-gravity` cargo feature.
   - When invoked, populate `EngineDiagnostics` counters
     (see below).
7. **`HorizonProvenance::MlGravity { model_id, sigma_rad }`**
   variant in `crates/bris-vision/src/horizon.rs`.
   Public-API addition; serialized in the replay-report
   JSON (extends `docs/design/replay_report.md`).
8. **`EngineDiagnostics` counter additions** (additive;
   AGENTS.md-approved):
   - `ml_gravity_invoked: u64`
   - `ml_gravity_hypothesized: u64`
   - `ml_gravity_corroborated: u64`
   - `ml_gravity_imu_disagreement: u64`
   - `ml_gravity_nan_outputs: u64`
   - `ml_gravity_preprocess_failed: u64`
   - `ml_gravity_load_failed: bool`
   - `ml_gravity_inference_ms_p99: f64` (gauge)
9. **`bris-cli replay --ml-gravity`** flag that flips
   `enable_ml_gravity = true` in the engine config and
   logs the provider's per-frame outputs in the report.
10. **`bris-ffi`**: additive `enable_ml_gravity: Option<bool>`
    in `FfiEngineConfig`; default `None` means use the core's
    default (false).
11. **Tests**: unit tests for the provider (coordinate
    conversion, preprocessing, σ Jacobian, convention
    self-test); integration test against the tiny fixture
    ONNX model.

PR-2 acceptance: cargo test passes with `--features
ml-gravity`; replay against a synthetic frame with known
gravity produces a hypothesis within the σ_global of truth;
CI is green.

### Spike PR 3: Corpus validation

**Goal:** validate on the operator's existing corpus and
document the result.

12. Re-run replay against the bedroom-moon corpus with
    `--ml-gravity` enabled.
13. Document outcome in `docs/design/ml_gravity_results.md`
    (new file): per-capture sight count, fix count, σ
    statistics, which frames produced sights, before/after
    comparison.
14. If sights emerge from previously-stuck captures, declare
    the spike successful. If not, document the next
    bottleneck (Stage E, body identification, etc.).
15. Update `docs/design/ml_gravity.md` (this doc) with
    "Status: spike complete" and a pointer to the results.

PR-3 acceptance: documented results; the explorer view of
the corpus shows model-derived horizons; operator agrees
spike is done.

### Layer 2 (separate PR, follows the spike)

16. **Retrain GeoCalib head with heteroscedastic loss.**
    Training scripts live under `scripts/ml-gravity/`.
    Validated against held-out OpenPano + MegaDepth.
17. **Re-export ONNX** with the new head. The output tensor
    shape changes from 2 scalars to 4 scalars; the loader
    detects this and switches σ mode.
18. **Update `MlGravityProvider`** to use the per-prediction
    σ output instead of the global constant. Behavior change
    is purely σ-honest; no API change.

### Layer 3 (deferred — follows `bris-MLGravity-trainer`)

19. **Marine fine-tune.** Combines OpenPano + MegaDepth +
    operator's marine corpus.
20. **Recalibrate σ** on marine subset; document expected
    σ floor for marine scenes.
21. **Re-export, re-ship.**

### bris-MLGravity-trainer (deferred companion workstream)

A separate Android APK that captures frames + IMU gravity at
controlled poses, optimized for training-data efficiency
(deduplication, pose diversity, exposure spread). Out of
scope for this design doc; tracked separately. The training
app feeds Layer 3.

The trainer is **separate from the existing Bris APK** so
that:
- Training-data-capture concerns don't pollute the user-
  facing capture UI.
- Training-data captures can be high-frequency / high-volume
  without confusing the operator's sight log.
- The trainer can be removed from devices after data
  collection; the main app doesn't carry the burden.

Trainer dataset format: identical to the existing Bris
debug-bundle format (per `docs/design/debug_bundle_schema.
md`), with `gravity_camera_frame` populated in every sidecar
from IMU. Reuses `bris-bundle` for the on-disk schema. The
fine-tuning script reads bris-bundle directories the same
way `bris replay` does.

## Open questions

These need operator sign-off before code lands.

1. **Vendor the GeoCalib ONNX (~30 MB) in the repo via Git
   LFS, or fetch at build time?** Vendoring keeps reproducible
   builds and embedded targets simple; fetching keeps the
   repo small. Precedent: the segmentation model lives at
   (path to confirm). Recommend **vendor via LFS** for
   reproducibility, with a fetch-at-build escape hatch
   documented.
2. **`ml-gravity` cargo feature default: on or off?** On =
   everyone gets the model in their build (~30 MB binary
   bloat). Off = operator must opt in, smaller default
   binary. **Recommend off for the spike, revisit after
   Layer 2 ships.**
3. **Where in `plan.org` does this work live?** Recommend
   **new Phase 7.7 "Gravity provider stack"** alongside the
   pre-classification masking work. Sister entries:
   `Pre-classification masking & pipeline reordering` and
   `ML gravity provider (3-PR spike + Layer 2/3 follow-ups)`.
4. **Global σ_global value for Layer 1 spike.** Honest
   answer is "measure it on GeoCalib's validation set" but
   the operator may want a conservative default until the
   calibration is run. **Recommend 0.05 rad (~3°) as a
   placeholder; calibration script in Spike PR 1 produces
   the real value and replaces the default.**
5. **Per-frame cache lifetime.** N=10 is the obvious default.
   The right number depends on Pi Zero 2W timing
   measurements that don't exist yet. **Operator confirms
   default; measurement-driven retune in a follow-up.**
6. **Drift rate α for cached gravity.** 1°/sec for hand-held;
   higher for boats. **Operator confirms default; per-
   profile override via `Session.profile` is the natural
   place for the boat-specific value.**
7. **σ_imu default.** 0.5° is a placeholder for Android
   accelerometer typical; the real value comes from a
   per-device profile when one exists. **Operator confirms
   default.**
8. **Agreement threshold k.** Default 3 (3-σ test). **Operator
   confirms default.**
9. **Layer 2 retrain compute.** Operator's local GPU vs
   cloud? Reproducibility (Dockerfile + exact dataset
   checksums) regardless. **Operator decides; affects
   workflow not architecture.**
10. **License / IP review for GeoCalib weights.** BSD-3 on
    the code; the weights are presumably the same but worth
    confirming the redistribution terms before vendoring.
    **Operator confirms before Spike PR 1 lands.**
11. **Failed convention self-test behavior.** If the model
    fails the load-time convention test (e.g. y-axis sign
    flip after re-export), should the provider refuse to
    initialize, or initialize with an inverted-y flag set
    automatically? **Recommend refuse to initialize** —
    surfaces the bug rather than papering over it. Operator
    confirms.

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

## Validation criteria

The spike (Spike PRs 1-3) is "done" when:

- ML gravity provider produces a hypothesis on every frame
  of the bedroom-moon corpus (currently zero providers do
  on those captures).
- The hypothesis is geometrically consistent with the
  visible scene (verifiable via corpus explorer renders —
  the red horizon line should land at a plausible position
  given how the camera was held).
- The σ is reported and honestly large (Layer 1: matches
  the global constant; Layer 2: per-frame heteroscedastic).
- Replay produces ≥ 1 sight from at least one bedroom-moon
  capture, OR honestly fails Stage E with `Apparent` /
  `BelowHorizon` / `Stitch` for documentable geometric
  reasons (NOT `NoHorizonHypothesis`).
- Pi Zero 2W per-frame budget unchanged or within +200 ms.
- The corpus explorer shows model-derived horizons with
  `HorizonProvenance::MlGravity` in the tooltip/badge.

Layer 2 (heteroscedastic σ) is "done" when:

- Predicted σ correlates with actual residual on validation
  set (per-prediction calibration plot is monotonic).
- σ on the bedroom-moon corpus is honestly large (the model
  knows it's out of distribution).
- The `ml_gravity_ood_warning` counter fires on bedroom-
  moon frames at >50% rate (model knows OOD when it sees it).

Layer 3 (marine) is "done" when:

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
