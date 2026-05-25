# Auto-Detection of Horizon References

Status: design draft. Pairs with `horizon_brainstorm.md`
(method catalog) and `artificial_horizon.md` (IMU path). This
doc answers: **can Bris detect its own horizon aids — mirror,
puddle, cup, plumb line, vanishing points — without the
operator declaring them?** And: **what is the right
generalisation that covers all of them?**

## 1. The generalisation

A horizon reference is anything in the image that constrains
the **local-vertical direction in camera frame** (`g_cam`).
Given `g_cam`, the horizon line drops out as `ℓ = K⁻ᵀ g_cam`
(see `artificial_horizon.md` §3).

Three concrete constraint types cover every method in the
brainstorm:

| Constraint type | Yields | Examples |
|-----------------|--------|----------|
| **Mirror-pair**: same body seen twice via a horizontal reflector | `g_cam` direction *and* an altitude measurement directly | Cup, puddle, mirror, polished surface |
| **Vertical line**: a 1D feature in the scene known to be parallel to gravity | `g_cam` direction (up to sign) | Plumb line, building corner, lamp post, doorframe |
| **Horizontal vanishing point**: two or more parallel world-horizontal lines | a point on the horizon line; two such points fix `ℓ` (and hence `g_cam`) | Building edges, road markings, tile grids |

Plus a degenerate fourth that already exists:

| **Horizon-line direct**: detected sky-ground boundary | `ℓ` directly | Sea horizon, terrain skyline |

This taxonomy means **one engine abstraction** —
`HorizonProvider` returning a `g_cam` estimate with σ — covers
mirror, plumb, vanishing-points, IMU, segmentation. The
question "can we auto-detect" reduces to: **can we, in a
generic image, identify which of these features are present
and trust them enough to constrain `g_cam`?**

## 2. The general auto-detection problem

Bris already runs body detection (Stage B). Auto-detection of
horizon aids reuses the same image budget by adding:

1. A **proposal stage** that finds *candidate* horizon-aid
   features (reflective regions, near-vertical lines, line
   clusters with vanishing points).
2. A **scoring stage** that ranks proposals by physical
   plausibility and by **consistency with each other** and
   with detected bodies.
3. A **fusion stage** that combines the surviving proposals
   into a single `g_cam` posterior with honest σ.

Two principles govern the whole pipeline:

- **Cross-validation is the only protection against hallucination.**
  Any individual horizon aid can be spoofed by a scene (a
  reflection in a window pane isn't horizontal; a building
  edge isn't truly vertical; the plumb line might be swinging).
  Three independent aids agreeing is strong; one aid asserting
  is weak. The fusion stage's job is to encode this honestly.
- **Geometric self-consistency before model confidence.**
  Detected mirror pair must actually subtend `2·Ho` for a
  known body's predicted altitude (using the rough position
  fix as prior). Plumb line must be parallel to other plumb
  lines if multiple are detected. Vanishing points must lie
  on a single line in the image. These geometric tests are
  cheap and discard ~all hallucinations.

## 3. Reflection auto-detection

### 3.1 Operator's framing — exactly the right idea

> "If two bodies are detected, one in a sky region and one in
> a cup/mirror/puddle, automatically calculate as artificial
> horizon."

Yes. This is the cleanest formulation because it **inverts the
problem**: instead of asking "is that puddle in the lower
half of my frame?" we ask "do I see this body twice, with the
right relationship?" The answer is detectable from the body
detections alone, without ever segmenting or classifying the
reflective surface.

### 3.2 The pair-detection algorithm

Given Stage B output (a set of detected body candidates with
sub-pixel centroids and brightness), find pairs `(b_up, b_dn)`
satisfying:

**Test 1 — Geometric: mirror-pair angle.**
A horizontal reflector produces a virtual image such that the
real body, the reflector point, and the virtual body are
coplanar (the plane being vertical, containing the body and
the observer). The angle between the real and virtual body
rays at the camera equals `2·Ho`. So:

- For each pair `(b_up, b_dn)`, compute the angle θ between
  their camera-frame rays (via `K⁻¹`).
- Compute the candidate vertical direction as the **bisector**
  of the two rays: it must point upward, i.e. its inferred
  `g_cam` must have negative z-component (roughly: the camera
  is looking forward, not floorward).
- Verify the pair lies in a roughly **vertical plane through
  the optical centre** — i.e. the cross-product of the two
  rays is nearly horizontal in camera frame. This is the
  strongest geometric filter; it rules out arbitrary star
  pairs that happen to have plausible angles.

**Test 2 — Photometric: brightness ratio.**
Reflections lose energy. Typical water reflectivity is 2–6%
near grazing, rising toward 100% at grazing; a mirror is ~90%;
oil ~5%. The reflected image must be **dimmer** than the
direct image (after correcting for any local exposure
gradient).

- Reject pairs where `b_dn` is brighter than `b_up` by more
  than measurement noise.
- Use the brightness ratio as a *prior* on reflector type
  (mirror vs water) — informational, not gating.

**Test 3 — Catalog consistency.**
This is the killer test. Use the rough position prior (DR,
last fix, GNSS if available, or even just "Earth-sized") and
the timestamp to predict each detected body's altitude `Ho_pred`
with its σ from the prior. Then:

- For each candidate pair, compute the implied altitude
  `Ho_meas = θ/2`.
- A pair is **consistent** iff `|Ho_meas − Ho_pred| < k·σ_pred`
  for some k (3–5).
- Bodies whose direct detection already gives an unambiguous
  catalog match (post-plate-solve) provide the strongest
  anchor; for these, `σ_pred` is tight and the pair test is
  nearly binary.

**Test 4 — Multi-body consistency.**
If two pairs `(b1_up, b1_dn)` and `(b2_up, b2_dn)` both pass
Tests 1–3, the **inferred `g_cam` from each pair must agree**.
This is the equivalent of a sextant "swung" against multiple
stars. Two-pair agreement is near-conclusive evidence of a
genuine reflector.

**Test 5 — Reflector region (optional, weak).**
Once a pair is hypothesised, the **reflector surface** must
contain the reflection point. Cast the ray from the virtual
body to the camera; where it crosses the inferred horizontal
plane (at any height ≤ camera height) is the reflection point.
If the image at that pixel is consistent with a reflective
surface (high local variance from sky-replica content, smooth
boundary), confidence rises. If the pixel is on a face, a
tree, the sky, etc., the pair is rejected.

This test is *useful* but not *necessary*; Tests 1–4 are
sufficient. Test 5 is an additional filter for ambiguous
single-pair cases.

### 3.3 What the operator does

Nothing. They hold the phone up so the sky and the puddle are
both in frame. Stage B detects the body and its reflection
independently. The pair-finder picks them out and emits a
horizon hypothesis. The operator never declares "there is a
puddle here."

### 3.4 Edge cases and failure modes

- **Window reflections** — a sun or moon reflected in a vertical
  window pane will fail Test 1 (the bisector won't be vertical
  unless the window is on the floor) and Test 3 (the implied
  altitude won't match any catalog body).
- **Curved reflectors** — a reflection in a car body or kettle
  is non-planar; the pair geometry won't be consistent across
  multiple body pairs (Test 4 fails). One pair might pass
  Tests 1–3 by accident; this is the main reason to require
  multi-pair confirmation for high-σ-floor confidence.
- **Refraction (looking down into water)** — the *refracted*
  body image you see *below* the water surface is at a
  different angle than the *reflected* image at the surface.
  In practice the reflected image dominates at grazing
  incidence (where the body is high in the sky); the refracted
  image dominates at near-normal incidence (looking straight
  down). Bris should only trust the **reflection**, which is
  what passes Test 1 by geometry.
- **Two genuine bodies** that happen to be at altitudes
  `Ho` and approximately `-Ho` (i.e. equal angles above and
  below horizon) — astronomically rare for two catalogued
  bodies but not impossible (a star setting in the west and
  one rising in the east). Test 4 (multi-pair, with a real
  reflector all pairs share the same `g_cam`) and Test 2
  (brightness — they'd be equally bright) discriminate.
- **No catalog prior available** (cold-start, no time, no DR
  position). Drop Test 3; require *three* concordant pairs
  for Test 4 instead of two. Cold-start mirror-only detection
  is harder but possible.

### 3.5 Generalisation to "any reflection source"

The algorithm doesn't care what the reflector *is*. Cup, mirror,
puddle, lake, polished hood, marble floor, mercury bath — all
that matters is that the surface is locally horizontal across
the reflection point. Tests 1, 3, and 4 enforce horizontality
geometrically; the algorithm is *agnostic* to reflector
identity.

This is the cleanest possible generalisation: **one detector,
all reflectors**. The only "type-specific" knowledge that
might be added later is brightness-ratio priors (Test 2 with a
mixture model over reflector types) for marginal cases.

### 3.6 Engine integration

- New `ReflectionPairProvider` implementing `HorizonProvider`.
- Consumes Stage B body candidates (no extra image processing).
- Emits `HorizonHypothesis { g_cam, sigma_rad, ho_direct, evidence }`
  where `ho_direct` is the **directly measured altitude** for
  the participating body pair — this is the rare case where a
  horizon provider also produces a sight, and the engine
  should consume both.
- Cost: O(N²) over body candidates, N is typically ~3–30.
  Negligible.

## 4. Plumb-line auto-detection

### 4.1 Auto-detection feasibility

Yes, with caveats. A plumb line is a near-vertical line
segment in the image. Naive line detection (Hough, LSD, ELSED)
finds *many* near-vertical lines in any realistic scene —
building edges, doorframes, poles, even noise. The
discriminators are:

**Geometric**: a true plumb line in the camera frame projects
to an image line that is the **projection of the gravity
vector** — i.e. it must lie on `ℓ_v = K⁻¹ g_cam · (point on
camera frustum origin)` … which is circular: we're trying to
*find* `g_cam`. What we can use:

- A plumb line and a vertical vanishing point are the **same
  feature** geometrically. Multiple plumb lines in the image
  converge to the vertical vanishing point. So: if `≥2`
  near-vertical lines converge to a single point off-image
  (and that point is on the appropriate side — below the
  image for a camera held upright, above for one held
  upside-down), they jointly attest a vertical direction.
- A *single* plumb line, alone, gives only weak evidence; it
  could be a building corner. The single-line case requires
  one of:
  - Operator declaration ("there is a plumb line in this
    region of the frame"), reducing the problem to detection
    in an ROI.
  - A **swing signature**: across N consecutive frames, the
    line's *angle* oscillates symmetrically about a mean —
    pendulum behaviour. Building edges don't swing. This is
    the only known method to discriminate a single plumb
    line from a static vertical structure without operator
    input.

**Photometric**: in practice plumb-line accessories will be
high-contrast (dark string on light bracket, or LED-illuminated
for night use). A purpose-built accessory simplifies detection
dramatically; a found plumb line (random hanging cord in the
scene) is much harder.

### 4.2 The "plumb line ≡ vertical vanishing point" insight

This is the key generalisation: **auto-detected plumb lines and
auto-detected vertical vanishing points are the same algorithm
with different priors on line density.**

- "Plumb line" mode: expect 1–2 strong near-vertical lines;
  high confidence each.
- "Vanishing point" mode: expect many near-vertical lines; each
  weak; aggregate them.

Both reduce to: find clusters of image lines whose intersection
(in homogeneous coordinates) defines a vanishing point; pick
the cluster whose vanishing point is most "vertical-looking"
(below the image when camera is roughly upright); declare that
vanishing point's direction in camera frame as `g_cam`.

So implementing **vertical vanishing point detection
generically subsumes plumb-line detection**, and the plumb-line
accessory becomes "a vertical edge that improves the detection
robustness, not a special case."

### 4.3 Single-frame ambiguity: vertical vs gravity

A vertical vanishing point gives `g_cam` **up to sign**. Is
"down" the direction with negative-y vanishing point, or the
opposite? The camera could be held upright or upside-down.
Disambiguation:

- A horizontal vanishing point (any building edge in plan view)
  is by construction *perpendicular* to gravity. Two
  perpendicular vanishing points + one near-vertical: the
  ambiguity collapses.
- Detected body positions: bodies are always above the horizon
  (negative altitudes are not observable for sight reduction).
  If the candidate `g_cam` would put all detected bodies
  below the horizon, flip the sign.
- IMU (if present) — even crude phone IMU disambiguates
  trivially.

### 4.4 Engine integration

- New `VerticalLineProvider` (handles both plumb and vertical
  vanishing point) consumes a line-segment extraction
  (LSD/ELSED, ~5–15 ms on a phone).
- For a single near-vertical line + multi-frame buffer:
  optional **swing detector** flags pendulum-like oscillation
  and confirms plumb-line interpretation.
- For multiple near-vertical lines: standard J-linkage or
  EM clustering yields vanishing point with σ.

## 5. Vanishing-point auto-detection (horizontal)

### 5.1 The horizon line, directly

Two or more **horizontal** vanishing points lie *on the horizon
line by definition*. So:

1. Extract all line segments in the image.
2. Cluster lines by intersection point (RANSAC, J-linkage, or
   ML — NeurVPS, CTRL-C).
3. The vertical cluster gives `g_cam`-direction up to sign;
   the horizontal clusters give points on `ℓ`.
4. Cross-check: the horizontal vanishing points must be
   colinear with each other, *and* that line must be the same
   `ℓ = K⁻ᵀ g_cam` predicted by the vertical cluster. Mutual
   consistency is the validation.

### 5.2 What counts as a horizontal line in the world?

Anything you might call "level": tabletops, lintels, road
markings, fence rails, courses of bricks, even the printed
lines of a book held flat. The detector doesn't classify; it
clusters by image geometry. Whatever clusters into a strong
vanishing point that **passes the horizon-line consistency
check with the vertical cluster** is, ipso facto, horizontal
in the world.

The classifier-free formulation is important: there's no
"detect buildings" or "detect roads" step. The geometry is
self-validating.

### 5.3 Failure modes

- **Manhattan-world violation**: scenes with structure along
  three or more non-orthogonal directions (some natural
  landscapes, some interiors). Clustering produces extra
  vanishing points; horizon-line consistency check still
  picks out the right one if vertical is unambiguous.
- **Single-direction scenes**: a long straight road with no
  cross-streets gives one horizontal vanishing point. One
  point + vertical direction is sufficient (the horizon line
  passes through the vanishing point and is perpendicular,
  in image, to the vertical-vanishing-point direction).
- **Curved-line scenes** (forests, organic landscapes): the
  detector finds few or no consistent line clusters; method
  returns no hypothesis. Graceful failure.
- **Tilted Manhattan-world** (a leaning tower of Pisa or a
  furniture-store aisle): the dominant cluster is not
  vertical. Cross-check against IMU or detected bodies
  disambiguates.

### 5.4 Engine integration

- `VanishingPointProvider` consumes line segments shared with
  `VerticalLineProvider`. (They are the same module under the
  hood; the split above is conceptual.)
- Outputs one `g_cam` posterior per scene if successful.

## 6. The unified pipeline

```
                       ┌────────────────────────┐
                       │  Frame + Intrinsics    │
                       └───────────┬────────────┘
                                   │
              ┌────────────────────┼─────────────────────┐
              │                    │                     │
        Body detection       Line segments         (optional)
        (Stage B, shipped)   (LSD/ELSED, new)      IMU gravity
              │                    │                     │
   ┌──────────┴───────┐    ┌───────┴─────────┐    ┌──────┴──────┐
   │ ReflectionPair   │    │ VerticalLine    │    │   IMU       │
   │   Provider       │    │   Provider      │    │  Provider   │
   │                  │    │ + VanishingPt   │    │             │
   └────────┬─────────┘    └───────┬─────────┘    └──────┬──────┘
            │                      │                     │
            └──────────────────────┼─────────────────────┘
                                   ▼
                       ┌──────────────────────┐
                       │  HorizonFusion       │
                       │   (σ-weighted        │
                       │    consensus +       │
                       │    independence-     │
                       │    aware combiner)   │
                       └──────────┬───────────┘
                                  ▼
                           HorizonHypothesis
                       (g_cam, sigma_rad, provenance)
```

Each provider produces zero or one `g_cam` hypothesis per frame
with σ. The fusion stage:

1. Discards providers whose σ exceeds a threshold (e.g. 1°).
2. Tests pairwise agreement: if two independent providers
   disagree by > k·σ_combined, both are marked suspect and σ
   inflated for both.
3. Combines surviving hypotheses by σ-weighted average,
   respecting independence (mirror vs IMU: independent; mirror
   vs sea horizon at sea: not independent — both rely on the
   same image content for body localisation).
4. Emits a single `g_cam` with a covariance, plus a
   `provenance` field listing which providers contributed.

This last is critical for PBRIS honesty: the output sentence
says exactly which methods drove the fix and at what σ.

## 7. What can and can't be auto-detected — summary table

| Method                | Auto-detect feasible? | Best discriminator | Failure mode |
|-----------------------|:---------------------:|--------------------|--------------|
| Cup/puddle/mirror reflection | **Yes** (pair detection) | Bisector vertical + catalog consistency (Test 3) | Convex reflectors give one false pair per scene |
| Plumb line (alone)    | Weak                  | Multi-frame swing signature | Static building edges look identical |
| Plumb line (≥2)       | **Yes** (= vertical vp) | Joint convergence | Same as vanishing-point |
| Vertical vanishing point | **Yes**            | Horizontal-vp consistency | Curved/organic scenes |
| Horizontal vanishing point | **Yes**          | Colinearity = horizon line | Single-direction scenes (still works, weaker σ) |
| IMU gravity           | Always available, no detection needed | — | Acceleration ambiguity |
| Sea horizon / skyline | **Yes** (shipped)     | Sky-ground gradient | Haze, false horizons |

## 8. PBRIS provenance

The `$PBRIS` diagnostic sentence (per `docs/protocol/pbris.md`)
should grow a `horizon_provenance` field encoding which
providers contributed to the fix. Proposed encoding (versioned
per the existing pbris versioning discipline):

```
horizon_provenance="mirror+imu"
horizon_provenance="vanishingpt"
horizon_provenance="sea"
horizon_provenance="mirror+imu+vanishingpt"
```

with each named provider's σ contribution available in a
companion field. This makes a fix self-documenting; an operator
reviewing a debug-mode submission can immediately see *why*
the engine thought it knew the horizon.

## 9. Implementation priority

Subjective ordering for follow-up planning:

1. **`ReflectionPairProvider`** — highest leverage. Reuses
   Stage B output, requires no new image processing, gives
   *both* a horizon and a sight in one pair. Geometrically
   self-validating via Tests 1, 3, 4. **Phase 1 (this branch).**
2. **`VerticalLineProvider` + `VanishingPointProvider`** —
   single module under the hood; closes the IMU-less daylight
   land case. Requires adding line-segment extraction (one
   well-understood algorithm, modest compute).
3. **Fusion layer** — full version only worth building after
   ≥2 providers exist. **Phase 1 lands a trait + single-
   provider stub fusion** that simply forwards the one
   available hypothesis; this keeps the integration seam in
   place so the second provider drops in cleanly.
4. IMU provider (per `artificial_horizon.md`) — orthogonal;
   independent track.
5. Multi-frame swing detector for single-plumb-line case —
   optional; only if §3.1 lone-plumb support is desired.

## 10. Decisions for Phase 1 (reflection-pair, intra-frame)

These answer the open questions for the implementation
landing on this branch. Out-of-scope items remain open.

- **Sight emission policy: option (ii).** A successful
  reflection pair emits **both** a `HorizonHypothesis` and a
  direct sight `Ho = θ/2` for the participating body. The
  direct sight is the more accurate of the two for that body
  at that instant; suppressing the body's separately-derived
  horizon-based sight to avoid double-counting is the
  responsibility of the sight-combination stage in
  `bris-nav` (which already de-duplicates per-body sights in
  a window). Phase 1 documents the double-counting risk in
  code comments and a regression test; Phase 2 may revisit.
- **Catalog-consistency prior (Test 3): last successful fix,
  with cold-start fallback.** Preference order:
  1. Last successful fix (tight σ).
  2. DR projection from last fix.
  3. GNSS (phone only; Pi has none unless attached).
  4. **Cold start** → drop Test 3, require **≥3 concordant
     pairs** under Test 4 before emitting a hypothesis.
  Cold-start mirror detection is permitted but conservative.
- **Operator UX: none.** No toggle, no opt-in. The provider
  is always-on inside the engine, runs intra-frame against
  Stage B output, and only contributes when its tests pass.
  This is consistent with the honest-uncertainty rule: the
  engine reports what it sees, the operator does not have to
  configure it. There is no live-view UI change for Phase 1.
- **Trait + stub fusion landed together.** `HorizonProvider`
  trait + a one-provider passthrough fusion layer ship in the
  same PR as `ReflectionPairProvider`. The seam exists; the
  second provider is then a drop-in.
- **Cross-frame: out of scope for Phase 1.** Intra-frame only
  (Δq = identity). See §11 for the cross-frame design.

## 11. Cross-frame registration

The per-provider treatments in §3–§5 implicitly assume the
body, the reflection, the plumb line, and the parallel lines
used for vanishing points all live **in the same frame**.
This section formalises what happens when they don't, and
how the engine should treat the boundary.

### 11.1 The problem

Stage B's stitching combines per-body detections across frames
into a single sight, using the streaming engine's inter-frame
pose link (`Δq`) to register the frames into a common
attitude reference. A camera that pans, tilts, or rolls
between frames still produces stitchable sights because Δq
is known (when plate-solve links the frames) and the σ of Δq
is tracked.

The providers in this document each rely on a *geometric*
relationship between features at one instant:

- **Reflection pair (§3)** — the bisector of the two rays is
  vertical *in the camera frame at the moment of capture*.
  If body and reflection arrive in different frames and the
  camera rotated between them, the naïve bisector is **not**
  vertical; it is the bisector of two rays measured in
  different attitude frames.
- **Vertical line / plumb (§4)** — a near-vertical line
  detected in frame N and another in frame M only attest a
  joint vertical vanishing point after both are expressed in
  a common frame.
- **Vanishing points (§5)** — same issue: clustering line
  segments across frames requires rotating each segment into
  a common attitude reference first.

### 11.2 The fix

For each cross-frame use, rotate the off-frame feature into
the reference frame using the streaming engine's pose-chain:

1. For two frames N, M used together, retrieve
   `Δq_{N→M}` and its covariance from the streaming engine
   (this is already maintained for sight stitching).
2. Rotate the off-frame feature (a body ray, a line segment,
   a vanishing-point direction) by `Δq_{N→M}` into frame M.
3. Run the provider's geometric tests in frame M as if
   intra-frame.
4. **Inflate the resulting σ by σ_Δq.** This is non-negotiable
   under the honest-uncertainty rule; the σ of the pose link
   is part of the measurement chain.

Window eligibility:

- Cross-frame integration requires a **valid pose link** for
  every frame in the window (plate-solve succeeded on at least
  the linking frames). Without a link, the provider declines
  cross-frame use and falls back to intra-frame only.
- A `frame_window_s` config bounds how far back the engine
  looks; default proposal 5 s for sights of stars and the sun,
  shorter for the moon (which moves visibly).
- Bodies that *move on the catalog timescale* (planets, moon)
  must be evaluated at **each detection's own timestamp** for
  Test 3 (catalog consistency), not the window midpoint. This
  is already required by Stage B stitching, so the machinery
  exists.

### 11.3 Failure modes specific to cross-frame

- **No pose link** — provider declines; logs
  `provider_skipped_no_pose_link` diagnostic.
- **Large pan between frames** — σ_Δq grows; resulting horizon
  σ may exceed `horizon_early_termination_sigma_rad`. Engine
  drops the hypothesis honestly rather than reporting a
  fragile one.
- **Reflector moved between frames** (puddle ripple, mirror
  bumped) — the pair fails Test 1 in the rotated common
  frame, exactly as a curved-reflector pair fails intra-frame.
  Multi-pair agreement (Test 4) catches this if multiple
  bodies are detected.
- **Body moved between frames** — handled by per-detection
  timestamping; not a cross-frame-specific failure.
- **Plate-solve disagreement** between the linking frames
  inflates σ_Δq directly; honest σ flow handles the rest.

### 11.4 Provider responsibilities

Each `HorizonProvider` declares whether it operates
per-frame or window-wide via a trait method (e.g.
`fn temporal_scope(&self) -> TemporalScope`). The fusion
layer routes detections to providers accordingly. A
window-wide provider receives a registered pose chain along
with the frame buffer; an intra-frame provider receives only
the current frame.

`ReflectionPairProvider` is hybrid:
- **Intra-frame mode** is the default and the only Phase 1
  scope.
- **Cross-frame mode** is added in a later phase; same
  algorithm, with rays rotated into a common frame and σ
  inflated by σ_Δq.

`VerticalLineProvider` and `VanishingPointProvider` are
naturally window-wide once they exist: line segments from
multiple frames in a short window strengthen the clustering.
They will be designed as window-wide from the start.

### 11.5 PBRIS provenance for cross-frame

Extend the provenance encoding (§8) with a temporal-scope
flag per provider:

```
horizon_provenance="reflection:intraframe"
horizon_provenance="reflection:crossframe(window=2.4s,sigma_dq=0.7arcmin)"
horizon_provenance="vanishingpt:window(2.0s)+imu"
```

This makes after-the-fact debugging of why a fix's horizon σ
is larger than expected possible from PBRIS alone.

### 11.6 Phase 1 boundary

Phase 1 lands intra-frame `ReflectionPairProvider` only. The
provider's trait method `temporal_scope()` returns
`TemporalScope::IntraFrame`. The cross-frame path (§11.2) is
designed in this document but not implemented; the trait shape
must accommodate it so the later phase is additive, not a
refactor.

## 12. Open questions (deferred)

- Line-segment extractor choice: LSD (classical, deterministic,
  Apache-licensed), ELSED (faster, MIT), or an ML detector?
  LSD is the safe choice when the line-using providers land.
- Does Pi Zero 2W have the compute budget for LSD + clustering
  + body detection per frame? Probably yes at 1280-long-edge
  (Stage B is the dominant cost), but worth a benchmark before
  committing.
- Should we ship a list of "common false reflector geometries"
  (window panes, car bodies) for the operator to mark as
  no-go regions? Probably no — Tests 3 and 4 already discard
  these.
- For cross-frame reflection pairs, what's the right
  `frame_window_s` default per body class (sun, moon, planet,
  star)? Empirical; defer until the corpus has examples.
