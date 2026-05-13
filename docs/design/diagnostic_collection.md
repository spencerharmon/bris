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
toggle in app settings. When off, no diagnostic-collection UI
appears anywhere in the app. When on, three contextual actions
appear:

- **Debug capture.** A persistent toggle, visible during a live
  session. When enabled, the app retains *every* frame the
  streaming engine processes, plus the engine's per-frame
  diagnostic snapshot and tracing log, in a rolling on-device
  buffer (capped by disk usage, not by time). Without this
  toggle, the on-device retention is the same as normal
  operation: only frames whose body or horizon record landed in
  the published fix are kept.

- **Send fix.** Surfaced from the fix-detail view. Uploads
  exactly the data the device retains for that fix in *normal*
  operation: the frames that contributed body or horizon
  records to the published sight window, plus the `$PBRIS`
  sentence window covering the fix, plus the engine config and
  versions. No additional capture beyond what was already on
  disk.

- **Send calibration.** Surfaced from the calibration screen.
  Uploads the full calibration session: every input frame,
  the detected corners, the per-frame reprojection residuals,
  the persisted intrinsics TOML, and the calibration doctor
  output.

Every send action navigates to a **single-screen pre-upload
review** that lists exactly what is about to leave the device:

- Media items (thumbnail + filename + size).
- Metadata fields (timestamps, GPS if present, app + core +
  data versions, device model, OS version).
- Free-text note (optional, operator-supplied).

Two buttons: **Send** and **Cancel**. There is no per-field
opt-out — debug-mode-on is the consent. Operators who don't
want to send something disable debug mode or cancel the review.

## Versioning in every submission

Every manifest carries four version fields:

- `app_version` — the Android app's version (e.g. `0.1.3`).
- `bris_core_version` — the version string returned by
  `bris_ffi::version()`, which reads it from `bris-core`'s
  `CARGO_PKG_VERSION` at FFI build time.
- `bris_data_version` — the version of the OTA `bris-data`
  payload (almanac coefficients, leap-second table, star catalog,
  segmentation model). `None` if no payload has been applied
  since install.
- `submission_schema_version` — the version of the manifest
  schema itself. Incremented when fields change incompatibly so
  the collector can reject (or accept-with-fallback) old
  clients.

## On-device storage

Normal operation (debug mode off): the streaming engine retains
only the frames whose detection records are currently in the
body or horizon queue, plus the raw-frame ring buffer for the
stitching window (see `frame_scheduling.md`). When a fix
publishes, the frames that contributed are also retained until
the fix ages out of the sight window.

Debug capture (toggle on): the app retains *every* processed
frame, plus the per-frame `DiagnosticSnapshot`, plus the
tracing log, in a rolling on-device buffer under
`<app-files>/debug-capture/`. The buffer is capped by total disk
usage (default 1 GB, configurable in settings); oldest frames
evict first. The capture toggle stops adding new entries when
turned off, but does not delete existing ones — those persist
until the operator either uploads them or explicitly clears the
buffer from settings.

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

HTTPS POST `multipart/form-data` to
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
