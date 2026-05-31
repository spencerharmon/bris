# Capture (mobile Start/Stop)

How the operator captures a fix on the phone, what gets saved
to the device, and how the engine surface supports it.

A **capture** is one Start/Stop window. Contiguous frame
burst → one bundle (`bundle.json` + frames + per-frame
sidecars + per-capture `sights/` live-fix output). Captures
belong to **sessions** (operator-defined grouping of related
captures); see `docs/design/testing_strategy.md` for the
session model.

Read first: `docs/design/diagnostic_collection.md` (debug-
mode, collector, debug-capture buffer); `docs/design/
testing_strategy.md` (sessions, corpus, replay);
`docs/design/debug_bundle_schema.md` (per-capture bundle
schema).

## Why captures are a UX concept, not an engine concept

The streaming engine (`crates/bris-streaming`) runs
continuously. On the embedded Pi appliance it lives as a
systemd service: frames in, fixes out, no boundary. The
engine retains a sight in its window for as long as the
configured age permits, replaces worst-σ on insertion, and
keeps publishing.

The phone is different. The operator runs the app, taps
**Start capture**, holds the phone toward the horizon, and
taps **Stop** — or the app captures automatically when a
sustained-green fix arrives. That action defines a
**capture**: a bounded window during which we record what
the engine did.

The engine is session-aware **only** through the
`EngineConfig` overrides a session supplies (sight
retention, assumed kinematics). It has no notion of capture
boundaries; the per-capture recorder layers on top of the
unchanged engine.

## Engine surface (`crates/bris-streaming`, `crates/bris-ffi`)

Two affordances:

1. **Per-fix contributing-frame IDs.** `PublishedFix` carries
   `contributing_frame_ids: Vec<u64>` — engine-assigned IDs
   of every frame referenced by a sight in the fix's active
   window. Each sight contributes one or two frames (body
   and, when different, horizon). De-duplicated, ordered by
   first occurrence.

2. **Frame retrieval by ID.** `StreamingEngine::frame_by_id(u64)
   -> Option<Frame>` looks up a frame in the engine's ring
   buffer and clones it across. Returns `None` once evicted.

The per-capture recorder uses these: on fix publish, iterate
`contributing_frame_ids` and call `frame_by_id` for each to
copy pixel bytes into the capture's bundle. **Promptly** —
the engine keeps processing and contributing frames
eventually evict.

## Capture lifecycle

Engine lifecycle is independent of capture lifecycle. The
streaming engine is constructed once when the live screen
composes; lives until the screen leaves composition. **No
per-capture engine reset.**

```
┌─ LiveScreen composes ────────────────────────────────────┐
│                                                          │
│  Engine constructed once. Preview always bound.          │
│  Analyzer (engine processing) bound only during capture. │
│                                                          │
│  ┌─ Idle ────────┐   Start capture    ┌─ Capturing ──┐   │
│  │ Preview only  │ ────────────────→  │ Preview      │   │
│  │ Engine quiet  │                    │ + Analyzer   │   │
│  └───────────────┘                    │ Engine fed   │   │
│                                       │ Recorder     │   │
│                                       │ scoring fixes│   │
│                                       └──────┬───────┘   │
│                                              │           │
│                  ┌──────────────────────┬────┴────────┐  │
│              sustained-green      operator Stop    timeout │
│                  │                      │             │  │
│                  ▼                      ▼             ▼  │
│  ┌─ Saving ─────────────────────────────────────────────┐│
│  │ Pull contributing-frame bytes via frame_by_id(...).  ││
│  │ Write bundle + sights/<ulid>/ under                  ││
│  │ sessions/<session-uuid>/captures/<cap-id>/.          ││
│  │ Unbind Analyzer; return to Idle.                     ││
│  └──────────────────────────────────────────────────────┘│
└──────────────────────────────────────────────────────────┘
```

A capture cannot save without a session. Auto-create a
"Default session" on first launch if none exists; thereafter
"New session" / "Resume session" picker (see
`testing_strategy.md`).

## Engine defaults under session control

The engine's static defaults
(`sight_window_seconds = 7200`, `sight_window_capacity = 50`,
`PublicationGateConfig::assumed_max_speed_kn = 0.0`) are
**defaults only**. A session's `kinematics` and
`sight_retention_*` fields override them at engine
construction (live and replay alike). A stationary operator
capturing sun sights over days raises retention to days; the
math (per-sight `anchor_jd`, body GP anchored to sight-time)
works without further change.

The publication gate's σ-inflation
(`motion_sigma_nm = assumed_max_speed_kn * oldest_age / 3600`,
RSS) is the honest representation of kinematic uncertainty.
`Stationary` → 0 inflation. `MaxSpeedKn(5)` → 10 nm by 2 h.

## Threshold model

Each published fix scores into a band by σ_major:

| Band   | Range                                  | Behavior |
|--------|----------------------------------------|----------|
| GREEN  | σ ≤ targetσ (default 1.0 nm)           | Counts toward sustained-green auto-accept |
| YELLOW | targetσ < σ ≤ hardσ (default 5.0 nm)   | Eligible for accept on Stop / timeout |
| RED    | σ > hardσ (default 5.0 nm)             | Never accepted, even at timeout |

End conditions, in priority order:

1. **Sustained green** (default 3 s of consecutive green
   fixes). Recorder finalizes with best-σ green observed.
2. **Operator Stop.** Best non-red ever observed;
   otherwise `CaptureOutcome::NoFix` with reason
   `"operator stopped"`.
3. **Timeout** (default 5 min). Same accept-best-non-red;
   otherwise `NoFix` with reason `"timeout after Nms"`.

Thresholds + sustained-green duration + timeout in
`CaptureThresholds` (Kotlin); operator-facing settings UI
tracked under Phase 7.

## On-disk layout

Each captured Start/Stop window lives under its session:

```
<external-files>/sessions/<session-uuid>/captures/<cap-id>/
  bundle.json                  per docs/design/debug_bundle_schema.md
  frames/NNNNNNNN.pgm          contributing-frame bytes
  frames/NNNNNNNN.json         FrameSidecar (incl. per-frame gps_truth,
                               gravity_camera_frame)
  pbris.log                    formatted $PBRIS,FIX line(s)
  sights/
    <sight-ulid>/              one per published live fix
      manifest.json            collector schema v1, kind = "fix"
      media/                   thumbnails / aux media
```

`bundle.json` is the canonical capture artifact (replayable
by `bris replay --bundle` or `bris replay --session`).
`sights/<sight-ulid>/manifest.json` matches
`bris_collector::manifest::Manifest` so the same downstream
tooling (review web UI, regression promoter, collector ingest)
can consume both on-device entries and uploaded submissions.

`fix` sub-object format:

```json
{
  "capture_outcome": "captured" | "no_fix",
  "verdict": "green" | "yellow" | "red",
  "latitude_deg": 47.6062,
  "longitude_deg": -122.3321,
  "sigma_major_nm": 0.84,
  "sigma_minor_nm": 0.62,
  "orientation_rad": 0.31,
  "n_sights": 4,
  "dominant_source": "horizon",
  "reason": "..."   // populated when capture_outcome = no_fix
}
```

## Why external-files (not internal app-files)

Operators pull entries via `adb pull` / MTP / Files. The
app's internal files dir is private (`run-as` needed; doesn't
work on non-debuggable APKs).

External-files dir (`/sdcard/Android/data/io.github.spencerharmon.bris/files/sessions/...`):

- Visible to MTP and `adb pull` without `run-as`.
- Auto-deleted on app uninstall (clean teardown).
- Writable without scoped-storage MediaStore APIs.
- No runtime storage-permission prompt.

## Per-frame metadata

`FrameSidecar` (`crates/bris-bundle/src/lib.rs:355`) records
per-frame state at the moment of capture:

- `seq`, `captured_unix_ms`, `width`, `height`
- `exposure_us`, `sensor_gain`
- `diagnostic_snapshot` (engine state at frame time)
- **`gravity_camera_frame: Option<[f64; 3]>`** — sensor
  gravity vector. Live `Frame` carried this; without
  recording it, replay silently falls back to image-down
  and miscomputes artificial-horizon reflection pairs.
- **`gps_truth: Option<GpsTruth>`** — per-frame truth
  stamp (debug feature). Captures running hours at speed
  need per-frame truth; one-bundle truth was implicitly
  wrong for any moving capture. `None` when no GPS truth
  source is available.

GPS truth is **never** substituted for AP at engine time.
Replay scoring is the only consumer.

## Sight log review screens

`SightLogScreen` lists every `sights/<sight-ulid>/` under
every capture under every session, sorted oldest-first.
Each row: captured-at timestamp, verdict band, one-line
summary (σ + sight count, or no-fix reason). Soft-deleted
entries (under `.trash/`) hidden.

`SightLogDetailScreen` is per-entry: spike-grade text dump
of the manifest + media listing (capped at 50). Two delete
affordances:

- **Delete images only** — drops PGM frames; manifest,
  per-frame sidecars, pbris.log remain.
- **Delete entry** — soft-delete; moves dir under
  `<external-files>/sessions/.trash/`. Future cleanup
  sweep removes past the configured retention window.

Frame thumbnails, map preview with uncertainty ellipse,
and a debug-mode-only "Send to collector" affordance are
follow-ups.

## What's deliberately not here

- **Multiple concurrent captures.** One capture at a time.
- **Pause / resume of a capture.** A capture is one
  Start/Stop window. Backgrounding aborts under
  `DisposableEffect` cleanup. The eventual Phase 7
  production UX adds a foreground service.
- **Engine state reset between captures.** Engine keeps
  its sight window across Start/Stop boundaries. A new
  capture sees fixes including sights from frames pushed
  before Start — those *did* inform where the body is.
  Cross-capture fusion within a session is exactly what
  the session model is for.
- **Bandwidth between engine and recorder.** Recorder
  consumes via the existing `Engine.fixes` Flow over the
  FFI's `subscribe_fixes` callback. Runs in the engine's
  coroutine scope.

## Code rename map

The Kotlin code today uses "session" for what this doc now
calls "capture". The rename:

| Old | New |
|---|---|
| `SessionRecorder.kt` | `CaptureRecorder.kt` |
| `SessionRecorder.sessionId` | `CaptureRecorder.captureId` |
| `SightLog::sessionId` | `SightLog::captureId` |
| `SessionThresholds` (proposed name) | `CaptureThresholds` |
| `<external-files>/sights/<id>/` | `<external-files>/sessions/<uuid>/captures/<cap-id>/sights/<sight-ulid>/` |

Calibration uses of "session" (`bris-calibrate`,
`vision-calibration::CalibrationSession`) are a distinct
scope and stay. `IntrinsicsSource::UserCalibration::session_id`
renames to `calibration_id` to disambiguate.
