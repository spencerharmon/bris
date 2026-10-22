"""GHCR workflow contracts and executable image/digest step regression tests.

Run with python3 scripts/test_image_workflows.py (requires PyYAML).
Actual registry publication is verified by skopeo in each publishing workflow.
"""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]
IMAGES = {
    "collector-image": "bris-collector",
    "corpus-explorer-image": "bris-corpus-explorer",
    "bris-cli-image": "bris-cli",
}


class ImageWorkflows(unittest.TestCase):
    def test_ghcr_publishing(self):
        for workflow, image in IMAGES.items():
            with self.subTest(workflow=workflow):
                config = yaml.safe_load(
                    (ROOT / ".github/workflows" / f"{workflow}.yml").read_text()
                )
                job = config["jobs"]["build-and-push"]
                self.assertEqual(job["permissions"], {
                    "contents": "read", "packages": "write",
                })
                steps = job["steps"]
                login = next(s for s in steps if s.get("uses", "").startswith("docker/login-action@"))
                self.assertEqual(login["with"], {
                    "registry": "ghcr.io",
                    "username": "${{ github.actor }}",
                    "password": "${{ secrets.GITHUB_TOKEN }}",
                })
                build = next(s for s in steps if s.get("id") == "build")
                self.assertTrue(build["with"]["push"])
                self.assertEqual(build["with"]["tags"], "${{ steps.image.outputs.repository }}:${{ github.sha }}")
                self.assertIn(
                    "org.opencontainers.image.source=https://github.com/${{ github.repository }}",
                    build["with"]["labels"].splitlines(),
                )
                self.assertNotIn("vars.", str(job))
                self.assertNotIn("secrets.REGISTRY_", str(job))
                naming = next(s for s in steps if s.get("id") == "image")
                self.assertEqual(naming["env"]["OWNER"], "${{ github.repository_owner }}")
                record = next(s for s in steps if s.get("id") == "push")
                self.assertEqual(record["env"]["REPO"], "${{ steps.image.outputs.repository }}")
                self.assertEqual(record["env"]["DIGEST"], "${{ steps.build.outputs.digest }}")
                self.assertEqual(job["outputs"], {
                    "digest": "${{ steps.push.outputs.digest }}",
                    "image": "${{ steps.push.outputs.image }}",
                })
                verify = steps[-1]
                self.assertIn("skopeo inspect --no-creds", verify["run"])
                self.assertNotIn("secrets.", str(verify))
                self.assertIn("${{ steps.push.outputs.image }}", verify["env"].values())

                # Execute the real shell steps, including mixed-case owner and
                # the missing-digest failure path; do not just match source text.
                with tempfile.TemporaryDirectory() as tmp:
                    output = Path(tmp) / "output"
                    summary = Path(tmp) / "summary"
                    env = dict(os.environ, OWNER="ExampleOwner",
                               GITHUB_OUTPUT=str(output), GITHUB_STEP_SUMMARY=str(summary))
                    subprocess.run(["bash", "-e", "-c", naming["run"]], env=env, check=True)
                    repo = f"ghcr.io/exampleowner/{image}"
                    self.assertEqual(output.read_text(), f"repository={repo}\n")
                    digest = "sha256:" + "a" * 64
                    env.update(REPO=repo, DIGEST=digest)
                    subprocess.run(["bash", "-e", "-c", record["run"]], env=env, check=True)
                    self.assertIn(f"digest={digest}\nimage={repo}@{digest}\n", output.read_text())
                    self.assertIn(f"{repo}@{digest}", summary.read_text())
                    before = output.read_text()
                    env["DIGEST"] = ""
                    failed = subprocess.run(["bash", "-e", "-c", record["run"]],
                                            env=env, capture_output=True, text=True)
                    self.assertNotEqual(failed.returncode, 0)
                    self.assertIn("did not emit a digest", failed.stderr)
                    self.assertEqual(output.read_text(), before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
