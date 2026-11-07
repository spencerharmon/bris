#!/usr/bin/env bash
# Keep the documented entry point on the same runtime checks used before publishing.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
exec python3 "$REPO_ROOT/scripts/test_container_images.py" --image explorer --platform linux/amd64 "$@"
