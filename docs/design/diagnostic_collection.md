# Diagnostic collection

How operator-initiated diagnostic data flows from a Bris device
to a server-side review surface, so that real captured footage
can drive pipeline improvements and feed the regression-test +
ML-training corpus.

This document is the source of truth for the spike that
introduced `crates/bris-ffi`, `crates/bris-collector`, and
`bris-android/`. Operator-facing UX in the Android app
references it; the AGENTS.md hard rules derive from it.

## Why this exists

`readme.org` and `plan.org` are explicit: Bris does **no
telemetry, no analytics, no automatic network calls.** That rule
is non-negotiable, and the diagnostic-collection subsystem is
its only exception — but only because every byte that leaves the
device does so as the result of an explicit operator action,
made visible in a one-screen pre-upload review.

The motivating use is concrete: `plan.org` Phase 2 ML work
(notably the Bris-specific segmentation model, L345-398) cannot
proceed without a corpus of real shipboard footage. Field
captures contributed by users — with consent — feed that corpus.
The regression-test infrastructure (`tests/regression/*/case.toml`)
already exists; this subsystem is the on-ramp.

## Operator surface

There is **one** UI control: a single boolean **Debug mode**
toggle in app settings. When OFF, the engine runs normally,
publishes fixes, the operator sees the live overlay, and
the app persists the *operator-meaningful* artifacts of
each capture (sight-log manifest, replay-bundle manifest,
$PBRIS log, and the small set of fix-frame pixels that
backed any published fix). When ON, the analyzer
additionally streams every analyzer frame to disk,
marked as Debug-only retention.

Debug mode gates exactly two things:

1. **Per-frame disk writes for non-fix frames.** With
   Debug ON, the `CaptureRecorder` taps every analyzer
   frame during Start→Stop into the capture's `frames/`
   directory with sidecar `retention: "debug"`. With
   Debug OFF the tap is inert.
2. **GPS-truth attachment.** Bundles emitted with Debug
   ON include the device's coarse last-known location as
   `bundle.gps_truth` (when permission is granted).

Fix-frame pixels (the 1–3 frames that contributed to a
published fix) are always written to the same
`captures/<cap-id>/frames/` directory, with sidecar
`retention: "fix_frame"`. This happens regardless of
Debug mode — the operator gets the contributing pixels
for a published fix without having to opt into bulk frame
archival. With Debug ON a fix frame is first written as
`"debug"` (by the analyzer tap) and promoted to
`"fix_frame"` at finalize when its frame ID is in the
published fix's contributing list; no file copy.

There is **no** rolling debug buffer, **no** separate
"Save buffer" button, **no** parallel `bris-exports/`
tree, **no** in-app submission upload. Off-device
transfer is operator-driven via a single Settings
**Share sessions** action that SAF-zips the entire
`<external-files>/sessions/` tree (or `adb pull` directly).
The collector service (`crates/bris-collector`) remains
in the repo for future use but the Android-side
`Submitter` and submission-review UI are not part of
this surface.

## On-device storage

One tree:

```
<external-files>/sessions/<UUID>/
  session.json                 # operator-edited; SessionManifest
  engine-store/
    sights/current.log         # bris_streaming::SightStore
    fixes/current.log          # 96-byte binary records,
                               # session-scoped via path,
                               # crate format unchanged
  captures/<cap-id>/
    manifest.json              # always-on; sight-log entry
                               #   for SightLogScreen review
    bundle.json                # always-on; replay manifest
                               #   (bris_bundle::BundleManifest)
                               #   bris replay --bundle consumes
    pbris.log                  # always-on; $PBRIS narrative
    index.jsonl                # Debug ON only; frame catalog
    frames/
      NNNNNNNN.pgm             # frame pixels (P5 grayscale)
      NNNNNNNN.json            # sidecar with retention class

<external-files>/calibration/<calibration-UUID>/
  calibration.json
  frames/                      # checkerboard inputs
```

Frame retention classes (sidecar `retention` field):

- `"fix_frame"` — contributed to a published fix. Kept
  through any future debug-data purge.
- `"debug"` — captured because Debug mode was ON at write
  time. Eligible for future purge (deletion semantics
  deferred to keep this refactor scoped).

Disk cost:
- Debug OFF capture: KB for manifests + a few PGMs (only
  the fix-frames).
- Debug ON capture: ~4 MB × fps × duration (every frame
  persists). A 30-second capture at 4032×3024 / 30 fps
  is ~3.6 GB. Operator-aware: enable Debug only when you
  intend to share or analyze.

### Engine persistence is per-session

`bris_streaming::SightStore` (96-byte binary log,
append-only, rotated hourly + at 8 MiB, 7-day retention)
is instantiated once per active session at
`<external-files>/sessions/<UUID>/engine-store/`. The
store's record format is session-blind; session-awareness
lives purely in the path the caller picks. Sights from
session A never bleed into session B's hydrated pool on
restart because they are different files.

The Android `SessionHolder` rebuilds the engine instance
when the active session changes — dropping the old
`Arc<Engine>` and constructing a new one against the new
`data_root`. On Linux, `bris replay --session <UUID>` and
the future `bris capture --session <UUID>` derive
`data_root` the same way from
`<corpus>/sessions/<UUID>/engine-store/`. The 96-byte
record schema (`bris_streaming::store`) is unchanged by
this arrangement; per-session isolation is purely a
filesystem property.

No-active-session fallback (orphan capture): the store
opens at `<external-files>/sessions/orphan/engine-store/`.
When the operator subsequently creates and selects a real
session, the orphan engine is dropped; orphan sights stay
on disk for inspection but no longer hydrate the live
engine pool.

## CameraX backpressure

CameraX's `ImageAnalysis` use case has its own backpressure
model. The Android app uses **`STRATEGY_KEEP_ONLY_LATEST`**: if
the analyzer is busy when a new frame arrives, the previous
unanalyzed frame is dropped at the CameraX layer before reaching
the engine. This is the right choice for a streaming engine
that already drops frames internally based on σ — adding a
second drop layer is fine, but the two layers should not fight.

Specifically:

- CameraX hands the analyzer an `ImageProxy` with a luminance
  plane.
- The Kotlin side **copies** the luminance plane into a
  `ByteArray` and calls `Engine.pushFrame(...)` over UniFFI.
  The copy lets CameraX release the `ImageProxy` immediately.
- The UniFFI `bytes` semantic is owned-by-value: the Rust core
  receives its own copy. The engine's input ring buffer holds
  the Rust-side copy; if the ring is full, `push_frame` drops
  silently per its existing contract.

The per-frame copy is the cost of decoupling the two
backpressure systems. On a Pi-class device it would matter; on
a phone running a single Bris session it does not.

## The `DiagnosticSnapshot` contract

The FFI exposes `Engine.snapshot()` returning an FFI-friendly
re-shape of `bris_streaming::EngineDiagnostics`. The shape is:

- Per-engine counts: frames pushed, frames dropped.
- Per-stage counts: a list of `StageStats { name, entered,
  produced, failed, skipped }`. The name is the stable string
  label (`"classifier"`, `"body"`, `"horizon"`, `"plate-solve"`,
  `"sight-assembly"`); the fixed-array shape of the Rust-side
  type is converted to a `Vec` because UniFFI prefers it.
- Queue depths: body queue, horizon queue, ring buffer, sight
  window.
- Last classification: `Option<String>` rendering of
  `bris_vision::Condition`.
- Last processed frame TT (`Option<f64>`, seconds since J2000).
- Last published fix TT (same shape).

Adding a field to `EngineDiagnostics` requires a matching FFI
addition (a new optional field in `DiagnosticSnapshot`) and is a
semver-minor change in the FFI. Renaming or removing is
semver-major. Track this when updating either side.

## Submission wire format

**Status (as of the storage-paths refactor):** the in-app
upload path was removed. There is no Android `Submitter`, no
submission-review screen, no per-submission HTTPS POST. The
subsystem below describes the **server-side** ingest contract
that will be re-attached when a future PR rebuilds an upload
path — either as a separate `bris-uploader` companion tool
that targets share-capture zips, or as an in-app action built
on top of the canonical capture layout. The collector crate
(`crates/bris-collector`) remains in the workspace.

When reintroduced, submissions will be HTTPS POST
`multipart/form-data` to
`{collector_base}/v1/submissions`. Bearer token in
`Authorization: Bearer <token>`. Token comes from a config
field built into the APK (spike-grade; see Security below).

Parts:

- `manifest` — `application/json`, the manifest schema below.
- One `media[i]` part per file (`image/png`, `image/jpeg`,
  `text/plain` for log files, `application/toml` for
  intrinsics, etc.). Each part has a `Content-Disposition` with
  a `filename`; the manifest references these by filename.

Manifest schema (v1):

```json
{
  "schema_version": 1,
  "submission_kind": "fix" | "calibration" | "debug_capture",
  "submitted_at": "2026-05-13T14:22:01Z",
  "device": {
    "uuid": "01HXYZ...",                  // per-install UUID
    "model": "Pixel 7",
    "os": "Android 14 (API 34)"
  },
  "versions": {
    "app": "0.1.3",
    "bris_core": "0.0.1",
    "bris_data": null,
    "submission_schema": 1
  },
  "captured_at": "2026-05-13T14:18:55Z",  // capture start
  "gps": {                                 // null if unavailable
    "lat_deg": 47.6062,
    "lon_deg": -122.3321,
    "horizontal_accuracy_m": 4.8,
    "source": "fused"                      // "gps" | "fused" | "network"
  },
  "note": "user-entered free text, or null",
  "fix": { ... },           // populated when kind = "fix"
  "calibration": { ... },   // populated when kind = "calibration"
  "debug_capture": { ... }, // populated when kind = "debug_capture"
  "media": [
    { "filename": "frame_0001.png", "role": "fix_frame",
      "frame_index": 1, "captured_at": "...", "size_bytes": ... },
    ...
  ]
}
```

The `fix` / `calibration` / `debug_capture` sub-objects hold
the kind-specific payload (e.g. `fix` contains lat/lon, the
covariance, the `$PBRIS,FIX` line, per-sight breakdowns; the
exact field set is the same as what the on-device fix-detail
view shows).

## Server-side filesystem layout

Under `<data-root>/`:

```
submissions/
  2026/05/13/
    01HXYZ...01/
      manifest.json
      media/
        frame_0001.png
        frame_0002.png
        ...
      pbris.log
    01HXYZ...02/
      manifest.json
      calibration/
        intrinsics.toml
        residuals.json
        frame_001.jpg
        ...
      media/
index.sqlite                # list/filter index, rebuildable
collector.log               # operator log; no PII
```

`index.sqlite` mirrors a flat row per submission (id,
submitted_at, captured_at, kind, device_uuid, app_version,
bris_core_version, has_gps, note_present, soft_deleted_at).
Rebuildable from the manifests; treated as a cache.

Soft-delete: setting `soft_deleted_at` in the manifest and the
mirror row hides the submission from the default review UI;
files remain on disk for the retention window (default 30 days,
configurable). Hard-delete after the window is a separate
explicit operator action.

## Security and privacy posture

Spike-grade, documented as such:

- **Shared bearer token** compiled into the APK. Anyone with
  the APK can submit. Acceptable for a closed beta; replace
  with per-device tokens issued on first contact for any wider
  distribution.
- **TLS required** in production. The collector serves HTTPS;
  the APK refuses plaintext.
- **No PII in collector logs.** The collector logs request IDs,
  submission IDs, kinds, sizes, and outcome (accepted /
  rejected with reason). It does **not** log bearer tokens, GPS
  coordinates, device UUIDs (logged hashed-truncated), or note
  contents.
- **GPS is opt-in via debug mode itself.** When debug mode is
  off, the app does not request location permission. When debug
  mode is on, the app requests coarse location only.
- **The pre-upload review** shows the operator the exact bytes
  about to leave the device. No surprises.

## Corpus promotion (next step, not in spike)

The intended next step beyond this spike is one-click promotion
of a submission to a regression case: the review UI selects a
submission, the operator picks a slug (e.g.
`sailing_sun_upper_left_hazy_2026_05`) and a case kind
(`working` / `expected_failure` / `expected_low_confidence`), and
the server emits a `case.toml` skeleton with the submission's
frames staged into `crates/bris-vision/tests/regression/<slug>/`.
The operator reviews the generated TOML, fills in the assertions,
and commits.

This is sketched here because the manifest schema is what the
emitter consumes. Anything that should be in a future
regression case must therefore land in the manifest now.
