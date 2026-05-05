# `$PBRIS` proprietary NMEA 0183 sentences

This document is the authoritative field-level specification for Bris's
proprietary NMEA 0183 sentences. It is the contract that downstream
tooling (NMEA → metrics converters, log analyzers, etc.) is built
against.

**Schema version: 1** (emitted in `$PBRIS,VER` at session start).

## General format

Each subtype is a standalone NMEA 0183 sentence:

```
$PBRIS,<subtype>,<fields...>*XX\r\n
```

Where `XX` is the standard NMEA 0183 XOR checksum of the bytes between
`$` and `*`. Each sentence stays under the NMEA 0183 82-character
per-sentence limit; downstream tools reassemble the subtypes belonging
to one fix by their shared `hhmmss.ss` UTC timestamp field.

Field separators are commas. Empty fields are permitted (e.g.
`,,`); they encode "value not available."

Numeric quantities use a fixed precision documented per subtype. All
times are UTC.

## Subtypes

### `$PBRIS,VER`

Schema version handshake. Emit at session start so consumers can detect
incompatible schema changes.

```
$PBRIS,VER,<schema_version>*XX
```

| Field | Format | Meaning |
|-------|--------|---------|
| `schema_version` | integer | Bumps when any field below changes. Currently `1`. |

### `$PBRIS,TIME`

Clock state at the time of the most recent fix.

```
$PBRIS,TIME,hhmmss.ss,<seconds_since_sync>,<drift_ppm>,<step_detected>*XX
```

| Field | Format | Meaning |
|-------|--------|---------|
| `hhmmss.ss` | UTC time | Time of the fix this diagnostic is for. |
| `seconds_since_sync` | integer (or empty) | Seconds since the most recent successful NTP sync; empty if never synced. |
| `drift_ppm` | float, 3 decimals (or empty) | Estimated local oscillator drift, parts per million; empty if drift learning is disabled or has insufficient data. |
| `step_detected` | `0` or `1` | `1` if a clock step was detected since the previous fix. |

### `$PBRIS,UNC`

Per-source 1σ contributions to the current fix's position uncertainty,
in nautical miles, plus the dominant source.

```
$PBRIS,UNC,hhmmss.ss,<centroid>,<horizon>,<calibration>,<stitching>,<refraction>,<dip>,<timing>,<dominant>*XX
```

| Field | Format | Meaning |
|-------|--------|---------|
| `hhmmss.ss` | UTC time | Time of the fix. |
| `centroid` | float, 4 decimals | 1σ from body centroiding, nm. |
| `horizon` | float, 4 decimals | 1σ from horizon line fit, nm. |
| `calibration` | float, 4 decimals | 1σ from lens calibration residual, nm. |
| `stitching` | float, 4 decimals | 1σ from cross-frame stitching alignment, nm. |
| `refraction` | float, 4 decimals | 1σ from atmospheric refraction model, nm. |
| `dip` | float, 4 decimals | 1σ from horizon dip / eye-height uncertainty, nm. |
| `timing` | float, 4 decimals | 1σ from clock state (NTP staleness, drift, step events), nm. |
| `dominant` | string | Field name (one of: `centroid`, `horizon`, `calibration`, `stitching`, `refraction`, `dip`, `timing`, or `none`) of the largest contribution. The operator's remediation guide. |

### `$PBRIS,SIGHT,n`

One sentence per sight used in the current fix.

```
$PBRIS,SIGHT,<n>,<body_name>,<altitude_deg>,<azimuth_deg>,<intercept_nm>,<sigma_nm>*XX
```

| Field | Format | Meaning |
|-------|--------|---------|
| `n` | integer | Sight index, 0-based. |
| `body_name` | string (no spaces) | E.g. `Sun`, `Moon`, `Sirius`, `Mars`. |
| `altitude_deg` | float, 4 decimals | Apparent altitude, degrees. |
| `azimuth_deg` | float, 4 decimals | True azimuth (clockwise from north), degrees. |
| `intercept_nm` | float, 3 decimals | Marc Saint-Hilaire intercept, nm; positive = toward body's GP. |
| `sigma_nm` | float, 3 decimals | 1σ on the intercept, nm. |

### `$PBRIS,ERR`

Capture and processing error counters since the previous fix.

```
$PBRIS,ERR,hhmmss.ss,<frames_dropped>,<horizon_failures>,<centroid_failures>,<sights_rejected>*XX
```

| Field | Format | Meaning |
|-------|--------|---------|
| `hhmmss.ss` | UTC time | Time of the fix. |
| `frames_dropped` | integer | Frames dropped at capture (camera, queue overflow). |
| `horizon_failures` | integer | Horizon detections that failed (insufficient candidates / low confidence). |
| `centroid_failures` | integer | Centroiding failures (no bright region / too small). |
| `sights_rejected` | integer | Sights rejected by the per-sight blunder screen. |

## Canonical emission order

For each fix, sentences are emitted in this order:

1. `$PBRIS,VER` — only at session start.
2. `$PBRIS,TIME`
3. `$PBRIS,UNC`
4. `$PBRIS,SIGHT,0` … `$PBRIS,SIGHT,N-1`
5. `$PBRIS,ERR`

Standard `$GP*` sentences (`$GPGLL`, `$GPRMC`, `$GPGGA`, `$GPGST`)
are emitted *before* the `$PBRIS` set so chartplotters that consume
only the standard sentences see the fix immediately and can ignore
the proprietary tail.
