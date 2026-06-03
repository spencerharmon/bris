### L1590 Phase 1 ReflectionPairProvider
- Test 5 reflector-region check deferred per body text (in-code TODO acknowledged); body explicitly notes this so not a shortcut.
- DR projection of stale fix priors deferred (body acknowledges); current staleness gate hardcoded at 30 s lifted to config — clean per body.
- CLEAN against own body.

### L1606 Phase 2 Day-mode multi-centroid
- Body itself flags four deferred items (lens-flare rejection, specular-vs-diffuse, glitter-path, Pi Zero 2W headroom) as TODO(phase 3) at `crates/bris-vision/src/horizon_providers/reflection_pair.rs:14,22,30,37`. Honest deferral, but Day-mode success path requires `position_prior` per body — acknowledged caveat.
- CLEAN against body (caveats disclosed).

### L1693 Phase 6 Multi-source HorizonProvider fusion
- Pairwise concordance is approximated by greedy "current cluster mean" not true pairwise per body text (`crates/bris-vision/src/horizon_providers/fusion.rs:172-209`). Body says "Pairwise concordance threshold" — implementation uses cluster-mean concordance, not pairwise; minor spec drift.
- Otherwise matches body (σ floor 1e-4, Fused provenance, four diagnostics, `Vec<DirectSight>` propagation at `crates/bris-streaming/src/pipeline/horizon.rs:61,386`).

### L1721 Cold-start no-AP fix
- CLEAN. `bris_nav::cold_start_fix` in `crates/bris-nav/src/circle_of_position.rs`, Stage-E fallback present (stage_e.rs:874), `FixProvenance` variants surfaced.

### L1764 Single sight LOP
- CLEAN. `line_of_position` at `crates/bris-nav/src/sight.rs:91+`.

### L1767 Multi-sight fix with full covariance
- Body says "Time uncertainty enters as a one-dimensional longitude variance (≈15′/s × cos(lat)) added to the position covariance." No such term present: `multi_sight_fix` at `crates/bris-nav/src/fix.rs:112-189` builds covariance purely from per-LOP intercept σ; no time-σ → longitude-variance inflation anywhere in `bris-nav` or `bris-streaming` (grep `time_sigma`/`seconds_since_sync` → no matches).
- `total_weight` computed then discarded with `let _ = total_weight;` (fix.rs:139) — chi-square diagnostic deferred without TODO marker.

### L1776 Sanity checks
- `screen_sights` exists in `crates/bris-nav/src/screen.rs:73` with intercept-magnitude + outlier-from-consensus checks, but body also requires "azimuth disagreement" — not implemented (no azimuth-disagreement screen present; grep returned no matches).
- Screener is never wired into Stage E: `grep screen_sights crates/bris-streaming` returns no matches. Pipeline doesn't actually invoke the sanity checks, so the diagnostics named-in-output requirement is unmet end-to-end.

### L1817 NMEA 0183 sentence builders
- CLEAN. `gpgll`, `gprmc`, `gpgga` at `crates/bris-nmea/src/standard.rs:114,146,188` with checksums and FAA mode flag.
- Minor: `$GPGGA` altitude hardcoded `0.0` (standard.rs:189) — body OK with this ("sea-level convention").

### L1820 Uncertainty in NMEA output
- `$GPGGA` HDOP approximated as `sigma_major_nm.max(0.1)` divided by nothing (`crates/bris-nmea/src/standard.rs:187`) — body says "HDOP ≈ σ_major / 1 nm" so technically OK, but a coarse proxy not from full fix covariance.
- Otherwise CLEAN: GST emits ellipse axes, GGA quality degrades 1→6→0 via `QualityThresholds`, RMC status flips.

### L1897 Structured $PBRIS payload
- CLEAN. VER/TIME/UNC/SIGHT/ERR subtypes implemented at `crates/bris-nmea/src/pbris.rs:24,69,142,241,281`; ordering test at pbris.rs:458; <82 char test at :502.

### L1911 Document NMEA→metrics extraction contract
- CLEAN. `docs/protocol/pbris.md` exists, all subtypes specified, schema-versioned.

### L1936 V4L2 capture
- CLEAN per body. YUYV-only acknowledged; MJPEG/NV12/RAW deferred openly; libcamera deferred openly. Compile-verified, not yet exercised on hardware — body says so.
- Unrelated TODO at `crates/bris-capture/src/lib.rs:70` about `--probe` CLI is acknowledged drift.

### L2316 Linux session create/resume CLI
- CLEAN. `bris session new|list|show|attach` at `crates/bris-cli/src/main.rs:1923-2070`, flags match body, 3 unit tests present (main.rs:2080-2226).

### L2339 Android session UI + on-device session creation
- Body lists `ap_seed` overlay onto observer; in `LiveScreen.kt:223-227` the observer fed to bundle/engine is still hardcoded `(0.0, 0.0, eyeHeightM=2.0)` — Phase 7.5 explicitly flags this as load-bearing shortcut. The `defaultEngineConfig(session=...)` overlay path (LiveScreen.kt:780) does read `apSeed`, but the bundle-writer path still emits Null Island.
- Otherwise wiring (SessionStore, SessionPickerScreen, SessionEditScreen, CaptureRecorder.onCaptureSaved, store_data_root) appears in place; the AP-from-session inconsistency between the two consumers is the open shortcut.

### L2578 SightLogScreen lists per-session captures
- CLEAN. `CaptureCatalog` + `SightLogScreen` at `bris-android/.../ui/SightLogScreen.kt:50,96` and `engine/CaptureCatalog.kt:20,109`. Detail-screen routing follow-up disclosed in body.

### L2588 Plumb assumed_max_speed_kn through FfiEngineConfig
- CLEAN. Wired through replay at `crates/bris-cli/src/main.rs:974`; session-overlay test at main.rs:2205+.

### L2594 Rotation handle rotate-lock
- CLEAN. `DeviceOrientationSource` at `bris-android/.../engine/DeviceOrientationSource.kt:42`, subscribes to `TYPE_ACCELEROMETER`, LiveScreen wires at `ui/LiveScreen.kt:474`. Open follow-up on `gravity_camera_frame` reuse disclosed.

### L2663 Lens selection
- CLEAN. `LensCatalog` enumerates via Camera2 `cameraIdList` filtered to back-facing, labels by focal length, picks longest non-ultrawide default (`engine/LensCatalog.kt:80-145`); `selectorFor(lensId)` uses `Camera2CameraInfo` filter (LensCatalog.kt:155-170); `CalibrationStore` keys by `lens_id` (CalibrationStore.kt:60,116,154,234); Prefs persist `selected_lens_id` (Prefs.kt:91).
- Minor drift vs body: body says identify telephoto by "longest `LENS_FOCAL_LENGTHS` entry"; implementation labels role by zoom relative to median focal length, not the longest entry — semantic equivalent but different rule. Low-light heuristic ("sensor area ≥ 1/3\"") from body is not implemented (`pickDefault` only filters ultrawides, not by sensor area).
