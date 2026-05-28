# Replay: bris-exports 2026-05-17 (Austin Sun)

Captured at the Austin location on 2026-05-17 (UTC), 39 frames
of the Sun, on a Cat S62 Pro at the device's native 4032×3024.
The on-device session ran cold-start (no operator-entered AP).
GPS truth was supplied post-hoc by the operator
(30.148766°N, -97.843221°E, σ ≈ 5 m).

Captured 2026-05-28 from `bris replay --bundle … --all-modes`
on a freshly built `release` binary. Stage E never produced a
publishable fix in any mode: the sight window remained empty
because Stage E sight-assembly was skipped on every frame
(`body_queue_depth=39`, `horizon_queue_depth=39`,
`sight_window_depth=0`). This is the engine telling us it has
the raw detections but they did not pair into a usable sight on
this corpus — a Phase 3+ Stage E improvement, not a replay
issue. Honest silence is the correct outcome.

The four modes still ran the full vision pipeline; the summary
shows that 39 frames pushed cleanly in each, that GPS-truth-
seeded AP modes used the post-hoc GPS coordinates, and that
neither cold-start nor `lock_ap_for_replay` was triggered (the
fix never reached `try_publish`).

## Command

```
bris replay --bundle bris-exports/2026/05/16/debug-0019e335c46cb8a6e7dcf21552e8c --all-modes
```

## Captured output (stderr + final summary on stdout)

```
2026-05-28T05:28:53.235719Z  INFO bris: replay: first-frame BLAKE3 checksum verified
2026-05-28T05:28:53.236205Z  INFO bris: replay: bundle resolved bundle=…/debug-0019e335c46cb8a6e7dcf21552e8c frame_count=39 rotation_deg=90
2026-05-28T05:28:53.236217Z  INFO bris: replay: running mode mode="default"
2026-05-28T05:29:17.755978Z  INFO bris: replay: engine diagnostics mode="default" frames_pushed=39 frames_dropped=0 body_queue_depth=39 horizon_queue_depth=39 sight_window_depth=0 last_classification=Some(Day) fixes_published_total=0 fix_publish_attempts=0 singular_geometry_rejections=0 publication_gate_rejections=0 cold_start_attempts=0 cold_start_published=0 ap_rederive_suppressed_count=0
2026-05-28T05:29:17.873335Z  INFO bris: replay: mode complete mode="default" frames_pushed=39 fixes=0 suppressed=0
2026-05-28T05:29:17.873349Z  INFO bris: replay: no fix published (honest silence; see diagnostics above) mode="default"
2026-05-28T05:29:17.873352Z  INFO bris: replay: running mode mode="ap_seed_truth"
2026-05-28T05:29:39.601802Z  INFO bris: replay: engine diagnostics mode="ap_seed_truth" frames_pushed=39 frames_dropped=0 body_queue_depth=39 horizon_queue_depth=39 sight_window_depth=0 last_classification=Some(Day) fixes_published_total=0 fix_publish_attempts=0 singular_geometry_rejections=0 publication_gate_rejections=0 cold_start_attempts=0 cold_start_published=0 ap_rederive_suppressed_count=0
2026-05-28T05:29:39.641729Z  INFO bris: replay: mode complete mode="ap_seed_truth" frames_pushed=39 fixes=0 suppressed=0
2026-05-28T05:29:39.641738Z  INFO bris: replay: no fix published (honest silence; see diagnostics above) mode="ap_seed_truth"
2026-05-28T05:29:39.641742Z  INFO bris: replay: running mode mode="ap_lock_truth"
2026-05-28T05:30:00.968102Z  INFO bris: replay: engine diagnostics mode="ap_lock_truth" frames_pushed=39 frames_dropped=0 body_queue_depth=39 horizon_queue_depth=39 sight_window_depth=0 last_classification=Some(Day) fixes_published_total=0 fix_publish_attempts=0 singular_geometry_rejections=0 publication_gate_rejections=0 cold_start_attempts=0 cold_start_published=0 ap_rederive_suppressed_count=0
2026-05-28T05:30:01.002770Z  INFO bris: replay: mode complete mode="ap_lock_truth" frames_pushed=39 fixes=0 suppressed=0
2026-05-28T05:30:01.002781Z  INFO bris: replay: no fix published (honest silence; see diagnostics above) mode="ap_lock_truth"
2026-05-28T05:30:01.002784Z  INFO bris: replay: running mode mode="no_ap"
2026-05-28T05:30:22.229545Z  INFO bris: replay: engine diagnostics mode="no_ap" frames_pushed=39 frames_dropped=0 body_queue_depth=39 horizon_queue_depth=39 sight_window_depth=0 last_classification=Some(Day) fixes_published_total=0 fix_publish_attempts=0 singular_geometry_rejections=0 publication_gate_rejections=0 cold_start_attempts=0 cold_start_published=0 ap_rederive_suppressed_count=0
2026-05-28T05:30:22.266263Z  INFO bris: replay: mode complete mode="no_ap" frames_pushed=39 fixes=0 suppressed=0
2026-05-28T05:30:22.266276Z  INFO bris: replay: no fix published (honest silence; see diagnostics above) mode="no_ap"

================= replay --all-modes summary =================
mode            frames   fixes      ap_lat      ap_lon       err_nm   sig_maj_nm
default             39       0           -           -            -            -
ap_seed_truth       39       0   30.148766  -97.843221            -            -
ap_lock_truth       39       0   30.148766  -97.843221            -            -
no_ap               39       0           -           -            -            -
==============================================================
```
