# Training Corpus On-Ramp

Export retained collector frames for Phase-2 segmentation annotation:

```sh
bris corpus training-index --data-root ./collector-data --output ./training.json
```

Use a read-only snapshot of the collector data root for reproducible results.
The output parent directory must exist and the output file must not exist.
The command validates all selected submissions before atomically publishing a
new JSON file. It does not open SQLite, modify submissions, copy pixels, run
inference, or contact a network service.

## Selection

Discovery walks `submissions/`, excluding `.staging` and directories carrying
`deleted_at.txt`. Submission manifests without a `bundle_manifest` media item
(for example, log-only fixes) and calibration-only bundles without capture frames
are not training captures. Other bundles with no declared capture frames fail
rather than quietly producing an incomplete capture.
An empty submissions directory produces an empty `captures` array; a missing
data root or submissions directory is an error.

The collector routes ordinary capture bundles to `<submission>/media/` and
calibration-kind bundles to `<submission>/calibration/`. The exporter follows
the manifest's roles and collector filename sanitization in both layouts.
Only `capture_frame`, `fix_frame`, and `debug_frame` media are training frames;
attached calibration checkerboards are not accidentally treated as capture
frames. Every frame must have a declared `frame_sidecar` at the matching stem.
Symlinks in the scanned hierarchy or selected media are rejected.

All declared media sizes and any declared SHA-256 values are checked. The bundle
schema and recorded first-frame BLAKE3 are checked, as are unique frame sequence
numbers, frame-index agreement, supported rotation, and decoded image dimensions
against sidecars. Missing or corrupt input fails the export; it is not silently
skipped. Older submissions without SHA-256 still get a computed image digest in
the index. The capture's original `frame_count` is preserved, not used as the
retained-frame count: a field submission can contain only a retained subset.

## Index Contract

The top-level `format` is `bris-training-corpus`, `schema_version` is `1`, and
`path_base` is `collector-data-root`. All paths are relative to the data root
passed to the command, **not** to the JSON file. Keep that root available to the
annotation/training loader. Output is deterministic for unchanged storage:
captures sort by submission path and frames by `(captured_unix_ms, seq)`.

Each `captures` entry contains:

- `submission_id`, `submission_kind`, `submitted_at`, and `submission_manifest`:
  provenance back to the collector's original submission, without copying its
  free-text note or device UUID into a separate index field.
- `bundle_manifest` and `bundle_metadata`: source path and the complete original
  JSON object, including bundle/session identity, device, calibration, capture
  rotation, AP, and GPS truth when recorded. Unknown additive fields survive.
- `annotation_status: "unlabeled"`: this exporter supplies annotation candidates,
  not segmentation masks or a claim that captures were reviewed.
- `frames`: each entry has `image`, `sidecar`, computed `checksum_sha256`, decoded
  `width`, `height`, `pixel_format` (`gray8` or `gray16`), and the complete sidecar
  JSON in `metadata`. This preserves timestamps, exposure, gain, gravity,
  retention, per-frame GPS truth, and diagnostic snapshots when present. Absent
  sensor measurements remain absent; no defaults are invented.

Image coordinates are the **stored, unrotated PGM pixel grid**. The declared
clockwise `bundle_metadata.capture.source_rotation_deg` tells a downstream
loader how to rotate pixels and annotations together if it wants upright input.
No RGB channels are recovered from grayscale captures; a training recipe must
explicitly account for the source bit depth and grayscale input.

## Promotion And Annotation

This complements [regression promotion](corpus_promotion.md). For a collector
submission, point `bris corpus promote --capture` at its `media/` (or
`calibration/`) bundle directory. Promotion emits a one-frame regression case;
this index references **all retained source capture frames**, without duplicating
the promoted PNG or interpreting regression detector expectations as masks.
There is no collector promotion/annotation flag to filter on. Index entries are
therefore candidates even if the same capture was previously promoted.

Before supervised training, review selected scenes and attach human-reviewed
segmentation masks in the downstream annotation dataset. Keep model predictions
and diagnostic horizon estimates separate from labels. Group train/validation
splits by session (or bundle when no session exists), not adjacent frames; repeated
submissions can refer to the same bundle and should stay in the same split.

The index is not anonymized: original bundle and sidecar metadata can contain
GPS, notes, and other sensitive field data. Apply the same access and retention
policy as to raw submissions. Regenerate after deletion and discard prior
indexes/derived datasets as required; an index is a snapshot, not a live deletion
subscription. No deployment or model training is performed by this command.
