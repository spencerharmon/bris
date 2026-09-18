#!/usr/bin/env bash
# Regression test for the corpus-explorer static-server image.
#
# Builds the image and runs it with a throwaway "corpus data-root"
# mounted at /srv/corpus, then asserts the merged-web-root routing
# actually works in a running container:
#
#   1. GET /                          → 302 to the explorer entrypoint
#   2. GET /healthz                   → 200 ok
#   3. GET /tools/corpus-explorer/index.html → the baked SPA (200)
#   4. GET /index.json                → the mounted data-root file (200)
#   5. GET /tools/corpus-explorer/explorer.js → baked SPA asset (200)
#
# This FAILS if the routing is wrong (e.g. the PVC mount shadows the
# baked assets, or the data-root isn't served at the web root) and
# PASSES when the image serves the corpus correctly. It is the honest
# definition-of-done test the image-digest check gates on downstream.
#
# Usage:
#   tools/corpus-explorer/deploy/verify_image.sh [--engine docker|podman]
#
# Run from the repo root (the build context).

set -euo pipefail

ENGINE="docker"
if [[ "${1:-}" == "--engine" && -n "${2:-}" ]]; then
  ENGINE="$2"
fi
if ! command -v "$ENGINE" >/dev/null 2>&1; then
  if command -v podman >/dev/null 2>&1; then
    ENGINE="podman"
  else
    echo "verify_image.sh: neither docker nor podman found on PATH" >&2
    exit 127
  fi
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO_ROOT"

IMAGE="bris-corpus-explorer:verify-$$"
CONTAINER="bris-corpus-explorer-verify-$$"
DATAROOT="$(mktemp -d)"
FAILED=0

cleanup() {
  "$ENGINE" rm -f "$CONTAINER" >/dev/null 2>&1 || true
  "$ENGINE" rmi -f "$IMAGE" >/dev/null 2>&1 || true
  rm -rf "$DATAROOT"
}
trap cleanup EXIT

# A minimal but real corpus data-root: a schema-version-1 index.json
# the SPA would fetch at the web root.
cat > "$DATAROOT/index.json" <<'JSON'
{ "schema_version": 1, "sessions": [] }
JSON
# The unprivileged nginx user (uid 101) must be able to traverse and
# read the mounted data-root; mktemp -d is 0700 by default.
chmod 755 "$DATAROOT"
chmod 644 "$DATAROOT/index.json"

echo "==> building image ($ENGINE)"
BUILD_ARGS=()
if [[ "$ENGINE" == "podman" ]]; then
  # Emit docker-format so HEALTHCHECK is honoured (OCI drops it).
  BUILD_ARGS+=(--format docker)
fi
"$ENGINE" build "${BUILD_ARGS[@]}" -f tools/corpus-explorer/deploy/Dockerfile -t "$IMAGE" .

# Pick a free-ish high host port (auto-assign `:0:` is docker-only).
PORT=$(( (RANDOM % 20000) + 20000 ))

echo "==> starting container with data-root mounted at /srv/corpus"
# `:Z` relabels the bind mount for SELinux hosts (a no-op elsewhere);
# without it the container cannot read the mounted data-root.
MOUNT_OPTS="ro,Z"
if [[ "$ENGINE" == "docker" ]]; then
  MOUNT_OPTS="ro"
fi
"$ENGINE" run -d --name "$CONTAINER" \
  -v "$DATAROOT":/srv/corpus:"$MOUNT_OPTS" \
  -p "127.0.0.1:${PORT}:8080" \
  "$IMAGE" >/dev/null

BASE="http://127.0.0.1:${PORT}"

# Wait for nginx to answer.
for _ in $(seq 1 30); do
  if curl -sf -o /dev/null "$BASE/healthz"; then break; fi
  sleep 0.5
done

check() {
  local desc="$1"; shift
  if "$@"; then
    echo "PASS: $desc"
  else
    echo "FAIL: $desc"
    FAILED=1
  fi
}

# 1. bare root redirects to the explorer entrypoint. The Location's
# host:port is the container's own (8080), not the mapped host port,
# so assert only on the path.
loc="$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' "$BASE/")"
check "GET / → 302 redirect to the explorer entrypoint path" \
  bash -c "echo '$loc' | grep -Eq '^302 https?://[^/]+/tools/corpus-explorer/index.html$'"

# 2. healthz
check "GET /healthz → 200 ok" \
  bash -c "curl -sf \"$BASE/healthz\" | grep -qx ok"

# 3. baked SPA entrypoint reachable
check "GET /tools/corpus-explorer/index.html → 200 (baked SPA)" \
  bash -c "curl -sf \"$BASE/tools/corpus-explorer/index.html\" | grep -q 'Bris corpus explorer'"

# 4. mounted data-root file served at the web root
check "GET /index.json → 200 (mounted data-root)" \
  bash -c "curl -sf \"$BASE/index.json\" | grep -q '\"schema_version\": 1'"

# 5. baked SPA asset (proves the PVC mount did NOT shadow the assets)
check "GET /tools/corpus-explorer/explorer.js → 200 (baked asset, unshadowed)" \
  curl -sf -o /dev/null "$BASE/tools/corpus-explorer/explorer.js"

if [[ "$FAILED" -ne 0 ]]; then
  echo "==> verify_image.sh FAILED"
  exit 1
fi
echo "==> verify_image.sh PASSED"
