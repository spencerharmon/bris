#!/usr/bin/env bash
# Reproducible aarch64 cross-tool shim for the Pi Zero 2W appliance build.
#
# The appliance definition-of-done check is a BARE
#   cargo build --release --target aarch64-unknown-linux-gnu -p bris-cli
# run WITHOUT a wrapping `nix develop`. Cargo (as the target linker) and cc-rs
# (as CC/CXX/AR for native deps) look up the cross tools by their target-triple
# names `aarch64-unknown-linux-gnu-{gcc,g++,ar}`. On a host not already inside
# `nix develop`, those are not on PATH. This shim — installed under each of those
# three real names — resolves the SAME pinned toolchain from flake.nix's devShell
# and execs it, so the bare check is reproducible on any nix host with no apt/sudo
# and no manual `nix develop` step. If a real cross tool of that name is already
# on PATH (inside `nix develop`, or a Debian/CI host that renamed its
# gcc-aarch64-linux-gnu via the config overrides), that is used and nix is never
# invoked.
#
# NO infra identifiers are baked in here.
set -euo pipefail

TRIPLE="aarch64-unknown-linux-gnu"
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
case "$(basename "$0")" in
  *-gcc) SUFFIX=gcc ;;
  *-g++) SUFFIX=g++ ;;
  *-ar)  SUFFIX=ar ;;
  *)     echo "nix-cross-tool: unrecognized invocation name '$(basename "$0")'" >&2; exit 2 ;;
esac
TOOL="${TRIPLE}-${SUFFIX}"

# Look for a REAL tool of this name on PATH, excluding this shim's own directory
# so we never recurse into ourselves.
find_real() {
  local IFS=:
  for d in $PATH; do
    [[ -z "$d" || "$d" == "$SELF_DIR" ]] && continue
    if [[ -x "$d/$TOOL" ]]; then printf '%s\n' "$d/$TOOL"; return 0; fi
  done
  return 1
}

if REAL="$(find_real)"; then
  exec "$REAL" "$@"
fi

# Reproducible fallback: resolve through the pinned flake. We use `nix build` of
# the `crossToolchain` package (flake.nix exposes it) to get the toolchain's
# store path deterministically WITHOUT entering the devShell — entering it would
# run the shellHook, whose banner pollutes stdout. The tool lives at
# <toolchain>/bin/<TOOL>.
FLAKE_DIR="$(cd "$SELF_DIR/.." && pwd)"
if ! command -v nix >/dev/null 2>&1; then
  echo "nix-cross-tool: '$TOOL' not on PATH and 'nix' unavailable to resolve it." >&2
  echo "  Fix: install nix (flake at $FLAKE_DIR pins the toolchain), enter 'nix develop'," >&2
  echo "  or (Debian/CI) install gcc-aarch64-linux-gnu and export the config CC overrides" >&2
  echo "  (scripts/pi-appliance/build.sh does this automatically)." >&2
  exit 127
fi
TOOLCHAIN_PATHS="$(nix build --no-link --print-out-paths "$FLAKE_DIR#crossToolchain" 2>/dev/null)"
RESOLVED=""
while IFS= read -r p; do
  [[ -x "$p/bin/$TOOL" ]] && { RESOLVED="$p/bin/$TOOL"; break; }
done <<< "$TOOLCHAIN_PATHS"
if [[ -z "$RESOLVED" ]]; then
  echo "nix-cross-tool: no '$TOOL' found under the nix crossToolchain outputs:" >&2
  echo "$TOOLCHAIN_PATHS" >&2
  exit 127
fi
exec "$RESOLVED" "$@"
