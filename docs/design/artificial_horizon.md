# Artificial Horizon — Science and Design

Status: design draft (Phase 7 on-ramp; pairs with the IMU-prior
TODOs in `plan.org` lines 1062, 1278, and 560-580).

This document covers the **IMU-based** artificial horizon: how
phone (or external MEMS) inertial sensors give a local-vertical
reference and how that becomes a horizon line. For the
broader catalog of horizon-finding methods see
`horizon_brainstorm.md`; for auto-detection of optical horizon
aids (reflection pairs, plumb lines, vanishing points) and the
cross-frame registration model that ties all providers
together see `horizon_autodetect.md`. The Phase 1 work
currently on the implementation branch is the reflection-pair
provider from that doc, **not** the IMU path described here;
the IMU path remains a planned future provider sharing the
same `HorizonProvider` trait introduced in Phase 1.

This document explains *why* an artificial horizon is meaningful
for Bris, *what physical quantity* it actually measures, *how
accurate* it can be on commodity phone hardware, and *how* it
slots into the existing sight-reduction pipeline without
violating the honest-uncertainty invariant.

The implementation plan that consumes this document lives
alongside it (see the 10-point plan in the PR description /
`progress.md`); this file is the science reference the
implementation cites.

## 1. What a horizon is, for celestial navigation

A celestial sight reduces to one number per body: the **observed
altitude** `Ho` — the angle between the line of sight to the
body and the local horizontal plane at the observer. "Local
horizontal" means perpendicular to the local gravity vector
(the geoid normal, ignoring deflection of the vertical, which
is sub-arcsecond at sea).

Every sextant — wet or dry — is just a way of measuring that
angle. The instrument has two jobs:

1. Establish a reference direction that is (or maps onto) the
   local horizontal.
2. Measure the angle between that reference and the body.

A **natural-horizon sextant** uses the visible sea horizon as
the reference. This is cheap and astonishingly accurate (the
sea horizon is the tangent to the geoid at eye height, modulo
refraction), but it requires (a) a clear sea horizon and (b) a
**dip correction** — the geometric drop of the visible horizon
below true horizontal because the observer's eye is above the
sea surface:

```
dip ≈ 1.753' · √(h_eye_m)        (standard refraction)
```

A **bubble sextant** (used in aviation since the 1930s) replaces
the sea horizon with a fluid bubble or pendulum that finds local
vertical mechanically. No sea horizon required, no dip
correction, but the bubble is sensitive to vehicle acceleration
— any non-gravitational acceleration tilts the bubble in
exactly the same way as a tilt of the airframe, and the bubble
cannot tell them apart.

An **artificial horizon** in Bris is the digital descendant of
the bubble sextant: it uses the phone's inertial sensors to
estimate the local gravity vector, and from that derives a
synthetic horizon line in the camera image. Bris already has
the rest of the pipe (star detection, plate solve, sight
reduction); the artificial horizon just substitutes for the
optical horizon detector at Stage C.

## 2. The physical quantity actually measured

The phone's accelerometer measures **specific force** `f` in the
sensor frame:

```
f = a_inertial − g
```

where `a_inertial` is the sensor's proper acceleration in an
inertial frame and `g` is the local gravity vector. When the
phone is stationary (`a_inertial = 0`), the accelerometer
reads `−g`, i.e. it points "up" along the local vertical with
magnitude `|g| ≈ 9.81 m/s²`.

This is the only thing that ties an artificial horizon to the
geoid. Everything else — rotation vector, gyro, magnetometer —
is for *holding* the gravity estimate steady while the phone
moves; none of them can find vertical on their own.

The Android `TYPE_ROTATION_VECTOR` sensor fuses accelerometer,
gyroscope, and (optionally) magnetometer into a quaternion
representing device orientation relative to a world frame whose
Z-axis is local vertical and whose X-axis points roughly east
(when magnetometer is included) or is gyro-drift-free relative
to the initial pose (when it isn't, `TYPE_GAME_ROTATION_VECTOR`).
For our purposes only the Z-axis (vertical) matters; we ignore
heading. This means:

- **`TYPE_ROTATION_VECTOR`** — preferred; gyro-stabilized
  vertical, accelerometer-disciplined, magnetometer-aided.
- **`TYPE_GAME_ROTATION_VECTOR`** — fine; no magnetometer
  dependency, vertical is still accelerometer-disciplined.
- **`TYPE_GRAVITY`** — fallback; pre-fused gravity vector,
  but on some devices it is just a low-pass-filtered
  accelerometer (poor under motion).
- **`TYPE_ACCELEROMETER`** raw — last resort; we'd need to
  implement our own low-pass / complementary filter.

We extract the gravity direction (unit vector) from whichever
source is available, then express it in the **camera frame**
using the device-to-camera extrinsic (display rotation + lens
facing + sensor orientation, from the CameraX
characteristics). That camera-frame gravity vector is the
single input the engine needs.

## 3. From gravity to a horizon line in the image

Given:

- `g_cam` — unit gravity vector in camera frame (points "down"
  in the world).
- `K` — 3×3 camera intrinsics matrix (focal length, principal
  point; already present in `FfiFrame` / `bris-core`).

The **true horizontal plane** at the observer is the plane
through the camera optical centre with normal `g_cam`. Its
image under the pinhole projection is a line — the horizon
line — given by the dual:

```
ℓ = K⁻ᵀ · g_cam         (homogeneous line in pixel coords)
```

This is standard projective geometry: a plane normal `n` in
camera coordinates projects to the image line `K⁻ᵀ n`. The
line equation `ℓ · [u, v, 1]ᵀ = 0` is then converted to the
slope-intercept form Bris's `HorizonLine` already uses
(`bris-vision/src/horizon.rs:49`).

Two edge cases:

- **Camera pointed near the zenith / nadir.** `g_cam` is nearly
  parallel to the optical axis; the horizon line goes to
  infinity (the horizon is outside the image). We detect this
  (`|g_cam.z| > cos(θ_min)`) and return `HorizonStageOutcome::None`
  with reason `"artificial_horizon_outside_frame"`. This is the
  same failure mode as an optical detector seeing only sky.
- **Camera rolled.** Roll just rotates `ℓ` in the image; no
  special handling needed — the projection formula already
  accounts for it.

No dip correction is applied. The artificial horizon *is* the
true horizontal; it does not look at sea level. Eye-height has
no effect. This is one of the two operational advantages over
a natural horizon (the other being "works at night, in fog, on
land").

## 4. Uncertainty budget — σ_alt from σ_gravity

The point of this section is to write down, honestly, why
artificial-horizon sights will generally be **worse** than
natural-horizon sights at sea, and *how much* worse, so that
fix covariances reflect reality.

### 4.1 Static tilt error

When the phone is **stationary**, the dominant error is
accelerometer bias + noise. Typical MEMS accelerometers in
modern phones have:

- bias stability: ~5–20 mg (1 mg ≈ 0.057°)
- noise density: ~150 µg/√Hz

After ~1 s of averaging this gives a 1σ tilt error in the
range **0.1°–0.5°** (~6–30 arcminutes). Specific devices vary
by an order of magnitude; the implementation must read the
actual sensor's `resolution` and characterise it per device.

For comparison, a careful natural-horizon sextant sight from a
small boat is 1–3 arcminutes (1σ). The artificial horizon at
its **best** is ~5× worse than a sextant; at its **typical**
it is ~10–20× worse.

This is not a defect to be hidden. It is the σ contribution
that goes into `observed_altitude.sigma` and propagates into
the fix covariance. A two-body fix with σ_alt = 0.3° gives a
1σ position ellipse on the order of 15–25 nm — usable for
landfall-quality navigation, useless for entering a harbour.
The user sees this directly because the fix covariance is
honest.

### 4.2 Dynamic tilt error — the acceleration ambiguity

When the phone is **accelerating** (the operator walks, the
boat pitches, the vehicle turns), the accelerometer reads
`f = a_inertial − g`. The fusion filter
(`TYPE_ROTATION_VECTOR`) uses the gyro to predict
orientation and the accelerometer only to *correct* it on the
assumption that, averaged over seconds, `a_inertial ≈ 0`. On a
rolling boat this assumption is violated continuously: the
horizontal acceleration component biases the estimated
vertical toward the apparent gravity (gravity + centripetal +
lever-arm).

The classical bubble-sextant remedy is to take many sights and
average; the error is zero-mean *if* the motion is. Bris can
do better:

- **Reject** frames whose accelerometer magnitude deviates from
  `|g|` by more than a threshold (e.g. 50 mg). Easy gate;
  catches obvious motion.
- **Weight** the σ contribution by recent specific-force
  variance (high variance ⇒ inflate σ).
- **Window-average** the gravity estimate over the integration
  window already used for stacking frames (Phase 3.5
  streaming engine), so a single bad frame doesn't dominate.

These mitigations live in step 3 of the implementation plan
(`synth_horizon_from_gravity`) and step 5 (diagnostics expose
the rejection counters). The σ floor in `EngineConfig`
(`HorizonSource::Artificial { sigma_floor_rad }`) prevents the
filter from ever reporting an over-optimistic vertical.

### 4.3 Magnetic / heading error

Irrelevant. The horizon line depends only on the vertical
direction. Magnetic disturbance affects heading, not pitch/roll.
If we ever derive *azimuth* from the IMU (we don't currently —
plate solving gives us absolute attitude including heading),
that would need its own σ analysis.

### 4.4 Camera-to-IMU extrinsic error

The IMU and the camera are not coincident. Misalignment between
the IMU sensor axes and the camera optical axes contributes a
**constant tilt bias**. On phones the factory calibration of
this extrinsic is usually within ~1°; for an artificial-horizon
σ already in the 0.1°–0.5° range, an uncalibrated extrinsic is
the *dominant* error and the per-device calibration step
(Phase 5 calibration workflow in `bris-calibrate`) must learn it.

For the spike: ship with identity extrinsic (assume IMU axes ==
camera axes after the documented display-rotation transform),
log the residual bias from any successful star-based fixes,
and offer a "calibrate artificial horizon" action that solves
for the constant offset by comparing artificial-horizon
predicted altitudes against plate-solved actual altitudes over
N sights. This is exactly the dual of how a bubble sextant is
"swung" against known stars at a known position.

## 5. Comparison summary

| Aspect                       | Natural horizon          | Artificial horizon (phone IMU) |
|------------------------------|--------------------------|--------------------------------|
| Reference                    | Sea horizon (geoid tangent) | Local gravity (geoid normal) |
| Requires visible sea horizon | Yes                      | No                              |
| Requires daylight            | Effectively yes          | No                              |
| Dip correction               | Yes (~√h_eye)            | No                              |
| Refraction correction        | Yes (terrestrial + astro)| Astronomical only               |
| Typical 1σ at sights         | 1–3′                     | 6–30′ (device-dependent)        |
| Sensitive to observer motion | Mildly (eye height)      | **Strongly** (acceleration ambiguity) |
| Sensitive to magnetic field  | No                       | No (for vertical)               |
| Per-device calibration       | None                     | **Required** (IMU→camera extrinsic) |
| Works on land                | No                       | Yes                             |

The artificial horizon is **not a replacement** for the natural
horizon; it is a complement. The two principal use cases are:

1. **Land-based and night-time sights** where no sea horizon is
   available at all. An honest 20-arcminute σ beats no fix.
2. **Cross-check** of the optical detector at sea. Disagreement
   between IMU-derived and image-detected horizons is a strong
   signal that one of them is wrong (haze, false horizon from
   distant cloud bank, IMU under motion); both are then
   suspect and the σ should inflate accordingly.

Use case 1 is what the live-view toggle exposes. Use case 2 is
future work and intentionally outside the spike.

## 6. Failure modes the implementation must handle

Each of these maps to an `EngineDiagnostics` counter
(implementation plan step 5):

- **`artificial_horizon_no_gravity`** — frame arrived without a
  gravity vector (sensor unavailable, listener not yet warm,
  Android delivered camera frame before first sensor sample).
  Outcome: `HorizonStageOutcome::None`, sight reduction
  proceeds without a horizon for this frame.
- **`artificial_horizon_outside_frame`** — camera pointed too
  near zenith/nadir; horizon line not in image. Same outcome
  as above. Not an error; the engine reports it for the HUD.
- **`artificial_horizon_high_motion`** — recent specific-force
  variance exceeds threshold. σ inflated; frame still used
  unless inflation pushes σ over `horizon_early_termination_sigma_rad`.
- **`artificial_horizon_stale_gravity`** — newest gravity sample
  is older than `max_gravity_age_ms` (e.g. 100 ms). Treated as
  `no_gravity`.

None of these cause silent fallback to the optical detector. If
the operator chose `HorizonSource::Artificial`, the engine
respects that choice and reports honestly when it cannot
deliver. Falling back silently would violate the
honest-uncertainty rule and would also hide IMU misconfiguration
from the operator.

## 7. References

- *The American Practical Navigator* (Bowditch), 2017, vol. 1,
  ch. 16 — sextant altitude corrections; ch. 22 — dip.
- *Air Navigation* (AFM 51-40), 1951 — bubble-sextant theory
  and the acceleration-ambiguity problem.
- Titterton & Weston, *Strapdown Inertial Navigation
  Technology*, 2nd ed., 2004 — specific-force model and
  attitude-from-accelerometer derivation.
- Hartley & Zisserman, *Multiple View Geometry in Computer
  Vision*, 2nd ed., 2003 — §8.1, projection of a 3-space plane
  to an image line (`ℓ = K⁻ᵀ n`).
- Android `Sensor` developer docs — fused sensor types and
  per-device characteristics:
  https://developer.android.com/reference/android/hardware/Sensor
- Bris design context: `docs/design/pipeline.md` (Stage C),
  `crates/bris-vision/src/horizon.rs` (`HorizonLine` type),
  `plan.org` lines 1062, 1278, 560-580 (IMU-prior TODOs).
