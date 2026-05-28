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
  index.jsonl                 # optional; not required for replay
```

The legacy `bris-exports/...` captures used `media/` instead of
`frames/`. `bris_bundle::enumerate_frames` handles both layouts.
Frames are always sorted by the sidecar's `captured_unix_ms`,
never by filename, so out-of-order writes still replay
correctly.

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

## TODO: Android-side writer

The Android `bris-android/` app does **not** yet write
`bundle.json` (or this crate's `FrameSidecar` extensions). The
on-device debug-capture path still writes the legacy `media/` /
`frames/` layout with a minimal sidecar. The deferred work is:

1. Add a `bris_bundle::BundleManifest` writer to the Kotlin
   side via UniFFI bindings (or compute the manifest in Rust
   and expose a `write_bundle_manifest` FFI call).
2. Populate `ap_input` with whatever the on-device session
   actually used (operator-entered, prior-fix, cold-start).
3. Populate `intrinsics` from `FactoryCalibration` or the
   user-calibration session.
4. Optionally populate `gps_truth` from Android's `LocationManager`
   when debug mode is on and GPS permission has been granted.
5. Compute and record `first_frame_blake3` as the first frame is
   written.

Once that lands, the manifest synthesis path in
`bris-cli replay --bundle` becomes the only consumer; the
`--frames`-only fallback can stay as a legacy escape hatch for
hand-curated corpora.
