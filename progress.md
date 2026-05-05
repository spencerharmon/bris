# Bris progress

Status snapshot. Updated as work proceeds.

For the full design and the per-task roadmap, see `plan.org`.
For the project overview, see `readme.org`.

---

## Current status

**Phase 0 (scaffolding):** complete. Both follow-ups (vendored
leap-second source + full BSC5 catalog import) are now in place.
**Phase 1 (almanac):** **8 of 9 tasks complete.** Only Pi Zero 2W
benchmarking remains.
**Phase 2 (vision):** **6 of 8 tasks complete + 2 partial.** End-to-end
within-FOV altitude measurement and cross-frame panorama stitching
both work on synthetic data. The classic problem case (telephoto +
high altitude, body and horizon in different frames) is handled by
`bris-vision::panorama` via Harris corners + NCC matching + RANSAC
rigid alignment. Calibration *workflow* and the streaming-engine
quality knob remain.

**Phase 4 (sight reduction & fix):** **3 of 4 tasks complete.**
End-to-end synthetic LOP path works: from observed altitude +
assumed position + body's apparent place → line of position →
multi-sight fix with full position covariance + uncertainty ellipse.
Per-sight blunder screening (absolute + leave-one-out outlier)
catches obvious bad sights without rejecting honest uncertainty.
Running fix (DR advance) and per-stage uncertainty propagation
refinements still pending.

**Phase 5 (NMEA output):** **4 of 6 tasks complete.** All standard
sentences (`$GPGLL`, `$GPRMC`, `$GPGGA`, `$GPGST`) and the full
multi-subtype `$PBRIS` payload (VER / TIME / UNC / SIGHT / ERR)
are implemented as pure formatters. Quality field degrades from a
single `QualityThresholds::classify(σ_nm)` call. Every emission
logs at debug level via `tracing` with the sentence bytes and a
small context payload. Transport layer (TCP/UDP/serial) and the
OpenCPN integration test still pending.

**Phase 6 (CLI / embedded Linux):** not started in earnest. The
synthetic `bris demo` subcommand was useful as a stack validator
while building Phases 4-5 but has been removed (the project's real
"demo" should drive the camera, not synthesize inputs). All
subcommands are stubs.

All other phases not started.

**Workspace metrics:** 21 commits, 156 tests passing (29 in
`bris-core`, 53 in `bris-almanac`, 36 in `bris-vision`, 18 in
`bris-nav`, 20 in `bris-nmea`), zero clippy warnings under
`-D warnings`, zero `cargo fmt` diffs. The almanac now ships with
the full 9,096-star Yale BSC catalog.

**Last commit:** `3458fa9` — Phase 0 vendoring complete.

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
- ✅ **Vendored leap-second source + build-time regenerator.**
  `crates/bris-core/data/leap-seconds.list` from IANA; `build.rs`
  parses + validates it and emits the table consumed by
  `bris-core::time`. Refreshing the table is a single-file `cp`.
- ✅ **Vendored Yale BSC5 catalog import.** `scripts/import_bsc.py`
  consumes the BSC5 fixed-width source from VizieR and emits
  `crates/bris-almanac/data/stars.tsv`. Catalog is now 9,096 stars
  (was 11 vetted starter stars). Documented and worked around an
  off-by-one in the BSC ReadMe's J2000 declination column layout.

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
- ⚠️ **Multi-frame stitching** (`bris-vision::fusion`,
  `bris-vision::track`, `bris-vision::panorama`). Two paths:
  - Within-FOV: `fuse_altitudes` inverse-variance combines per-frame
    altitude measurements within a bounded time window.
  - Cross-frame: Harris corner detection (`detect_corners`) with
    3×3 box-windowed structure tensor, NCC patch matching with
    sub-pixel best candidate, RANSAC rigid-transform fitting +
    closed-form Procrustes refit on inliers (`track`). The
    `panorama_altitude` function composes pairwise transforms into
    a chain that bridges the body frame to the horizon frame and
    runs the standard angle-measurement pipeline at the end.
  Documented limitations: assumes frame-to-frame overlap (no IMU
  prior yet), no sidereal motion correction within a sweep, pose
  chain in pixel coordinates is approximate for fisheye lenses.
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

### Phase 5: NMEA output

- ✅ **NMEA 0183 sentence builders** (`bris-nmea::checksum`,
  `bris-nmea::standard`). Pure formatters returning `String` for
  `$GPGLL`, `$GPRMC`, `$GPGGA`, `$GPGST`. Cross-checked against
  the classic GPGGA documentation example (checksum 0x47).
- ✅ **Uncertainty in NMEA output** (`bris-nmea::standard`). The
  `FixQuality` enum + `QualityThresholds::classify(σ_nm)` produce
  the GGA quality digit (1/6/0) and RMC status (A/V) from the
  fix's overall σ. `$GPGST` emits per-axis position σ in metres
  for OpenCPN. Same fix produces consistent classification across
  all sentence types.
- ✅ **Structured `$PBRIS` payload (multi-sentence)**
  (`bris-nmea::pbris`). Full subtype set: VER, TIME, UNC (with
  dominant-source attribution), SIGHT × N, ERR. All subtypes verified
  to fit under the NMEA 82-char limit even with maximum-magnitude
  inputs. `pbris_full()` emits the canonical set in canonical order.
- ✅ **Documented `$PBRIS` schema** (`docs/protocol/pbris.md`). The
  contract that downstream metrics-converter tools build against,
  with per-field tables and emission ordering. Schema versioned
  via `$PBRIS,VER`.
- ✅ **Debug logging of every emitted sentence.** Every formatter
  calls `tracing::debug!` with the sentence type, the sentence
  bytes, and a small context payload (σ, dominant source, etc.).
  A test installs a temporary `tracing-subscriber` and asserts the
  log fired with the expected sentence type — proves the logging
  works, doesn't just trust the call site.

---

## Not yet done

### Phase 1 remainder

- ⏳ **Benchmark on Pi Zero 2W.**

### Phase 1 follow-ups (acknowledged stubs in apparent-place)

- ⏳ **Annual aberration** (~20″ placeholder σ in budget).
- ⏳ **Light-time iteration for planets** (~1-2″).
- ⏳ **Lunar topocentric parallax** (~1°! — required before Moon
  sights are accurate).

### Phase 6: CLI / embedded Linux (essentially not started)

The synthetic `bris demo` subcommand was removed in commit `3458fa9`
because it didn't match the user's intent (the real demo should
drive a webcam). The webcam-fed `bris fix` is the obvious next
piece: capture a frame, run the vision pipeline, reduce the sight
against an `--assumed-position`, and emit the fix and NMEA stream.
Capture wiring needs `v4l` (Linux) and platform-native equivalents
on macOS/Windows; for the Pi Zero appliance target, `v4l` is the
right answer.

All CLI subcommands (capture, calibrate, fix, serve, replay, log,
update) are still stubs.

### Phase 2 remainder

- ⏳ **Lens calibration workflow** (checkerboard capture, corner
  detection, parameter solve, persistence). Math is in place; needs
  the CLI hook.
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
  diagnostic. *Note:* the `$PBRIS,UNC` formatter already emits a
  dominant-source field; the pipeline that *populates* the
  `UncertaintyBudget` from real per-stage σ values is still the
  open work.

### Phase 5 remainder

- ⏳ **Transport layer** (TCP server, UDP broadcast, serial via
  `serialport` crate). Configurable via small TOML/JSON config.
- ⏳ **OpenCPN integration test.** Scripted: spin up Bris emitting
  a canned fix, connect OpenCPN in a container/VM, assert vessel
  icon appears at expected position and `$GPGST` uncertainty is
  rendered.

### All later phases (1.5, 3, 3.5, 7, 8, 9)

Not started. See `plan.org` for the full task list. Phase 6 is
partially started (just the `bris demo` subcommand for end-to-end
testing); rest of Phase 6 (capture, serve, replay, etc.) and the
related Phase 5 transport layer remain.

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

**`bris fix` subcommand backed by webcam capture** is the primary
next step. The user explicitly asked for "point my webcam at the
sky and have bris tell me my lat/long." Pieces needed:

1. **Camera capture (V4L2 on Linux)** via the `v4l` crate: open
   device, configure format, capture one frame, decode (MJPEG or
   YUYV depending on the device), convert luminance → `bris_vision::Frame`.
2. **Wire the vision → almanac → nav → NMEA pipeline** in a
   `bris fix` subcommand, parameterized by:
   - `--device /dev/video0`
   - `--assumed-position LAT,LON` (required; one LOP doesn't
     constrain a fix without plate solving — be explicit about this)
   - `--eye-height-m N` (default 2.0)
   - `--body sun|moon|...` (manual selection until plate solving lands)
3. **Honest output**: print the fix with σ, and the NMEA stream at
   debug level (matching what the synthetic demo did, but with real
   data).

Limitations to call out in the subcommand's help text:
- Single-LOP fix needs `--assumed-position` and produces a fix
  *along* the LOP, not a true 2D fix. For a true fix you need
  multiple sights of different bodies (which the streaming engine
  in Phase 3.5 will accumulate automatically).
- Manual body selection is a stopgap until plate solving (Phase 3)
  lands.
- Lunar parallax is not yet applied (Phase 1 follow-up); Moon
  sights will be off by up to ~1°.

After `bris fix` lands, the next milestones are:

- **Phase 3: plate solving** so night-sky pointing produces a fix
  without `--body` selection.
- **Phase 5 transport layer** so the NMEA stream goes out a real
  TCP/UDP/serial port.
- **Phase 1.5 time integrity** as a cleanup pass.

---

## Open questions

None blocking the next step.
