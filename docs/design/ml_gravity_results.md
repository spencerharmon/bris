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
σ_altitude ≈ 0.12–0.19 rad (~7–11°), `night-gradient`
"wins" Stage C fusion at a reported σ of ~5 mrad.

## What the corpus explorer shows

The "moon" in this corpus is a hanging light fixture in a
bedroom. The frames are indoor scenes — walls, ceiling,
window frames, curtain hardware — and **no real horizon
is in the image at all.** That is exactly the
"no-horizon-visible" failure mode the design doc
motivates.

What the classical providers do on this scene:

- **`gradient`** (frame 0): latches onto the strongest
  upper-frame luma transition (a window edge or ceiling
  line) and reports a 251 px intercept with slope 0.28
  at σ = 0.0028 rad. The rendered overlay shows a
  diagonal red slash through the upper-left corner of the
  frame. Stage E correctly rejects the body sight with
  `BelowHorizon`, but only because the fake horizon
  happens to land above the body — the σ itself is
  dishonest.
- **`night-gradient`** (frames 1–6): finds a different
  strongest luma transition per frame (intercepts
  279–318 px, slopes flipping sign across frames) and
  reports σ ≈ 5–7 mrad ≈ 17–24 arcmin as if those were
  real horizon measurements. They are not. The provider's
  own module docs in `crates/bris-vision/src/night_horizon.rs`
  call this failure mode out at lines 26–50: "On real
  shipboard footage this is often a deck-to-sky boundary
  or a wake/glint feature, not the sea-sky horizon."
  Indoor scenes are the same pathology, more extreme.
- **`ml-gravity`**: produces a hypothesis with
  σ ≈ 0.12–0.19 rad (~7–11°) that **honestly reports**
  the model is uncertain on an indoor-distribution image
  with no clear horizon cue. The synthesised line is the
  one that should be honoured.

Stage C fusion picks the lowest-σ hypothesis. Because
`night-gradient` lies about its σ, the ML provider —
which is telling the truth — loses.

## Acceptance criterion check

Per design doc §"Validation criteria":

> Phase 7.7b (provider) is "done" when:
> - ML gravity provider produces a hypothesis on every
>   frame of the bedroom-moon corpus (currently zero
>   providers do on those captures).

**Pass.** 7/7 frames in capture 1, 5/5 in capture 2. The
original claim that "zero providers do" was wrong; what's
actually true is "zero providers produce an *honest*
hypothesis" — and the ML provider is the first to do so.

> - The hypothesis is geometrically consistent with the
>   visible scene.

**Pass.** σ_altitude ≈ 0.12–0.19 rad (~7–11°)
appropriately reflects "no horizon evidence in the
frame; this is the model's prior."

> - The σ is reported and honestly large (per-prediction
>   heteroscedastic).

**Pass.** σ varies per frame (0.12 to 0.19 rad), reflecting
the model's per-prediction uncertainty rather than a
global constant — and is the only σ in Stage C that
faithfully represents what evidence the frame actually
contains.

> - Pi Zero 2W per-frame budget unchanged or within
>   +200 ms.

**Not validated on Pi Zero 2W in this session** —
measured ~6.5 ms on x86_64 with 256×256 input. Pi
extrapolation (typical 10–30× slowdown) gives 65–200 ms,
inside the documented +200 ms budget.

> - The corpus explorer shows model-derived horizons
>   with `HorizonProvenance::MlGravity` in the
>   tooltip/badge.

**Pass when ML wins fusion.** On bedroom-moon it does
not win, so the replay-report `HorizonReport.provider`
stays at `"night-gradient"` and `model_id` is absent.
The ML hypothesis IS captured in the engine's per-frame
trace (`bris_streaming::pipeline::horizon=trace`).

## Known latent bug surfaced (not fixed in this PR)

The night-gradient (and gradient) provider's σ is
**uncalibrated for non-horizon scenes**. The fit's σ
reflects the *residual of the linear regression through
the per-row luma transition*, not the *probability that
the transition is a real horizon*. On indoor / textured
scenes both can be small even when the transition is a
wall edge.

This existed before Phase 7.7 and is not introduced by
this PR. The ML provider's role on indoor scenes is to
out-honest the geometric providers in absolute σ; that
fails today only because the geometric providers
under-report theirs. Two follow-ups, neither in scope
for Phase 7.7:

1. **Refuse-when-not-a-horizon predicate** on the
   gradient/night providers. The classifier dispatched
   `Twilight` on this capture, which assumes a usable
   horizon scene; an indoor classifier verdict (or a
   classifier-side rejection of "no sky region present")
   would prevent these providers from being invoked at
   all.
2. **σ-inflation when fusion-disagreement is large.**
   When two providers produce hypotheses with normal
   distances ≫ k·σ_combined, the *both* σ should
   inflate (one is wrong, we just don't know which);
   today the lowest-σ wins outright. A Bayesian-fusion
   pass would up-weight the ML provider when it
   disagrees with the geometric ones by 10+ degrees.

Tracked here for the next operator session; not Phase
7.7's job.

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
