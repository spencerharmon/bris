# Replay: bris-exports 2026-05-17 (Austin Sun)

Captured at the Austin location on 2026-05-17 (UTC), 39 frames
of the Sun, on a Cat S62 Pro at the device's native 4032×3024.
The on-device session ran cold-start (no operator-entered AP).
GPS truth was supplied post-hoc by the operator
(30.148766°N, -97.843221°E, σ ≈ 5 m).

Captured 2026-05-28 from `bris replay --bundle … --all-modes`
on a freshly built `release` binary after wiring Stage E
cross-frame sight execution (`bris-vision::panorama_altitude_for_pair`).
Stage E now produces sights — the AP-seeded modes show
`sight_window_depth=26`, `fix_publish_attempts=14` — but the
publication gate rejects every attempt because the body in
this single-target Sun corpus has effectively no azimuth
diversity over the 12 s capture (`spread_deg` ≈ 0.01–0.02).
Honest silence is still the correct outcome; the failure mode
has shifted one stage downstream, from "no sights" to "sights
too co-azimuthal to publish".

## Command

```
bris replay --bundle bris-exports/2026/05/16/debug-0019e335c46cb8a6e7dcf21552e8c --all-modes
```

## Captured output (stderr + final summary on stdout)

```
2026-05-28T06:12:27.753586Z  INFO bris: replay: first-frame BLAKE3 checksum verified
2026-05-28T06:12:27.754051Z  INFO bris: replay: bundle resolved bundle=…/debug-0019e335c46cb8a6e7dcf21552e8c frame_count=39 rotation_deg=90
2026-05-28T06:12:27.754066Z  INFO bris: replay: running mode mode="default"
2026-05-28T06:12:49.630846Z  INFO bris: replay: engine diagnostics mode="default" frames_pushed=39 frames_dropped=0 body_queue_depth=39 horizon_queue_depth=39 sight_window_depth=0 last_classification=Some(Day) fixes_published_total=0 fix_publish_attempts=0 singular_geometry_rejections=0 publication_gate_rejections=0 cold_start_attempts=0 cold_start_published=0 ap_rederive_suppressed_count=0
2026-05-28T06:12:49.721045Z  INFO bris: replay: mode complete mode="default" frames_pushed=39 fixes=0 suppressed=0
2026-05-28T06:12:49.721058Z  INFO bris: replay: no fix published (honest silence; see diagnostics above) mode="default"
2026-05-28T06:12:49.721061Z  INFO bris: replay: running mode mode="ap_seed_truth"
2026-05-28T06:13:06.754989Z  INFO bris_streaming::pipeline::stage_e: fix gated spread_deg=0.0102373935422391 axis_ratio=17999.54373479544 sigma_major_nm=35126.331617638025 effective_sigma_major_nm=35126.331617638025 motion_sigma_nm=0.0 oldest_age_s=5.698983371257782
...(13 further `fix gated` lines per AP-seeded mode; first / last shown)...
2026-05-28T06:13:23.342638Z  INFO bris_streaming::pipeline::stage_e: fix gated spread_deg=0.022509011714640467 axis_ratio=9102.636303025745 sigma_major_nm=11810.403816992683 effective_sigma_major_nm=11810.403816992683 motion_sigma_nm=0.0 oldest_age_s=11.572980880737305
2026-05-28T06:13:29.969881Z  INFO bris: replay: engine diagnostics mode="ap_seed_truth" frames_pushed=39 frames_dropped=0 body_queue_depth=39 horizon_queue_depth=39 sight_window_depth=26 last_classification=Some(Day) fixes_published_total=0 fix_publish_attempts=14 singular_geometry_rejections=0 publication_gate_rejections=14 cold_start_attempts=14 cold_start_published=0 ap_rederive_suppressed_count=0
2026-05-28T06:13:30.058123Z  INFO bris: replay: mode complete mode="ap_seed_truth" frames_pushed=39 fixes=0 suppressed=0
2026-05-28T06:13:30.058136Z  INFO bris: replay: no fix published (honest silence; see diagnostics above) mode="ap_seed_truth"
2026-05-28T06:13:30.058138Z  INFO bris: replay: running mode mode="ap_lock_truth"
2026-05-28T06:14:09.456134Z  INFO bris: replay: engine diagnostics mode="ap_lock_truth" frames_pushed=39 frames_dropped=0 body_queue_depth=39 horizon_queue_depth=39 sight_window_depth=26 last_classification=Some(Day) fixes_published_total=0 fix_publish_attempts=14 singular_geometry_rejections=0 publication_gate_rejections=14 cold_start_attempts=0 cold_start_published=0 ap_rederive_suppressed_count=14
2026-05-28T06:14:09.491051Z  INFO bris: replay: mode complete mode="ap_lock_truth" frames_pushed=39 fixes=0 suppressed=14
2026-05-28T06:14:09.491059Z  INFO bris: replay: no fix published (honest silence; see diagnostics above) mode="ap_lock_truth"
2026-05-28T06:14:09.491063Z  INFO bris: replay: running mode mode="no_ap"
2026-05-28T06:14:30.917000Z  INFO bris: replay: engine diagnostics mode="no_ap" frames_pushed=39 frames_dropped=0 body_queue_depth=39 horizon_queue_depth=39 sight_window_depth=0 last_classification=Some(Day) fixes_published_total=0 fix_publish_attempts=0 singular_geometry_rejections=0 publication_gate_rejections=0 cold_start_attempts=0 cold_start_published=0 ap_rederive_suppressed_count=0
2026-05-28T06:14:30.954313Z  INFO bris: replay: mode complete mode="no_ap" frames_pushed=39 fixes=0 suppressed=0
2026-05-28T06:14:30.954322Z  INFO bris: replay: no fix published (honest silence; see diagnostics above) mode="no_ap"

================= replay --all-modes summary =================
mode            frames   fixes      ap_lat      ap_lon       err_nm   sig_maj_nm
default             39       0           -           -            -            -
ap_seed_truth       39       0   30.148766  -97.843221            -            -
ap_lock_truth       39       0   30.148766  -97.843221            -            -
no_ap               39       0           -           -            -            -
==============================================================
```

## Pre-fix vs post-fix comparison

The previous run (captured 2026-05-28 05:28 UTC, prior to Stage
E cross-frame execution) showed `sight_window_depth=0`,
`fix_publish_attempts=0`, and `publication_gate_rejections=0`
in every mode — Stage E was *selecting* cross-frame pairs but
the actual `panorama_altitude` call was deferred TODO, so no
sights were ever inserted.

After this change (`bris-vision::panorama_altitude_for_pair`
wired into Stage E's Day arm), the AP-seeded modes (`ap_seed_truth`,
`ap_lock_truth`) now populate the sight window with 26 sights
each and reach `try_publish` 14 times. The Stage E pipeline is
working end-to-end.

`default` and `no_ap` modes still show `sight_window_depth=0`:
without an AP seed the engine cannot compute Sun apparent place
to anchor the LOPs, so Stage E's Day path bails before sight
insertion. That's the *expected* prior-required behaviour of
the current Stage E (cold-start fix is only triggered after
sights are in the window); a Stage-E refactor to bootstrap
without an AP is the next blocker for these modes.

The AP-seeded modes still publish zero fixes, but for a
different and downstream reason: the publication gate rejects
because azimuth spread is ≈ 0.01–0.02° across all 14 attempts.
This corpus is 39 frames of the Sun captured over ~12 seconds;
the Sun moves ~0.05° in azimuth in that interval, far below
the gate's `min_azimuth_spread_rad` of 30° (the gate's purpose:
require LOPs at very different azimuths so the position
intersection is well-conditioned). The σ_major reported
(11 800 – 35 100 nm) is the LSQ telling us the same thing —
two nearly-parallel LOPs do not pin a position. Cold-start
fires 14 times in `ap_seed_truth` but also can't converge
because the two co-azimuth circles barely intersect.

The honest read on this corpus: a single-body, short-duration
Sun capture cannot produce a published Saint-Hilaire fix
regardless of Stage E correctness. To publish from this kind
of corpus we need either: (a) widely-spaced sights of the *same*
Sun (longer capture or repeat captures across an hour to get
azimuth spread), (b) a second body in the same window
(impossible at noon-Sun), or (c) a different fix algorithm
(time-spaced single-body running fix). All three are Phase 4+
work. The cross-frame Stage E execution this change enables is
necessary for those future paths.
