# Topocentric Parallax Correction

Status: implemented for all Solar System bodies via Meeus
*Astronomical Algorithms*, Ch. 40 (rigorous diurnal-parallax
transform). Lives in `crates/bris-almanac/src/apparent.rs`
inside `topocentric_equatorial`, threaded through
`common_apparent_place` / `finalize_to_horizontal`. The
stellar path (`star_apparent_place`) does not apply this
correction; star parallax is handled separately via the
catalog `parallax_mas` field.

This document explains *why* the correction is required, *how
much* it matters for each class of body, *how* the rigorous
transform works, and *what residuals remain* after applying
it.

## Why it exists

Sight reduction compares two altitudes:

- **Ho** — observed altitude. What the operator measured from
  their position on Earth's surface, with their local horizon
  as reference. This is by construction *topocentric* (Greek:
  *topos*, "place" — the observer's place).
- **Hc** — computed altitude. What the almanac says the body's
  altitude would be at the same instant and observer.

The intercept `Ho − Hc` only carries navigational meaning if
both quantities are in the same reference frame. If Hc is
computed for an observer at Earth's *center* (geocentric)
while Ho is measured from Earth's *surface* (topocentric),
the intercept absorbs the geometric offset between those two
viewpoints — a systematic bias unrelated to position error.

For a body at distance `d` from Earth's center, viewed from a
surface observer offset `R⊕` from the center, the apparent
direction differs by an angle whose maximum value is the
body's **horizontal parallax** (HP):

```
HP = asin(R⊕ / d)
```

At any altitude `h` the parallax shift is approximately

```
Δh ≈ −HP · cos(h)      (first order)
```

The shift is always *downward*: the surface observer sees the
body lower than the geocentric observer would, because the
surface observer is offset toward (or away from) the body by
some component of `R⊕`.

## Magnitudes

| Body | Mean distance | HP at distance | Shift at alt = 20° |
|---|---|---|---|
| Stars | ~∞ (parsecs) | ~0 (mas range) | ~0 |
| Sun | 1.000 AU | 8.794″ | 8.27″ |
| Mars | 0.5–2.5 AU | 4–25″ | 4–24″ |
| Jupiter | 4.2–6.2 AU | 1.5–2.2″ | 1.4–2.1″ |
| **Moon** | **0.00257 AU (385 000 km)** | **~57′** | **~53.5′** |

The Moon's HP is *three orders of magnitude larger* than any
other navigational body's. For Bris's targets of sub-nautical-
mile fixes (sub-arcminute Hc), the Moon **cannot be reduced
without parallax correction**. The Sun, planets, and stars are
all dominated by other error sources at their respective HP
levels (Sun shift is comparable to bris's current 20″
aberration placeholder σ; planet shifts are below the
truncated-ephemeris residual).

## How we surfaced it

The bug had been documented in the source since the original
almanac commit: `apparent.rs` line 145, "the topocentric
correction in this commit is a stub." It went undetected
because:

1. The first end-to-end navigational test against a real-world
   capture happened on the moonlit-pond Austin corpus, the
   first scene exercising a Moon sight.
2. Stars (the historical primary target) and the Sun (when
   tested in isolation) hide the bug: stars produce HP = 0,
   and the Sun's 8″ shift is inside the existing aberration σ
   placeholder.
3. Synthetic regression tests used a single body with
   matching geocentric Hc/Ho, sidestepping the issue.

The moonlight-pond regression test in `bris-streaming/tests/
moonlight_pond_lop.rs` produced an LOP intercept of **−61.1
arcmin / −61.13 nm** with the stub in place; cross-check
against Skyfield/JPL DE421 isolated the discrepancy to Hc, and
the magnitude (53.5 arcmin, matching `HP · cos(alt)` for
Moon at 19.8° altitude) identified the missing transform.

After the fix the same test produces an intercept of **−8.2
arcmin / −8.18 nm** — a 7.5× improvement from a single source.
The remaining residual is attributable to:
- Hand-thresholded operator centroids (not sub-pixel)
- Stubbed annual aberration (~20″, separate plan.org TODO)
- Truncated Meeus 47.A lunar series vs full ELP-2000
  (~arcseconds)
- Per-ray σ assumption of 1 px (vs ~0.3 px for a saturated
  disk centroid)

None of these are reflection-pair-specific or almanac-
specific; each has its own followup.

## The transform

Meeus Ch. 40 gives the diurnal-parallax correction in
equatorial coordinates. Inputs:

- Body's geocentric apparent right ascension `α`, declination
  `δ`, and distance `d` (consistent epoch, typically apparent
  of date)
- Observer's geocentric latitude `φ′` and geocentric distance
  `ρ` (in units of Earth equatorial radius `R⊕`). For our
  current implementation we use the spherical-Earth
  approximation `φ′ = φ` (geodetic) and `ρ = 1`; the
  oblateness correction would shift `φ′` by up to ~11′ and
  `ρ` by ±0.003, both negligible for the Moon at our
  arcminute-scale targets but worth noting.
- Local hour angle of the body `H = LAST − α`

The parallax in right ascension and declination are:

```
sin(π) = R⊕ / d
tan(Δα) = (−ρ cos(φ′) sin(π) sin H) /
          (cos δ − ρ cos(φ′) sin(π) cos H)
sin(δ′) = (sin δ − ρ sin(φ′) sin(π)) · cos(Δα) /
          (cos δ − ρ cos(φ′) sin(π) cos H)
α′ = α + Δα
```

The topocentric (`α′`, `δ′`) replace the geocentric (`α`, `δ`)
before conversion to local horizontal coordinates.

For the Moon at typical distances this gives `sin(π) ≈ 0.0166`
(0.95°), and the resulting altitude shift integrates the
component of the observer offset projected toward the body
— recovering the textbook `HP · cos(h)` first-order behaviour
plus second- and third-order corrections (worth ~30″ at low
altitudes, well inside our targeted accuracy band).

## What stays geocentric

- **Star catalog lookup**. Trigonometric parallax for stars is
  handled at the catalog level (`StarRecord::parallax_mas`)
  during proper-motion propagation — the relevant scale is
  milliarcseconds, not arcminutes, and the geometry differs:
  for stars `R⊕` is too small to matter, but the orbital
  baseline of Earth around the Sun (~30 000 R⊕) is what
  produces the catalog parallax. These are distinct
  corrections; we do not apply diurnal parallax to stars.
- **Sun, planets**. They do receive the diurnal-parallax
  correction (the same code path), but the magnitudes are
  small enough that the correction is invisible against the
  current annual-aberration placeholder σ. Applying it
  uniformly is cleaner than special-casing and costs
  nothing.

## Verification

A regression test (`apparent::tests::
moon_topocentric_matches_skyfield_at_austin`) asserts that
the post-correction Moon altitude at AP `30.150588°N,
97.844170°W` at `2026-05-25T06:29:06.752Z` matches the
Skyfield/JPL DE421 reference value of `18.9431°` within
**1′ altitude / 2′ azimuth**. The actual residual is ~0.6′
in altitude; the looser bound covers future drift from
ephemeris refinement.

End-to-end validation is via the env-var-gated
`moonlight_pond_lop` regression in `bris-streaming/tests/`,
which exercises the full FFI-facing pipeline against the
captured corpus and asserts a 5° intercept bound (currently
hitting 0.14°).

## Followups

These are NOT implemented in this commit; each has a separate
plan.org line:

1. **Oblateness-corrected geocentric latitude** (`φ′`,
   `ρ`). Would tighten the Moon parallax model from
   spherical-Earth to WGS-84-ish. Magnitude: tens of
   arcseconds in altitude near 45° geodetic latitude.
2. **Annual aberration**. ~20″ direction shift from Earth's
   orbital velocity; currently a fixed σ placeholder. Meeus
   Ch. 23.
3. **Light-time correction for planets**. Currently TODO at
   `apparent.rs:155`. ~1–2″ for Mars at opposition; larger
   for Jupiter and beyond. Required only when planets
   become navigational targets.
4. **Full ELP-2000 lunar theory**. Replaces the truncated
   Meeus 47.A series. Sub-arcsecond residual reduction.
   Only matters once everything above is fixed.

These will collectively shrink the moonlight-pond intercept
from ~8 nm toward the noise floor set by centroid precision
(currently ~1 px hand-thresholded; arcminute equivalent on
a 3000 px focal length is ~1.1′ ≈ 1.1 nm).
