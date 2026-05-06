# Frame scheduling and fix combination strategy

This document captures the design questions raised during the
pipeline-burst session about how the streaming engine (Phase 3.5)
should choose which frames to process, when to drop frames, and
how many sights to combine into a fix before diminishing returns
make further accumulation pointless.

## Problem

Bris is intended to operate continuously, processing camera
frames at 30-60 fps. The full pipeline (segmentation, plate
solving, multi-pass horizon detection, refinement) takes ~100 ms+
per frame on the embedded target. **The processing throughput is
dramatically lower than the capture rate.**

The naive approach — "process every captured frame in order" —
is wrong: while we're processing a low-quality frame, the camera
has already captured several better ones. The right approach
ranks frames by image properties before deciding which to spend
cycles on, then combines results from the best frames into a
fix.

## What "image properties" means

The key constraint the user raised: *image properties*, not
calibration error. Every frame from the same camera shares the
same lens-calibration uncertainty. Frame-to-frame quality
variation comes from:

- **Mean luma in the expected horizon band** (proxy for "is a
  horizon visible?").
- **Saturated-pixel count** (proxy for "is a body visible?" — for
  Sun/Moon — though a textured-water moonlight scene has the body
  saturated *and* a horizon-detection task that's easier on the
  *sea* not the *body*).
- **Per-row std-dev profile shape** (proxy for "does the scene
  have texture our detectors can use?").
- **Per-frame peak count + intensity distribution** (proxy for
  "are there star-like points to plate-solve?").
- **Frame-to-frame motion estimate** (proxy for "is the camera
  steady or sweeping fast?" — fast sweeps mean motion blur in
  long-exposure captures, and stitching alignment degradation).
- **Body and horizon detected in same frame** (no stitching
  required — single-frame fast path) vs. **body and horizon in
  different frames** (stitching needed; alignment residual
  contributes to σ).

These are all *cheap* to compute — single-pass over the pixel
buffer plus a centroid/peak-detection call we already have.

## Proposed: cheap quality scoring + priority queue + batched
processing

```
loop {
    // Capture: producer thread, ring buffer of raw frames.
    capture_thread.push_frame(raw_frame);

    // Cheap classifier + quality scorer: very-fast pass over
    // each frame to compute a quality score *without* running
    // the full pipeline. ~5-10 ms per frame, manageable at
    // 30-60 fps.
    let score = cheap_quality_score(&frame);
    quality_priority_queue.insert(frame, score);

    // Drop low-quality frames once the queue exceeds a window
    // size: we only need the best N frames per fix-publication
    // interval (default 1 second for a 1-Hz fix rate).
    if quality_priority_queue.len() > N {
        quality_priority_queue.pop_lowest();
    }

    // Process top-K frames from the window. K depends on how
    // much CPU budget we have; 1-3 in practice.
    for frame in quality_priority_queue.top_k() {
        let sights = full_pipeline(frame);
        sight_window.add(sights);
    }

    // Publish a fix when sight_window has enough azimuth-
    // diverse sights.
    if sight_window.has_diverse_sights() {
        publish_fix(sight_window.combine());
    }
}
```

This is a "best-effort backpressure" model. Frames are scored
cheaply on every capture; only the best of a recent window get
the expensive pipeline; the worst get dropped without ceremony.

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

### "Multi-body in one frame" vs. "best-frame-of-window" trade-off

The user's concrete question: should the engine accept a
higher-uncertainty horizon if the frame contains 3 bodies?

The math says: **a single 30-arcsec sight gives σ_pos ≈ 0.5 nm
(once you have a second LOP)**, while **three sights at 60
arcsec with 60° azimuth spread give σ_pos ≈ 0.67 nm**. So a
single best-frame sight beats three mediocre-frame sights by a
small margin.

But: **three 50-arcsec sights at 60° gives σ_pos ≈ 0.55 nm**,
comparable to one 30-arcsec sight from the best frame. So
there's a regime where multi-body wins — when the per-sight σ
penalty is less than √N.

**Practical rule for the streaming engine:**

> Prefer the best-σ frame for single-body sights. For
> multi-body frames, accept a per-sight σ penalty of up to √N
> over the best single-body frame's σ; reject frames whose
> per-sight σ is worse than √N × best.

This is essentially saying: don't take a multi-body sight
unless its accuracy contribution beats what you'd get by
processing the best single-body frames in your queue.

### When more isn't better

Beyond the N=5 inflection, adding more sights is wasted CPU
unless those sights have *better* σ than the existing N. The
streaming engine should:

1. Accumulate sights into a rolling window (e.g. 10 minutes).
2. Cap the window at N=10 sights or so; when the window is
   full, replace the worst-σ sight with each new better-σ sight.
3. Publish a fix on every successful new sight, not on a fixed
   schedule — fix freshness matters as much as fix tightness.

## Summary of design recommendations for Phase 3.5

1. **Cheap quality scorer**: 5-10 ms pass that computes
   - mean luma in the lower-2/3 of the frame
   - saturated-pixel count
   - peak count (for night frames)
   - per-row std-dev profile (for textured-night frames)
   - and emits a single scalar score.

2. **Priority queue + drop**: rolling window of recent frames,
   sorted by score, drop the worst when full. Window size set
   so the full pipeline can process the top-K frames within
   the fix-publication interval.

3. **Per-sight σ floor**: only feed sights into the rolling-
   sight window when the per-sight σ is below `best_recent_σ ×
   √N`. Multi-body frames get an automatic √N relaxation.

4. **Sight window cap**: 10 sights, replace worst when full.
   Diminishing returns kick in at N=5 with good azimuth spread;
   beyond N=10 marginal gain is < 10% per sight.

5. **Per-fix σ surface**: Bris already publishes fix σ via
   `$GPGST` and dominant-source via `$PBRIS`. The streaming
   engine should additionally surface the *number of sights*
   contributing to the current fix and the *azimuth spread*
   of those sights — both meaningful operator-facing
   diagnostics for "should I trust this fix?"

## Open questions

- **What's the right cheap-quality-score formula?** Probably
  multiple per-condition variants (one for day, one for night)
  weighted by the day/night classifier's output.

- **Should the quality scorer itself be ML?** A small image-
  classifier that outputs "expected fix σ" could work, but
  classical heuristics over the existing detector outputs are
  almost certainly fast enough and avoid the inference cost.

- **How to handle sight obsolescence?** A sight from 5 minutes
  ago has the same per-sight σ but the observer may have moved.
  The dead-reckoning correction is not Bris's job (we have no
  course/speed input by default), but we should weight older
  sights down within the rolling window. Linear age-weighting
  with a 10-minute time constant is a reasonable starting
  point.

- **Should multi-frame stitching be opportunistic or
  scheduled?** When a body and horizon are in different
  frames within the rolling window, we *could* stitch. Whether
  the resulting σ improves over the best single-frame sight
  in the window is empirically unclear.

These are Phase 3.5 design decisions; this document captures the
reasoning so we can act on it when the streaming engine work
begins.
