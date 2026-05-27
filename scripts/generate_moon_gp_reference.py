#!/usr/bin/env python3
# ruff: noqa
"""Generate reference geographic-position (sub-point) values for the
Moon and Sun using Skyfield + JPL DE421.

Usage:
    python -m venv .venv
    .venv/bin/pip install skyfield
    .venv/bin/python scripts/generate_moon_gp_reference.py

This script is run ONCE during reference generation; its output gets
pasted into the Rust regression test in
`crates/bris-streaming/src/pipeline/stage_e.rs`. CI does not invoke it
and Python is not a CI dependency.

Skyfield auto-downloads `de421.bsp` (JPL DE421 — canonical 1900-2050
planetary ephemeris) into `~/.skyfield/`. DO NOT commit that file.

Tolerance budget for the Rust test:
    Skyfield's apparent-of-date geocentric (RA, Dec) includes:
        * light-time
        * IAU 2000A/B nutation
        * annual aberration
    bris-almanac's `body_geocentric_apparent` applies the same chain.
    The dominant residual is the ~0.5" aberration-model approximation
    (see commit 53f8bf4) plus the ~ms-scale TT vs UT1 sidereal-time
    accuracy that bris's GMST routine carries.

    Rolled up over the apparent-place + GAST chain, residuals stay
    well under 10". A 1' = 60" tolerance is generous and protects
    against re-introducing the topocentric/refraction GP bias (PR #28)
    which would manifest as tens of arcmin (refraction) up to ~1°
    (lunar parallax) of error.
"""

from skyfield.api import load
from skyfield import almanac  # noqa: F401  (validates install)

ts = load.timescale()
eph = load("de421.bsp")
earth = eph["earth"]
moon = eph["moon"]
sun = eph["sun"]


def gp(target, t_utc_iso: str):
    t = ts.utc(*_parse_iso(t_utc_iso))
    astrometric = earth.at(t).observe(target)
    apparent = astrometric.apparent()
    ra, dec, _ = apparent.radec(epoch="date")  # apparent-of-date
    # GAST in hours -> degrees.
    gast_deg = t.gast * 15.0
    ra_deg = ra._degrees
    dec_deg = dec.degrees
    # GP longitude: -GHA = RA - GAST, normalised to (-180, 180].
    lon = (ra_deg - gast_deg) % 360.0
    if lon > 180.0:
        lon -= 360.0
    return {
        "tt_jd": t.tt,
        "ut1_jd": t.ut1,
        "lat_deg": dec_deg,
        "lon_deg": lon,
    }


def _parse_iso(s: str):
    # "YYYY-MM-DDTHH:MM:SSZ"
    date, time = s.rstrip("Z").split("T")
    y, mo, d = (int(x) for x in date.split("-"))
    hh, mm, ss = time.split(":")
    return (y, mo, d, int(hh), int(mm), float(ss))


CASES = [
    ("Moon", moon, "2026-02-26T00:00:00Z"),   # high northern dec (~+28°)
    ("Moon", moon, "2026-07-06T06:00:00Z"),   # near equator (|dec|<0.1°)
    ("Moon", moon, "2026-02-12T12:00:00Z"),   # high southern dec (~-28°)
    ("Sun",  sun,  "2026-03-21T12:00:00Z"),   # near equinox
]

for label, body, iso in CASES:
    r = gp(body, iso)
    print(
        f"// {iso}  {label} GP: "
        f"TT_JD={r['tt_jd']:.6f}  UT1_JD={r['ut1_jd']:.6f}  "
        f"lat={r['lat_deg']:+.6f}°  lon={r['lon_deg']:+.6f}°"
    )
