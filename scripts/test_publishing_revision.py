"""Exercise publishing revision selection against real temporary Git history."""

from contextlib import redirect_stderr, redirect_stdout
import io
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError


SCRIPT = Path(__file__).with_name("publishing_revision.py")


class PublishingRevision(unittest.TestCase):
    def test_publishing_revision_survives_later_pushes(self):
        for merge in (False, True):
            with self.subTest(merge=merge), tempfile.TemporaryDirectory() as tmp:
                def git(*args):
                    return subprocess.check_output(
                        ["git", "-C", tmp, *args], text=True, stderr=subprocess.PIPE
                    ).strip()

                git("init", "-b", "main")
                git("config", "user.name", "Test")
                git("config", "user.email", "test@example.com")
                git("remote", "add", "origin", "git@github.com:example/bris.git")
                git("commit", "--allow-empty", "-m", "base")
                base = git("rev-parse", "HEAD")
                git("checkout", "-b", "implementation")
                git("commit", "--allow-empty", "-m", "migration")
                anchor = git("rev-parse", "HEAD")
                git("commit", "--allow-empty", "-m", "follow-up in same push")
                git("checkout", "main")
                git("update-ref", "refs/remotes/origin/main", "HEAD")

                pages = [[]]

                def fetch(request, timeout):
                    self.assertEqual(timeout, 30)
                    page = int(request.full_url.rsplit("=", 1)[1])
                    self.assertEqual(request.full_url,
                        "https://api.github.com/repos/example/bris/actions/workflows/"
                        f"collector-image.yml/runs?per_page=100&page={page}")
                    return io.BytesIO(json.dumps({"workflow_runs": pages[page - 1]}).encode())

                def resolve(revision, fetcher=fetch):
                    before = os.getcwd()
                    output = io.StringIO()
                    try:
                        os.chdir(tmp)
                        with patch.object(sys, "argv", [str(SCRIPT), revision]), \
                                patch("urllib.request.urlopen", side_effect=fetcher), \
                                redirect_stdout(output), redirect_stderr(io.StringIO()):
                            try:
                                runpy.run_path(str(SCRIPT), run_name="__main__")
                            except SystemExit as error:
                                return subprocess.CompletedProcess([], 1, output.getvalue(), str(error))
                        return subprocess.CompletedProcess([], 0, output.getvalue(), "")
                    finally:
                        os.chdir(before)

                def run(sha, **extra):
                    return dict(head_sha=sha, event="push", head_branch="main", **extra)

                self.assertNotEqual(resolve(anchor).returncode, 0)
                git("merge", "--no-ff" if merge else "--ff-only", "implementation",
                    "-m", "publish")
                published = git("rev-parse", "HEAD")
                git("update-ref", "refs/remotes/origin/main", "HEAD")
                self.assertNotEqual(resolve(anchor).returncode, 0)  # No run yet.
                pages[:] = [[run(published, conclusion=None), run(base)]]
                self.assertEqual(resolve(anchor).stdout.strip(), published)
                git("commit", "--allow-empty", "-m", "unrelated later push")
                git("update-ref", "refs/remotes/origin/main", "HEAD")
                # New main tip has no image run; do not infer publication from it.
                result = resolve(anchor)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), published)

                later = git("rev-parse", "HEAD")
                # Pagination, newer successful builds, and a failed first build
                # must still select the original publishing push's actual tip.
                pages[:] = [[run(later, conclusion="success")] * 100,
                            [run(published, conclusion="failure"), run(base)]]
                result = resolve(anchor)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), published)

                for event, branch in (("pull_request", "main"),
                                      ("workflow_dispatch", "main"),
                                      ("push", "implementation")):
                    pages[:] = [[dict(head_sha=published, event=event, head_branch=branch),
                                run(base)]]
                    self.assertNotEqual(resolve(anchor).returncode, 0)

                # An existing object with a run is insufficient if it is not
                # on the first-parent mainline (or does not contain the anchor).
                pages[:] = [[run(anchor), run(base)]]
                if merge:
                    self.assertNotEqual(resolve(anchor).returncode, 0)
                pages[:] = [[run(base)]]
                self.assertNotEqual(resolve(anchor).returncode, 0)
                self.assertNotEqual(resolve("main").returncode, 0)
                self.assertNotEqual(resolve("f" * 40).returncode, 0)

                def unavailable(request, timeout):
                    error = HTTPError(request.full_url, 403, "rate limited", {}, io.BytesIO())
                    error.close()
                    raise error

                result = resolve(anchor, unavailable)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn("403", result.stderr)

                # Both common origin transports resolve the same public API.
                pages[:] = [[run(published)]]
                git("remote", "set-url", "origin", "https://github.com/example/bris.git")
                self.assertEqual(resolve(anchor).stdout.strip(), published)


if __name__ == "__main__":
    unittest.main(verbosity=2)
