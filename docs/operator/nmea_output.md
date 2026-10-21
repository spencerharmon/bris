# NMEA output & operator remediation

`bris serve` drains the engine's fix stream and emits NMEA 0183
sentences to one or more **transport sinks**. This page describes
each sink, how to configure it, the sentence set Bris emits, and —
most importantly — what each NMEA-visible signal means and what to
do about it.

Bris's core invariant is on display here: **every fix carries its
real 1σ uncertainty, and it is never hidden, faked, or gated
away.** A high-uncertainty fix is still emitted — flagged as such —
rather than suppressed. Reading the uncertainty channels below is
how you tell a trustworthy fix from a rough one.

## Sinks

Configure sinks either in the config file (`[[nmea]]` tables) or
with `bris serve` flags. **Flags add to the file sinks; they don't
replace them.** All four sink types can run simultaneously — each
published fix fans out to every configured sink.

| Sink | Config table | `bris serve` flag | Typical use |
|------|--------------|-------------------|-------------|
| stdout | `type = "stdout"` | `--nmea-stdout` | Pipe into another tool (`bris serve --nmea-stdout \| gpsd`); debugging. |
| TCP server | `type = "tcp"`, `addr` | `--nmea-tcp ADDR` | `OpenCPN` / MaxSea / Coastal Explorer over the LAN. Convention port `10110`. |
| UDP | `type = "udp"`, `addr` | `--nmea-udp ADDR` | LAN broadcast to every plotter at once; point-to-point push. |
| serial | `type = "serial"`, `device` | `--nmea-serial DEVICE` | Classic NMEA-0183 wire to a dedicated marine plotter. |

### Example config

```toml
[observer]
latitude = 47.6
longitude = -122.3
eye_height_m = 2.5

# TCP server for OpenCPN on the LAN.
[[nmea]]
type = "tcp"
addr = "0.0.0.0:10110"

# UDP broadcast so every plotter on the segment sees the feed.
[[nmea]]
type = "udp"
addr = "255.255.255.255:10110"

# Serial wire to a fixed-mount plotter.
[[nmea]]
type = "serial"
device = "/dev/ttyUSB0"
```

### TCP server

Binds a TCP listener and broadcasts each sentence batch to every
connected client. Clients connect to the bound address; a client
whose write fails (broken pipe, slow consumer) is dropped silently
and rejoins on its next reconnect. Use `0.0.0.0:10110` to listen on
all interfaces at the `OpenCPN` default port.

### UDP

Sends each sentence batch as a single UDP datagram to a fixed
destination address. The socket has `SO_BROADCAST` enabled, so the
destination can be:

- a **broadcast** address (`255.255.255.255:10110`) — every plotter
  on the LAN segment receives the feed with no per-client
  connection state, or
- a **unicast** `host:port` — a point-to-point push to one
  consumer.

UDP is fire-and-forget: there is no delivery guarantee and no
back-pressure. At Bris's ~1 Hz fix cadence with small sentences
this is fine, and it is the lowest-overhead way to feed several
plotters at once.

### Serial

Writes each sentence batch to a serial device path (`/dev/ttyUSB0`,
or the Pi's on-board UART `/dev/ttyAMA0`). This is the classic
NMEA-0183 wire that most dedicated marine plotters accept.

**You must configure the tty line discipline (baud rate) out of
band with `stty` before starting `bris serve`.** Bris opens the
device and streams bytes to it; it does not set the baud rate
itself. This keeps the binary free of a platform serial dependency
(no `libudev`) so it cross-compiles cleanly to the aarch64 Pi
appliance target.

Standard NMEA-0183 is **4800 8N1**:

```sh
stty -F /dev/ttyUSB0 4800 cs8 -cstopb -parenb raw -echo
bris serve --nmea-serial /dev/ttyUSB0
```

High-speed (AIS-grade) links use 38400:

```sh
stty -F /dev/ttyAMA0 38400 cs8 -cstopb -parenb raw -echo
```

If sentences arrive garbled at the plotter, the baud rate is almost
certainly mismatched — re-run `stty` with the rate the plotter
expects. If `bris serve` fails to start with a permission error on
the device, add your user to the `dialout` group
(`sudo usermod -aG dialout $USER`, then re-login).

## The sentence set

For each published fix Bris emits, in canonical order:

1. **`$GPGLL`** — geographic position (lat/lon) + validity status.
2. **`$GPRMC`** — recommended minimum: lat/lon + `A`/`V` status.
3. **`$GPGGA`** — GPS-style fix data: lat/lon + quality indicator.
4. **`$GPGST`** — pseudorange error statistics: the honest 1σ
   error ellipse.
5. **`$PBRIS,FIX`** — Bris-specific engine summary (sight count,
   azimuth spread, oldest-sight age, dominant σ source). Standard
   consumers ignore it; the full `$PBRIS,*` contract is in
   [`docs/protocol/pbris.md`](../protocol/pbris.md).

Standard consumers (`OpenCPN`, MaxSea, Coastal Explorer) recognize
the first four. Every sentence is terminated with `\r\n` and
carries a valid `*XX` checksum.

## Reading the uncertainty — remediation guide

The uncertainty is carried in three cooperating channels. Read them
together.

### `$GPGST` — the honest error ellipse (primary uncertainty)

`$GPGST,hhmmss.ss,rms,σ_major,σ_minor,orient,σ_lat,σ_lon,`

- `σ_major` / `σ_minor` are the 1σ semi-axes of the position error
  ellipse, **in metres**. This is the real, un-fudged fix
  uncertainty. A 0.5 nm fix reads `926.0` here (0.5 × 1852 m/nm).
- `orient` is the major-axis bearing (degrees from north).
- `σ_lat` / `σ_lon` are the per-axis 1σ, in metres.

`OpenCPN` parses `$GPGST` and can render the ellipse. Many
fixed-mount plotters ignore it — which is exactly why Bris *also*
degrades the `$GPGGA` quality byte and the `$GPRMC`/`$GPGLL` status
below, so even a `$GPGST`-blind plotter still sees the confidence
signal.

**Remediation by σ:**

| Semi-major σ | Meaning | What to do |
|--------------|---------|------------|
| ≲ a few hundred m | Tight fix; trust it for navigation. | Nothing. |
| ~1–5 km | Rough fix — too few sights, or a narrow azimuth spread. | Take more sights spread around the horizon; check `$PBRIS,FIX` azimuth spread. |
| ≫ 5 km | Very weak geometry or stale sights. | Do **not** navigate on it. Re-shoot; check the dominant-source field. |

### `$GPGGA` quality byte + `$GPRMC`/`$GPGLL` status

Bris maps the combined fix σ onto the standard quality/validity
fields so a plotter that only reads these still gets the signal:

- `$GPGGA` quality digit degrades (green → yellow → the low-quality
  digit) as σ grows past the configured thresholds.
- `$GPRMC` and `$GPGLL` status flip from `A` (valid) to `V` (void)
  for a fix past the red threshold.

If your plotter shows the position as **void (`V`)** or greys out
the fix, that is Bris telling you the uncertainty crossed the red
threshold — it is **not** a Bris bug and **not** a dropped fix. The
position is still present in the sentence; the plotter is honoring
the void flag. Re-shoot to tighten the fix.

### `$PBRIS,FIX` — why the fix is as good (or bad) as it is

`$PBRIS,FIX,hhmmss.ss,n_sights,az_spread_deg,oldest_age_s,dominant`

- `n_sights` — how many sights fed this fix. More is better; a
  1–2-sight fix is inherently weak.
- `az_spread_deg` — the azimuth spread of the contributing bodies.
  A small spread (bodies clustered in one direction) gives a
  long, thin error ellipse; aim for well-distributed bearings.
- `oldest_age_s` — age of the oldest contributing sight. A large
  value means the fix leans on stale data (you've been drifting
  since); re-shoot.
- `dominant` — which uncertainty source dominates the budget
  (`horizon`, etc.). Tells you where to focus: a horizon-dominated
  fix improves with a better horizon reference; a calibration-
  dominated one wants a fresh `bris calibrate`.

## Common symptoms → causes

| Symptom at the plotter | Likely cause | Fix |
|------------------------|--------------|-----|
| No fixes at all, but `bris serve` is running | No sink configured for that transport, or wrong address/port. | Confirm the `[[nmea]]` sink / flag matches the plotter's input; check the startup log — with no sinks Bris logs "no NMEA sinks configured". |
| Garbled serial sentences | Baud-rate mismatch. | Re-run `stty` at the plotter's rate before `bris serve`. |
| Position shows void (`V`) / greyed out | Fix σ past the red threshold (honest low-confidence signal). | Re-shoot; add sights with wider azimuth spread. |
| Fix jumps around between updates | Genuinely large σ (see `$GPGST`). | This is honest — the ellipse is large. Improve geometry; don't expect GNSS-grade stability from few sights. |
| UDP feed reaches one plotter but not others | Sent to a unicast address, not broadcast. | Use `255.255.255.255:10110` (or the segment's directed broadcast). |
| Permission denied opening serial device | User not in `dialout`. | `sudo usermod -aG dialout $USER`, re-login. |

Bris never suppresses a fix to look more confident than it is. If a
fix looks bad on the plotter, the sentences are telling you the
truth about the observation quality — the remedy is always at the
sight/geometry end, never a knob that hides the σ.
