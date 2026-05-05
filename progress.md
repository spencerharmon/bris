# Bris progress

Status snapshot. Updated as work proceeds.

For the full design and the per-task roadmap, see `plan.org`.
For the project overview, see `readme.org`.

---

## Current status

**Phase 0 (scaffolding):** complete except two follow-ups.
**Phase 1 (almanac):** ~6 of 9 tasks complete.
All other phases not started.

**Workspace metrics:** 7 commits, 57 tests passing (29 in `bris-core`, 28
in `bris-almanac`), zero clippy warnings under `-D warnings`, zero `cargo
fmt` diffs.

**Last commit:** `1195f3c` — star catalog with build-time TSV codegen.

---

## Done

### Phase 0: project scaffolding

- ✅ Cargo workspace, 7 member crates (`bris-core`, `bris-almanac`,
  `bris-vision`, `bris-platesolve`, `bris-nav`, `bris-nmea`, `bris-cli`).
  `bris-ffi` deferred until the Rust core is stable.
- ✅ CI matrix (fmt, clippy, test on Linux + macOS, cross-build for
  `aarch64-unknown-linux-gnu`, `cargo-deny`).
- ✅ GPL-3.0 license in place. `cargo-deny` policy enforces compatible
  outbound licenses.
- ✅ `rustfmt`, `clippy` (deny warnings), `cargo-deny`, `proptest`
  wired up across the workspace.
- ✅ Documentation skeletons: `docs/design/`, `docs/operator/`,
  `docs/protocol/pbris.md`.

### Phase 1: almanac (in progress)

- ✅ **Coordinate types** (`bris-core::angle`, `bris-core::uncertainty`).
  Hand-rolled `Angle`, `Latitude` (range-checked ±π/2), `Longitude`
  (normalized to (-π, π]), plus `Sigma` and `Uncertain<T>` for
  first-class 1σ propagation. Quadrature combination via `Sigma::combine`.
- ✅ **Time scales** (`bris-core::time`). UTC ↔ TAI ↔ TT ↔ UT1
  conversions with embedded leap-second table (1972 through 2017-01-01,
  TAI−UTC = 37 s). `LEAP_TABLE_EXPIRES` constant lets the runtime
  detect a stale table and inflate time uncertainty rather than refuse
  to compute fixes.
- ✅ **Precession / nutation / mean obliquity** (`bris-almanac::frame`).
  IAU 2006 precession, IAU 2000B nutation (77-term truncated series),
  IAU 2006 obliquity polynomial. Cross-validated against an IAU SOFA
  `iauNut00b` reference test vector to 1e-7 rad (~20 µas), well below
  the per-sight budget.
- ✅ **Solar System ephemeris** (`bris-almanac::ephemeris`). Wraps the
  permissive-licensed `vsop87` crate (VSOP87D, heliocentric ecliptic of
  date) with a Bris-flavored API. Sun's geocentric position derived
  from heliocentric Earth via reflection. Cross-checked against the
  Astronomical Almanac for 2024-01-01.
- ✅ **Lunar ephemeris** (`bris-almanac::lunar`). Hand-implemented
  Meeus *Astronomical Algorithms* Chapter 47 truncated series (~120
  terms total) with full A1/A2/A3 additive corrections and the
  eccentricity factor E. Cross-checked against Meeus Example 47.a
  (1992-04-12) to within the 10″ truncation tolerance.
- ✅ **Star catalog** (`bris-almanac::catalog`). Build-time TSV codegen:
  `data/stars.tsv` is parsed by `build.rs`, sanity-checked
  (range/duplication), and emitted as a `const STARS` array with a
  sorted `HR_INDEX` for binary-search lookup. Runtime applies linear
  proper motion with the standard tangent-rate convention and a guard
  against the cos(δ)→0 singularity at the celestial pole.

---

## Not yet done

### Phase 0 follow-ups

- ⏳ **Vendored leap-second source + build-time regenerator.** Vendor
  IETF/IERS `leap-seconds.list`; have `build.rs` regenerate the
  `LEAP_TABLE` const so refreshing the table is a one-file change
  rather than hand-editing Rust source.
- ⏳ **Vendored Yale BSC import + cross-check script.** Current catalog
  is a starter set of 11 vetted stars. `scripts/fetch_bsc.py` will
  pull the full BSC5 + Hipparcos main from VizieR and emit a
  production `data/stars.tsv`. Runtime API is unchanged when this
  lands. **Hard prerequisite for Phase 3 (plate solving).**

### Phase 1 remainder

- ⏳ **Refraction model.** Bennett or Sæmundsson formula; altitude-
  dependent uncertainty (sub-arcsec at high altitude, several arcmin
  near the horizon). Temperature and pressure overrides.
- ⏳ **Almanac API surface.** `body_position(BodyId, Instant, Observer)
  -> ApparentPosition`. This is the apparent-place pipeline that
  composes ephemeris + frame + observer geometry into a single typed
  output. The honest expected design difficulty here is high — every
  body type (planet, Sun, Moon, star) needs the same chain
  (frame rotation, light-time, aberration, geocentric→topocentric)
  with per-body correctness, and this is where the design pays off
  for keeping per-body modules unaware of these corrections.
- ⏳ **Benchmark on Pi Zero 2W.** Confirm < 1 ms per body. Likely
  trivially true; just need to measure once we have the API surface.

### All later phases (1.5, 2, 3, 3.5, 4, 5, 5.5, 6, 7, 8, 9)

Not started. See `plan.org` for the full task list.

---

## Crucial design notes (concise)

These are decisions we've already committed to in design conversations.
Encoded in code, plan, or both.

1. **Continuous-operation model.** The Rust core streams frames in,
   automatically detects horizon and bodies, selects targets, and
   publishes fixes whenever the rolling sight window changes. The
   mobile UI wraps this in a session-based UX (red/yellow/green by
   uncertainty, never auto-accept red, sustained-green auto-accept,
   timeout picks best yellow-or-green). The embedded device runs
   continuous output to the chartplotter.

2. **Honest uncertainty as the central invariant.** Every measurement
   carries a 1σ. Every fix carries a full 2×2 position covariance
   built from per-source contributions (centroiding, horizon,
   calibration, stitching, refraction, dip, timing, almanac). The
   dominant-source field tells operators what to fix. This is what
   makes Bris trustable offshore.

3. **Optical fixes are always published.** Nothing silently corrects
   them. Nothing rejects them on prior-position grounds. The post-MVP
   DR cross-check, if implemented, publishes on a separate `$DR*`
   talker ID — the operator visually sees disagreement.

4. **Mean equinox per body, apparent-place pipeline does the rest.**
   Each ephemeris/lunar/catalog function returns a true (mean-equinox-
   of-date or J2000-with-proper-motion) position. The apparent-place
   pipeline applies precession, nutation, light-time, aberration, and
   refraction uniformly. Tested early when implementing lunar — saved
   us from a per-body nutation bug.

5. **Build-time data integrity.** The leap-second table, star catalog,
   and (eventually) almanac coefficient generators all sanity-check
   inputs at build time. A typo in a coordinate is a build error with
   file:line, not a runtime fix that's silently wrong.

6. **No ML, no telemetry, no internet at runtime.** Classical CV
   throughout the vision stack. No Prometheus, no OTLP, no status web
   page on embedded — observability rides on the `$PBRIS` proprietary
   NMEA sentence (multi-subtype) consumed by an external converter
   tool if anyone wants metrics.

7. **NMEA 0183 first; NMEA 2000 deferred.** OpenCPN is the recommended
   chartplotter because it parses `$GPGST` (uncertainty) cleanly.
   Quality-field degradation triggers chartplotter alarms for free.
   `$PBRIS` carries Bris-specific diagnostics including per-source
   uncertainty contributions.

8. **57 navigational stars + automatic body selection.** The user
   never picks a body. Day mode locks the Sun (or Moon if higher);
   night mode plate-solves and picks 3-5 stars by altitude band
   (20-70°), azimuth diversity, and brightness.

9. **Pi Zero 2W is the embedded floor.** Embedded-Linux appliance
   model (Buildroot/pi-gen + systemd unit), not bare-metal.
   Read-only rootfs, writable overlay for logs and config.

10. **One Rust core, native mobile shells.** Kotlin on Android, Swift
    on iOS, both calling into the core via UniFFI (deferred until the
    core API is stable — currently no `bris-ffi` crate). Mobile UI
    state is local to the shells, not the core.

11. **OTA updates are bundled.** Leap-second table, almanac
    coefficients, and star catalog ship together as `bris-data`,
    user-initiated `bris-cli update` on embedded, app-store update on
    mobile. Auto-update infrastructure deferred until shipping
    hardware justifies it.

---

## Crucial implementation notes (caveats and invariants)

1. **`f64` Julian Date precision is ~100 µs after arithmetic.** Fine
   for celestial nav (4 orders of magnitude better than position
   budget). If sub-microsecond ever needed, upgrade path is a split
   `(jd_int, jd_frac)` representation. Not now.

2. **Catalog is currently 11 stars.** Sufficient for sight-reduction
   testing of those specific stars. **Insufficient for plate solving.**
   The full BSC import (Phase 0 follow-up) is a hard prerequisite for
   Phase 3 (plate solving) work.

3. **Proper-motion convention is dα/dt × cos(δ).** This is the
   tangent-rate convention used by Hipparcos and SIMBAD. To convert
   to a rate of change of α itself, divide by cos(δ). Getting this
   wrong produces fixes systematically biased near the celestial poles.

4. **No FFI surface yet.** `bris-ffi` is intentionally absent.
   Mobile work cannot start until the Rust core API is stable, by
   design — avoids churning the bindings while the core evolves.

5. **`cargo-deny` allows MIT, Apache-2.0, BSD, ISC, MPL-2.0, Unicode,
   Zlib, CC0, GPL/LGPL-3.** New dependencies whose license isn't on
   that list will fail CI.

6. **Independence assumption in uncertainty quadrature is not strictly
   true.** Stitching error and centroiding error are correlated through
   shared image SNR. Documented limitation. A small "uncertainty
   inflation factor" tunable against real-world data is planned for
   Phase 8 validation.

7. **The vsop87 crate is MIT/Apache.** We wrap it; output is
   GPL-3.0 (allowed per cargo-deny config). If someone ever wants
   to use the Bris core under a permissive license, they'd need to
   replace this crate or relicense the project (not happening).

8. **The lunar series accuracy is ~10″ in longitude / ~4″ in
   latitude / ~1 km in distance.** Three orders of magnitude better
   than per-sight budget. Documented in the module.

---

## Next concrete step

**Refraction model** (`bris-almanac` next module). Either Bennett or
Sæmundsson formula. Should:
- Accept apparent altitude (radians) and observer atmospheric
  conditions (temperature K, pressure mbar, both with sane defaults).
- Return a refraction angle and an uncertainty estimate that grows
  near the horizon.
- Be sub-arcsecond accurate at altitudes above 15° and explicitly
  acknowledge irreducible uncertainty below ~5° altitude.
- Be testable against published refraction tables in the Astronomical
  Almanac.

After refraction: the **almanac API surface** task assembles
ephemeris + lunar + catalog + frame + refraction into the single
`body_position(...)` apparent-place function. That's the most
intricate single piece of Phase 1 and the gateway to Phase 2 (vision).

---

## Open questions

None blocking the next step. The standing question of **target
accuracy** has been decided ("best we can do"; design baseline 1-2 nm,
stretch 0.5 nm) and shapes calibration rigor and validation effort but
does not block almanac work.
