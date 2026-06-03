### L154 Choose workspace layout
- `crates/` includes extras `bris-math`, `bris-streaming`, `bris-capture`, `bris-calibrate`, `bris-collector`, `bris-bundle` not in spec list; `bris-android/bris-ios` thin shells deferred (only `bris-android/` present). Acceptable per spec ("separate repos optional"). CLEAN as substantive deviation.

### L165 CI matrix
- `.github/workflows/ci.yml:48-55` macOS targets (`x86_64-apple-darwin`, `aarch64-apple-darwin`) commented out / disabled; spec required them.
- `.github/workflows/ci.yml` no `aarch64-linux-android` or `aarch64-apple-ios` build/test jobs — only aarch64-linux-gnu cross-build.
- cross-build at `.github/workflows/ci.yml:88-99` excludes `bris-cli` and `bris-capture` from aarch64 build (admitted).

### L170 License/CoC/contributing
- No `CODE_OF_CONDUCT.md` file in repo (find returned nothing); spec lists "code of conduct" explicitly.
- `CONTRIBUTING.md` exists but minimal (just points at plan.org per grep).

### L171 rustfmt/clippy/cargo-deny
- `deny.toml:13-19` ignores `RUSTSEC-2025-0141` and `RUSTSEC-2024-0436` advisories rather than fixing.
- `Cargo.toml:46-49` allows lints `module_name_repetitions`, `must_use_candidate`, `missing_errors_doc`, `missing_panics_doc` (documented as "too noisy"); workspace-wide weakening of pedantic.

### L172 proptest harness
CLEAN — proptest wired as dev-dep across 8 crates.

### L173 leap-second source + regenerator
- `bris-data` payload generation from same source not implemented; `crates/bris-bundle/src` has no `leap` references (spec says regenerator also produces the bris-data payload's leap-second portion).

### L182 Yale BSC import + cross-check
- `scripts/import_bsc.py:30-87` `NAVIGATIONAL_STARS_HR` set contains admitted-wrong HR numbers with comments like "incorrect alias?", "no - 337 is Diphda", duplicate `617`; author notes "Cleaning this list is a follow-up" (`import_bsc.py:86-90`) — navigational flag is not authoritative as spec implies.
- No `scripts/fetch_bsc.py` (spec names this script); no VizieR fetch step — operator must manually `curl` per `import_bsc.py:194-200`.
- No magnitude filter to ≤6.5 + nav57 superset; emits all parsed rows.
- Cross-check script not present; only inline tests in catalog.rs.
- Starter `data/stars.tsv` shows `HR1`, `HR2` placeholder names (no real names imported into starter), `hip` always 0 (`import_bsc.py:131` `'hip': 0`); spec says cross-reference Hipparcos.

### L198 Coordinate types
CLEAN — `bris-core/src/angle.rs` `Angle`/`Latitude`/`Longitude` newtypes with explicit constructors and range checks; time types in `time.rs`.

### L201 Time scales
- No build-time check that warns when leap-second table is older than N months; spec required it. `time.rs:46-50` only exposes `LEAP_TABLE_UPDATED_AT_UNIX`; staleness check is deferred to Phase 1.5 (still TODO).
- `LEAP_TABLE_EXPIRES` const-fn `time.rs:60-92` uses `civil_from_days`; fine.

### L205 Precession/nutation
- Implements IAU 2006 precession + IAU 2000B nutation (`frame.rs:127` "2000B" — 77 luni-solar terms only) rather than the full IAU 2000A spec listed; module docs explicitly call this out (2000B vs 2000A truncation). Spec allowed either 2006/2000A or simplified IAU 1980, so 2000B is a deliberate intermediate — flag as deviation.

### L207 Solar System ephemeris
- `ephemeris.rs:87` uses `EarthMoonBarycenter` for Earth heliocentric (admitted ≤6 arcsec error at `ephemeris.rs:104-106`); spec target <1 arcsec.
- No JPL Horizons validation in repo (grep for `Horizons` in bris-almanac returned nothing); spec explicitly requires validation against Horizons.

### L211 Lunar ephemeris
- `lunar.rs:1-21` uses Chapront/ELP-derived truncated series (not labeled ELP2000-82B specifically; truncation acknowledged).
- No JPL Horizons cross-validation; only one inline tolerance test (`lunar.rs:403` 10″ tol).

### L213 Star catalog
- Starter `data/stars.tsv` contains placeholder `HR1`, `HR2` names rather than the 57 nav + 1000 brightest from Hipparcos (`stars.tsv:21-22`); spec says "Embed the 57 standard navigational stars + ~1000 brightest from Hipparcos as a `const` table".
- `hip` cross-reference always 0 (`import_bsc.py:131`); not actually Hipparcos.

### L217 Refraction model
CLEAN — `refraction.rs` Bennett formula with pressure/temperature overrides, validation, and uncertainty.

### L219 Almanac API surface
- Spec signature `body_position(BodyId, Instant, Observer) -> ApparentPosition`. Actual API is `body_apparent_place(...) -> ApparentPlace` and `body_geocentric_apparent(...)` (`apparent.rs:127`, `apparent.rs:305`); no `BodyId`/`ApparentPosition` types as named. Cosmetic naming deviation only.

### L221 Annual aberration
CLEAN — `apply_annual_aberration` in `apparent.rs:392`.

### L226 Diurnal aberration
CLEAN — `apply_diurnal_aberration` in `apparent.rs:532`.

### L286 Image abstraction
- `Frame` (`bris-vision/src/frame.rs:379-410`) has fields matching spec but renames `timestamp` to `capture_tt`; adds `gravity_in_camera` etc. Substantively complete. CLEAN.

### L368 Horizon detection
- All three strategies implemented (`horizon.rs:138`, `horizon.rs:185`, `segment.rs:327`); `HorizonError::LowConfidence` returned (`horizon.rs:305`). CLEAN.

### L534 Sun/Moon centroiding via saturated-body thresholding
CLEAN — `centroid_saturated_body_in_mask` at `centroid.rs:435`; tests cover halo isolation, mask honoring, rejection of unsaturated frame.

### L564 Sun/Moon centroiding
- Limb correction NOT implemented for partial-occlusion case; `centroid.rs:21-26` explicitly defers: "For MVP we report the centroid as-is and note the bias as a TODO contribution to per-sight uncertainty". Spec body says "Account for partial occlusion by horizon (limb correction)" — shipped as DONE without this.
- No ellipse fit found in code (grep `ellipse_fit` empty); pipeline uses connected-component centroid + subpixel refinement only. Spec body said "ellipse fit, sub-pixel refinement".

### L567 Multi-frame stitching
- IMU/gyro priors not implemented; documented as follow-up at `panorama.rs:33`.
- Sidereal-motion correction between frames not implemented (spec required); grep `sidereal` in panorama returns only the docs comment block.
- Lens distortion compensation across the chain explicitly approximate (`panorama.rs:39-43`) — not corrected for fisheye.
- Uses Harris+NCC rather than ORB as spec named (`track.rs:7-24` acknowledges this is "not full ORB").

### L596 Angle measurement pipeline
- `measure.rs:71` `Sigma::new(centroid_sigma_rad).unwrap_or(Sigma::ZERO)` swallows non-finite sigma silently to zero, masking real failure cases (would propagate spurious zero-uncertainty result).
- Body sigma is single-axis (`sigma_px / fy`); no distinct per-axis (x/y) propagation — y-axis only used. Spec didn't explicitly demand per-axis but altitude measurement uses only fy → acceptable.
- Horizon endpoint sample uses hardcoded `x = 1000.0` (`measure.rs:108`) with comment "We don't actually know the image width here, but slope is small ... so x = 1000 is fine"; shortcut where intrinsics width would be the principled source.
