# Debug bundle schema (v1)

A **debug bundle** is the self-describing on-disk artifact that
captures everything needed to re-run a Bris fix attempt offline:
device identity, capture metadata, camera intrinsics, assumed
position, optional GPS ground-truth, atmosphere hint, frame
payload + per-frame sidecars.

The canonical Rust types live in
[`crates/bris-bundle/src/lib.rs`](../../crates/bris-bundle/src/lib.rs).
This document explains the schema's intent; the source is
authoritative for field names and types.

## Layout

```
<bundle-root>/
  bundle.json                 # BundleManifest
  frames/NNNNNNNN.pgm         # frame payload (16-bit PGM)
  frames/NNNNNNNN.json        # FrameSidecar
  index.jsonl                 # optional sidecar (see below)
```

The legacy `bris-exports/...` captures used `media/` instead of
`frames/`. `bris_bundle::enumerate_frames` handles both layouts.
Frames are always sorted by the sidecar's `captured_unix_ms`,
never by filename, so out-of-order writes still replay
correctly.

### `index.jsonl` (optional)

When the on-device `DebugCaptureBuffer` persists frames, it
appends one JSON object per line to `index.jsonl` next to the
`frames/` directory. The file is a **convenience catalog** for
fast directory scans (no need to stat + parse N JSON sidecars
to know what's on disk) and is **not required for replay** —
replay enumerates the JSON sidecars directly.

Each line is an `IndexEntry`:

| field              | type   | source/meaning                                  |
|--------------------|--------|-------------------------------------------------|
| `seq`              | u32    | matches `FrameSidecar::seq` for this frame      |
| `captured_unix_ms` | i64    | matches `FrameSidecar::captured_unix_ms`        |
| `width`            | u32    | frame width in pixels                           |
| `height`           | u32    | frame height in pixels                          |
| `pgm_bytes`        | u64    | size of the PGM file on disk, in bytes          |
| `json_bytes`       | u64    | size of the JSON sidecar on disk, in bytes      |
| `retention`        | string | `"debug"`, `"fix_frame"`, ... (writer-defined)  |

`pgm_bytes`, `json_bytes`, and `retention` are not duplicated
in the per-frame sidecar; tooling that needs them (corpus-size
reports, retention sweeps) reads `index.jsonl`. Tooling that
only needs frame timestamps / dimensions reads the sidecars.

Load with `bris_bundle::load_index_jsonl(bundle_dir)`, which
returns `Ok(None)` when the file is absent and `Ok(Some(vec))`
when present. Additive within `schema_version: 1` — new
fields are appended and old readers ignore them.

## Three independent axes: AP, GPS-truth, derivation

The schema is deliberate about distinguishing three concepts
that the previous code path conflated:

1. **`ap_input`** — the *assumed position* the on-device engine
   was running against. Saint-Hilaire intercepts are referenced
   here. `None` means the session ran cold-start.
2. **`gps_truth`** — a *ground-truth* location, optionally
   captured out-of-band. Used only by replay scoring (`bris
   replay --ap-seed-truth` etc.) to score the celestial fix
   against a known answer. **Never silently substituted for a
   missing `ap_input`** — that conflation was rejected
   explicitly during design.
3. **`ap_derivation_trace`** — provenance of how the AP was
   decided. Loose by design (free-text `method`, optional
   stale-prior-age / prior-fix-σ / CoP-intersection-ref). Evolves
   as the engine grows more AP sources.

## Rotation provenance: option B

`capture.source_rotation_deg` is the rotation the replay path
applies to PGM bytes before feeding the engine. For legacy
sensor-native bundles this is non-zero (typically 90); for
newer captures that write gravity-up frames directly it is 0.
`capture.pre_rotation_was_deg` records what the on-device
pipeline applied *before* writing, for audit. Together they
let an operator answer "what orientation are the bytes on disk
in?" without ambiguity.

The `first_frame_blake3` checksum is over the raw PGM file
bytes — *not* over a post-rotation buffer. When
`source_rotation_deg != 0` the PGM is sensor-native and the
rotation has not yet been applied. This is documented in the
`CaptureInfo` field doc and enforced by `verify_first_frame_
checksum`.

## Distortion enum: three variants reserved from day one

`Distortion` is `BrownConrady | FisheyeEquidistant | None`,
even though only Brown-Conrady is non-trivially used today.
Reserving the variants up front means adding fisheye captures
later doesn't require a schema bump (and so doesn't churn the
collector / Android writer / replay reader in lockstep). The
replay path currently warns when a bundle declares
`FisheyeEquidistant` and falls back to pinhole (TODO: extend
`bris_vision::Intrinsics` to carry fisheye coefficients).

## Forward-compat plan

Within `schema_version: 1`:

- **Add** optional fields freely; older readers ignore them
  (serde's default `#[serde(default, skip_serializing_if =
  "Option::is_none")]` pattern).
- **Add** enum variants in the open-ended `ApProvenance::Other
  { detail }` slot — operators can record new provenance kinds
  without a schema bump.
- **Do not** rename fields, remove fields, or change semantics.

Breaking changes bump `schema_version` to 2 and the loader
rejects mismatched versions with `BundleError::UnsupportedSchema`.

## Per-frame sidecar

`FrameSidecar` deserializes the existing on-device schema
(seq, captured_unix_ms, width, height, diagnostic_snapshot) and
adds two optional fields used by `bris-cli replay`:

- `exposure_us` — exposure in microseconds (V4L2 / CameraX
  reports it post-capture).
- `sensor_gain` — multiplier; passed into
  `bris_core::SensorGain::new`.

Both fall back to manifest defaults (exposure 1000 µs, gain 1.0)
via `FrameSidecar::exposure_us_or` /
`FrameSidecar::sensor_gain_or` so old bundles still load.

## Android-side writer

The Android `bris-android/` app writes `bundle.json` and the
extended `FrameSidecar` (with `exposure_us` and `sensor_gain`)
as part of the canonical "Save buffer" flow. Implemented via:

1. `bris-ffi` exposes `write_bundle_manifest(dir,
   manifest_json)` and `blake3_hex(bytes)`. Manifest JSON is
   round-tripped through `serde_json` against
   `bris_bundle::BundleManifest` so Kotlin schema drift fails
   at save time.
2. `DebugCaptureBuffer` records the first-frame BLAKE3, session
   start/end Unix-ms, and capture resolution to
   `.bundle-meta.json`. Sidecar JSON carries optional
   `exposure_us` / `sensor_gain`.
3. `DebugBundleWriter` composes the manifest from the live
   `CalibrationSource` (operator / factory / placeholder),
   the operator-entered observer (currently a placeholder —
   the operator-entered AP UI is the outstanding follow-up),
   and an optional `CoarseLocation` ground-truth GPS fix that
   is **never** substituted for `ap_input`.
4. `DebugBufferActions.saveAll` takes a
   `prepareManifest(bundleDir, bundleId)` hook invoked before
   the export zip is enumerated, so the saved archive carries
   `bundle.json` at its root.

Outstanding deferred work:

- Replace the placeholder observer in `LiveScreen` with the
  operator-entered AP once that UI lands; thread the same
  value into both the `EngineConfig` and the manifest so
  `ap_input` stays honest about what the engine ran against.

Resolved:

- `CalibrationSource::Operator` now carries the real
  calibration session UUID. New calibrations recorded via
  `CalibrationStore.newSession` stamp a `UUIDv4` into
  `calibration.json` and `latestCalibrationIdFor` returns it
  verbatim; `IntrinsicsSource::UserCalibration::session_id`
  in the bundle therefore reflects the session that
  produced the intrinsics. Pre-#58 on-disk calibrations
  that have no recorded UUID surface as `"legacy:WxH"` so
  consumers can tell a legitimately untraceable
  calibration apart from a real UUID and from the
  synthesised `operator-WxH` placeholder earlier builds
  shipped. The marker is migration-only and falls out of
  the corpus once those calibrations are re-run.
