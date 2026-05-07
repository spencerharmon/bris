# Frame scheduling and fix combination strategy

This document captures the design questions raised about how the
streaming engine (Phase 3.5) should choose which frames to
process, when to drop frames, when to stitch, and how many
sights to combine into a fix before diminishing returns make
further accumulation pointless.

## Problem

Bris is intended to operate continuously, processing camera
frames at 30-60 fps. The full pipeline (segmentation, plate
solving, multi-pass horizon detection, refinement) takes ~100 ms+
per frame on the embedded target. **The processing throughput
is dramatically lower than the capture rate.**

The naive approach — "process every captured frame in order" —
is wrong: while we're processing a low-quality frame, the camera
has already captured several better ones. **But "score frames
up front, then run the full pipeline only on the best" is also
wrong**: it misses the case where a frame is great for one task
(say, body detection) but useless for another (horizon). And it
discards frames that might be valuable as stitching
intermediaries even though they have no body or horizon
themselves.

## What we actually want

The pipeline is a sequence of stages with monotonically
increasing cost. After each stage we know more about the
frame's σ contribution than we did before. **A frame can be
rejected (or further work on it can be cancelled) after each
stage, once its accumulated σ exceeds what we can already get
from a fix in the queue.**

Body detection and horizon detection are independent of each
other within a single frame. They produce *separate* records
that can be paired with records from *other* frames via
stitching to compute a sight observation. So the streaming
engine should:

- Maintain two priority queues — one for high-quality body
  records, one for high-quality horizon records — both keyed
  on σ.
- Stitch lazily, pairing the best body with the best nearby-
  in-time horizon. The decision to stitch is driven by the
  *combined* σ across the pair, not by either side alone.
- Keep raw frames in a ring buffer covering the stitching
  window (e.g. 2 seconds at 30 fps = 60 frames) so that even
  body-less, horizon-less frames remain available as stitching
  intermediaries. Frames are evicted from the ring buffer only
  when they can no longer contribute to any improvement of any
  sight in the active sight window.

## The pipeline as a sequence of staged σ contributions

Each stage has a cost and produces a partial σ. After each
stage we know more about whether further work on this frame is
worthwhile.

```
Stage A — Classifier (~1-5 ms, almost free).
  Produces: day/night/twilight + confidence.
  Reject criterion: condition = Unusable, OR mean luma fully
    saturated, OR no useful structure detectable. Rare.

Stage B — Body detection (~5-20 ms).
  - Day: centroid_saturated_body_in_mask.
  - Night: detect_peaks (then plate solve in stage C).
  - Twilight: try day path first; fall back to night.
  Produces: zero or more body candidates with per-body σ.
  Reject criterion: no bodies AND no peaks above threshold.

Stage C — Horizon detection (cheap variants first).
  - detect_horizon (gradient): ~5 ms.
  - detect_horizon_via_sky_region: ~10 ms.
  - detect_horizon_night (mean-grad): ~10 ms.
  - detect_horizon_night_textured: ~15 ms.
  - detect_horizon_via_segmentation: ~100 ms (last resort).
  Produces: zero or more horizon line candidates with per-line σ.
  Reject criterion: all detectors failed AND we have a horizon
    in the queue with σ better than what segmentation could
    plausibly improve to. Skip segmentation in that case.

Stage D — Plate solve (night only, ~10-50 ms once db is built).
  Only runs if Stage B produced peaks AND the classifier said
  night/twilight. Produces: identified stars + camera attitude.
  Reject criterion: insufficient peaks for a 4-tuple, or
    refinement-residual gate trips.

Stage E — Sight assembly (per body × per horizon × stitch).
  For each body record, find the best-σ horizon record that
  pairs with it (same frame is free; different frames need a
  stitch which adds its alignment-residual σ). Compute the
  combined per-sight altitude σ.
  Reject criterion: combined σ exceeds the worst sight in the
    active sight window.
```

A frame that fails Stage B (no body) can still contribute its
horizon detection (Stage C) to the queue. A frame that fails
Stage C (no horizon) can still contribute its body to be paired
with a different frame's horizon at Stage E. **Both queues are
populated independently; pairing happens at Stage E.**

## Stitching is part of pair selection, not an early stage

The previous draft of this document had stitching happen before
σ assessment, which is backward. Stitching costs ~50-200 ms per
frame pair and contributes its own alignment σ; we don't want
to stitch speculatively.

The right pattern: at Stage E, when pairing a body record with
candidate horizon records, the engine considers:

```
combined_σ(body, horizon) =
    sqrt(body.σ² + horizon.σ² + stitch.σ²)
where stitch.σ = 0 if body.frame == horizon.frame
              = predicted_alignment_σ otherwise
```

`predicted_alignment_σ` is cheap to estimate from the time gap
and the frame-to-frame motion (estimated from horizon-line
shift across recent frames). Frames closer in time stitch
better; the engine only commits to actually running the
expensive stitch on the *one* (body, horizon) pair it's chosen
to produce a sight.

This is essentially the standard pattern: **plan with cheap σ
estimates, commit to the expensive operation only on the chosen
pair.**

## Multi-body fix combination math

For N independent altitude observations of the same observer
position, with per-sight altitude σ_i, the combined position
covariance is:

    Σ_pos = (J^T · W · J)^{-1}

where W = diag(1/σ_1², 1/σ_2², ...) and J is the geometry
matrix (rows = ∂altitude_i/∂lat, ∂altitude_i/∂lon). For
azimuth-diverse sights with similar σ, this approximates to

    σ_pos ≈ σ_alt / sqrt(N · favorable-geometry-factor)

where the geometry factor is 1.0 for ideal 90°-azimuth-spaced
sights, dropping to < 0.1 for nearly co-azimuthal sights.

### Rough table (per-sight σ_alt = 30 arcsec)

| N sights | Azimuth spread | σ_pos (arcsec) | σ_pos (nm) |
|----------|----------------|----------------|------------|
| 1        | n/a            | undefined (1D LOP) | n/a |
| 2        | 90°            | 21             | 0.35       |
| 2        | 30°            | 60             | 1.0        |
| 3        | 60° each       | 17             | 0.28       |
| 3        | 30° clustered  | 50             | 0.83       |
| 5        | well-spread    | 13             | 0.22       |
| 10       | well-spread    | 10             | 0.17       |

**The diminishing-returns inflection is around N=5 with good
azimuth spread.** Beyond that, marginal accuracy improvement
falls below 20% per additional sight. Beyond N=10 the gain is
< 10% per sight unless those sights have substantially better σ
than the existing ones.

### "Multi-body in one frame" vs. "best-frame-of-window"

The math: a single 30 arcsec sight gives σ_pos ≈ 0.5 nm (once
you have a second LOP); three sights at 60 arcsec with 60°
azimuth spread give σ_pos ≈ 0.67 nm. So a single best-frame
sight beats three mediocre-frame sights by a small margin. But
three 50 arcsec sights at 60° gives σ_pos ≈ 0.55 nm,
comparable to one 30 arcsec sight from the best frame.

**Practical rule:**

> Prefer best-σ frames for single-body sights. Accept
> multi-body frames only when their per-sight σ penalty is
> ≤ √N over the best alternative — the regime where multi-body
> actually wins.

This falls out naturally from the staged-pipeline approach: a
multi-body frame produces N body records, each of which can
pair with the best available horizon. If the resulting combined
σ for any of them improves the active sight window, the sight
is accepted; otherwise discarded.

## Ring buffer and frame eviction

The ring buffer holds raw frames so they can be used as
stitching intermediaries. The buffer's lifetime is bounded by
the stitching window (how far apart in time can two frames be
and still produce a usable stitch). Realistic values:

- 30-60 fps capture; 1-2 second stitching window.
- Buffer holds 30-120 raw frames + their intermediate
  detection results (body candidates, horizon candidates).
- Memory: ~few hundred MB at HD resolution; manageable on
  Pi-class hardware.

A frame is **evictable** from the ring buffer when both of:

1. No body or horizon record from this frame is in the active
   queues (i.e. it was rejected at Stages B/C, or its records
   have been replaced by better ones).
2. No frame currently in the body or horizon queue has this
   frame as its closest viable stitching partner.

In practice the first condition catches most evictions; the
second is the safety check that keeps stitching intermediaries
alive.

## Sight window: cap, age-weighting, and operator surface

- **Cap at N=10 sights.** Diminishing returns inflection at
  N=5 with good azimuth spread; beyond N=10 marginal gain is
  < 10% per sight. When the window is full, replace the worst-σ
  sight with each new better-σ sight rather than evicting by age.
- **Age-weight older sights linearly with a 10-minute time
  constant.** A sight from 10 minutes ago has the same per-sight
  σ as a fresh one but the observer may have moved (we have no
  course/speed input by default). Linear weighting reflects this
  without making heroic assumptions.
- **Per-fix surface for the operator** (extending `$PBRIS`):
  - Number of sights contributing.
  - Azimuth spread (max - min azimuth across sights).
  - Age of oldest contributing sight.
  - Dominant per-sight σ source.
  All of these are meaningful "should I trust this fix?"
  diagnostics.

## Summary of design recommendations for Phase 3.5

1. **Staged pipeline with per-stage early rejection.** Each
   stage produces a partial σ; subsequent work on a frame is
   cancelled when accumulated σ exceeds what we already have
   in the sight window.

2. **Two parallel detection queues** — one for body records,
   one for horizon records — both keyed on σ. Body and horizon
   detection are independent within a frame.

3. **Lazy stitching at pair selection.** Estimate the stitching
   σ contribution cheaply; commit to running the expensive
   stitch only on the chosen (body, horizon) pair.

4. **Ring buffer for raw frames** sized to the stitching
   window. Frames are evicted only when no current queue entry
   could need them as a stitching intermediary.

5. **Sight window cap at N=10** with replace-worst on insertion;
   age-weighting with a 10-minute time constant.

6. **Per-fix N + azimuth-spread + oldest-sight-age surface** in
   the existing `$PBRIS` diagnostic stream.

## Open questions

- **Body and horizon parallelism.** Body detection and horizon
  detection on the same frame are independent and could run on
  separate threads. Whether the orchestration overhead is worth
  it on Pi-class hardware needs measurement.

- **Stitching σ prediction model.** The "cheap" stitching σ
  estimate from time-gap-and-motion is a reasonable starting
  point, but it might consistently under- or over-predict. A
  feedback loop where actual post-stitch σ is compared to
  predicted-σ and the model is updated is straightforward; the
  question is whether we ever need that level of polish.

- **What does "Unusable" Stage A do?** The classifier's Unusable
  output should presumably skip the frame entirely; no other
  stages run. Worth confirming this is the only place the
  classifier's verdict gates further work — currently the
  classifier is read-only diagnostic output.

- **Plate solving's database build cost.** The geometric hash
  database build is ~10-30 seconds in release. The streaming
  engine should build it once at startup and never again.
  Whether the build runs at process start (eats 30s of warm-up)
  or lazily on first night frame (sight pipeline blocks for 30s
  the first time it tries to plate-solve) is a UX call.

These are Phase 3.5 design decisions; this document captures the
reasoning so we can act on it when the streaming engine work
begins.

## Implementation skeleton

This section is for the next agent picking up Phase 3.5. It
sketches the crate layout, public API, and how each pipeline
stage maps to existing code.

### New crate

The streaming engine lives in a new `bris-streaming` workspace
crate. It depends on every existing crate (`bris-vision`,
`bris-almanac`, `bris-platesolve`, `bris-nav`, `bris-nmea`,
`bris-core`) but no other workspace crate depends on it — it's
the top of the stack, the orchestration layer that consumers
(CLI, FFI, mobile) call into.

### Public API sketch

```rust
/// Top-level engine. Owns the ring buffer, queues, sight
/// window, and worker thread(s).
pub struct StreamingEngine {
    config: EngineConfig,
    // ... internals
}

pub struct EngineConfig {
    pub observer: bris_almanac::Observer,
    pub stitching_window_seconds: f64,    // default 2.0
    pub sight_window_seconds: f64,        // default 600.0 (10 min)
    pub sight_window_capacity: usize,     // default 10
    pub min_fix_publication_interval_ms: u64,  // default 1000
    pub max_concurrent_pipeline_workers: usize, // default 1
    pub plate_solver_config: PlateSolverInit,
    // ...
}

pub enum PlateSolverInit {
    /// Build the hash db at engine construction. Blocks ~10-30s.
    AtStartup,
    /// Build lazily on first night frame. First night fix
    /// blocks ~10-30s.
    Lazy,
    /// Use a pre-built db (loaded from disk; future).
    Cached(/* path */),
}

impl StreamingEngine {
    pub fn new(config: EngineConfig) -> Self;

    /// Push one captured frame into the engine. Non-blocking;
    /// the engine queues it and processes asynchronously. Drops
    /// the frame silently if the input ring is full
    /// (backpressure).
    pub fn push_frame(&self, frame: bris_vision::Frame);

    /// Subscribe to fix publications. Each fix carries a
    /// position estimate, full covariance, dominant-source
    /// attribution, and the diagnostic fields the $PBRIS
    /// sentence needs.
    pub fn fix_stream(&self) -> impl Stream<Item = PublishedFix>;

    /// Snapshot of current engine state for diagnostics.
    pub fn diagnostics(&self) -> EngineDiagnostics;
}

pub struct PublishedFix {
    pub position: bris_nav::FixPosition,
    pub covariance: [[f64; 2]; 2],
    pub n_sights: usize,
    pub azimuth_spread_rad: f64,
    pub oldest_sight_age_seconds: f64,
    pub dominant_source: PBrisDominantSource,
    pub timestamp: bris_core::time::Tt,
}
```

The CLI's eventual `bris serve` subcommand wraps this engine
plus a frame-capture path (V4L2 / libcamera) plus an NMEA
transport layer.

### Stage-by-stage mapping to existing code

Each stage in the design above maps directly to existing code:

- **Stage A (classifier)**: `bris_vision::condition::classify`.
  The engine adds the almanac call (one per batch / per
  publication interval, not per frame) for sun altitude.

- **Stage B (body detection)**:
  - Day path: `centroid_saturated_body_in_mask`.
  - Night path: `detect_peaks` (then plate solve).
  - For non-saturated bodies (Moon at dusk, planets):
    `centroid_brightest_body_in_mask` with the sky-segmentation
    mask if available.

- **Stage C (horizon detection)** in cheap-first order:
  - `detect_horizon` (gradient).
  - `detect_horizon_via_sky_region`.
  - For night/twilight: `detect_horizon_night` and
    `detect_horizon_night_textured` (try both; keep the better
    σ); `detect_horizon_night_multi_pass` if neither is
    confident.
  - `detect_horizon_via_segmentation` last, because of the
    100ms inference cost.

- **Stage D (plate solve)**: `bris_platesolve::plate_solve`.
  The hash database is constructed once at engine startup (or
  lazily on first call; see `PlateSolverInit`).

- **Stage E (sight assembly)**:
  - Same-frame body+horizon: `bris_vision::measure_altitude`.
  - Cross-frame: `bris_vision::panorama::panorama_altitude`
    or the lower-level stitching primitives in
    `bris_vision::track`.
  - Per-star altitude (night, after plate solve):
    `bris_platesolve::star_altitudes`.

- **Sight reduction + fix**: `bris_nav::sight::line_of_position`
  per sight, then `bris_nav::fix::multi_sight_fix` over the
  rolling window.

- **NMEA emission**: `bris_nmea::standard` for `$GP*` and
  `bris_nmea::pbris` for `$PBRIS,*` diagnostics. The engine's
  per-fix surface (N sights, azimuth spread, oldest age,
  dominant source) drops into the existing `$PBRIS,UNC` and
  related sentences.

### Threading model

Start with **one worker thread** processing the ring buffer. A
single Pi Zero 2W core running the staged pipeline at the
target throughput is the baseline; if that's insufficient, the
two parallel queues (body, horizon) are independent and can be
processed on separate worker threads with the obvious cost in
synchronization complexity. Don't optimize until measurement
demands it.

The capture side runs on its own thread (V4L2 callback, AVF
delegate, etc.) and pushes frames into the engine via
`push_frame`. Backpressure is "drop on full ring."

### Test strategy

The engine is testable in CI without real cameras by feeding
it synthetic frame sequences:

1. **Per-stage unit tests** (already exist in their respective
   crates): each detector's behavior on the regression corpus.
2. **Engine integration tests**: synthetic sequences of
   `bris_vision::Frame` constructed from corpus PNGs, fed
   through `push_frame` at controlled cadences. Assert:
   - The right frames make it past each stage.
   - Eviction logic respects the stitching window.
   - Multi-frame stitching pairs the right body+horizon.
   - The final fix matches an expected position to within σ.
3. **Stress tests**: feed at 60 fps with 100 ms per-frame
   processing budget and assert fix-publication interval
   stays within bounds (i.e. backpressure works correctly).

The regression corpus's `case.toml` schema may need a new
`[fix]` table type that captures expected fix output for
multi-frame sequences. Currently the `[fix]` table exists in
the schema as a stub; the runner is unimplemented. Phase 3.5
implementation should give it a runner and exercise it on
sequences derived from `orig_test_video/` (which has 603
frames of the `sailing_sun_upper_left` scene — a good
multi-frame stress test).

### Concrete first commits

A reasonable order for Phase 3.5 commits:

1. **Skeleton crate + EngineConfig + push_frame + fix_stream
   API surface** (no actual processing). Wire types together,
   verify the shape compiles against the existing crates'
   public APIs.
2. **Stage A + B (single-threaded, no queueing)**: classify,
   detect body, log results. No fix publication yet. This
   surfaces any API mismatches (e.g. does the classifier need
   a sun-altitude wrapper?).
3. **Add Stage C with the cheap-first ordering**.
4. **Add the body/horizon priority queues + ring buffer +
   eviction logic**. Pure data-structure work; no new
   detectors.
5. **Stage E pair selection + sight emission + sight window**.
   First end-to-end fix publishes here.
6. **Plate solving (Stage D) + per-star altitude**.
7. **Hysteresis on the day/night classifier**, with config-
   driven settle time (default ~3 seconds = ~90 frames).
8. **NMEA emission integration** (`$PBRIS,*` extensions for
   N + azimuth-spread + age).
9. **Stress + integration tests**.

The CLI's `bris serve` subcommand and the V4L2 capture path
are post-engine work; Phase 6 territory.
