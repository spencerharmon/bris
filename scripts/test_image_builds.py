"""Build the shipping Dockerfiles and run their binaries in the runtime images.

Run with python3 scripts/test_image_builds.py; set CONTAINER_ENGINE=podman
if Docker is unavailable. ARM64 needs a native runner or registered QEMU.
Individual unittest methods can be selected for native CI matrix jobs.
"""

import os
from pathlib import Path
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[1]
ENGINE = os.environ.get("CONTAINER_ENGINE", "docker")


class ImageBuilds(unittest.TestCase):
    def build_and_run(self, image, arch):
        tag = f"localhost/{image}-smoke:{arch}"
        subprocess.run([
            ENGINE, "build", "--platform", f"linux/{arch}",
            "-f", f"crates/{image}/Dockerfile", "-t", tag, ".",
        ], cwd=ROOT, check=True)
        result = subprocess.run([
            ENGINE, "run", "--rm", "--platform", f"linux/{arch}",
            "-e", "BRIS_COLLECTOR_DATA_ROOT=/tmp/collector-smoke",
            tag, "--help" if image == "bris-cli" else "index-rebuild",
        ], check=True, capture_output=True, text=True)
        if image == "bris-cli":
            self.assertIn("Usage:", result.stdout)
            self.assertIn("replay", result.stdout)
        else:
            self.assertIn("rebuilt index: 0 submissions", result.stdout)

    def test_collector_amd64(self):
        self.build_and_run("bris-collector", "amd64")

    def test_collector_arm64(self):
        self.build_and_run("bris-collector", "arm64")

    def test_cli_amd64(self):
        self.build_and_run("bris-cli", "amd64")


if __name__ == "__main__":
    unittest.main(verbosity=2)
