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
