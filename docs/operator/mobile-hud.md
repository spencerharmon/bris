# Mobile HUD chrome

The Android live screen (`LiveScreen`) renders, on top of the
camera preview:

## Confidence ellipse (top-right, 120 dp square)

A compass-rose-aligned (north-up, east-right) overlay showing the
current fix's 1σ covariance ellipse:

- Frame: square box with a `N` label at the top and an
  auto-selected `1 nm` / `10 nm` / `100 nm` / `1000 nm` scale
  along the bottom (smallest power of ten that comfortably
  fits the current σ_major).
- Ellipse semi-axes scaled by `sigma_major_nm` /
  `sigma_minor_nm`, rotated by `orientation_rad` (clockwise
  from north of the semi-major axis). The major/minor pair is
  canonicalised before drawing — if the engine reports
  `sigma_minor > sigma_major`, the axes are swapped and the
  orientation rotated 90°.
- Centre dot marks the fix point.
- Faint blue lines through the centre are the contributing
  sights' lines of position (perpendicular to each sight's
  azimuth) — the classic "cocked-hat" intersection picture.
- When the displayed fix is the *recovered* fix (no live fix
  yet this session), the ellipse outline is yellow and a
  yellow `RECOVERED` badge plus the fix's original timestamp
  (`HH:mm:ss z`) appear in the top-right of the chip.

Empty / non-finite / pathological covariances suppress the
overlay rather than drawing nonsense.

## Pool summary chip

One-line chip immediately above the action buttons, e.g.
`Pool: 7 sights (Moon: 3, HR 2491: 2, HR 7001: 2)`. Updates
on every published fix from the in-memory sight pool
(`engine.poolSights()`).

## Recovered-fix banner

When the screen opens, the engine's `lastPersistedFix()` is
consulted (off the Main thread). If present, a blue banner reads
"Recovered fix from previous session" with the lat/lon/σ and
the fix's original timestamp, and fades after 10 s. The
confidence-ellipse overlay shows the recovered fix in the
yellow `RECOVERED` style until the engine publishes its first
live fix this session, at which point the recovered state is
cleared and the overlay reverts to the normal green ellipse.

## Provenance badge

A small chip below the action buttons labels the displayed
fix's solver provenance: `Saint-Hilaire` (green) for the
standard intercept-method fix, `Cold start` (orange) for the
cold-start CoP fallback, or `Cold start (ambiguous)` (orange)
when the cold-start solver returned two candidates and the
coarse-hemisphere hint picked one.

## Sight log screen

Now split into two sections:

- **Recent sights (N)** — the most recent 200 sights from the
  on-disk store (`engine.recentSights(200)`). One line per
  sight: time (HH:MM:SS local), body label, intercept (nm), 1σ
  altitude (arcsec), and `live`/`disk` provenance.
- **Saved captures (N)** — the existing list of operator-
  initiated session captures under
  `<external-files>/sights/<ulid>/`.

## Settings — coarse-hemisphere hint

Settings has a `Coarse hemisphere hint` radio group
(`Unset` / `North` / `South`). The choice persists locally and
is applied to the next streaming-engine startup as
`FfiEngineConfig.cold_start_coarse_hemisphere`, which the
engine forwards to `ColdStartEngineConfig::coarse_hemisphere`
for cold-start CoP disambiguation. Changing the setting
mid-session does not retro-apply to the in-flight engine.
