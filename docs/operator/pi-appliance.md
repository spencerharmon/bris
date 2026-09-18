# Pi Zero 2W appliance (aarch64)

The Bris **appliance** is the embedded-Linux frontend from the ROI "Frontends"
axis: a Raspberry Pi Zero 2W (aarch64) running a single self-contained `bris`
binary as the on-device frontend, wired to the camera and to NMEA sinks. There
is one binary per device; the CLI (`crates/bris-cli`) *is* the appliance's
frontend — no desktop, no Android shell.

This document is the reproducible build + image-assembly recipe. It bakes **no**
infrastructure identifiers: hostnames, IPs, device names, credentials, and NMEA
peer addresses are all supplied at flash / first-boot time, never at build time.

## What gets built

`scripts/pi-appliance/build.sh` cross-compiles `bris-cli` for
`aarch64-unknown-linux-gnu` and assembles a staging tree:

```
target/pi-appliance/
  usr/local/bin/bris                       the cross-built CLI (aarch64 ELF)
  usr/share/bris/ml-gravity/               bris-data ml-gravity payload
  etc/bris/bris-appliance.env              runtime config TEMPLATE (placeholders)
  etc/systemd/system/bris-appliance.service systemd unit (single frontend)
  BUILDINFO.txt                            toolchain + git-rev provenance
```

Overlay that tree onto the Pi rootfs (a `pi-gen` / `rpi-image-gen` stage, or a
plain copy onto the mounted SD boot+root partitions) to produce the appliance
image. The binary is dynamically linked against glibc, which the Pi OS
(Raspberry Pi OS / Debian bookworm, aarch64) provides.

## Reproducible cross toolchain

The aarch64 GNU cross toolchain is **pinned by `flake.nix`** at the repo root, so
the build is reproducible on any host with Nix — no `sudo`, no `apt`, no
`gcc-aarch64-linux-gnu` package to install:

```sh
nix develop            # puts aarch64-unknown-linux-gnu-gcc on PATH
scripts/pi-appliance/build.sh --tarball
```

`.cargo/config.toml` names the cross linker and `cc-rs` compiler
(`aarch64-unknown-linux-gnu-{gcc,g++,ar}`) for the `aarch64-unknown-linux-gnu`
target, so a bare `cargo build --release --target aarch64-unknown-linux-gnu -p
bris-cli` cross-compiles correctly as long as that toolchain is on PATH. The
`[env]` entries use `force = false`, so an environment override (CI's Debian
prefix, or a developer's own toolchain) still wins.

### Building in CI / on Debian

CI (`.github/workflows/ci.yml`, the `cross-build` job) installs Debian's
`gcc-aarch64-linux-gnu`, whose binaries carry the shorter `aarch64-linux-gnu-*`
prefix. `build.sh` auto-detects that flavour and exports
`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` / `CC_aarch64_unknown_linux_gnu`
overrides so the same recipe works there too. Nothing else changes.

## Runtime prerequisites on the device

The appliance binary needs, at runtime on the Pi:

- **Camera / V4L2 access** (`bris-capture`): the Pi camera enumerates as a V4L2
  device (usually `/dev/video0`). The service runs with the `video` group.
- **NMEA sinks** (`bris-nmea`): a serial tty (autopilot / chartplotter, `dialout`
  group) and/or a UDP peer on the local network. Both are configured in
  `bris-appliance.env`; leave a sink blank to disable it.
- **bris-data ml-gravity payload**: staged read-only at `/usr/share/bris`; the
  CLI's segmentation + ml-gravity features load it via `BRIS_DATA_ROOT`.

## First-boot configuration (no baked identifiers)

`etc/bris/bris-appliance.env` ships with **placeholders only**. On first boot the
operator fills in the device-specific values — the camera device, the serial tty
+ baud, the UDP NMEA `host:port`, and the data root. This is the single point
where site-specific facts enter the appliance; the image itself is
site-agnostic and identical across every device of a given build.

## Verifying the build

The task's definition-of-done check is exactly the cross-compile step:

```sh
cargo build --release --target aarch64-unknown-linux-gnu -p bris-cli
```

Run inside `nix develop` (or with the Debian cross toolchain installed). A
successful build produces `target/aarch64-unknown-linux-gnu/release/bris`, an
`ELF 64-bit LSB … ARM aarch64` executable — confirm with `file` on it.
