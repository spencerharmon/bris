### L890 TOML-driven regression test harness
CLEAN

### L901 Day/night/twilight classifier
CLEAN — disagreement handling + conservative-pick + 0.4 cap implemented (crates/bris-vision/src/condition.rs:212-274); wired to harness via `[expected_classifier]`.

### L913 Load-time rotation infrastructure (opt-in)
CLEAN — `Rotation`, `rotate_pixels`, `Frame::source_rotation`, with-rotation loader/segmenter all present (crates/bris-vision/src/frame.rs:70-507).

### L926 Promote test_video/ scenes to regression cases
CLEAN — 14 case dirs under crates/bris-vision/tests/regression/.

### L973 Run pipeline against each scene in test_video/
CLEAN — superseded by harness as documented.

### L982 Night-horizon detection v1: sea-sky luma boundary
CLEAN — `bris-vision::night_horizon::detect_horizon_night*` + `detect_horizon_night_excluding_body` exist.

### L1018 Multi-pass night-horizon detection
CLEAN — `detect_horizon_night_multi_pass` implemented per body text.

### L1106 Star detection in frame
- Body text concedes PSF/magnitude estimation deferred; called out as "not yet implemented but adequate." Caveat about daylight peak detector is also acknowledged. No undisclosed shortcuts.
CLEAN (with documented limitations)

### L1132 Geometric hash database
- Body text acknowledges "not yet serialized via build.rs"; lazy build accepted. No undisclosed shortcuts in crates/bris-platesolve/src/hash.rs.
CLEAN

### L1156 Matcher
CLEAN — Kabsch+verify+one-to-one all present in crates/bris-platesolve/src/solve.rs:200-460.

### L1198 Verification refinement to reduce false matches
CLEAN — refinement gate at crates/bris-platesolve/src/solve.rs:436-459, default 30 arcsec at solve.rs:60.

### L1225 Plate-solve regression integration
- Generated tests are `#[ignore]` (acknowledged), `max_rms_residual` loosened to 60 arcsec for placeholder intrinsics in crates/bris-platesolve/tests/real_data.rs:86 — documented in plan as load-bearing refusal under placeholder. No shortcut beyond what plan states.
CLEAN

### L1261 Per-star altitude extraction
- Plan body has contradictory tail block ("*Status:* unimplemented") at plan.org ~L1313-1321 but `bris_platesolve::altitude::star_altitudes` is implemented (crates/bris-platesolve/src/altitude.rs:69-91) and Stage E consumes it. Implementation present; per-star σ propagation from refinement residual is not derived inside `star_altitudes` — it's the caller's `per_star_sigma` (treated as opaque input); plan says it should propagate "through the lens model" — only quadrature with horizon σ is done, no lens-model propagation (crates/bris-platesolve/src/altitude.rs:118).
- Below-horizon stars silently skipped — matches plan.

### L1322 bris-streaming crate skeleton + EngineConfig + push_frame + fix_stream API
CLEAN

### L1334 Stage A + B: classifier + body detection
CLEAN — wired in crates/bris-streaming/src/pipeline/mod.rs; classifier hysteresis path exists.

### L1347 Stage C: horizon detection in cheap-first order
- All listed detectors dispatched (crates/bris-streaming/src/pipeline/horizon.rs:70-336). Segmentation only behind cargo feature `segmentation` and stubbed when feature off (horizon.rs:335-336) — acceptable since model path is optional.
CLEAN

### L1363 Body and horizon priority queues + ring buffer + eviction
CLEAN

### L1376 Stage E: pair selection + sight emission + sight window
- Cross-frame stitch implemented; selection-time σ uses a coarse `STITCH_SIGMA_PER_SECOND_RAD = 0.5 arcmin/s` placeholder (crates/bris-streaming/src/pipeline/stage_e.rs:120-127) — plan explicitly calls this out as cheap estimate, superseded by executed Kabsch RMS at sight time. Documented, not a shortcut.
CLEAN

### L1408 Stage D: plate solving + per-star altitude
- Stage D promotes Night→IdentifiedStars and Stage E expands per-star. Night payloads Stage E sees post-failure return `Ok(empty)` silently (crates/bris-streaming/src/pipeline/stage_e.rs:721-724) — documented.
CLEAN

### L1422 Day/night classifier hysteresis
CLEAN — crates/bris-streaming/src/pipeline/hysteresis.rs implemented with default 90 frames (config.rs:495).

### L1432 $PBRIS extensions: N sights, az spread, oldest age
CLEAN — crates/bris-nmea/src/pbris.rs:170-223 `$PBRIS,FIX` carries all three.

### L1444 Engine integration + stress tests
- Plan explicitly defers the case.toml `[fix]` runner. README confirms "[fix] optional; runner pending" (crates/bris-vision/tests/regression/README.md:148) and build.rs treats `[fix]` as opaque `_fix: toml::Value` (crates/bris-vision/build.rs:219-220). Disclosed in plan body.
CLEAN

### L1472 Per-fix contributing-frame IDs + frame_by_id
CLEAN — `frame_by_id` at crates/bris-streaming/src/engine.rs:943; `contributing_frame_ids` populated and tested.

### L1495 Engine sight persistence
- `SightStore` opens with advisory lock; lock-poison errors swallowed via `unwrap_or_else(PoisonError::into_inner)` (crates/bris-streaming/src/store.rs:164,177,349) — standard mutex-poison recovery, not a real shortcut.
- 96-byte fixed records, hourly+size rotation, retention prune, hydrate-on-start, append-on-publish all present.
CLEAN

### L1551 Engine tuning: opportunistic-flow defaults + publication gate + cumulative counters
CLEAN — defaults at crates/bris-streaming/src/config.rs:488-533 (7200s, 50 cap, gate sub-config), `assumed_max_speed_kn = 0.0` default per spec, all six counters present in crates/bris-streaming/src/diagnostics.rs, gate unit tests present (stage_e.rs:1646-1745).

---

Cross-cutting note: Many code paths use `Intrinsics::placeholder(...)`. Plan repeatedly acknowledges placeholder intrinsics as a known gap blocking absolute accuracy across all listed Phase 3 items, so this is disclosed shortcut, not hidden. The only un-disclosed shortcut found is the L1261 per-star σ: plan body promises "Per-star σ propagation from the plate-solve residuals through the lens model" but actual code at crates/bris-platesolve/src/altitude.rs:118 just quadrature-combines a caller-supplied scalar `per_star_sigma` with horizon σ — no lens-model propagation.
