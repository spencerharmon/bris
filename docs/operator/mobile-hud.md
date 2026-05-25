# Mobile HUD chrome

The Android live screen (`LiveScreen`) renders, on top of the
camera preview:

## Confidence ellipse (top-right, 120 dp square)

A compass-rose-aligned (north-up, east-right) overlay showing the
current fix's 1σ covariance ellipse:

- Frame: square box with a `N` label at the top and either
  `1 nm` or `10 nm` along the bottom (auto-selected at the 1 nm
  σ_major threshold).
- Ellipse semi-axes scaled by `sigma_major_nm` /
  `sigma_minor_nm`, rotated by `orientation_rad` (clockwise
  from north of the semi-major axis).
- Centre dot marks the fix point.
- Faint blue lines through the centre are the contributing
  sights' lines of position (perpendicular to each sight's
  azimuth) — the classic "cocked-hat" intersection picture.

Empty / non-finite / pathological covariances suppress the
overlay rather than drawing nonsense.

## Pool summary chip

One-line chip immediately above the action buttons, e.g.
`Pool: 7 sights (Moon: 3, HR 2491: 2, HR 7001: 2)`. Updates
on every published fix from the in-memory sight pool
(`engine.poolSights()`).

## Recovered-fix banner

When the screen opens, the engine's `lastPersistedFix()` is
consulted. If present, a blue banner reads "Recovered fix from
previous session" with the lat/lon/σ, and fades after 10 s.
The current fix overlay shows the recovered value until a new
fix arrives.

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

Settings now has a `Coarse hemisphere hint` radio group
(`Unset` / `North` / `South`). The choice is persisted locally
but does not yet propagate to the engine; the next engine
update will wire it to `EngineConfig::cold_start.coarse_hemisphere`
for cold-start CoP disambiguation.
