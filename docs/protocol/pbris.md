# `$PBRIS` proprietary NMEA 0183 sentences

This document is the authoritative field-level specification for Bris's
proprietary NMEA 0183 sentences. It is the contract that downstream
tooling (NMEA → metrics converters, log analyzers, etc.) is built
against. Versioned via `$PBRIS,VER`.

**Status: skeleton.** Field definitions are filled in as Phase 5 / 5.5
of `plan.org` is implemented.

## Subtypes (planned)

- `$PBRIS,VER,...` — protocol version handshake at session start.
- `$PBRIS,UNC,...` — per-source 1σ uncertainty contributions to the
  current fix and the dominant source.
- `$PBRIS,SIGHT,n,...` — one per sight used in the fix.
- `$PBRIS,TIME,...` — clock state (time-since-sync, drift estimate ppm,
  step-detected flag, last-sync timestamp).
- `$PBRIS,ERR,...` — capture/processing error counters since last fix.

Each sentence stays under the NMEA 0183 82-character per-sentence
limit; downstream tools reassemble subtypes by their shared timestamp.
