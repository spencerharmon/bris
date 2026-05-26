# Observer-motion staleness inflation

How a stale sight degrades a fix when the operator may have
moved between captures, and how the publication gate handles
the case without a course/speed input.

This doc is the rationale for
`EngineConfig::publication_gate.assumed_max_speed_kn` and the
σ-inflation arithmetic in
`crates/bris-streaming/src/pipeline/stage_e.rs::try_publish`.

## Why stale sights are a problem

The opportunistic-flow defaults (see
`docs/design/sight_persistence.md` and
`docs/design/circle_of_position.md`) extend the active sight
window to 2 hours so a cold-start CoP solver can intersect two
same-body LOPs separated by 30+ minutes. That's wonderful for
the stationary operator: the geometry diversity comes from the
body moving across the celestial sphere while the assumed
position stays put.

It's *misleading* for the operator who moved. A sight taken at
t = -30 min was reduced at the assumed position **as of t = -30
min**. If the boat then drifted 3 nm before the t = 0 sight,
the old LOP is now offset by 3 nm in the wrong direction;
`multi_sight_fix` happily reports a fix with an honest LSQ
ellipse that doesn't account for the offset.

The position is wrong; the σ is too small. That's exactly the
"honest but misleading" failure mode the publication gate
exists to prevent.

## What we can and can't do without DR

Without a course-and-speed input (GNSS, log, manual DR entry)
the engine has no way to project the old LOP forward. The
honest reply is "you may be anywhere within `max_speed × Δt`
of where you took that sight."

That's a 1σ position uncertainty, not a position correction.
It can be combined in quadrature with the sight's intrinsic
σ, and the inflated σ can be used as a *gate*: refuse to
publish a fix when the assumed motion budget says the
ellipse should be wider than what the LSQ produced.

```
σ_motion(t) = assumed_max_speed_kn × t / 3600     [nm, 1σ]
σ_effective = sqrt(σ_lsq_major² + σ_motion(t_oldest)²)
```

If `σ_effective > max_position_sigma_nm`, gate the fix.

The cold-start solver and `multi_sight_fix` receive the
sights unchanged. Inflation is purely a publish-time gate
concern; the underlying LOPs are still useful inputs to a
future, better-conditioned combination once a fresh sight
arrives.

## Configuration

`EngineConfig::publication_gate.assumed_max_speed_kn` defaults
to 0.0 (stationary). Operators on the move set it to a
plausible worst-case speed for their vessel:

- 0 kn — anchored, moored, ashore.
- 5 kn — leisurely sailing yacht.
- 10 kn — cruising sailboat or trawler.
- 30 kn — fast power vessel.
- > 30 kn — uncommon for celestial workflows; if it fits
  your platform, set it explicitly.

The default of zero matches Bris's "no telemetry, no surprise
behaviour" stance: a fresh install on an unconfigured device
publishes fixes that ignore motion (because no one told it
the vessel moves), and the operator opts in by stating an
upper bound.

## Implementation

```
let motion_sigma_nm = gate.assumed_max_speed_kn * oldest_age_s / 3600.0;
let effective_sigma_major_nm =
    (fix.sigma_major_nm.powi(2) + motion_sigma_nm.powi(2)).sqrt();
if effective_sigma_major_nm > gate.max_position_sigma_nm {
    bump publication_gate_rejections;
    log fix gated;
    return None;
}
```

The arithmetic is at the publish site
(`stage_e::try_publish`) so the sight pool and the LSQ are
unaware. A future DR-aware version replaces the inflation
with a forward projection of each sight to "now," with the
DR covariance accumulating into each sight's intrinsic σ;
that work belongs in `bris-nav` and is out of scope here.

## What this is not

- It is not dead reckoning. It does not move the LOP.
- It is not a per-sight σ inflation that affects the fix
  geometry. It is a gate.
- It is not a substitute for a real GNSS / log feed. When
  those land, this gate continues to apply as a
  belt-and-braces upper bound.
- It is not the same as the `oldest_sight_age_seconds`
  field on `PublishedFix`, which is operator-facing
  diagnostics regardless of whether the gate is configured.

## Cross-references

- `crates/bris-streaming/src/config.rs` —
  `PublicationGateConfig`.
- `crates/bris-streaming/src/pipeline/stage_e.rs` — gate
  arithmetic in `try_publish`.
- `docs/design/sight_persistence.md` — why the sight window
  is 2 hours.
- `docs/design/circle_of_position.md` — why same-body sights
  30 min apart are valuable.
