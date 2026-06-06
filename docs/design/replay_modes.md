# Replay modes

`bris-cli replay` supports four AP-handling modes that let the
operator bisect the celestial error budget.

| Mode | AP source | `lock_ap_for_replay` | Use |
|------|-----------|-----------------------|-----|
| **Default** | `manifest.ap_input` (may be `None` → cold-start) | `false` | Reproduce the on-device session. |
| **ApSeedTruth** | `manifest.gps_truth` | `false` | "What happens if we seed the engine with the right answer?" |
| **ApLockTruth** | `manifest.gps_truth` | **`true`** | Hold AP fixed at truth; isolate non-AP error sources. **Diagnostic-only.** |
| **NoAp** | none (observer at 0,0) | `false` | Force the cold-start path; measure cold-start error alone. |

`--all-modes` runs every mode the bundle's data supports and
prints a side-by-side summary at the end. Modes whose
preconditions aren't met are skipped with a warning (e.g.
ApSeedTruth needs `gps_truth`).

## What each mode isolates

- **Default ↔ ApSeedTruth**: the difference is how good the
  AP was. If Default produces tighter fixes than ApSeedTruth
  the engine's prior-fix machinery is doing useful work; if
  ApSeedTruth is much better the operator should record a
  better AP up front (or wait for cold-start to converge).
- **ApSeedTruth ↔ ApLockTruth**: the difference is whether
  the engine *changed* AP across the run. A large divergence
  means the engine's AP feedback loop is moving away from
  truth — usually a sign of a bad sight that should have been
  rejected at the publication gate.
- **NoAp**: pure cold-start. The error here is the cold-start
  solver's own ceiling for this bundle.

## `lock_ap_for_replay` (engine hook)

A diagnostic-only flag on `bris_streaming::EngineConfig`. When
set:

- `position_prior_from_state` returns `None` (no published-fix
  feedback to horizon providers);
- Stage E's cold-start fallback is skipped on `multi_sight_fix`
  failure;
- Stage E's stale-prior trigger (the SH-vs-cold-start race) is
  skipped.

Every suppressed re-derivation increments
`EngineDiagnostics::ap_rederive_suppressed_count`. Production
code must leave the flag `false`; the only legitimate caller
is `bris-cli replay --ap-lock-truth` (and `--all-modes`).
`EngineConfig::new` documents the default.

## Why this is not in production

`lock_ap_for_replay` deliberately defeats the engine's
self-correction. In live operation that would be exactly
wrong: the whole point of the prior-fix / cold-start /
stale-prior triggers is to course-correct a bad AP. The lock
exists only to *bisect* an error budget offline. Forbid in
production code paths; the engine surface advertises this in
the field docstring and `EngineConfig::new`'s doc comment.

## Session-engine vs. per-capture engine lifetimes

`bris-cli replay` builds one engine instance per **session**,
not per **capture**, when invoked with `--session` or
`--all-sessions`. All captures in a session share the same
`StreamingEngine`: the `SightWindow`, cold-start state, and
last-published-fix continuity are preserved across
capture boundaries.

This matches what the APK does in production via
`SessionHolder` (engine constructed when the active session
is acquired; reused across capture start/stop cycles; rebuilt
only when the active session UUID changes). The CLI's prior
behaviour — fresh engine per capture inside `--session` — was
a replay-only bug: it systematically under-produced fixes vs.
what the device would have published live, by wiping the
sight-window and forcing a fresh cold-start each capture.
For an adversarial corpus with degenerate single-azimuth
body geometry, the bug *over*-produced fixes (cold-start
happily committed once per capture; the session-engine
refuses because it sees the degeneracy across all captures).
Either direction is a quiet-corruption-of-evidence pattern
AGENTS.md rule zero exists to prevent.

The `--bundle` path (single capture, no session context)
still builds a fresh engine per invocation. That is correct:
there is no other capture for the engine to maintain
continuity with.

Mode selection in `--session --all-modes` runs **one engine
per mode** for the full session (default → ap_seed_truth →
ap_lock_truth → no_ap). Per-capture mode comparison is not
meaningful when captures share an engine: you can't lock AP
to truth on capture 1 and run default on capture 2 inside
one engine.

AP comes from the **first capture's manifest** when running
at session scope. All captures within a session share AP
semantics by design (the operator sets AP once at session
create; per-capture AP overrides aren't part of the
session-engine contract).

## Publication-gate overrides

Three `bris-cli replay` flags expose the
`PublicationGateConfig` knobs for diagnostic replays:

- `--max-position-sigma-nm <value | inf>` (default 50.0)
- `--min-azimuth-spread-rad <value>` (default 30°)
- `--max-ellipse-axis-ratio <value | inf>` (default 10.0)

Production captures leave these unset. The diagnostic use
case is "where would we be if we accepted this sigma" on
adversarial or low-evidence corpora (single-body /
single-azimuth, indoor-with-no-real-horizon, etc.) where
the gate honestly refuses to publish but the operator wants
to see the underlying LSQ position.

Setting all three to disabling values (`inf` / `0` /
`inf`) recovers pre-gate publish-everything-the-LSQ-accepts
behaviour. The published fix will carry honestly-large
`sigma_major_nm`; downstream consumers must respect the
reported sigma.

## Render artifacts: base PNG + client-side SVG overlay

`--render-frames` writes two things per frame:

1. **One PNG** (`<frame>-render.png`) containing only the
   downsampled base image. Cache-friendly + idempotent: if
   the file already exists the CLI re-derives the geometry
   metadata from the source frame and skips the encode.
   Multi-mode + multi-replay loops therefore pay the per-
   frame PNG cost exactly once across all subsequent runs.
2. **JSON in the per-capture report** carrying everything
   the overlay needs: classification, body centroid
   (`x`, `y`, `sigma_px`, `area_px`), horizon
   (`intercept_px`, `slope`, `sigma_rad`, `provider`,
   `model_id`), Stage E outcomes, and `render_geometry`
   (`source_width`, `source_height`, `canvas_width`,
   `canvas_height`, `scale`).

The corpus explorer (`tools/corpus-explorer/`) renders the
horizon line, centroid marker, and HUD as SVG overlays
layered over the cached PNG, scaled per
`render_geometry`. The overlay redraws on every report load
without touching the PNG. The replay engine writes ~2 KB of
JSON per frame for the overlay metadata; replays that only
need the JSON (re-running with different gate thresholds,
mode selection, or provider subsets) avoid PNG encode
entirely on the second invocation.

## A note on perf

Replay is much faster in release: the streaming engine runs
5–15× faster than dev on this stack. AGENTS.md §"Build-cache
/ disk hygiene" calls out that any per-frame perf assertion
or multi-capture diagnostic should run with `--release`.
The `--render-frames` cache cuts a re-replay loop further by
elapsed PNG encode time on top of that.
