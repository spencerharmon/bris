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

You do **not** have to enter `nix develop` first, though. `.cargo/config.toml`
routes the aarch64 linker and the `cc-rs` compiler/archiver
(`aarch64-unknown-linux-gnu-{gcc,g++,ar}`) through the shim
`.cargo/nix-cross-tool.sh` (installed under those three real tool names). The
shim execs a matching cross tool if one is already on `PATH` (inside
`nix develop`, or a Debian/CI host that renamed its `gcc-aarch64-linux-gnu` via
the overrides below) and otherwise resolves the SAME pinned toolchain straight
from the flake with `nix build .#crossToolchain`. So a **bare**

```sh
cargo build --release --target aarch64-unknown-linux-gnu -p bris-cli
```

cross-compiles correctly on any Nix host with no wrapping `nix develop` and no
manual step — which is exactly the appliance definition-of-done check. The
`rust-toolchain.toml` declares `targets = ["aarch64-unknown-linux-gnu"]`, so
`rustup` auto-provisions the aarch64 `rust-std` on first build. The `[env]`
entries use `force = false`, so an environment override (CI's Debian prefix, or
a developer's own toolchain) still wins.

### libclang for bindgen

`bris-cli` transitively runs `bindgen` at build time (`v4l2-sys-mit` via
`bris-capture`, and `ort-sys`), which needs **libclang** on the *host* (bindgen
parses C headers on x86_64 to emit target-agnostic Rust bindings — this is
independent of the aarch64 cross toolchain). If libclang is missing, `bindgen`
panics `Unable to find libclang … set LIBCLANG_PATH`.

Like the cross toolchain, libclang is resolved **reproducibly** so the bare DoD
check needs no host `libclang-dev` and no wrapping `nix develop`:

- `.cargo/config.toml` sets `LLVM_CONFIG_PATH` to the `.cargo/llvm-config` shim
  (a symlink to `.cargo/nix-cross-tool.sh`). `clang-sys` (bindgen's libclang
  loader) locates libclang via `LIBCLANG_PATH`, then by running
  `llvm-config --prefix` and searching `<prefix>/lib`. The shim answers
  `--prefix`/`--libdir` from `nix build .#libclang` (the pinned nix clang), so
  bindgen finds `<prefix>/lib/libclang.so*` on any Nix host. This is resolved at
  *exec* time when bindgen loads libclang, so there is no build-script ordering
  dependency (unlike the cross CC, which cargo only invokes for C deps).
- If a real `llvm-config` whose prefix actually ships a libclang is already on
  `PATH` (inside `nix develop` — the devShell also exports `LIBCLANG_PATH`
  directly — or a Debian/CI host with `libclang-dev`), the shim defers to it and
  nix is never invoked.
- `scripts/pi-appliance/build.sh` additionally exports an explicit
  `LIBCLANG_PATH` when it can discover one (host `llvm-config`, else
  `nix build .#libclang`) so the staging build is explicit and fast.

### Building in CI / on Debian

CI (`.github/workflows/ci.yml`, the `cross-build` job) installs Debian's
`gcc-aarch64-linux-gnu`, whose binaries carry the shorter `aarch64-linux-gnu-*`
prefix, plus `libclang-dev`. `build.sh` auto-detects that flavour and exports
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

Run inside `nix develop`, or bare on any Nix host (the `.cargo/nix-cross-tool.sh`
shim resolves the pinned toolchain), or with the Debian cross toolchain +
`libclang-dev` installed. A successful build produces
`target/aarch64-unknown-linux-gnu/release/bris`, an `ELF 64-bit LSB … ARM
aarch64` executable — confirm with `file` on it.
