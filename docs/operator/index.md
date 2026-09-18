# Bris operator documentation

Operator-facing documentation.

Available now:
- [`calibration.md`](calibration.md) — lens calibration workflow:
  target preparation, capture procedure, running `bris calibrate`,
  interpreting the diagnostic, deploying the result.
- [`mobile-hud.md`](mobile-hud.md) — Android live-HUD chrome: the
  confidence ellipse, pool / recent-sights views, recovered-fix
  banner.
- [`nmea_output.md`](nmea_output.md) — NMEA 0183 output: the stdout /
  TCP / UDP / serial transport sinks, the emitted sentence set, and
  a remediation guide for every NMEA-visible uncertainty signal
  (how to read `$GPGST` / `$GPGGA` quality / `$GPRMC` status and
  what to do about a low-confidence fix).

Planned (alongside future plan.org phases):
- `chartplotter-symptoms.md` — every NMEA-visible Bris signal, what it
  means, and what to do about it.
- `installation-embedded.md` — installing the Pi appliance image.
- `mobile-quickstart.md` — using the phone app.
