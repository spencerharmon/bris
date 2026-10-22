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
NATIVE_IMAGES = {"collector-image": "collector", "bris-cli-image": "cli"}


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
                build_steps = config["jobs"].get("build-native", job)["steps"]
                build = next(s for s in build_steps if s.get("uses", "").startswith("docker/build-push-action@"))
                if workflow not in NATIVE_IMAGES:
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

    def test_native_builds_test_before_publishing(self):
        for workflow, image in NATIVE_IMAGES.items():
            with self.subTest(workflow=workflow):
                config = yaml.safe_load(
                    (ROOT / ".github/workflows" / f"{workflow}.yml").read_text()
                )
                # PyYAML's YAML 1.1 loader treats the Actions `on` key as True.
                triggers = config[True]
                self.assertIn("workflow_dispatch", triggers)
                self.assertEqual(triggers["push"]["branches"], ["main"])
                self.assertTrue({
                    "crates/**", "Cargo.toml", "Cargo.lock", ".cargo/**",
                    "rust-toolchain.toml", ".dockerignore",
                    "scripts/test_container_images.py",
                    f".github/workflows/{workflow}.yml",
                }.issubset(triggers["push"]["paths"]))
                jobs = config["jobs"]
                self.assertEqual(set(jobs), {"build-native", "build-and-push"})
                native = jobs["build-native"]
                publish = jobs["build-and-push"]
                self.assertEqual(publish["needs"], "build-native")
                self.assertEqual(native["runs-on"], "${{ matrix.runner }}")
                self.assertEqual(native["strategy"]["matrix"], {"include": [
                    {"arch": "amd64", "runner": "ubuntu-latest"},
                    {"arch": "arm64", "runner": "ubuntu-24.04-arm"},
                ]})
                self.assertEqual(native["permissions"], publish["permissions"])
                self.assertNotIn("qemu", str(config).lower())
                self.assertNotIn("vars.", str(native))
                self.assertNotIn("secrets.REGISTRY_", str(native))
                for job in jobs.values():
                    self.assertNotIn("if", job)  # Dispatch on a branch must publish too.
                    self.assertNotIn("continue-on-error", job)
                    for step in job["steps"]:
                        self.assertNotIn("if", step)
                        self.assertNotIn("continue-on-error", step)
                steps = native["steps"]
                self.assertEqual(steps[0]["uses"], "actions/checkout@v4")
                self.assertNotIn("ref", steps[0].get("with", {}))
                self.assertEqual(
                    next(s for s in steps if s.get("id") == "image"),
                    next(s for s in publish["steps"] if s.get("id") == "image"),
                )
                for action in ("docker/login-action@", "docker/setup-buildx-action@"):
                    self.assertEqual(
                        next(s for s in steps if s.get("uses", "").startswith(action)),
                        next(s for s in publish["steps"] if s.get("uses", "").startswith(action)),
                    )
                build = next(s for s in steps if s.get("uses", "").startswith("docker/build-push-action@"))
                tag = "${{ steps.image.outputs.repository }}:${{ github.sha }}-${{ matrix.arch }}"
                self.assertEqual(build["with"]["context"], ".")
                self.assertEqual(build["with"]["file"], f"crates/{IMAGES[workflow]}/Dockerfile")
                self.assertEqual(build["with"]["platforms"], "linux/${{ matrix.arch }}")
                self.assertIs(build["with"]["push"], False)
                self.assertIs(build["with"]["load"], True)
                self.assertIs(build["with"]["provenance"], False)
                self.assertEqual(build["with"]["tags"], tag)
                scope = image + "-${{ matrix.arch }}"
                self.assertEqual(build["with"]["cache-from"], f"type=gha,scope={scope}")
                self.assertEqual(build["with"]["cache-to"], f"type=gha,mode=max,scope={scope}")
                smoke = steps[steps.index(build) + 1]
                self.assertEqual(smoke["env"], {"TAG": tag, "PLATFORM": "linux/${{ matrix.arch }}"})
                self.assertEqual(smoke["run"],
                                 f'python3 scripts/test_container_images.py --image {image} '
                                 '--platform "$PLATFORM" --tag "$TAG" --skip-build')
                push = steps[steps.index(smoke) + 1]
                self.assertEqual(push["env"], {"TAG": tag})
                self.assertEqual(push["run"], 'docker push "$TAG"')

    def test_manifest_publication(self):
        for workflow in NATIVE_IMAGES:
            config = yaml.safe_load(
                (ROOT / ".github/workflows" / f"{workflow}.yml").read_text()
            )
            steps = config["jobs"]["build-and-push"]["steps"]
            manifest = next(s for s in steps if s.get("id") == "build")
            self.assertEqual(manifest["env"], {
                "REPO": "${{ steps.image.outputs.repository }}", "SHA": "${{ github.sha }}",
                "REF": "${{ github.ref }}",
            })
            # Run the workflow shell with a fake Docker command: no registry or
            # builds, but real argument expansion and failure propagation.
            docker = '''
docker() {
    printf '%s\\n' "$*" >> "$CALLS"
    if [ "$3" = "$FAIL" ]; then return 1; fi
    if [ "$3" = inspect ]; then printf '%s\\n' "$DIGEST"; fi
}
'''
            for ref, fail in ((ref, fail) for ref in ("refs/heads/main", "refs/heads/feature")
                              for fail in ("", "create", "inspect")):
                with self.subTest(workflow=workflow, ref=ref, fail=fail), tempfile.TemporaryDirectory() as tmp:
                    output = Path(tmp) / "output"
                    calls = Path(tmp) / "calls"
                    repo = f"ghcr.io/exampleowner/{IMAGES[workflow]}"
                    sha = "b" * 40
                    digest = "sha256:" + "a" * 64
                    env = dict(os.environ, REPO=repo, SHA=sha, REF=ref, DIGEST=digest,
                               FAIL=fail, CALLS=str(calls), GITHUB_OUTPUT=str(output))
                    result = subprocess.run(["bash", "-e", "-c", docker + manifest["run"]],
                                            env=env, capture_output=True, text=True)
                    latest = f"--tag {repo}:latest " if ref == "refs/heads/main" else ""
                    expected = [f"buildx imagetools create --tag {repo}:{sha} {latest}"
                                f"{repo}:{sha}-amd64 {repo}:{sha}-arm64"]
                    if fail != "create":
                        expected.append(f"buildx imagetools inspect {repo}:{sha} "
                                        "--format {{.Manifest.Digest}}")
                    self.assertEqual(calls.read_text().splitlines(), expected)
                    if fail:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertFalse(output.exists())
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(output.read_text(), f"digest={digest}\n")


if __name__ == "__main__":
    unittest.main(verbosity=2)
