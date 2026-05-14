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
- [sight_session.md](sight_session.md) — mobile-only sight-capture
  session UX: Start/Stop on top of the continuous engine,
  contributing-frame retrieval, on-device sight log under
  `<external-files>/sights/`.
