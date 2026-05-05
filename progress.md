# Bris progress

Status snapshot. Updated as work proceeds.

For the full design and the per-task roadmap, see `plan.org`.
For the project overview, see `readme.org`.

---

## Current status

**Phase 0 (scaffolding):** complete except two follow-ups.
**Phase 1 (almanac):** **8 of 9 tasks complete.** Only Pi Zero 2W
benchmarking remains.
**Phase 2 (vision):** **5 of 8 tasks complete + 2 partial + 1 not
started.** Within-FOV altitude measurement works end-to-end.
Calibration *workflow* and the streaming-engine quality knob are
pending; full panorama stitching is required for the 0.5 nm target
and not yet implemented.
**Phase 4 (sight reduction & fix):** **3 of 4 tasks complete.**
End-to-end synthetic LOP path works: from observed altitude +
assumed position + body's apparent place → line of position →
multi-sight fix with full position covariance + uncertainty ellipse.
Per-sight blunder screening (absolute + leave-one-out outlier)
catches obvious bad sights without rejecting honest uncertainty.
Running fix (DR advance) and per-stage uncertainty propagation
refinements still pending.

All other phases not started.

**Workspace metrics:** 16 commits, 128 tests passing (29 in
`bris-core`, 53 in `bris-almanac`, 28 in `bris-vision`, 18 in
`bris-nav`), zero clippy warnings under `-D warnings`, zero
`cargo fmt` diffs.

**Last commit:** `7fb0bb5` — per-sight blunder screening.

---

## Done

### Phase 0: project scaffolding

- ✅ Cargo workspace, 7 member crates (`bris-core`, `bris-almanac`,
  `bris-vision`, `bris-platesolve`, `bris-nav`, `bris-nmea`, `bris-cli`).
  `bris-ffi` deferred until the Rust core is stable.
- ✅ CI matrix (fmt, clippy, test on Linux + macOS, cross-build for
  `aarch64-unknown-linux-gnu`, `cargo-deny`).
- ✅ GPL-3.0 license, `cargo-deny` policy, `rustfmt`/`clippy`/
  `proptest` wired up workspace-wide.
- ✅ Documentation skeletons: `docs/design/`, `docs/operator/`,
  `docs/protocol/pbris.md`.

### Phase 1: almanac

- ✅ **Coordinate types** (`bris-core::angle`, `bris-core::uncertainty`).
  `Angle`, `Latitude`, `Longitude`, `Sigma`, `Uncertain<T>`.
- ✅ **Time scales** (`bris-core::time`). UTC ↔ TAI ↔ TT ↔ UT1
  with embedded leap-second table through 2017-01-01 (TAI−UTC = 37 s).
- ✅ **Precession / nutation / mean obliquity** (`bris-almanac::frame`).
  IAU 2006 + IAU 2000B. SOFA cross-validated.
- ✅ **Solar System ephemeris** (`bris-almanac::ephemeris`). VSOP87D
  via the `vsop87` crate. Cross-checked against Astronomical Almanac.
- ✅ **Lunar ephemeris** (`bris-almanac::lunar`). Hand-implemented
  Meeus Ch. 47. Cross-checked against Meeus Example 47.a.
- ✅ **Star catalog** (`bris-almanac::catalog`). Build-time TSV codegen
  with sanity checks. Currently 11 vetted stars; full BSC import is a
  Phase 0 follow-up.
- ✅ **Refraction model** (`bris-almanac::refraction`). Bennett 1982
  with altitude-dependent uncertainty.
- ✅ **Almanac API surface** (`bris-almanac::observer`,
  `bris-almanac::coord`, `bris-almanac::apparent`). Full apparent-place
  pipeline. `body_apparent_place()` and `star_apparent_place()` return
  topocentric horizontal direction with attached altitude σ.

### Phase 2: vision

- ✅ **Image abstraction** (`bris-vision::frame`). `Frame` struct,
  row-major u16 grayscale, capture metadata, intrinsics. Construction
  enforces dimension invariants.
- ⚠️ **Lens model + calibration** (`bris-vision::lens`). Math complete
  (pinhole + Brown-Conrady, iterative undistortion, ray-direction).
  Calibration *workflow* (checkerboard corner detection, parameter
  solve, persistence) still pending — needs the CLI hook.
- ✅ **Horizon detection** (`bris-vision::horizon`). Classical
  pipeline: downsample → vertical gradient → per-column peak →
  RANSAC line fit → least-squares refit. Returns slope, intercept,
  inlier count, residual RMS, altitude σ. Custom RANSAC with
  data-seeded xorshift PRNG so results are deterministic and
  reproducible. Tests verify both flat and tilted (3° camera roll)
  synthetic horizons.
- ✅ **Sun/Moon centroiding** (`bris-vision::centroid`). Two-pass
  connected-components with union-find, picks the largest bright
  component, intensity-weighted sub-pixel centroid. Position σ
  combines 1/√N statistical with a 0.5 px bias floor.
- ✅ **Angle measurement pipeline** (`bris-vision::measure`). The
  bridge from pixel space to sky space: given a horizon line and a
  body centroid, compute apparent altitude via cross-product
  geometry on lens-undistorted ray directions. End-to-end test:
  body 200 px above horizon at fy=1000 → 11.31° altitude (matches
  atan(200/1000)).
- ⚠️ **Multi-frame stitching** (`bris-vision::fusion`). *Only the
  within-FOV angle-fusion path is implemented* (inverse-variance
  weighted mean of per-frame altitude measurements within a bounded
  time window). The full ORB-feature panorama stitching path is
  *not yet implemented* and is genuinely required for high-altitude
  or telephoto sights — with a telephoto lens, body and horizon
  cannot fit in a single frame above the lens's vertical FOV.
  Wide-angle-only operation caps accuracy at ~5 nm. The 0.5 nm design
  target is unreachable without panorama stitching.
- ⏳ **Stitching/accuracy tradeoff knob.** A streaming-engine setting,
  not a vision-module change. Pending the streaming engine in Phase
  3.5.
- ✅ **Eye-height handling** (already done in Phase 1's `Observer`).

### Phase 4: sight reduction & fix

- ✅ **Single sight LOP** (`bris-nav::sight`). `line_of_position()`
  takes the assumed position, observed and computed altitudes (each
  with σ), and azimuth, returns a `LineOfPosition` with intercept
  in nm and combined intercept σ. Standard Marc Saint-Hilaire
  intercept method.
- ✅ **Multi-sight fix with full covariance** (`bris-nav::fix`).
  `multi_sight_fix(&[LineOfPosition])` solves the weighted normal
  equations for the (north, east) displacement, returns a `Fix`
  with new lat/lon, full 2×2 covariance in nm², and decomposed
  σ_major/σ_minor/orientation for chartplotter ellipse rendering.
  End-to-end test: two synthetic LOPs → recovered (1 N, 2 E) within
  0.01 nm. Singular geometry rejected explicitly.
- ✅ **Sanity checks** (`bris-nav::screen`). Two screens: absolute
  intercept (any |intercept| > 60 nm rejected) and leave-one-out
  outlier (any sight > 5σ from MAD-based consensus, when ≥ 3
  sights). Honest-σ invariant preserved: a high-σ sight with a
  consistent value is kept and down-weighted, never rejected for
  being uncertain.

---

## Not yet done

### Phase 0 follow-ups

- ⏳ **Vendored leap-second source + build-time regenerator.**
- ⏳ **Vendored Yale BSC import + cross-check script.** Hard
  prerequisite for Phase 3 (plate solving).

### Phase 1 remainder

- ⏳ **Benchmark on Pi Zero 2W.**

### Phase 1 follow-ups (acknowledged stubs in apparent-place)

- ⏳ **Annual aberration** (~20″ placeholder σ in budget).
- ⏳ **Light-time iteration for planets** (~1-2″).
- ⏳ **Lunar topocentric parallax** (~1°! — required before Moon
  sights are accurate).

### Phase 2 remainder

- ⏳ **Lens calibration workflow** (checkerboard capture, corner
  detection, parameter solve, persistence). Math is in place; needs
  the CLI hook.
- ⏳ **Multi-frame panorama stitching** (ORB feature alignment +
  pose chain across frames + IMU/sidereal soft constraints +
  projection into a common surface). Required for telephoto / high-
  altitude sights and therefore for the 0.5 nm accuracy target.
  Within-FOV angle fusion is in place but is not a substitute.
- ⏳ **Stitching/accuracy tradeoff knob** (streaming-engine setting).

### Phase 4 remainder

- ⏳ **Running fix.** Advance earlier LOPs by DR (course/speed)
  before intersection. Course/speed input is optional; if absent,
  only simultaneous fixes are produced. Inflate covariance by DR
  uncertainty when used.
- ⏳ **Per-stage uncertainty propagation refinements.** The current
  pipeline collects per-source σ contributions and quadrature-
  combines them; the dominant-source attribution task (per
  plan.org) needs explicit identification of which source
  contributes most to each fix, surfaced in the `$PBRIS,UNC`
  diagnostic.

### All later phases (1.5, 3, 3.5, 5, 5.5, 6, 7, 8, 9)

Not started. See `plan.org` for the full task list.

---

## Crucial design notes (concise)

1. **Continuous-operation model** with session-based mobile UX
   (red/yellow/green by uncertainty, never auto-accept red, sustained-
   green auto-accept, timeout picks best yellow-or-green). Embedded
   device runs continuous output to chartplotter.

2. **Honest uncertainty as the central invariant.** Every measurement
   carries 1σ. Every fix carries a full 2×2 position covariance
   built from per-source contributions. **This invariant is now
   load-bearing across both almanac and vision**: every `Centroid`,
   `HorizonLine`, fused altitude, and `ApparentPlace` carries an
   attached σ that combines through quadrature into the final fix
   uncertainty.

3. **Optical fixes are always published.** Nothing silently corrects
   them. Nothing rejects them on prior-position grounds.

4. **Mean equinox per body, apparent-place pipeline does the rest.**
   Each ephemeris/lunar/catalog function returns a true position;
   the pipeline applies precession, nutation, light-time, aberration,
   and refraction uniformly.

5. **Build-time data integrity.** Leap-second table, star catalog,
   and (eventually) almanac coefficient generators all sanity-check
   inputs at build time.

6. **No ML, no telemetry, no internet at runtime.** Classical CV
   throughout. Observability rides on the `$PBRIS` proprietary NMEA
   sentence.

7. **NMEA 0183 first; NMEA 2000 deferred.** OpenCPN is the
   recommended chartplotter.

8. **57 navigational stars + automatic body selection.** The user
   never picks a body.

9. **Pi Zero 2W is the embedded floor.** Embedded-Linux appliance
   model.

10. **One Rust core, native mobile shells.** Kotlin/Swift via UniFFI
    (deferred until the core API is stable).

11. **OTA updates are bundled** (`bris-data`: leap-second table +
    almanac coefficients + star catalog).

12. **Custom RANSAC + xorshift PRNG seeded from input data.**
    Deterministic, reproducible, and supports diff-able replays from
    saved frames. Same principle will apply to plate-solving.

13. **Vision modules are imageproc-free.** Each algorithm is simple
    enough to implement directly; avoiding the dep keeps the binary
    lean and the algorithms fully visible. Will reconsider if we ever
    need richer primitives.

---

## Crucial implementation notes (caveats and invariants)

1. **`f64` Julian Date precision is ~100 µs after arithmetic.** Fine
   for celestial nav. If sub-microsecond ever needed: split
   `(jd_int, jd_frac)` representation.

2. **Catalog is currently 11 stars.** Hard prerequisite for Phase 3
   (plate solving) is the full BSC import.

3. **Proper-motion convention is dα/dt × cos(δ).** Standard tangent-
   rate. Getting it wrong biases fixes near the celestial poles.

4. **The apparent-place pipeline carries ~20″ σ from the aberration
   placeholder.** Will drop to ~0.1″ once aberration is implemented.

5. **Lunar topocentric parallax is not yet applied.** Up to ~1°
   error for Moon sights. Don't use for Moon sight reduction.

6. **Light-time iteration for planets is not yet applied.** ~1-2″
   error. Acceptable for MVP.

7. **The vision fusion window is bounded to 5 seconds.** Frames
   spanning longer must be split into multiple fused groups. The
   alternative — advancing each frame to a common reference time
   via the apparent-place pipeline — is the upgrade once the
   streaming engine is in place. Note that `fuse_altitudes` only
   handles the case where each frame *individually* contains both
   the body and the horizon; the cross-frame case requires panorama
   stitching (see Phase 2 remainder).

8. **Centroiding picks the *largest* bright component**, not the
   *brightest single pixel*. This rejects hot pixels and small lens
   flares but means a bright cloud larger than the Sun could
   theoretically win. Mitigation: the streaming engine should know
   when it's in day mode and threshold accordingly; the centroiding
   module is correct in its own right.

9. **No FFI surface yet.** `bris-ffi` is intentionally absent.

10. **`cargo-deny` license allowlist.** New dependencies whose
    license isn't on the list will fail CI.

11. **Independence assumption in uncertainty quadrature is not
    strictly true.** Documented limitation; inflation factor planned
    for Phase 8.

12. **Lens calibration math is implemented; the workflow isn't.**
    Until the workflow lands, frames must ship with intrinsics from
    elsewhere (e.g. factory defaults baked into a hardware image).
    The `Intrinsics::placeholder()` constructor exists for tests
    only.

---

## Next concrete step

Three reasonable paths, with explicit tradeoffs:

**A. Multi-frame panorama stitching in `bris-vision`.** Build the
ORB-feature alignment + pose chain + projection pipeline. This
unblocks the 0.5 nm accuracy target and is the largest single piece
of remaining vision work. Substantial commit (probably 2-3 work
sessions; either implement ORB ourselves or pick a permissive
pure-Rust crate). **This is the single most important piece for
real-world capability.**

**B. NMEA 0183 sentence emission in `bris-nmea`** (Phase 5). The
fix path now produces a typed `Fix` with full position covariance;
emitting `$GPGLL`/`$GPRMC`/`$GPGGA`/`$GPGST` and the multi-subtype
`$PBRIS` from a `Fix` would close the loop end-to-end at the API
level, even before panorama stitching makes the fix accurate enough
for real use. This is a self-contained smaller commit.

**C. Phase 1.5 (time integrity).** Self-contained, unblocks
nothing else, pays off for offshore use. Same priority as before.

I'd lean **B → A → C**: get the NMEA-emission loop closed first
(it's small and gives us an end-to-end testable surface from frame
to NMEA stream), then tackle panorama stitching to make the fix
real, then time integrity as a cleanup pass.

---

## Open questions

None blocking the next step.
