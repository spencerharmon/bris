# Horizon-Finding Brainstorm

Status: brainstorm; companion to the formal designs in
`artificial_horizon.md` (IMU path) and `horizon_autodetect.md`
(reflection, plumb, vanishing-point auto-detection +
cross-frame registration). The point is to enumerate
every plausible way to obtain a local-horizontal reference for
a celestial sight, evaluate each, and *not* pre-filter. Picking
which ones to actually implement is a separate exercise that
follows this one; the current Phase 1 decision is the
reflection-pair provider, per `horizon_autodetect.md` §10.

## 0. Clearing up the confusion: vertical ⇒ horizon, exactly

> "I can't really understand how it's possible to know where the
> horizon is by only determining the vertical in a camera frame."

It's pure geometry, no approximation:

- The **true horizon** is the locus of points at infinity in the
  plane perpendicular to local gravity. By definition.
- For a calibrated camera (intrinsics `K` known), the image of
  any 3-space plane is a line determined entirely by that
  plane's normal `n`: `ℓ = K⁻ᵀ n`.
- Therefore: gravity vector `g_cam` in camera frame +
  intrinsics `K` ⇒ horizon line `ℓ` in pixels. Deterministic.
  No scene content needed.

The line you compute this way is **the actual horizon** — the
same line you would see if you were on an infinite, perfectly
flat plane at sea level with no obstructions. The image only
"shows" you the horizon when nothing is in the way; geometry
"knows" where the horizon is even when a mountain or your
ceiling is in front of it.

This is the same fact that lets architectural photographers
"find the horizon" in an image of a building: parallel
horizontal lines in the world converge to the **horizon line
at infinity**, regardless of whether you can see the actual
horizon in the frame. Vanishing-point methods (§3 below)
exploit exactly this.

So once we trust *any* source of local vertical — IMU, plumb
line, water surface, vertical building edges, ML model — we
trust the horizon line that geometry gives us from it. The
hard part is sourcing vertical; the horizon is then free.

This also explains the inverse: any method that gives us a
**horizon directly** (sea horizon, terrain-DEM match, sky
segmentation) implicitly gives us vertical — they are
equivalent under the projection.

## 1. Method taxonomy

Three broad families, with cross-cutting hardware requirements:

**A. Direct horizon observation** — image contains the actual
   horizon (or a proxy for it), detected optically.

**B. Vertical-reference observation** — image or sensor gives a
   local-vertical direction; horizon follows from §0.

**C. No horizon at all** — sight-reduction variants that don't
   need a horizontal reference.

Each method is rated rough-and-ready on:

- **σ_alt** — order-of-magnitude 1σ altitude error (arcmin)
- **Env** — daylight / night / indoor / land / sea
- **HW** — what hardware is required
- **Cal** — calibration burden (once-per-device, per-session, per-frame)
- **Bris fit** — Pi Zero 2W / phone / both

## 2. Family A — direct horizon observation

### A1. Sea horizon (already implemented)

- σ_alt: 1–3′
- Env: daylight, clear, at sea (or coast looking seaward)
- HW: camera only
- Cal: eye-height for dip
- Status: shipped, `pipeline/horizon.rs` Stage C gradient + sky-region detectors

### A2. Floating-mirror artificial horizon (operator's original idea)

The classical surveyor's method. A pool of mercury, oil, or
water (or a gimballed mirror floating on it) reflects the body.
The angle between the body and its reflection, as seen by the
observer, is **twice the body's true altitude**:

```
Ho = (angle_body_to_reflection) / 2
```

Because the reflecting surface is perpendicular to gravity by
hydrostatic equilibrium, the reflection geometry gives true
altitude with **no horizon needed** and **no dip correction**.

- σ_alt: 0.5–2′ at best (limited by mirror surface ripple,
  fluid meniscus, and how steady the operator holds the
  camera so both images stay in frame); historically the
  most accurate land-based method, used for geodesy.
- Env: daylight or night; needs flat ground for the dish, calm
  air for the surface
- HW: camera + reflecting dish (mercury — toxic; oil — viable;
  water + dark backing — easy; small first-surface mirror on a
  fluid bath — best). Bris-provided 3D-printable dish would be
  a nice touch.
- Cal: none, modulo verifying the surface is actually level
  (which the geometry *enforces* — it's why this method exists)
- Bris fit: **both Pi and phone**; particularly attractive for
  the headless Pi appliance because it needs zero extra
  electronics.

Algorithm sketch: detect the body in the upper half of the
frame (existing star/sun detector). Detect the reflection in
the lower half. Compute the pixel-space angle between them
relative to the camera optical centre, convert to a world
angle via intrinsics, halve it. Done.

Subtlety: the camera must be roughly above the dish so the
reflection geometry is the symmetric "double-altitude" case.
The general case (off-axis camera) involves the dish's surface
normal explicitly and reduces to A1's geometry with a known
horizontal mirror.

This is a strong candidate. Worth its own design doc.

### A3. Reflection in a natural water surface

Same as A2 but using a puddle, pond, lake, or harbour. The
operator doesn't carry the artifact; they find one.

- σ_alt: 2–10′ depending on ripple
- Env: requires a still water surface in view
- HW: camera only
- Cal: detect that the surface *is* horizontal (it should be,
  if it's water at rest)
- Bris fit: both. Pure software addition.

### A4. Terrain skyline matched against DEM

Given approximate position (GNSS or DR), render the predicted
terrain skyline from a Digital Elevation Model (SRTM, Copernicus
GLO-30) and align it to the detected skyline in the image. The
alignment yields **full camera attitude** (roll, pitch, yaw)
directly; the horizon line falls out.

- σ_alt: 1–5′ depending on terrain relief and DEM resolution
- Env: land with visible distinguishable terrain on the horizon
- HW: camera + on-device DEM tile + ~known position
- Cal: none
- Bris fit: phone (storage + memory); Pi marginal. SRTM tiles
  are ~25 MB each at 1 arcsecond; a sailor crossing oceans
  doesn't need them, a mountaineer does.

This is essentially "celestial navigation but with mountains
instead of stars." Excellent on land, useless at sea.

### A5. Building skyline / urban silhouette

Generalisation of A4 using OpenStreetMap building footprints +
height tags instead of a DEM. Same algorithm, different data
source.

- σ_alt: 2–10′ (OSM heights are coarse)
- Env: urban
- Bris fit: phone primarily. Niche but neat — a city operator
  could navigate by skyline.

### A6. Cloud base / cloud horizon

Bad idea. Cloud bases are not horizontal, vary in altitude, and
move. Listed only to be dismissed. (Aviators historically used
the "false horizon" of distant haze layers; this is famously
unreliable and contributed to many accidents.)

### A7. ML semantic horizon segmentation

Train (or use off-the-shelf) a sky/ground segmentation network
and treat the boundary as the horizon. Existing models:
SkyFinder, COCO-Stuff "sky", or recent transformer models.

- σ_alt: 5–30′ depending on scene. The "horizon" the model
  finds is the *sky-ground boundary*, which is the **terrain
  silhouette**, not the true horizon. Useful as a cue, not as
  a precise reference.
- Env: any with sky visible
- HW: phone (model size ~5–50 MB); Pi marginal
- Cal: model-dependent
- Bris fit: useful for **bootstrapping** other detectors or for
  rough fixes when nothing better is available. Already
  partially implemented (`segmentation_model_path` config exists).

### A8. ML horizon-line regression (single-image perspective)

A different ML genre: networks trained to predict the **true
horizon line** (and optionally roll, pitch, focal length) from
a single image, even when the actual horizon is occluded. Key
recent work:

- *PerspectiveFields* (Jin et al., CVPR 2023) — dense
  per-pixel up-vector + latitude fields; horizon line falls out.
- *GeoCalib* (Veicht et al., ECCV 2024) — predicts gravity
  direction + focal length jointly, designed for in-the-wild
  images.
- *DeepHorizon*, *UprightNet*, *CTRL-C*, etc.

These models effectively learn the prior "humans photograph
upright, vertical things are vertical, horizontal things are
horizontal" and infer gravity from the scene. On indoor and
outdoor scenes with even modest structure they achieve ~1–3°
on benchmarks — too coarse for primary sight reduction but
fine as a sanity check or prior.

- σ_alt: 30–120′ on average; better on structured scenes
- Env: anything photogenic
- HW: phone; Pi probably not (models 10–200 MB, inference cost)
- Bris fit: cross-check / fallback. Particularly valuable
  paired with §B5 (vanishing points), which these models
  essentially learned to imitate.

## 3. Family B — vertical-reference observation

### B1. Phone IMU (covered in `artificial_horizon.md`)

σ_alt: 6–30′. Phone-only.

### B2. External MEMS IMU on the Pi (BNO055, ICM-20948, BMI270…)

The Pi Zero 2W has no IMU. A USD$10–30 breakout over I2C
provides one with comparable performance to phone fused
rotation sensors. Some (BNO055) ship with on-chip fusion;
others (ICM-20948) require a software filter.

- σ_alt: 5–30′ as B1, modulo enclosure rigidity and bias
  characterisation. **High-end MEMS** (ADIS16500 series,
  ~$1000) can reach 0.3–1′ — interesting for the appliance
  product tier.
- Env: any
- HW: Pi + IMU breakout + rigid camera-IMU mount
- Cal: IMU↔camera extrinsic (per assembly); IMU bias (per power-on);
  scale factor (factory or once per device)
- Bris fit: **the** answer to the operator's question about
  Pi-side IMU support. Pi gets IMU input via the same
  `gravity` field on `Frame` that the phone path uses, sourced
  from the I2C driver instead of `SensorManager`. Engine code
  doesn't care where the gravity vector came from.

### B3. Plumb line in frame

Hang a weight on a string in the camera's field of view. The
string is, by construction, parallel to local gravity. Detect
it (it's a high-contrast line of known approximate location)
and read off the vertical direction.

- σ_alt: 1–5′ if the string is steady and the camera sees
  enough of it. Limited by pendulum oscillation period
  (~1 s for a 25 cm string); average over many frames.
- Env: any windless environment; works at night with a small
  illuminator on the weight
- HW: string + weight + clip-on bracket
- Cal: none (geometry is self-correcting; the string is the reference)
- Bris fit: **both Pi and phone**, particularly the appliance.
  A folding plumb-line accessory is a clean answer to the
  "we need a vertical reference and don't want to ship MEMS"
  question. Trivially detectable optically (Hough line in a
  predictable image region).

A clever variant: a plumb line in front of a **printed
reference target** so the algorithm gets both vertical and
focal length / distortion check in one frame.

### B4. Liquid level in a transparent vessel

A cup of water in the camera's field of view. The water surface
is horizontal; its edge against the vessel wall is a horizontal
**line** in the world that projects to a line in the image
whose orientation reveals camera roll, and whose perspective
reveals camera pitch.

- σ_alt: 5–20′; lower than plumb because the contrast at the
  meniscus is weaker than at a string
- Env: any windless
- HW: cup + water
- Bris fit: charming but probably dominated by B3 in every
  dimension.

### B5. Vanishing points (Manhattan world)

In any scene with multiple parallel horizontal lines (building
edges, road markings, tile grids, doorframes, books on a
shelf), the lines converge to **vanishing points on the horizon
line**. Two such horizontal vanishing points determine the
horizon line uniquely. A third vanishing point (vertical
lines: building corners, doorframes, lamp posts, tree trunks
in cultivated forests) determines vertical.

- σ_alt: 1–10′ in scenes with clean parallel structure; much
  worse otherwise
- Env: any structured scene — urban, indoor, even sailboat
  interiors
- HW: camera only
- Cal: none, *if* intrinsics are calibrated
- Bris fit: **strong**. Pure software. Works at night with
  street lighting. Works indoors (you can fix indoor by
  shooting through a window). Operator's instinct about
  "averaging vertical edges of buildings and poles" is
  *exactly* this; the math is mature (J-linkage, EM, RANSAC
  variants — Lezama et al. 2014, Antunes & Barreto 2013;
  modern ML drop-in: NeurVPS, CTRL-C).

This deserves its own design doc. It is the most promising
**land-based, IMU-free, daylight-or-night** horizon source we
have.

### B6. Detected stars themselves (after plate solve)

Once Stage B detects ≥3 stars and the plate solver succeeds,
we have the camera's full attitude in **ICRS**. To convert
that to local horizontal we need our position — but the
position is what we're trying to find. Circular.

**However**: once a *first* fix is obtained (by any other
method), subsequent frames can use the previous frame's
horizon as a prior, refined by the new plate solve. This is
the IMU-prior horizon idea in `plan.org:1062` generalised to
"any prior horizon."

- σ_alt: bounded by the prior fix's σ; tightens with each
  frame
- Env: any with stars
- Bris fit: cross-check, smoothing, drift correction.

### B7. Sun/moon shadow direction

A vertical gnomon (stick, pencil) in the frame casts a shadow
whose direction encodes the sun's azimuth, and whose length
encodes the sun's altitude — *if* the gnomon is vertical. The
inverse is also true: if you know the sun's position (from
catalog + approximate time and place), the shadow's length
tells you how far from vertical your gnomon is.

Mainly useful as a calibration cross-check rather than a
primary horizon source. Listed for completeness.

### B8. GNSS doppler-derived vertical

GNSS doppler measurements can in principle separate the
gravity-aligned vs gravity-orthogonal components of velocity,
but the SNR is poor for stationary observers and this requires
multi-frequency receivers most phones don't expose. Dismissed.

### B9. Barometric gradient

The atmospheric pressure gradient is vertical (~12 Pa/m near
sea level). A pressure sensor in the phone can in principle
detect tilt-induced pressure changes when moved through known
height differences. Far too noisy in practice. Dismissed.

### B10. Magnetic dip

The geomagnetic field has a vertical component (dip angle)
that varies smoothly with location. Combined with a model
(IGRF), magnetometer dip readings constrain the vertical to
~1–2°. Worse than the accelerometer, and degraded by every
piece of ferromagnetic material in the environment.
Dismissed except as a sanity check on B1/B2 in unusual
environments.

### B11. Polarisation of skylight

The sky is polarised in patterns referenced to the sun's
position. With a polarising filter and a known sun position,
camera attitude is recoverable to ~1°. Cute, but requires
filter hardware and only works during daylight with clear sky
— a regime where A1 (sea horizon) or A2 (mirror) is already
better.

### B12. Wind on a tuft (telltale)

A piece of yarn in the frame aligns with the apparent wind.
This is not vertical. Dismissed; included only as a reminder
that "things that hang" only give you vertical if gravity
dominates the forces on them, which is why plumb lines work
and telltales don't.

## 4. Family C — no horizon at all

### C1. Zenith camera

Point the camera straight up. The zenith is, by definition,
opposite to gravity. If we know the camera is pointed at the
zenith (mechanically, via levelling feet) and we detect any
star at any position in the frame, the star's offset from the
image centre is its **zenith distance** = 90° − altitude.

- σ_alt: 0.5–2′ (geometric; limited by how level the mount is)
- Env: clear sky overhead; night
- HW: levelling feet + camera + stars
- Cal: trivial — a bubble level on the camera body
- Bris fit: **excellent for the Pi appliance** as a fixed
  installation (e.g. on a tripod or boat-mounted gimbal). The
  whole horizon problem dissolves; sight reduction is just
  catalog lookup. This is essentially the operating mode of
  professional zenith telescopes used for time keeping and
  geodesy.

Worth its own product mode in the Bris CLI / appliance.

### C2. Lunar distances

The angle between the moon and another celestial body is a
function of time only (the moon moves ~30′/hour against the
star background). Measuring that angle gives you GMT, which
combined with a sextant-style altitude and a chronometer-less
DR position, gives a fix. **No horizon required for the lunar
distance itself**; the altitude part still wants one but the
σ flow is different.

Historical method (pre-chronometer); included for completeness
and because Bris could implement it once the catalog has the
moon. Useful for the "what if your phone clock is wrong?" case.

### C3. Stellar occultations / transits

A star disappearing behind the moon's limb, or transiting a
known terrestrial object (lighthouse, tower) at a known
position, gives a position line **without any angle
measurement at all** — pure timing. Niche.

### C4. Sumner / equal-altitude method

Two observations of the same body at different times at the
*same altitude*. Doesn't eliminate the need to know that
altitude, so doesn't actually escape the horizon problem.
Listed to dismiss the common misconception that it does.

### C5. Differential photometry

The differential refraction between two bodies at different
altitudes encodes their altitudes (refraction grows
non-linearly near the horizon). In principle this gives
altitude from photometry alone. Far too noisy on commodity
cameras; dismissed.

## 5. Scoring summary

| Method | σ_alt | Day/Night | Sea/Land/Indoor | HW added | Pi | Phone |
|--------|------:|:---------:|:---------------:|:--------:|:--:|:-----:|
| A1 Sea horizon            | 1–3′    | D    | Sea         | —    | ✓ | ✓ |
| A2 **Floating mirror**    | 0.5–2′  | D+N  | All         | dish | ✓ | ✓ |
| A3 Natural water reflection | 2–10′ | D+N  | Land/Sea    | —    | ✓ | ✓ |
| A4 DEM skyline match      | 1–5′    | D    | Land        | DEM  | ~ | ✓ |
| A5 OSM building skyline   | 2–10′   | D    | Urban       | OSM  | ~ | ✓ |
| A7 ML sky segmentation    | 5–30′   | D    | All         | model | ~ | ✓ |
| A8 ML horizon regression  | 30–120′ | D    | All         | model | ✗ | ✓ |
| B1 Phone IMU              | 6–30′   | D+N  | All         | —    | ✗ | ✓ |
| B2 **External MEMS IMU**  | 5–30′   | D+N  | All         | IMU  | ✓ | ✓ |
| B3 **Plumb line**         | 1–5′    | D+N* | All         | string | ✓ | ✓ |
| B4 Liquid level           | 5–20′   | D+N* | All         | cup  | ✓ | ✓ |
| B5 **Vanishing points**   | 1–10′   | D+N  | Structured  | —    | ✓ | ✓ |
| B6 Star-prior horizon     | depends | N    | All         | —    | ✓ | ✓ |
| C1 **Zenith camera**      | 0.5–2′  | N    | All         | level | ✓ | ✓ |
| C2 Lunar distance         | n/a     | D+N  | All         | —    | ✓ | ✓ |

\* = with illumination on the reference

## 6. What this all suggests for Bris

Not a decision, just a synthesis:

- **Highest-leverage adds**, in rough order:
  1. **A2 floating mirror** — operator's original idea, classical
     accuracy, zero electronics, works everywhere with line of
     sight to sky.
  2. **B5 vanishing points** — pure software, free for users in
     structured environments, complements every other method.
  3. **B2 external MEMS IMU on Pi** — closes the IMU gap
     between phone and appliance; same engine code path as B1.
  4. **B3 plumb line** — cheapest fallback when all else fails;
     ships as a printable accessory.
  5. **C1 zenith camera mode** — distinct product mode for fixed
     installations; potentially the most accurate of all on
     the appliance.

- **Architectural implication**: the engine should accept
  horizon *evidence* from multiple independent channels and
  combine them with proper σ weighting. The `HorizonSource`
  enum sketched in the previous plan (`Optical | Artificial`)
  is too binary; the right abstraction is a
  `HorizonProvider` trait with concrete implementations per
  method, and a fusion layer that respects the
  honest-uncertainty rule (record each channel's σ; the fused
  σ is **smaller** than any input's σ only if the channels are
  independent — which they are not always; mirror+IMU are
  independent, IMU+vanishing-points are independent, mirror+sea
  horizon at sea are **not** independent because both depend
  on the same image).

- **Calibration becomes the cross-cutting engineering problem**:
  intrinsics (already in `bris-calibrate`), IMU↔camera
  extrinsic (per device), plumb-line mount geometry (per
  accessory). One unified calibration workflow that emits a
  per-device JSON consumed by every horizon provider would
  pay back across all the above methods.

- **What to drop or defer**: A6 (cloud), A8 (ML regression as
  primary), B7 (shadow as primary), B8–B12 (noise floor too
  high), C3–C5 (niche or impractical).

## 7. Open questions

- How does Bris want to present "this fix is from a mirror /
  plumb line / IMU / vanishing points" in PBRIS? A new
  diagnostic field (`horizon_provider="mirror"`) is cheap and
  honest; consumers can filter.
- For methods requiring an artifact in the frame (mirror, plumb,
  cup), should the artifact be **detected automatically** or
  **declared by the operator** in a setup step? Detection is
  more user-friendly; declaration is more robust.
- Is there appetite for a Bris-branded **physical accessory
  kit** (folding mirror dish + plumb-line bracket + IMU
  breakout for Pi)? Could be the appliance's killer add-on.
- Per-device extrinsic calibration UX: how does the operator
  know it's good enough? A single "calibrate against the night
  sky" routine that solves intrinsics + extrinsic + IMU bias
  together (using star detections as the absolute truth) would
  be the cleanest answer; expensive to build, but pays back
  across every method in Family B.

## 8. References

- Bowditch, *The American Practical Navigator*, 2017 — ch. 16,
  20–22 (altitude corrections; alternative methods; lunar
  distances).
- Cotter, *A History of Nautical Astronomy*, 1968 — floating
  mirrors and bubble sextants in historical context.
- Lezama et al., "Finding vanishing points via point alignments
  in image primal and dual domains," CVPR 2014.
- Antunes & Barreto, "A global approach for the detection of
  vanishing points and mutually orthogonal vanishing
  directions," CVPR 2013.
- Zhou et al., "NeurVPS: neural vanishing point scanning,"
  NeurIPS 2019.
- Lee et al., "CTRL-C: Camera calibration TRansformer with
  Line-Classification," ICCV 2021.
- Jin et al., "Perspective Fields for Single Image Camera
  Calibration," CVPR 2023.
- Veicht et al., "GeoCalib: Learning Single-image Calibration
  with Geometric Optimization," ECCV 2024.
- Hofmann-Wellenhof & Moritz, *Physical Geodesy*, 2nd ed.,
  2006 — zenith cameras and the geoid.
- Pi-suitable IMU breakouts: BNO055 (Bosch), ICM-20948
  (InvenSense / TDK), BMI270 (Bosch), ADIS16500 (Analog
  Devices, tactical-grade).
- DEM sources: SRTM 1″ (NASA), Copernicus GLO-30 (ESA),
  ASTER GDEM v3.
