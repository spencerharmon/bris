#!/usr/bin/env bash
# bris Pi Zero 2W (aarch64) appliance build + image-assembly recipe.
#
# Produces the single-binary-per-device appliance bundle: the `bris` CLI
# cross-compiled for aarch64-unknown-linux-gnu plus the runtime prerequisites a
# Pi Zero 2W appliance needs (camera/V4L2 access, serial + UDP NMEA sinks, the
# bris-data ml-gravity payload). The output is a self-contained staging tree
# (and, optionally, a tarball) that a Pi image builder (`rpi-image-gen`,
# `pi-gen`, or a plain `dd`-to-SD overlay) drops onto the device rootfs.
#
# REPRODUCIBILITY: the cross toolchain is pinned by ../../flake.nix. Run this
# inside `nix develop` (which puts `aarch64-unknown-linux-gnu-gcc` on PATH), or
# in CI where the Debian `gcc-aarch64-linux-gnu` toolchain is installed (this
# script auto-detects that prefix and exports the CC/linker overrides). No host
# `sudo`/apt is required in the nix path.
#
# LIBCLANG: bris-cli transitively needs libclang for bindgen (v4l2-sys-mit via
# bris-capture, and ort-sys) even though it targets aarch64 — bindgen itself
# runs on the HOST to parse C headers. `nix develop` exports LIBCLANG_PATH for
# you (see flake.nix); outside nix (e.g. a bare Debian/CI host) install
# `libclang-dev` (which provides `libclang.so`) and either export
# LIBCLANG_PATH yourself or let this script auto-detect it via `llvm-config`.
#
# NO INFRA IDENTIFIERS: this recipe bakes in zero site-specific facts —
# hostnames, IPs, device names, credentials, NMEA peer addresses, and camera
# device paths are all runtime/first-boot configuration (see the generated
# bris-appliance.env template and the operator doc), never build inputs.
#
# Usage:
#   scripts/pi-appliance/build.sh [--out DIR] [--tarball] [--no-data]
#
# Options:
#   --out DIR    staging output directory (default: target/pi-appliance)
#   --tarball    also emit <out>.tar.gz of the staging tree
#   --no-data    skip staging the bris-data ml-gravity payload (binary only)
set -euo pipefail

TARGET="aarch64-unknown-linux-gnu"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$REPO_ROOT/target/pi-appliance"
MAKE_TARBALL=0
STAGE_DATA=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out) OUT="$2"; shift 2 ;;
    --tarball) MAKE_TARBALL=1; shift ;;
    --no-data) STAGE_DATA=0; shift ;;
    -h|--help) grep -E '^#( |$)' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

cd "$REPO_ROOT"

# --- Toolchain discovery -----------------------------------------------------
# Prefer the nix/target-triple prefix (aarch64-unknown-linux-gnu-gcc, what
# .cargo/config.toml names and flake.nix provides). Fall back to the Debian
# short prefix (aarch64-linux-gnu-gcc) and export the overrides so both cargo's
# linker and cc-rs pick it up.
if command -v aarch64-unknown-linux-gnu-gcc >/dev/null 2>&1; then
  echo "using cross toolchain: aarch64-unknown-linux-gnu-* (target-triple prefix)"
elif command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then
  echo "using cross toolchain: aarch64-linux-gnu-* (Debian prefix); exporting overrides"
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
  export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
  export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
  export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
else
  echo "ERROR: no aarch64 GNU cross toolchain on PATH." >&2
  echo "  Reproducible fix: run inside 'nix develop' (see flake.nix)." >&2
  echo "  Debian/CI fix:    apt-get install gcc-aarch64-linux-gnu g++-aarch64-linux-gnu" >&2
  exit 1
fi

# --- libclang discovery for bindgen ------------------------------------------
# v4l2-sys-mit (via bris-capture) and ort-sys both invoke bindgen at build
# time, which needs libclang. `nix develop` already exports LIBCLANG_PATH (see
# flake.nix); if it's unset (e.g. invoked outside nix), try to discover a
# usable libclang so the build doesn't blindly panic with an opaque bindgen
# error.
if [[ -z "${LIBCLANG_PATH:-}" ]]; then
  if command -v llvm-config >/dev/null 2>&1; then
    CANDIDATE="$(llvm-config --libdir 2>/dev/null || true)"
    if [[ -n "$CANDIDATE" && -e "$CANDIDATE/libclang.so" ]]; then
      export LIBCLANG_PATH="$CANDIDATE"
    fi
  fi
fi
if [[ -z "${LIBCLANG_PATH:-}" ]]; then
  echo "WARNING: LIBCLANG_PATH is not set and no libclang could be auto-detected." >&2
  echo "  Reproducible fix: run inside 'nix develop' (flake.nix exports it)." >&2
  echo "  Debian/CI fix:    apt-get install libclang-dev, then export" >&2
  echo "                    LIBCLANG_PATH=\$(llvm-config --libdir)" >&2
  echo "  Continuing — bindgen will fail below if libclang truly isn't findable." >&2
else
  echo "using libclang: $LIBCLANG_PATH"
fi

# --- Cross-compile the CLI ---------------------------------------------------
# This is exactly the task's definition-of-done check command.
echo "==> cross-compiling bris-cli for $TARGET"
cargo build --release --target "$TARGET" -p bris-cli

BIN="target/$TARGET/release/bris"
[[ -x "$BIN" ]] || { echo "ERROR: expected binary $BIN not produced" >&2; exit 1; }
echo "==> built $(file -b "$BIN")"

# --- Assemble the appliance staging tree -------------------------------------
echo "==> assembling appliance staging tree at $OUT"
rm -rf "$OUT"
mkdir -p "$OUT/usr/local/bin" "$OUT/etc/bris" "$OUT/etc/systemd/system"

install -m 0755 "$BIN" "$OUT/usr/local/bin/bris"

# bris-data ml-gravity payload the CLI loads at runtime (segmentation +
# ml-gravity features). Staged read-only under /usr/share/bris.
if [[ "$STAGE_DATA" -eq 1 ]]; then
  echo "==> staging bris-data ml-gravity payload"
  mkdir -p "$OUT/usr/share/bris/ml-gravity"
  # Copy the payload EXCEPT the training scaffolding (not needed on-device).
  for f in data/ml-gravity/*; do
    base="$(basename "$f")"
    [[ "$base" == "training" ]] && continue
    cp -a "$f" "$OUT/usr/share/bris/ml-gravity/"
  done
fi

# Runtime config template — placeholders ONLY. Every value is filled at
# flash/first-boot time by the operator; NO real hostnames/IPs/devices here.
cat > "$OUT/etc/bris/bris-appliance.env" <<'ENV'
# bris Pi appliance runtime configuration (fill at flash/first-boot).
# These are PLACEHOLDERS — no site-specific values are baked into the image.
#
# Camera capture device (V4L2). The Pi camera usually enumerates as /dev/video0.
BRIS_CAMERA_DEVICE=/dev/video0
#
# NMEA sink: serial and/or UDP. Leave a sink blank to disable it.
#   Serial: a tty device + baud (e.g. an autopilot / chartplotter on RS-422).
BRIS_NMEA_SERIAL_DEVICE=
BRIS_NMEA_SERIAL_BAUD=4800
#   UDP: host:port of the NMEA consumer on the local network.
BRIS_NMEA_UDP_TARGET=
#
# ml-gravity payload location (staged by the image assembly).
BRIS_DATA_ROOT=/usr/share/bris
ENV

# systemd unit that runs the appliance as the single on-device frontend.
# References ONLY the env template above; carries no embedded identifiers.
cat > "$OUT/etc/systemd/system/bris-appliance.service" <<'UNIT'
[Unit]
Description=bris celestial-navigation appliance (single-binary frontend)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=/etc/bris/bris-appliance.env
# The concrete `bris` subcommand/flags are the on-device frontend invocation;
# operators tailor ExecStart to the deployment. Default: continuous streaming.
ExecStart=/usr/local/bin/bris stream
Restart=on-failure
RestartSec=5
# Camera + serial access; adjust to the deployment's hardening policy.
SupplementaryGroups=video dialout

[Install]
WantedBy=multi-user.target
UNIT

# Record the exact toolchain + source provenance next to the binary so the
# image is auditable and the build is traceable.
{
  echo "bris Pi appliance staging tree"
  echo "built:        $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "target:       $TARGET"
  echo "git-rev:      $(git rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "binary:       $(file -b "$BIN")"
  echo "cross-cc:     $(command -v aarch64-unknown-linux-gnu-gcc || command -v aarch64-linux-gnu-gcc)"
} > "$OUT/BUILDINFO.txt"

echo "==> staging tree contents:"
find "$OUT" -type f | sort | sed 's/^/    /'

if [[ "$MAKE_TARBALL" -eq 1 ]]; then
  TARBALL="$OUT.tar.gz"
  echo "==> writing tarball $TARBALL"
  tar -C "$OUT" -czf "$TARBALL" .
  echo "    $(du -h "$TARBALL" | cut -f1) $TARBALL"
fi

echo "==> done. Overlay $OUT/ onto the Pi rootfs (see docs/operator/pi-appliance.md)."
