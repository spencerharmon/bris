# Testing strategy — sessions, corpus, replay

How Bris testing works going forward.

Three pieces:

1. **Session / capture / corpus layout** — single on-disk
   format used by both the on-device save path and the
   workstation replay path.
2. **Cold-start coverage** — body / horizon / optical
   conditions the replay path should exercise; accumulated
   opportunistically.
3. **Replay extensions** — `bris replay` grows session and
   whole-corpus modes; no new harness binary.

Read first:

- `docs/design/capture.md` — per-capture (Start/Stop) UX and
  the engine surface that supports it.
- `docs/design/debug_bundle_schema.md` — per-capture
  `bundle.json` format. Captures wrap bundles 1:1.
- `docs/design/replay_modes.md` — the four AP-handling
  replay modes.

## Terminology

- **Capture** — one Start/Stop window. Contiguous frame
  burst written as a bundle (`bundle.json` + frames). The
  atomic unit. Per `capture.md`.
- **Session** — operator-defined grouping of one or more
  captures sharing the operator's intent (same vessel,
  same trip, same observing window). UUIDv4. Created
  explicitly ("New session"); never explicitly ended.
  Either exists or is deleted. A capture belongs to
  exactly one session.
- **Corpus** — workstation tree of imported sessions.
  Append-only.

The streaming engine itself is session-aware **only** through
session-supplied `EngineConfig` overrides (sight retention,
assumed kinematics). It has no notion of capture boundaries.

## Multi-device note (out of scope)

Fusing captures from physically distinct observers (five
fixed-mount cameras on one vessel) would require a
distributed gossip protocol between Bris instances sharing
sight/fix data live. Explicitly out of scope; flagged in
`plan.org` Phase 9. Session model below assumes one
observer.

## Session / corpus layout

### Capture id

`cap-<13-hex unix-ms>`, e.g. `cap-0019e7634306b`. No random
suffix.

Both Android save paths (`DebugBufferActions.kt`,
`Exporter.kt`) must share one helper. Today each defines its
own `ulidLike()`; unify.

### Session id

UUIDv4. UI displays short prefix (8 chars).

### On-device layout

```
<external-files>/
  sessions/
    <session-uuid>/
      session.json
      captures/
        <cap-id>/
          bundle.json
          frames/NNNNNNNN.pgm
          frames/NNNNNNNN.json    # FrameSidecar; includes per-frame gps_truth
          pbris.log               # optional
          sights/                 # per-capture live-fix output
            <sight-ulid>/         # per capture.md
              manifest.json
              media/
```

`sights/` is **per-capture, not per-session**. Each
Start/Stop window's live fix(es) land under that capture's
`sights/`. (Old layout was `<external-files>/sights/<id>/`;
moved.)

### Zip = extracted layout

Zips contain `sessions/<uuid>/...` at the root. Workstation
import:

```
unzip -n bris-debug-*.zip -d bris-corpus/
```

`-n` (never overwrite) makes it idempotent. Re-extracting
the same zip is a no-op; partial overlap (a second save of
the same session with one new capture) adds only the new
capture. No custom `bris corpus import` command; this is
the documented workflow.

### Workstation corpus

```
bris-corpus/
  sessions/
    <session-uuid>/
      session.json
      captures/<cap-id>/...
```

Gitignored (large blobs, not text-diffable). Filtering
("sessions with body=sun") is `find` + `jq` on the JSON.
No SQLite index until corpus size makes the JSON walk slow.

### `session.json` schema

In `crates/bris-bundle/src/lib.rs` next to `BundleManifest`:

```rust
pub struct SessionManifest {
    pub schema_version: u32,            // 1
    pub session_id: Uuid,
    pub created_unix_ms: i64,
    pub device: DeviceInfo,             // for indexing
    pub build: BuildInfo,               // plan.org Phase 8.5
    pub title: String,                  // operator-supplied; for picker UI
    pub notes: Option<String>,          // free-text
    pub ap_seed: Option<ApInput>,       // optional, operator-entered at create
    pub profile: UseCaseProfile,        // default UseCaseProfile::Custom
    pub kinematics: SessionKinematics,
    pub sight_retention_seconds: u64,   // EngineConfig override
    pub sight_retention_capacity: usize,
    pub expected_to_fail: bool,         // default false; adversarial cases
    pub ordered_capture_ids: Vec<String>,
}

pub enum SessionKinematics {
    Stationary,
    MaxSpeedKn(f64),
}

pub enum UseCaseProfile {
    Custom,          // default; values are operator-set
    Marine,          // reserved; drives defaults eventually
    Aeronautical,
    LandBased,
    Urban,
}
```

Notes:

- **No `gps_truth`.** Truth is per-frame (see Per-frame
  data below); a moving capture has different truth at
  different frames.
- **No `closed_unix_ms`.** Sessions don't end. Either
  exist or are deleted.
- **`ap_seed`** is optional. UI offers lat/lon fields at
  session create. Until the "pick AP from map" UI lands
  (`plan.org` Phase 7), text entry is the only path.
  Engine still treats the seed as advisory (`replay_modes.md`).
- **`profile`** classifies. Drives a small set of opinionated
  [`EngineConfig`] defaults via
  [`bris_streaming::apply_profile`] (Marine, Aeronautical,
  LandBased, Urban); `Custom` leaves the operator in charge.
  Resolution order: CLI `--profile` flag > `session.json` >
  `Custom`. The dispatcher only writes fields that are still
  at the engine default, so an operator-set `kinematics` or
  CLI override is never silently clobbered.
- **`kinematics` + retention** flow into `EngineConfig` on
  engine construction (live and replay). `Stationary` sets
  `PublicationGateConfig::assumed_max_speed_kn = 0`;
  `MaxSpeedKn(v)` sets it to `v`. Retention fields override
  `sight_window_seconds` / `sight_window_capacity`.
- **All fields except `session_id`** are editable from the
  session management UI.
- **`expected_to_fail`** is for adversarial captures where
  no fix is the correct answer. Default false; cold-start
  is the focus, not adversarial robustness, but the flag
  is wired so the regression harness can later honor it.
- **Append-only `ordered_capture_ids`**. New captures land
  at end; session.json is rewritten on each capture save.

Per-capture `bundle.json` carries a back-reference
`session_id: Uuid` so an orphaned capture can be traced.

### Per-frame data

`FrameSidecar` (`bris-bundle/src/lib.rs:355`) gains two
fields:

```rust
pub gravity_camera_frame: Option<[f64; 3]>,  // sensor gravity vec
pub gps_truth: Option<GpsTruth>,             // per-frame truth stamp
```

`gravity_camera_frame` closes a real bug: live `Frame` has
the field (`bris-vision/frame.rs`), bundle sidecar didn't.
Replay falls back to image-down, which silently miscomputes
the artificial-horizon reflection pairs. After this, replay
sees the same gravity vector live did.

`gps_truth` moves from the bundle level to per-frame.
Captures can run hours at speed; per-bundle truth was
implicitly wrong for any non-stationary capture. Per-frame
is the only honest place. Replay scoring averages or
interpolates as appropriate; non-truth frames carry `None`.

(Operator captures without GPS truth: sidecar `gps_truth =
None` on every frame. The σ-honesty check downgrades to
"did we publish anything" only.)

## Cold-start coverage

Descriptive, not prescriptive: we tabulate what the corpus
covers and look for gaps.

### Axes

| Axis | Values |
|---|---|
| Body | sun, moon, stars |
| Horizon | water, urban (vertical lines), artificial (bowl) |
| Optical | bare, ND filter |
| Captures per session | 1..N opportunistic |
| Replay mode | per `replay_modes.md` |

Fixed constraints (not axes):

- Frames per capture: ≥ 2 (never 1).
- AP at engine start: cold-start (no `ap_seed`) for the
  pure cold-start cells. Sessions with `ap_seed` populate
  the seeded-AP cells.
- Time source: NTP / device system time.
- Atmosphere: standard model.
- Lens: one calibrated lens per device; ND filter doesn't
  change intrinsics.

### Pass/fail

Single global rule. Per (session × replay-mode):

- **Pass**: at least one fix published when the mode is
  supposed to publish, AND each published fix satisfies
  `err_nm <= K * sigma_major_nm` where `err_nm` is the
  great-circle distance to the closest-in-time per-frame
  `gps_truth` (or session ap_seed if no per-frame truth).
  K = 3 (3σ ≈ 99.7%).
- **Fail**: published fix with `err_nm > K * sigma_major_nm`.
  Engine lying about its uncertainty. The AGENTS.md
  "honest uncertainty everywhere" mandate.
- **Honest silence**: no fix published. Recorded; not a
  failure. If `expected_to_fail = true`, a published fix
  flips this to a failure.

No per-session expected-range files.

## Replay extensions

Extend the existing `bris replay`:

```
bris replay --session <session-dir>             # new
bris replay --all-sessions <corpus-root>        # new
bris replay --bundle <bundle-dir>               # existing
```

### `--session`

- Load `session.json`. Apply `kinematics` /
  `sight_retention_*` to `EngineConfig`.
- Construct one engine.
- Feed captures in `ordered_capture_ids` order; frames per
  capture sorted by `captured_unix_ms`.
- Standard mode flags (`--ap-seed-truth`, `--ap-lock-truth`,
  `--all-modes`, `--no-ap`) apply session-wide.
- Score per the pass/fail rule. Output CSV row per mode.

### `--all-sessions`

- Walk `<corpus-root>/sessions/*/`.
- For each session, run every mode the session's data
  supports (modes needing per-frame `gps_truth` skip when
  absent).
- CSV row per (session, mode).
- Exit non-zero on any σ-honesty failure.
- Print coverage table at the end: count by
  (body × horizon × optical).

### Stage E across captures

Today's Stage E pair selection keys on per-frame proximity
within a session's sight window. For two captures 15+
minutes apart, both sights live in the (operator-configured)
sight window and pair selection should combine them. For
multi-day stationary spans (a sun sight Monday, another
Wednesday), retention must be set wide enough and
kinematics `Stationary`. The math works (each sight carries
its own anchor_jd; CoPs are anchored to body GP at
sight-time, not fix-time).

Verify this against a real multi-capture session as soon as
one exists; if Stage E drops cross-capture pairs even with
generous retention, that's a Stage E bug.

## Implementation order

1. Phase 8.5 (build provenance / version stamping) from
   `plan.org`. Required for meaningful harness output.
2. Unify `ulidLike()` → shared `captureId()`; drop random
   bits.
3. Rename collisions:
   - Android `SessionRecorder` → `CaptureRecorder`,
     `sessionId` → `captureId`.
   - Android `SightLog::sessionId` → `captureId`.
   - `IntrinsicsSource::UserCalibration::session_id` →
     `calibration_id`.
   See "session" overloading table below.
4. Add `gravity_camera_frame` + `gps_truth` to
   `FrameSidecar`. On-device writer populates both. Replay
   reads both.
5. Define `SessionManifest`, `SessionKinematics`,
   `UseCaseProfile` in `bris-bundle`. Round-trip tests.
6. Android: "New session" form (title, optional lat/lon,
   optional kinematics override). "Resume session" picker
   (by title, sorted by most-recent capture). Edit any
   field except UUID. Session is required before captures
   can save; auto-create a "Default session" on first
   launch if none exists.
7. CLI: `bris replay --session`. `--all-sessions` follows.
8. Document `unzip -n` import workflow in AGENTS.md.
9. Replay existing footage. Iterate Stage E cross-capture
   as gaps surface.

## "session" overloading in the existing code

| Location | Refers to | Action |
|---|---|---|
| `bris-calibrate` (`coverage.rs`, `detect.rs`, `solve.rs`, `lib.rs`) | calibration session | keep |
| `vision-calibration::session::CalibrationSession` | calibration session | keep |
| `IntrinsicsSource::UserCalibration { session_id }` | calibration session | **rename → `calibration_id`** |
| `bris-android SessionRecorder.kt`, `sessionId` field | per-capture (Start/Stop) | **rename → `CaptureRecorder`, `captureId`** |
| `bris-android SightLog::sessionId` | per-capture | **rename → `captureId`** |
| `DebugBundleWriter.kt:208` JSON key `"session_id"` | calibration session | rename in lockstep with bundle field |
| `bris-collector/src/manifest.rs:32,58` (doc text) | "debug-capture session" | **doc reword → "capture"** |
| `bris-ffi/src/lib.rs:1600+` | calibration coverage session | keep |
| `bris-capture/src/v4l2.rs:353,373` (log text) | v4l2 open window | keep (low priority) |
| `bris-nmea/src/pbris.rs:24` (doc) | NMEA emitter init | keep |

After this set, "session" in Bris means **only** "session
manifest" or "calibration session" (two distinct scoped
uses); "capture" replaces the old Android per-Start/Stop
"session" usage.

## Open questions

(Down from five; the three you resolved are settled.)

1. **`--all-sessions` ordering.** Replay sessions in
   alphabetical UUID order (deterministic but meaningless)
   or `created_unix_ms` order (chronological)? Suggest
   chronological — lets a regression sweep mirror history.
2. **Cross-capture cold-start in Stage E.** Likely works
   today with wide retention but unverified. Block on the
   first real multi-capture session.
