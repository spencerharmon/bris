# Why a Position Prior Is Needed for Initial Sight Reduction

Status: design explainer. Documents the geometric reason
sight reduction historically requires an assumed position
(AP) / dead reckoning (DR) prior, what would change if we
didn't have one, and the cold-start strategies bris uses
when no prior is available.

## The short version

Sight reduction with a sextant — including bris's Phase 4
single-LOP path — produces a **circle of position**, not a
point. A single body altitude defines the set of all
locations on Earth where that body could appear at that
altitude at that instant. That set is a circle on the
Earth's surface centered on the body's geographic position
(GP), with radius equal to the zenith distance.

A circle of position is too large to plot directly. The
zenith distance for a body at altitude 30° is 60° × 60 nm/°
= **3600 nautical miles** in radius. The entire chart
table at sea is ~24 inches wide; a circle of 3600 nm radius
in any practical projection covers most of an ocean.

The line-of-position (LOP) trick — the Marc Saint-Hilaire
intercept method, 1875 — *linearizes* this circle into a
short straight line by working **locally around an assumed
position**:

1. Pick an assumed position (AP) within ~50 nm of the true
   position. Operator's DR estimate or a previous fix is
   ideal.
2. Compute Hc — the altitude the body *would* have if the
   observer were at the AP.
3. Compute the intercept ITC = Ho − Hc.
4. From the AP, plot a line in the direction of the body's
   azimuth.
5. Mark a point on that line at distance |ITC| from the AP
   (toward the body if Ho > Hc, away from the body if Ho
   < Hc; 1 arcmin altitude = 1 nautical mile distance).
6. Draw the LOP perpendicular to the azimuth line, through
   that marked point.

That LOP is the *tangent* to the circle of position at
the marked point. It approximates the true circle of
position arbitrarily well within ~50 nm of the AP, and
becomes inaccurate as you move further from the AP.

A second body's LOP, drawn the same way, crosses the first
at the observer's position — modulo measurement error
(the "cocked hat" of three or more LOPs is the visible
intersection error).

## Why the linearization needs a prior

The linearization step (4–6 above) requires the AP. Without
it there is nothing to compute Hc *against*. Hc is a
function of three things:
- Time (known precisely from the camera frame timestamp)
- The body's GP at that time (known from the almanac)
- **The observer's assumed position** (needs to come from
  somewhere)

The intercept ITC is the difference between the body's
altitude at the AP (computable) and the observer's true
altitude (measured). Without an AP there is no Hc, no
intercept, no plottable LOP.

This is the textbook reason. There are deeper alternatives,
covered in the next section.

## Alternatives that don't need a prior

Cold-start sight reduction *can* be done without an AP.
These methods all trade off the simplicity of the
linearization for some other complication:

### Direct circle-of-position intersection

Given two observations, two circles of position. Their
intersections are the candidate fix(es). Most of the time
two great circles on a sphere intersect at two points,
diametrically opposite. With three bodies you can resolve
the ambiguity.

**Why this isn't the usual method**: solving the spherical
intersection algebraically is involved, the numerics are
poorly conditioned when the circles are nearly tangent,
and historically navigators didn't have computers. With a
computer it's straightforward — bris could implement it
and produce a fix from two simultaneous body sights with
no prior at all.

The catch: the streaming engine's existing LOP solver is
the Saint-Hilaire iterative path, which assumes a prior.
A cold-start direct-intersection solver is a separate
implementation. It's a real option for bris — see
"Cold-start strategies" below.

### Hill-climbing in (lat, lon) space

With two or more body sights, define a residual function
`sum((Ho_i − Hc(lat, lon, body_i))²)` and minimize it over
all of Earth's surface. This is a 2D optimization with a
known cost surface; gradient descent from a few candidate
seeds (one per major ocean basin?) finds the global
minimum reliably. Still requires multiple sights but no
single starting point.

### Stellar plate solve

If the scene has ≥ 3 identifiable stars, the
`bris_platesolve` crate matches the peak pattern to the
catalog and determines the camera's celestial pointing
direction. Combined with the camera's known gravity vector
(from IMU or horizon) this directly gives the observer's
latitude and the body's local hour angle, from which
longitude follows.

This is bris's natural cold-start path for stellar scenes
and it's already wired up in Phase 4. **It is unavailable
on the moonlight-pond corpus** because the scene is twilight
with the Moon as the only bright body and no plate-solvable
star pattern.

## Why bris asks for a prior anyway

Even though direct-intersection and stellar-plate-solve are
options, the standard Saint-Hilaire LOP path is the right
default:

1. **It's the same path for cold start, warm start, and
   continuous operation**. The first fix from a stellar
   plate solve produces a position that *is* the prior for
   the next sight. The streaming engine never has to switch
   modes.
2. **It linearizes the uncertainty correctly**. The
   per-sight σ flows cleanly into a per-LOP σ which combines
   into a per-fix covariance ellipse. Direct intersection
   on a sphere produces a covariance with cross terms that
   are messy to interpret on a chart.
3. **Single-body sights are useful**. A circle of position
   that pins your position to one of a few thousand
   nautical miles of arc *is* information if you already
   know your approximate longitude. The LOP is the way that
   information enters the chart.
4. **Multi-sight fixes degenerate gracefully**. As sight
   geometry weakens (azimuths cluster), the covariance
   ellipse elongates rather than failing outright. The
   user sees a long thin ellipse and understands the
   geometry constrains them poorly in one direction.

## Bris's cold-start strategy

The plan in priority order:

### Already works

- **Stellar plate solve** (Phase 4): if the scene has ≥ 3
  catalog stars, plate solving gives an immediate position
  with no prior. This is the primary cold-start path on a
  dark sky.

### Implemented but requires prior

- **Reflection-pair direct sight** (Phase 3.6 Phase 1):
  produces `Ho = θ/2` directly. Combined with a prior, it
  produces an LOP. Without a prior, the single LOP can't
  be plotted but Ho itself is still recorded for later
  cross-checking.

### Planned

- **Multi-body cold-start fix**. Given ≥ 2 sights of
  different bodies with no prior, solve directly for the
  intersection of circles of position. This is a new
  solver path in `bris-nav`; spec lives in `plan.org`
  Phase 4. Useful when stellar plate solve doesn't fire
  (twilight, partial cloud, Moon-dominated scenes).

### Operator escape hatch

- **Coarse manual position entry**. Operator dials in a
  city or lat/lon to the nearest degree. Saint-Hilaire
  iteration converges from that within 2–3 passes. This
  is the cheapest fallback and the bris-android UI
  already supports it.

### Future

- **One-shot single-body cold-start via measured altitude
  band**. Given a measured altitude and a time, the GP
  of the body is known and the circle of position is
  determined. Plotting it as a great-circle arc on a
  world chart shows the operator what the single sight
  pins them to. No "fix", but a meaningful constraint —
  e.g. "you are somewhere on this arc that crosses these
  continents". Useful as a sanity check.

## What the moonlight-pond regression actually does

The `moonlight_pond_lop` test sets up an AP at the
operator-provided coordinates (30.150588°N, 97.844170°W).
That AP is the **assumed position** the Saint-Hilaire
method needs. The test then:

- Computes Hc at that AP from the lunar almanac
  (topocentric, post-#15)
- Measures Ho from the reflection-pair geometry
- Reports the intercept Ho − Hc and the implied LOP

The −8.2 nm intercept means: the observer's true position
is on a line perpendicular to azimuth 258°, displaced 8.2
nm from the AP toward 078°. Combined with a *second*
sight (a different body, or the Moon at a later time when
its azimuth has shifted), the LOP intersection would
collapse the line to a point — a fix.

The corpus only contains one Moon sight from one short
capture window, so the test demonstrates an LOP, not a fix.
Producing a fix from this scene requires either another
sight or a multi-body cold-start solver. Both are on the
roadmap.

## Honest summary

Bris asks for a position prior because the standard sight-
reduction math is the Saint-Hilaire intercept method, which
linearizes around an AP. We could implement cold-start
methods that don't need a prior (direct circle intersection,
hill-climbing), and stellar plate solve already provides one
for star-rich scenes. The current pipeline requires a prior
for single-body cold-start because the alternative solver
isn't written yet.

For an operator on land or with any DR knowledge, a coarse
AP (city, nearest degree) is enough. For a no-prior cold
start in a dark, star-rich sky, plate solve handles it. The
gap — no-prior cold start with a moonlit scene and no
identifiable stars — is real but narrow.
