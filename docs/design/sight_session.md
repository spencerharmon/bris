# Sight session (mobile)

How the operator captures a fix on the phone, what gets saved
to the device, and how the engine surface supports that
without needing a session concept of its own.

This is a Phase 7 design doc — the mobile session UX. Read
`docs/design/diagnostic_collection.md` first for the broader
context (debug-mode, collector, debug-capture buffer); this
doc covers only the always-on operator-driven sight capture.

## Why mobile has sessions and the engine doesn't

The streaming engine (`crates/bris-streaming`) is designed for
continuous operation. On the embedded Pi-Zero-2W deployment it
runs forever as a systemd service: frames in, fixes out, no
session boundary anywhere. The engine retains a sight in its
window for as long as the configured age permits, replaces
worst-σ on insertion, and keeps publishing.

The phone is a different deployment. The operator runs the
app, taps **Start capture**, holds the phone toward the
horizon, and taps **Stop** — or the app captures
automatically when a sustained-green fix arrives. That
operator action defines a **session**: a bounded window
during which we want to record what the engine did. The
session is purely a mobile-UI construct on top of the
unchanged engine.

The engine doesn't know it. It just keeps doing what it does.

## Engine surface (`crates/bris-streaming`, `crates/bris-ffi`)

Two affordances are all the engine needs to support the
session model:

1. **Per-fix contributing-frame IDs.** `PublishedFix` carries
   `contributing_frame_ids: Vec<u64>` — the engine-assigned
   IDs of every frame referenced by a sight in the fix's
   active sight window. Each sight contributes one or two
   frames: its body frame and (when different) its horizon
   frame; same-frame fixes contribute one. IDs are
   de-duplicated and ordered by first occurrence in the
   window.

2. **Frame retrieval by ID.** `StreamingEngine::frame_by_id(u64)
   -> Option<Frame>` looks up a frame in the engine's ring
   buffer and clones it across. Returns `None` when the frame
   has been evicted (no record currently in the body or
   horizon queue references it AND no sight in the active
   window references it).

The mobile session-recorder uses these together: when a fix
publishes, iterate its `contributing_frame_ids` and call
`frame_by_id` for each to copy the pixel bytes into a sight-log
entry on disk. **Promptly**, because the engine continues to
process new frames during a session and the contributing
frames will eventually evict as the sight window rolls forward.
The session-recorder copies-on-publish; once the bytes are on
disk under `<external-files>/sights/...`, eviction proceeding
is fine.

## Mobile session lifecycle

Engine lifecycle is independent of session lifecycle. The
streaming engine is constructed once when the live screen
composes and lives until the screen leaves composition.
There is **no per-session engine reset** — the engine has no
notion of session.

```
┌─ LiveScreen composes ─────────────────────────────────────┐
│                                                           │
│  Engine constructed once. Camera Preview always bound.    │
│  Analyzer (and engine processing) only bound during a     │
│  session.                                                 │
│                                                           │
│  ┌─ Idle ────────┐    Start capture    ┌─ Capturing ──┐   │
│  │ Preview only  │  ───────────────→   │ Preview      │   │
│  │ Engine quiet  │                     │ + Analyzer   │   │
│  └───────────────┘                     │ Engine fed   │   │
│                                        │ Recorder     │   │
│                                        │ scoring fixes│   │
│                                        └──────┬───────┘   │
│                                               │           │
│                  ┌──────────────────────┬─────┴────────┐  │
│                  │                      │              │  │
│              sustained-green       operator Stop    timeout │
│              for 3s                                   5min │
│                  │                      │              │  │
│                  ▼                      ▼              ▼  │
│  ┌─ Saving ──────────────────────────────────────────────┐│
│  │ Pull contributing-frame bytes from engine via         ││
│  │ frame_by_id(...). Write sight-log entry under         ││
│  │ <external-files>/sights/<session-ulid>/.              ││
│  │ Unbind ImageAnalysis; return to Idle.                 ││
│  └───────────────────────────────────────────────────────┘│
└───────────────────────────────────────────────────────────┘
```

## Threshold model

Each published fix scores into one of three bands by σ_major:

| Band   | Range                                  | Behavior                                |
|--------|----------------------------------------|-----------------------------------------|
| GREEN  | σ ≤ targetσ (default 1.0 nm)           | Counts toward sustained-green auto-accept |
| YELLOW | targetσ < σ ≤ hardσ (default 5.0 nm)   | Eligible for accept on Stop / timeout    |
| RED    | σ > hardσ (default 5.0 nm)             | Never accepted, even at timeout         |

End conditions, in order of priority:

1. **Sustained green.** Default 3 seconds of consecutive green
   fixes. Recorder finalizes immediately with the best
   (lowest-σ) green fix observed.
2. **Operator Stop.** Recorder accepts the best non-red fix
   ever observed; otherwise records `SessionOutcome::NoFix`
   with reason `"operator stopped"`.
3. **Timeout.** Default 5 minutes. Same accept-best-non-red
   semantics as Stop; otherwise `NoFix` with reason
   `"timeout after Nms"`.

Threshold values + sustained-green duration + timeout live in
`SessionThresholds` (Kotlin) with the plan.org defaults baked
in. An operator-facing settings UI for them is tracked under
Phase 7's session-UX work item.

## On-disk layout

Each captured session produces a directory under
`<external-files>/sights/<session-ulid>/`:

```
<session-ulid>/
  manifest.json                schema-v1, submission_kind = "fix"
  media/
    frame_<frame_id>.pgm       contributing-frame bytes
    frame_<frame_id>.json      per-frame metadata
    pbris.log                  formatted $PBRIS,FIX line(s)
```

`manifest.json` matches `bris_collector::manifest::Manifest`
(schema v1) so the same downstream tooling — review web UI,
regression-case promoter, `bris replay` — can consume both
on-device entries and collector-received submissions
identically. The schema is documented in
`docs/design/diagnostic_collection.md`.

`fix` sub-object format for sight-log entries:

```json
{
  "session_outcome": "captured" | "no_fix",
  "verdict": "green" | "yellow" | "red",
  "latitude_deg": 47.6062,
  "longitude_deg": -122.3321,
  "sigma_major_nm": 0.84,
  "sigma_minor_nm": 0.62,
  "orientation_rad": 0.31,
  "n_sights": 4,
  "dominant_source": "horizon",
  "reason": "..."   // populated when session_outcome = no_fix
}
```

## Why external-files (not internal app-files)

Operators pull entries off the device via `adb pull` /
MTP / the system Files app. The app's *internal* files dir
(`/data/data/io.github.spencerharmon.bris/files/...`) is private — pulling
requires `adb shell run-as io.github.spencerharmon.bris` which doesn't
work for non-debuggable APKs and is awkward even for debug
builds.

The app's **external-files** dir
(`/sdcard/Android/data/io.github.spencerharmon.bris/files/sights/...`) is:

- Visible to MTP and adb pull without `run-as`.
- Auto-deleted on app uninstall (clean teardown).
- Writable without scoped-storage MediaStore APIs.
- Doesn't trigger a runtime storage-permission prompt.

Same justification as the debug-capture export path's
storage location.

## Sight log review screens

`SightLogScreen` lists every entry under
`<external-files>/sights/`, oldest-first by ULID name (=
chronological). Each row shows the captured-at timestamp,
verdict band, and a one-line summary (σ + sight count, or the
no-fix reason). Soft-deleted entries (under `.trash/`) are
hidden.

`SightLogDetailScreen` is the per-entry review: spike-grade
text dump of the manifest contents + a media file listing
(capped at 50 entries). Two delete affordances:

- **Delete images only** — drops all PGM frames in the media
  directory. Manifest, per-frame snapshots, and pbris.log
  remain so the diagnostic record survives. Frees the bulk of
  per-entry storage.
- **Delete entry** — soft-delete: moves the directory under
  `<external-files>/sights/.trash/`. A future cleanup sweep
  removes entries past the configured retention window.

Frame thumbnails, map preview with uncertainty ellipse, and a
debug-mode-only "Send to collector" affordance are all tracked
follow-ups; `adb pull` and workstation tools cover the spike's
review need.

## What's deliberately not here

- **Multiple concurrent sessions.** The phone runs one session
  at a time; the operator does one navigation event at a time.
  No collision-handling code, no per-session engine state to
  partition.

- **Pause / resume.** A session is a single capture window.
  Backgrounding the app aborts the session under the
  `DisposableEffect` cleanup; the operator starts a new one
  on return. The eventual Phase 7 production UX adds a
  foreground service to survive backgrounding; for the
  developer-iteration version, abort-on-background is a
  feature (no half-stuck sessions).

- **Engine session reset.** The engine is happy to keep its
  sight window across the user's Start/Stop boundaries. A
  fresh session sees fixes that include sights from frames
  pushed seconds before the user tapped Start, which is fine
  — those frames *did* contribute to the engine's understanding
  of where the body is. The session-recorder treats anything
  the engine publishes during the session window as in-scope.

- **Bandwidth between engine and recorder.** The recorder
  consumes via the existing `Engine.fixes` Flow (a
  shared-flow over the FFI's `subscribe_fixes` callback).
  The recorder runs in the engine's own coroutine scope; no
  separate thread, no per-fix copy until the contributing-
  frame retrieval at finalize time.
