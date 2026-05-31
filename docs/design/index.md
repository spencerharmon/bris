# Bris design notes

Long-form rationale for design decisions made in `plan.org`. New
documents go in this directory as design questions are settled.

## Index

- [pipeline.md](pipeline.md) — end-to-end camera-frame to fix flow.
- [frame_scheduling.md](frame_scheduling.md) — streaming engine
  scheduling: queues, eviction, sight window, σ-driven early
  rejection.
- [diagnostic_collection.md](diagnostic_collection.md) — operator-
  initiated diagnostic submission from device to collector; the
  one network surface in Bris.
- [capture.md](capture.md) — per-capture (Start/Stop) UX on top
  of the continuous engine: contributing-frame retrieval,
  per-capture live-fix sight log under
  `sessions/<uuid>/captures/<cap-id>/sights/`. A capture
  belongs to one session (see `testing_strategy.md`).
- [pre_classification_masking.md](pre_classification_masking.md) —
  draft: reorder per-frame pipeline so day/night classification
  consumes the seg sky-mask and a cheap bright-blob mask,
  rather than averaging the raw middle-band.
- [testing_strategy.md](testing_strategy.md) — session / corpus
  layout + cold-start coverage. Companion to Phase 8.5 (build
  provenance) in `plan.org`.
