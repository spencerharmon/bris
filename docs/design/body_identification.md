# Body Identification: How Do We Know It's the Moon?

Status: design draft. Documents the current pipeline's
body-identification chain and the failure modes that
deserve hardening before broad operator use.

The question motivating this document: *what stops bris from
reducing a streetlamp (or any bright terrestrial light) as if
it were the Moon?* The honest answer today is "several
things, in layered defense, none of them ironclad alone."
This doc walks the layers, the gaps, and what closes them.

## The threat model

A photo of the night sky contains the navigationally useful
bright spots (Moon, planets, bright stars) plus a long tail
of confusers:
- Streetlamps, lit windows, signage
- Aircraft (often moving but easy to freeze on a single
  exposure)
- Camera artifacts: lens flare ghosts, internal reflections,
  sensor hot pixels
- Reflections off windows, water, polished metal
- Distant terrestrial lights (yard lights, vessels at sea)
- Insects, bats lit by ambient light
- Birds, especially seagulls at distance

If bris accepts the wrong bright spot as "Moon" the entire
sight reduction is meaningless: Hc is computed from where the
Moon really is, Ho is measured from where a streetlamp
appears to be, and the intercept is geometrically arbitrary.

## What we have today

The pipeline gates body identification through several
checks. Each is independently weak; collectively they cover
many common confusers but leave gaps documented below.

### 1. Classifier-based dispatch (`bris_vision::classify`)

The first gate is the image-level classifier (Day / Twilight /
Night). Night-mode peak detection runs only when the scene is
dark enough that a streetlamp couldn't dominate the frame
without making it Twilight or brighter — so a single bright
peak on a truly dark scene is *probably* celestial. This is
correlational, not causal: a remote dark beach with one
distant lit boat would still classify as Night and the boat
would be the brightest peak.

### 2. Peak detection + count thresholds

`detect_night_peaks` extracts local maxima above a noise
floor. A scene with too many peaks (`> O(thousands)`) is
treated as confused — moonlit-water glitter is the canonical
failure mode here. A scene with too few peaks (1–3) is the
expected single-body or pair case.

This gate accidentally caught the streetlamp case for years:
on an outdoor capture the streetlamp would have hundreds of
visible bright peaks (lit windows, reflections, other lamps)
and the engine would refuse to identify any single one.

### 3. Plate solving (when ≥ 3 peaks)

For night scenes with a star field, `bris_platesolve` matches
the peak pattern against the star catalog by angular
distance. This is the strongest identity gate we have: if the
peak pattern is geometrically consistent with a region of
the celestial sphere, the brightest peaks are by construction
real stars. A streetlamp included in the peak list either
fails the plate-solve (no consistent solution) or gets
flagged as an outlier when the solver converges on the real
stars without it.

**Limitation**: plate solving requires ≥ 3 peaks with
known catalog counterparts. A single-body scene (Moon alone,
or Moon + 1 planet) has no constellation to lock onto. The
moonlight-pond corpus is exactly this case — twilight,
single body, no plate-solve. The current pipeline produces
**no plate-solve verification at all** on that scene.

### 4. Almanac consistency (`predicted_altitude`)

When a `PositionPrior` is available (from a previous fix or
operator DR entry), the engine computes the predicted
altitude and azimuth of the target body via
`body_apparent_place`. The reflection-pair Test 3 compares
the measured `Ho = θ/2` to this prediction with a configured
tolerance.

This is the gate that *would* catch a streetlamp posing as
the Moon: a streetlamp at altitude 5° (above a near horizon)
disagrees with a Moon prediction at 20° by 15°, well outside
any reasonable tolerance. Test 3 rejects.

**Limitation**: requires a position prior. On cold start
(first fix from a powered-down phone with no DR), Test 3 is
skipped and Test 4 (catalog-count gate, requires ≥ 3 pairs)
substitutes — but the single Moon + reflection scene has only
1 pair, so neither gate fires. **The cold-start single-Moon
case has no identity verification today.** The pipeline
trusts the scene composition.

### 5. Geometric reasonableness (reflection-pair specific)

For reflection-pair the direct-vs-reflection pair must satisfy:
- Same azimuth (within tolerance) — Test 1
- Direct brighter than reflection — Test 2
- Brightness ratio in physical range — Test 2

A streetlamp + its reflection in a wet road pavement would
**pass these tests**. A streetlamp + a passing car's
headlight ghost would fail Test 1 (different azimuths).

## Gaps, in priority order

### Gap 1: Cold-start single-body scenes have no identity check

The moonlight-pond corpus is the canonical example. Tests 1
and 2 (geometric, photometric) pass; Tests 3 and 4 can't
fire; the operator's expectation that "the bright thing is
the Moon" carries no machine-verifiable evidence.

**Mitigation candidates**:
- **Phase-of-Moon disk size check**. The Moon's angular
  diameter is ~30′ ± 3′. A bright disk of *measured*
  diameter outside this range is not the Moon. The
  `mean_intensity`-and-halo machinery from PR #8 already
  has the bounding box of each saturated component; convert
  to angular size via the intrinsics. A streetlamp at
  100 m looks like ~0.5′ across; an aircraft landing light
  at 5 km is ~0.1′. A 30′ disk on a dark scene is *almost
  certainly* the Moon. Easy add.
- **Sun-Moon angular distance prior**. At known UTC time,
  the Moon's altitude band is constrained even without a
  longitude prior (Bowditch §29 has the form). A peak whose
  *measured* altitude (from any horizon provider) is way
  outside the Moon's possible altitude at that UTC is
  rejected. Requires only a latitude prior to within ~30°.
- **Operator confirmation gate**. On cold-start single-body,
  the engine surfaces "I see one bright disk; if it's the
  Moon, tap Confirm; otherwise it's a confuser". This is
  the honest path — make the operator the explicit identity
  authority when the pipeline can't be.

### Gap 2: Streetlamp + wet-road reflection passes all current
reflection-pair tests

If a position prior is *available* (warm start) Test 3
catches this: the streetlamp's altitude doesn't match the
Moon's predicted altitude. But if the streetlamp's altitude
happens to coincide with the Moon's predicted altitude (rare
but possible — a tall lamp post in the direction of the
Moon), Test 3 passes too.

**Mitigation candidates**:
- **Test 5 reflector-region** (already planned in
  `horizon_autodetect.md` §reflection-pair). The reflector
  region (the wet pavement) is geometrically *below* and
  *near* the chord midpoint. If the implied reflector
  region is *above* the midpoint or pixel-discontinuous
  with surrounding texture, reject. A streetlamp + its
  reflection on a *small* puddle some distance below would
  fail this — the pavement texture between lamp and
  puddle is not "reflector".
- **Color discrimination**. The Moon's color temperature
  is ~4100 K (warm white); high-pressure sodium lamps are
  ~2000 K (deep amber); modern LED streetlamps are
  ~4000 K (cooler white). On a calibrated color sensor
  this is discriminative. Bris currently works grayscale-
  only — adding a single-channel color cue from the
  Bayer pattern (when raw is available) is a small change
  that catches sodium lamps cheaply.

### Gap 3: Aircraft and satellites

Aircraft strobes and Iridium-type satellite flares can
present as single bright peaks on a dark sky. Aircraft
typically move visibly between frames (~0.1°/s); satellites
faster. The cross-frame registration (Phase 3.6 Phase 3,
not yet landed) is the natural defense — a peak that moves
between consecutive frames at angular rates inconsistent
with celestial motion (~15″/s for sidereal, much less for
the Moon) is not a celestial body.

**Mitigation candidates**:
- **Multi-frame persistence gate**. A peak that appears in
  only 1 of N consecutive frames is suspect. Plumbed into
  the streaming engine's existing ring buffer.
- **Angular-velocity gate**. A peak whose centroid moves
  > 10″/s frame-to-frame is not stellar; > 1′/s rules out
  the Moon (~30″/s lunar motion is in the noise).
- **Strobe-period detection**. Anti-collision strobes flash
  at ~1 Hz. A peak that appears, disappears, reappears in
  a multi-second window is almost certainly an aircraft.

### Gap 4: Lens flare and internal reflections

A bright source (Moon, Sun, streetlamp) produces ghost
images at predictable positions along the line from optical
center through the source (and reflected through the
center for some lens designs). The Cat S62 calibration
documents `~k1, k2, p1, p2` parameters; the ghost geometry
follows from them.

**Mitigation candidates**:
- **Ghost-position rejection**. For each bright peak,
  predict the locations of the dominant ghost images from
  intrinsics; if a second peak appears within tolerance of
  a predicted ghost location, mask it.
- **Saturation-class discrimination**. A ghost is usually
  dimmer and softer-edged than the primary. The
  `mean_intensity`-over-halo metric already distinguishes
  this on a per-component basis.

This deserves its own PR; lens-flare rejection is already
a noted TODO in `reflection_pair.rs`.

## What the pipeline already gets right

To not undersell the existing defenses: on **warm-start
night scenes with a star field**, the chain of (classifier
→ peak count → plate solve → almanac consistency) is
genuinely strong. A streetlamp would have to be visually
indistinguishable from a star AND geometrically consistent
with a real star pattern AND in the predicted location of
the body being claimed AND within tolerance of the
predicted altitude. That's a four-of-four AND condition
across independent physical models.

The gap is specifically:
- Cold-start (no warm position prior)
- Single-body or single-pair scenes
- Reflection-pair on smooth small reflectors

This is the moonlight-pond corpus, and it's not coincidental
that's where we found problems.

## Recommended sequencing

1. **Disk-size sanity check** for any single-body
   classification (cheap, catches most non-Moon confusers).
2. **Operator-confirmation gate** on cold-start single-body
   scenes (no algorithmic complexity; pushes the responsibility
   to where it actually lies).
3. **Test 5 reflector-region** for reflection-pair (already
   planned; catches the streetlamp-on-wet-road case for
   warm starts).
4. **Cross-frame persistence** (Phase 3.6 Phase 3) — catches
   aircraft + satellites cheaply once registration lands.
5. **Lens-flare rejection** — Phase 4 territory.
6. **Color-channel discrimination** — depends on raw-Bayer
   capture path, not yet plumbed.

Each is independently shippable. The first two close the
honest-uncertainty gap on the corpus we have today.

## Honest summary

Today, on a cold-start single-Moon-and-reflection scene, the
pipeline trusts the operator's framing. If the operator
points it at a streetlamp and its puddle reflection, it will
compute a meaningless intercept and report a confident σ.
This is acceptable as a research instrument with a known
operator. It is not acceptable as a navigation tool for a
naive user, and the gaps above need closing before debug
mode comes off.
