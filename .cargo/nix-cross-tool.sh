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
FLAKE_DIR="$(cd "$SELF_DIR/.." && pwd)"

# Look for a REAL tool of a given name on PATH, excluding this shim's own
# directory so we never recurse into ourselves. Prints the resolved path.
find_real() {
  local want="$1" IFS=:
  for d in $PATH; do
    [[ -z "$d" || "$d" == "$SELF_DIR" ]] && continue
    if [[ -x "$d/$want" ]]; then printf '%s\n' "$d/$want"; return 0; fi
  done
  return 1
}

# Resolve a pinned nix flake package to its store path(s). Prints one per line.
nix_out() {
  local attr="$1"
  command -v nix >/dev/null 2>&1 || return 1
  nix build --no-link --print-out-paths "$FLAKE_DIR#$attr" 2>/dev/null
}

# ---------------------------------------------------------------------------
# llvm-config shim (for bindgen/clang-sys libclang discovery).
#
# bindgen (v4l2-sys-mit via bris-capture, and ort-sys) dlopens libclang at build
# time; clang-sys locates it via LIBCLANG_PATH, then `llvm-config --prefix`
# (joined with `lib`). The bare appliance DoD check runs OUTSIDE `nix develop`
# and under a checkout-confined sandbox where the host libclang is not visible,
# so `.cargo/config.toml` points LLVM_CONFIG_PATH at this shim (installed as
# `.cargo/llvm-config`). If a REAL llvm-config whose prefix actually carries a
# libclang is on PATH (inside `nix develop`, or a Debian/CI host with
# llvm/libclang-dev), we defer to it; otherwise we `nix build .#libclang` and
# answer --prefix/--libdir/--bindir from that pinned store path so clang-sys
# finds `<prefix>/lib/libclang.so*`. EXEC-time resolution — no build-script
# ordering dependency. NO infra identifiers are baked in.
if [[ "$(basename "$0")" == "llvm-config" ]]; then
  # Prefer a real llvm-config ONLY if its prefix actually ships a libclang the
  # bare check can dlopen (a bare llvm-config with no libclang is useless here).
  if REAL="$(find_real llvm-config)"; then
    real_prefix="$("$REAL" --prefix 2>/dev/null || true)"
    if [[ -n "$real_prefix" ]] && compgen -G "$real_prefix/lib/libclang.so*" >/dev/null 2>&1; then
      exec "$REAL" "$@"
    fi
  fi
  LIBCLANG_OUT=""
  while IFS= read -r p; do
    [[ -z "$p" ]] && continue
    if compgen -G "$p/lib/libclang.so*" >/dev/null 2>&1; then LIBCLANG_OUT="$p"; break; fi
  done <<< "$(nix_out libclang || true)"
  if [[ -z "$LIBCLANG_OUT" ]]; then
    echo "nix-cross-tool(llvm-config): no libclang on PATH and could not resolve" >&2
    echo "  '$FLAKE_DIR#libclang' via nix. Enter 'nix develop' (flake.nix exports" >&2
    echo "  LIBCLANG_PATH), or (Debian/CI) install libclang-dev + llvm-config." >&2
    exit 127
  fi
  # Answer the queries clang-sys makes; default to the prefix for anything else.
  out=""
  for arg in "$@"; do
    case "$arg" in
      --prefix)     out="$LIBCLANG_OUT" ;;
      --libdir)     out="$LIBCLANG_OUT/lib" ;;
      --bindir)     out="$LIBCLANG_OUT/bin" ;;
      --includedir) out="$LIBCLANG_OUT/include" ;;
      *)            [[ -z "$out" ]] && out="$LIBCLANG_OUT" ;;
    esac
  done
  [[ -z "$out" && $# -eq 0 ]] && out="$LIBCLANG_OUT"
  printf '%s\n' "$out"
  exit 0
fi

case "$(basename "$0")" in
  *-gcc) SUFFIX=gcc ;;
  *-g++) SUFFIX=g++ ;;
  *-ar)  SUFFIX=ar ;;
  *)     echo "nix-cross-tool: unrecognized invocation name '$(basename "$0")'" >&2; exit 2 ;;
esac
TOOL="${TRIPLE}-${SUFFIX}"

if REAL="$(find_real "$TOOL")"; then
  exec "$REAL" "$@"
fi

# Reproducible fallback: resolve through the pinned flake. We use `nix build` of
# the `crossToolchain` package (flake.nix exposes it) to get the toolchain's
# store path deterministically WITHOUT entering the devShell — entering it would
# run the shellHook, whose banner pollutes stdout. The tool lives at
# <toolchain>/bin/<TOOL>.
if ! command -v nix >/dev/null 2>&1; then
  echo "nix-cross-tool: '$TOOL' not on PATH and 'nix' unavailable to resolve it." >&2
  echo "  Fix: install nix (flake at $FLAKE_DIR pins the toolchain), enter 'nix develop'," >&2
  echo "  or (Debian/CI) install gcc-aarch64-linux-gnu and export the config CC overrides" >&2
  echo "  (scripts/pi-appliance/build.sh does this automatically)." >&2
  exit 127
fi
TOOLCHAIN_PATHS="$(nix_out crossToolchain || true)"
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
