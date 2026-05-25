# Onboard Intelligence CelNav Walk-Through — Bris Comparison

Status: external reference review. Compares the
methodology described at
<https://www.onboardintelligence.com/CelestialNav/CelNav1>
against bris's current pipeline and flags techniques worth
adopting, deferring, or rejecting.

The source is an introductory + practical celestial-
navigation tutorial maintained by Johan Machtelinckx, author
of the ASNAv navigation program. The page covers the
geometric setup (equatorial and horizontal coordinate
systems), the spherical-triangle solution, traditional LOP
construction, and — most usefully for us — statistical
treatment of multi-sight fixes with realistic error models.

## Sections compared

### 1. Introduction (lighthouse analogy, circle of position)

**Source**: Establishes that a single altitude measurement
gives a circle of position; a fix needs two or more.

**Bris**: Same model. The streaming engine's `bris-nav`
crate produces per-sight LOPs and accumulates them in the
sight window for a least-squares fix.

**Difference**: None of substance. Tutorial framing.

### 2. Equatorial Coordinate System (declination, GHA)

**Source**: Standard textbook treatment. Body position on
the celestial sphere is (δ, GHA); observer position on
Earth is (lat, lon).

**Bris**: `bris-almanac` returns declination and right
ascension for any body; GHA is derived from RA + GAST.
Equivalent representation.

### 3. Horizontal Coordinate System (altitude, azimuth)

**Source**: The local (alt, az) is what a sextant
measures.

**Bris**: `ApparentPlace { direction: Horizontal {
altitude, azimuth }, altitude_sigma }` is the canonical
return type from `body_apparent_place`. Identical.

### 4. Spherical Triangle / Celestial Navigation Equation

**Source**: The classic cosine-rule solution:

> `sin(h) = sin(φ)·sin(δ) + cos(φ)·cos(δ)·cos(H)`

where H is the local hour angle. With two observations the
system of two equations in two unknowns (lat, lon) can be
solved iteratively from an assumed position.

**Source highlights** (worth quoting):

> With more than 2 observations, it's even possible to
> improve the traditional method and to perform a
> statistical analysis: to give a certain weight to each
> observation according to its reliability in the normal
> law model; to compute and eliminate the possible
> systematic error of the observer; to correct the assumed
> course and speed if enough observations are provided
> (exactly the same way the GPS is able to give the course
> and speed of the vessel if enough satellites are visible).
> To do this, a program like ASNAv is using the least-square
> method with iterative weighting adjustment by the
> **Biweight function** on a system of equations given by
> the differential correction method.

**Bris today**: Uses weighted least squares on per-sight σ
combined via root-sum-square. Weights are 1/σ². See
`bris-nav::fix::weighted_least_squares` (or the equivalent).

**Gap → improvement candidate**: bris does *not* currently
do iterative reweighting (IRLS) with a robust ψ-function
like Tukey's biweight. Our current weights are derived from
the σ chain alone; if a sight has a gross error (a misread
peak, a confuser, lens flare) it still gets full Gaussian
weight, biasing the fix. IRLS-biweight downweights outliers
based on their post-fit residual, iteratively.

**Recommendation**: **Add IRLS-biweight to the fix solver**
once we have multi-sight scenes to test on. Estimated
complexity: ~100 LOC in `bris-nav` plus a property test.
Lever-arm: medium-large — protects every multi-sight fix
from being corrupted by a single bad sight. **High value
once we have ≥ 3 sights routinely.**

### 5. Saint-Hilaire LOPs and intercepts

**Source**: Walks through the textbook procedure: take a
sextant altitude Hs; correct for instrument error, dip,
refraction, parallax, semi-diameter; that's Ho. Compute Hc
from the assumed position. Plot the LOP perpendicular to
azimuth at distance ITC from AP, toward or away from body.

**Bris**: This is exactly the `bris-nav::line_of_position`
path. The corrections are:
- Instrument error: N/A (no sextant; camera intrinsics
  serve the analogous role, calibration handles it)
- Dip: `Observer::horizon_dip_rad` (Bowditch §16, applied)
- Refraction: Bennett, applied in `apparent.rs`
- Parallax: Meeus Ch. 40, applied as of PR #15
- Semi-diameter: NOT YET APPLIED — see gap below

**Gap → semi-diameter correction**

The Sun and Moon have non-zero angular semi-diameters
(~16′ each). Sextant sights traditionally measure the
*lower limb* of the body and add the semi-diameter to get
the *center* altitude (which is what the almanac
references). Bris measures the centroid of a saturated
disk, which on a calibrated camera is *approximately* the
center — but on a partially saturated disk (gibbous Moon,
Sun through thin cloud) the centroid is biased toward the
bright side by up to several arcminutes.

For the moonlight-pond corpus the Moon was nearly full
(phase ~0.95 at the capture time), so the centroid bias
is small (< 1′), inside our other error sources. For
other phases this becomes important.

**Recommendation**: For Moon and Sun, implement a
lit-fraction-aware centroid bias correction. Inputs: body
phase (from almanac), saturated component shape (from
`extract_multi_saturated_centroids`). Output: σ-aware
center-of-disk estimate. Lever-arm: small for full Moon,
up to ~10′ for crescent Moon. **Schedule for the next
multi-phase-Moon corpus capture.**

### 6. Cocked hat (3-LOP triangle)

**Source**: Three LOPs rarely intersect at a point. The
triangle they form is the cocked hat. Common-sense
wisdom puts the true position at the center of the
triangle, but the source debunks this:

> If (and only if) the observations azimuths are spread
> over more than 180°, then the most probable position
> (MPP) is inside the cocked hat, but with a probability
> of only 25%!

**Bris today**: We don't have an explicit "cocked hat" UI
because we don't yet have multi-sight scenes to display.
The least-squares fix already does the right thing
algebraically (it produces the centroid of the LOPs
weighted by σ), but we don't currently *show* the operator
the cocked hat or its centroid.

**Recommendation**: When operator-visible multi-sight fixes
land, the UI MUST show the cocked hat alongside the fix.
Hiding the spread misrepresents the actual uncertainty.

### 7. Systematic vs random errors

**Source**: Distinguishes:
- **Systematic error**: index error of the sextant +
  observer's personal bias toward over- or under-reading
  altitudes. A constant of the same sign on every sight.
  Can be solved out only if azimuths span > 180° AND
  the operator knows their personal bias.
- **Random errors**: per-sight noise from sea state,
  refraction anomalies, observer fatigue, etc.

**Bris**:
- **Systematic**: The closest analog is the camera
  calibration residual. Bris carries this as the
  calibration RMS (Cat S62: 0.733 px aggregate). Per-sight
  this contributes a σ but does NOT contribute a constant
  bias — calibration absorbs the bias, residual is
  ~mean-zero noise. **Different from sextant: bris has
  much less systematic bias** because the camera
  calibration step explicitly fits and removes the
  constant offsets the sextant operator must track in
  their head.
  - But: bris does have analogous systematic sources we
    don't fully track today, including:
    - Time-since-calibration drift (lens shifts with
      temperature)
    - Eye-height entry error (constant, biases all dip)
    - Frame timestamp epoch error (constant, biases all
      GHA → all longitudes)
- **Random**: Per-sight σ via the existing per-stage σ
  budget (centroid σ, horizon fit σ, refraction σ, dip σ,
  aberration placeholder σ). Combined via root-sum-square.
  Equivalent to source's framing.

**Source highlight**:

> The systematic error is a constant of the same sign.

This is the textbook argument for sextant practice. **Bris's
calibration model already removes most of this**; what
remains is the operator-confirmable input parameters
(eye height, time). We could expose these as separately
adjustable post-hoc to allow the operator to "tune out"
suspected systematic bias by inspection — same idea as
sextant index-error correction.

**Recommendation**: **Low priority** for now (calibration
absorbs most of the equivalent error), but worth
remembering. When multi-sight fixes routinely have
sub-arcmin per-sight σ and minutes of inexplicable
cocked-hat spread, the systematic-bias correction logic
in the source is the place to look.

### 8. Random error and the 25% probability

**Source**: Walks through the proof that the true position
has only a 25% probability of being inside the cocked hat
even with no systematic error. Most-probable-position
(MPP) ≠ cocked-hat center in general.

**Bris**: Our weighted least squares produces the MPP
correctly — it's the maximum-likelihood estimate under
the assumed Gaussian noise model. We just need to display
it correctly (item 6 above).

### 9. Confidence ellipse

**Source**:

> More helpful than the cocked hat or the MPP (Most
> Probable Position) by itself is the confidence ellipse.
> The confidence ellipse defines the area within which the
> True Position lies with a given probability (95% or 99%
> for instance). A statistical analysis is needed to be
> able to draw this ellipse. Confidence ellipse
> characteristics: its centre is the MPP; its size depends
> on the size of the random errors and on the chosen
> probability; its shape depends on the number of
> observations and distribution of the azimuths.

**Bris**: The fix solver produces a 2×2 position covariance
matrix as a first-class output (see Phase 4 "Multi-sight
fix with full covariance" in `plan.org`). We have the math;
we just don't yet have a UI rendering it.

**Recommendation — high priority**: **Implement the
confidence ellipse rendering** in the bris-android fix
overlay UI before multi-sight fixes go in front of users.
This is the visualisation of the honest-uncertainty
invariant from AGENTS.md. The 2×2 covariance → ellipse
math is standard (eigendecomposition, scaling by χ²
quantile for the chosen confidence level). The math
already lives in `bris-nav`; this is purely a UI task.

Lever-arm: enormous for *trust* and *interpretation*.
Showing a tight ellipse vs a long thin ellipse vs a giant
circle communicates fix quality far better than a single
number.

## Summary table of recommendations

| # | Item | Priority | Effort | Notes |
|---|---|---|---|---|
| 1 | IRLS-biweight outlier rejection in fix solver | High | ~100 LOC | Useful only once we have ≥ 3 sights routinely |
| 2 | Semi-diameter / lit-fraction centroid correction | Medium | ~50 LOC + test corpus | Important for crescent Moon and Sun-through-cloud |
| 3 | Cocked-hat UI on multi-sight fix | High | UI work | Don't hide what the data shows |
| 4 | Operator-visible "systematic bias tuning" sliders | Low | UI + plumbing | Calibration already handles most of this |
| 5 | **Confidence ellipse rendering** | **Highest** | UI + standard math | The visualisation of honest σ; non-negotiable for user trust |

## Things bris does that the source doesn't cover

The source is fundamentally about manual sextant + paper
chart practice with statistical post-processing in ASNAv.
Bris's automation surface is richer:

- **Real-time camera capture** instead of single
  point-in-time sextant readings. Different error model:
  motion blur, autoexposure transients, rolling shutter.
  The source doesn't address these because they don't
  exist for sextants.
- **Multiple horizon providers with fusion** (our Phase
  3.6). The source assumes a single visible sea horizon
  with known dip. We work with reflection-pair, plumb-line,
  vanishing-point, and (planned) IMU horizons, each with
  its own σ. Fusion is bris-specific.
- **Streaming continuous operation** instead of discrete
  sight sessions. Implications for sight scheduling, body
  selection, fix update cadence that have no sextant analog.
- **Diagnostic submission** for failed/ambiguous scenes
  (Phase 6/7). The source's error-analysis chapter would
  benefit from this corpus of real failure modes; the
  reverse is also true (their statistical models inform
  what to log).

## Things the source does that bris doesn't (and won't)

- **Personal index-error tracking**. Sextant practice
  involves the operator periodically measuring their own
  bias against a known horizon. Bris's calibration model
  subsumes this — the calibration session removes the
  equivalent biases. Don't reintroduce manual personal-
  error tracking; it's a sextant-era workaround for
  unmodeled instrument bias.
- **Hour-angle methods using nautical-almanac paper
  tables**. Historical. Bris does the calculation directly
  from the almanac source.

## Process notes

The source was published 1999–2026 (perennial; the original
author maintains it). It is a clear and well-illustrated
introduction; recommended reading for any contributor new to
celestial navigation. The statistical-treatment chapters are
the highest-leverage portions for bris's design.

For the math-heavy treatment of refraction, parallax, and
aberration, prefer Meeus *Astronomical Algorithms* (already
cited throughout `bris-almanac`); for the practical
operator's view, the Bowditch *American Practical Navigator*
remains the standard reference.
