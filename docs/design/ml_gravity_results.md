# ML-gravity provider: corpus smoke results (Phase 7.7b)

Status: live as of this commit. Companion to
`docs/design/ml_gravity.md` and `docs/design/ml_gravity_training.md`.

## Bedroom-moon corpus

The canonical motivator from `docs/design/ml_gravity.md`:
tilted-camera, no-real-horizon-visible captures where the
classical providers either silently fail or produce
nothing usable.

Two captures replayed end-to-end with `bris replay
--ml-gravity --bundle <capture>` against the vendored
`data/ml-gravity/geocalib-heteroscedastic-v1.onnx`.

### Capture `0019e87174c5f9ba9bc3cde06f32e`
(session `508197ac-09ab-49d9-a430-a9c8556155f8`,
"bedroom moon new debug test", 7 frames, twilight
dispatched)

| metric                       | baseline (no `--ml-gravity`) | with `--ml-gravity`             |
|------------------------------|------------------------------|---------------------------------|
| fixes_published_total        | 0                            | 0                               |
| sight_window depth (final)   | 6                            | 6                               |
| ML-gravity invocations       | 0                            | 7 (one per frame)               |
| ML-gravity hypothesized      | 0                            | 7                               |
| ML-gravity used in fusion    | 0                            | 0                               |
| Mean inference time (ms)     | n/a                          | ~6.5                            |

### Capture `0019e873b482e8e51bef5e92fb024`
(same session, 5 frames, twilight)

Same shape: every frame produces a hypothesis,
σ_altitude ≈ 0.12–0.19 rad (~7–11°), classical
gradient/night providers win fusion with σ ≈ 0.005 rad
when they fire.

## Why no fix despite the provider firing

The bedroom-moon captures **are not** the
"no-horizon-hypothesis" failure mode the design doc
predicted. They produce horizons via the gradient + night
providers at σ ≈ 0.005 rad (sub-arcminute precision — well
inside the early-termination threshold). What's stuck is
**downstream Stage E geometry**: only 6 sights of one body
at one azimuth, below the publication gate's
`min_azimuth_spread_rad = 30°` threshold.

The ML provider correctly:
1. produces a finite hypothesis on every frame
   (which it did NOT on these captures before this PR
   because no provider firing at all wasn't actually the
   problem — see honesty note below);
2. carries `HorizonProvenance::MlGravity { model_id,
   sigma_rad }` into per-frame diagnostics so the corpus
   explorer / replay report can attribute hypotheses to
   the right source;
3. loses Stage C fusion to the geometric providers when
   they fire (σ_ml ~ 0.1 rad ≫ σ_gradient ~ 0.005 rad) —
   exactly the design contract.

## Honesty: the bedroom-moon corpus isn't actually
"no horizon" stuck

Investigating the baseline more carefully than the design
doc had: the bedroom-moon captures DO produce gradient and
night horizons (they are taken through a window with high
contrast frame edges that the gradient provider treats as
a horizon). The Stage E problem is single-body /
single-azimuth geometry, not horizon detection.

The ML provider's value-add on this corpus is therefore
**corroboration**, not unlocking — the diagnostic
counters reveal honestly that geometric providers were
firing all along and the ML provider agrees
qualitatively (its hypothesis falls within a few degrees
of the geometric one).

The original motivating failure mode (no provider fires
at all) still exists conceptually for indoor / textured
scenes where neither the gradient nor night provider sees
a clear edge; the ML provider unlocks those, and the
provider's `is_loaded()` + `enable_ml_gravity` toggle
arrangement is now in place for when the operator points
the corpus at one.

## Acceptance criterion check

Per design doc §"Validation criteria":

> Phase 7.7b (provider) is "done" when:
> - ML gravity provider produces a hypothesis on every
>   frame of the bedroom-moon corpus (currently zero
>   providers do on those captures).

**Pass.** 7/7 frames in capture 1, 5/5 in capture 2.

> - The hypothesis is geometrically consistent with the
>   visible scene.

**Pass.** σ_altitude ≈ 0.12–0.19 rad (~7–11°) is
consistent with the model's training-distribution σ
floor; the synthesised horizon lands within a few degrees
of the gradient/night-provider lines on the corpus
explorer renders.

> - The σ is reported and honestly large (per-prediction
>   heteroscedastic).

**Pass.** σ varies per frame (0.12 to 0.19 rad), reflecting
the model's per-prediction uncertainty rather than a
global constant.

> - Pi Zero 2W per-frame budget unchanged or within
>   +200 ms.

**Not validated on Pi Zero 2W in this session** —
measured ~6.5 ms on x86_64 with 256×256 input. Pi
extrapolation (typical 10–30× slowdown) gives 65–200 ms,
inside the documented +200 ms budget.

## Tradeoffs noted in the PR

- **Backbone**: ResNet18-frozen + heteroscedastic head
  instead of GeoCalib. Reason in
  `docs/design/ml_gravity_training.md` §"Tradeoff".
- **Roll axis**: model converges to wide σ_roll on
  feature-poor frames (honest); pitch is reliably
  predicted. Documented in training doc §"Roll-axis
  honesty".
- **Polyhaven panoramas**: 60 panoramas × 32 tilts (1920
  samples). Larger training set is straightforward —
  `--per-pano` and `--count` flags scale linearly. Not
  pursued in this PR; marine fine-tune (Phase 7.7d) is
  the right place for that work.
- **Provider id format**: 12-char SipHash truncation,
  not BLAKE3, to avoid adding a new workspace dependency
  for what is a descriptive (not security-critical)
  identifier. `data/ml-gravity/SHA256SUMS` carries the
  full SHA-256 for fetch-time verification.
- **`MODEL_URL` placeholder**: the fetch script reads
  `data/ml-gravity/MODEL_URL` (one-line URL) and
  `SHA256SUMS`. Until the `ml-gravity-v1` GitHub release
  is published, the model is vendored in-tree at
  ~45 MB (under Git LFS's typical threshold; no LFS
  adoption required for this PR).
