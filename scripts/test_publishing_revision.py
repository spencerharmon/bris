"""Exercise publishing revision selection against real temporary Git history."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


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
                git("commit", "--allow-empty", "-m", "base")
                git("checkout", "-b", "implementation")
                git("commit", "--allow-empty", "-m", "migration")
                anchor = git("rev-parse", "HEAD")
                git("checkout", "main")
                git("update-ref", "refs/remotes/origin/main", "HEAD")

                def resolve(revision):
                    return subprocess.run(
                        [sys.executable, str(SCRIPT), revision], cwd=tmp,
                        capture_output=True, text=True,
                    )

                self.assertNotEqual(resolve(anchor).returncode, 0)
                git("merge", "--no-ff" if merge else "--ff-only", "implementation",
                    "-m", "publish")
                published = git("rev-parse", "HEAD")
                git("commit", "--allow-empty", "-m", "unrelated later push")
                git("update-ref", "refs/remotes/origin/main", "HEAD")
                result = resolve(anchor)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), published)
                self.assertNotEqual(resolve("main").returncode, 0)
                self.assertNotEqual(resolve("f" * 40).returncode, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
