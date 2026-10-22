"""Regression coverage for the exact-tree native image CI gate."""

import unittest
from unittest.mock import patch

from verify_container_builds import verify


class ContainerBuildGate(unittest.TestCase):
    def response(self, path):
        if "/git/commits/" in path:
            return {"tree": {"sha": self.tree}}
        if "/jobs?" in path:
            return {"jobs": self.jobs}
        return {"workflow_runs": [{"status": "completed", "conclusion": self.conclusion,
                                  "head_sha": "commit", "id": 1, "html_url": "https://example.com/run/1"}]}

    def setUp(self):
        self.tree = "checkout-tree"
        self.conclusion = "success"
        self.jobs = [
            {"name": f"build-native ({arch}, {runner})", "conclusion": "success", "steps": [
                {"name": name, "conclusion": "success"} for name in (
                    "Build and load native image", "Run native container regressions", "Push tested native image")
            ]} for arch, runner in (("amd64", "ubuntu-latest"), ("arm64", "ubuntu-24.04-arm"))
        ] + [{"name": "build-and-push", "conclusion": "success", "steps": [
            {"name": name, "conclusion": "success"} for name in (
                "Publish multiarch manifest", "Verify the pushed image is pullable by digest")
        ]}]
        patcher = patch("verify_container_builds.api", side_effect=self.response)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_successful_exact_tree(self):
        verify("example/project", "checkout-tree")

    def test_rejects_other_tree(self):
        self.tree = "different-tree"
        with self.assertRaisesRegex(RuntimeError, "no successful"):
            verify("example/project", "checkout-tree")

    def test_rejects_failed_run(self):
        self.conclusion = "failure"
        with self.assertRaisesRegex(RuntimeError, "no successful"):
            verify("example/project", "checkout-tree")

    def test_rejects_missing_arm64(self):
        del self.jobs[1]
        with self.assertRaisesRegex(RuntimeError, "missing successful job"):
            verify("example/project", "checkout-tree")

    def test_rejects_skipped_runtime(self):
        self.jobs[1]["steps"][1]["conclusion"] = "skipped"
        with self.assertRaisesRegex(RuntimeError, "missing build/runtime"):
            verify("example/project", "checkout-tree")

    def test_rejects_failed_anonymous_pull(self):
        self.jobs[2]["steps"][1]["conclusion"] = "failure"
        with self.assertRaisesRegex(RuntimeError, "missing build/runtime"):
            verify("example/project", "checkout-tree")


if __name__ == "__main__":
    unittest.main(verbosity=2)
