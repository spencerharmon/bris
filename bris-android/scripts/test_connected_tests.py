import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import os
import subprocess


spec = importlib.util.spec_from_file_location("connected_tests", Path(__file__).with_name("connected_tests.py"))
harness = importlib.util.module_from_spec(spec)
spec.loader.exec_module(harness)


class InstrumentationResultsTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.directory = Path(self.tmp.name)

    def report(self, classes, status=""):
        cases = "".join(f'<testcase classname="{name}" name="test">{status}</testcase>'
                        for name in classes)
        (self.directory / "TEST-device.xml").write_text(f"<testsuite>{cases}</testsuite>")

    def test_requires_real_results(self):
        with self.assertRaises(RuntimeError):
            harness.verify_results(self.directory)

    def test_rejects_zero_tests(self):
        self.report([])
        with self.assertRaises(RuntimeError):
            harness.verify_results(self.directory)

    def test_requires_both_classes(self):
        self.report(harness.REQUIRED_CLASSES[:1])
        with self.assertRaises(RuntimeError):
            harness.verify_results(self.directory)

    def test_rejects_nonpassing_tests(self):
        for tag in ("failure", "error", "skipped"):
            with self.subTest(tag=tag):
                self.report(harness.REQUIRED_CLASSES, f"<{tag}/>")
                with self.assertRaises(RuntimeError):
                    harness.verify_results(self.directory)

    def test_accepts_both_executed_classes(self):
        self.report(harness.REQUIRED_CLASSES)
        harness.verify_results(self.directory)


class HarnessEntryTest(unittest.TestCase):
    def test_wrapper_routes_exact_task_and_preserves_arguments(self):
        # A fake Python executable exercises the actual POSIX wrapper dispatch,
        # without provisioning an SDK or accidentally running Gradle.
        with tempfile.TemporaryDirectory() as directory:
            python = Path(directory) / "python3"
            python.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n')
            python.chmod(0o755)
            env = dict(os.environ, PATH=directory + os.pathsep + os.environ["PATH"])
            env.pop("BRIS_CONNECTED_DEVICE", None)
            for task in ("connectedDebugAndroidTest", ":app:connectedDebugAndroidTest"):
                result = subprocess.run(
                    [str(harness.ANDROID / "gradlew"), "-p", "path with spaces", task],
                    env=env, capture_output=True, text=True, check=True)
                self.assertEqual(result.stdout.splitlines(),
                                 [str(harness.ANDROID / "scripts/connected_tests.py"),
                                  "-p", "path with spaces", task])

    def test_state_survives_clean_and_avd_is_removed_on_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            android = Path(directory)
            homes = []

            def failing_run(args, state, avd_home):
                self.assertEqual(state, android / ".connected-harness")
                self.assertTrue(Path(avd_home).is_dir())
                homes.append(Path(avd_home))
                raise RuntimeError("provisioning failed")

            with patch.object(harness, "ANDROID", android), \
                    patch.object(harness.os, "access", return_value=True), \
                    patch.object(harness, "execute", side_effect=failing_run):
                with self.assertRaisesRegex(RuntimeError, "provisioning failed"):
                    harness.main(["connectedDebugAndroidTest"])
            self.assertFalse(homes[0].exists())

    def test_rejects_local_sdk_override_before_provisioning(self):
        with tempfile.TemporaryDirectory() as directory:
            android = Path(directory)
            (android / "local.properties").write_text("sdk.dir=/shared/sdk\n")
            with patch.object(harness, "ANDROID", android), \
                    patch.object(harness.os, "access", return_value=True), \
                    patch.object(harness, "execute") as execute:
                with self.assertRaisesRegex(RuntimeError, "local.properties"):
                    harness.main(["connectedDebugAndroidTest"])
                execute.assert_not_called()


if __name__ == "__main__":
    unittest.main()
