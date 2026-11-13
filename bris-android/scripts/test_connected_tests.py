import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import MagicMock, patch
from contextlib import redirect_stdout
import io
import os
import subprocess
import hashlib
import json


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
                        for name in classes for _ in range(harness.MINIMUM_TESTS[name]))
        (self.directory / "TEST-device.xml").write_text(f"<testsuite>{cases}</testsuite>")

    def test_requires_real_results(self):
        with self.assertRaises(RuntimeError):
            harness.verify_results(self.directory)

    def test_rejects_zero_tests(self):
        self.report([])
        with self.assertRaises(RuntimeError):
            harness.verify_results(self.directory)

    def test_requires_every_class(self):
        for missing in harness.REQUIRED_CLASSES:
            self.report([name for name in harness.REQUIRED_CLASSES if name != missing])
            with self.assertRaises(RuntimeError):
                harness.verify_results(self.directory)

    def test_rejects_partial_original_suite(self):
        self.report(harness.REQUIRED_CLASSES)
        report = self.directory / "TEST-device.xml"
        report.write_text(report.read_text().replace('<testcase ', '<removed ', 1)
                          .replace('</testcase>', '</removed>', 1))
        with self.assertRaises(RuntimeError):
            harness.verify_results(self.directory)

    def test_rejects_nonpassing_tests(self):
        for tag in ("failure", "error", "skipped"):
            with self.subTest(tag=tag):
                self.report(harness.REQUIRED_CLASSES, f"<{tag}/>")
                with self.assertRaises(RuntimeError):
                    harness.verify_results(self.directory)

    def test_accepts_all_executed_classes(self):
        self.report(harness.REQUIRED_CLASSES)
        harness.verify_results(self.directory)


class CollectorEvidenceTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.device = self.root / "device"
        self.data = self.root / "data"
        session = self.device / "sessions/session-id"
        session.mkdir(parents=True)
        (session / "session.json").write_text(json.dumps({
            "session_id": "session-id", "ordered_capture_ids": ["first", "second"],
        }))
        for capture_id in ("first", "second"):
            capture = session / "captures" / capture_id
            capture.mkdir(parents=True)
            (capture / "manifest.json").write_text('{"submission_kind":"fix"}')
            received = self.data / "submissions/2026/01/01" / capture_id
            (received / "media").mkdir(parents=True)
            files = {"bundle.json": json.dumps({"bundle_id": capture_id, "session_id": "session-id"}).encode(),
                     "pbris.log": b""}
            media = []
            for name, content in files.items():
                (capture / name).write_bytes(content)
                (received / ("pbris.log" if name == "pbris.log" else f"media/{name}")).write_bytes(content)
                media.append(dict(filename=name, size_bytes=len(content),
                                  role="pbris_log" if name == "pbris.log" else "bundle_manifest",
                                  checksum_sha256=hashlib.sha256(content).hexdigest()))
            (received / "manifest.json").write_text(json.dumps({"media": media}))

    def test_accepts_identical_canonical_bytes(self):
        harness.verify_collector(self.data, self.device)

    def test_rejects_reserialized_bundle(self):
        bundle = self.data / "submissions/2026/01/01/first/media/bundle.json"
        bundle.write_text(json.dumps(json.loads(bundle.read_text()), indent=2))
        with self.assertRaisesRegex(RuntimeError, "changed canonical bytes"):
            harness.verify_collector(self.data, self.device)

    def test_rejects_duplicate_submission(self):
        import shutil
        submissions = self.data / "submissions/2026/01/01"
        shutil.rmtree(submissions / "second")
        shutil.copytree(submissions / "first", submissions / "duplicate")
        with self.assertRaisesRegex(RuntimeError, "duplicated or omitted"):
            harness.verify_collector(self.data, self.device)

    def test_rejects_missing_log(self):
        manifest = self.data / "submissions/2026/01/01/first/manifest.json"
        value = json.loads(manifest.read_text())
        value["media"] = value["media"][:1]
        manifest.write_text(json.dumps(value))
        with self.assertRaisesRegex(RuntimeError, "media set"):
            harness.verify_collector(self.data, self.device)

    def test_rejects_wrong_checksum(self):
        manifest = self.data / "submissions/2026/01/01/first/manifest.json"
        value = json.loads(manifest.read_text())
        value["media"][0]["checksum_sha256"] = "0" * 64
        manifest.write_text(json.dumps(value))
        with self.assertRaisesRegex(RuntimeError, "changed canonical bytes"):
            harness.verify_collector(self.data, self.device)

    def test_rejects_missing_sight_log_manifest(self):
        (self.device / "sessions/session-id/captures/first/manifest.json").unlink()
        with self.assertRaisesRegex(RuntimeError, "Missing canonical artifact"):
            harness.verify_collector(self.data, self.device)

    def test_rejects_unbound_capture(self):
        path = self.data / "submissions/2026/01/01/first/media/bundle.json"
        path.write_text('{"bundle_id":"first"}')
        with self.assertRaisesRegex(RuntimeError, "unbound or unexpected"):
            harness.verify_collector(self.data, self.device)

    def test_rejects_orphan_artifact(self):
        orphan = self.device / "sights/orphan"
        orphan.mkdir(parents=True)
        (orphan / "bundle.json").write_text("{}")
        with self.assertRaisesRegex(RuntimeError, "Legacy orphan"):
            harness.verify_collector(self.data, self.device)

    def test_fix_frame_bytes_are_checked(self):
        capture = self.device / "sessions/session-id/captures/first"
        (capture / "frames").mkdir()
        received = self.data / "submissions/2026/01/01/first"
        manifest = received / "manifest.json"
        value = json.loads(manifest.read_text())
        for name, content in {
            "000001.pgm": b"P5\n2 1\n255\n\x00\xff",
            "000001.json": b'{"retention":"fix_frame"}',
        }.items():
            (capture / "frames" / name).write_bytes(content)
            (received / "media" / name).write_bytes(content)
            value["media"].append(dict(filename=name, role="fix_frame", size_bytes=len(content),
                                      checksum_sha256=hashlib.sha256(content).hexdigest()))
        manifest.write_text(json.dumps(value))
        harness.verify_collector(self.data, self.device)
        (received / "media/000001.pgm").write_bytes(b"P5\n2 1\n255\n\xff\x00")
        with self.assertRaisesRegex(RuntimeError, "changed canonical bytes"):
            harness.verify_collector(self.data, self.device)

    def test_rejects_debug_archival(self):
        frames = self.device / "sessions/session-id/captures/first/frames"
        frames.mkdir()
        (frames / "000001.json").write_text('{"retention":"debug"}')
        with self.assertRaisesRegex(RuntimeError, "non-fix frames"):
            harness.verify_collector(self.data, self.device)


class CollectorProcessTest(unittest.TestCase):
    def test_real_binary_runtime_configuration_and_cleanup(self):
        process = MagicMock()
        process.poll.return_value = None
        response = MagicMock()
        response.__enter__.return_value.status = 200
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(harness.subprocess, "Popen", return_value=process) as popen, \
                patch.object(harness.urllib.request, "urlopen", return_value=response), \
                patch.object(harness, "free_port", return_value=12345):
            root = Path(directory)
            with self.assertRaisesRegex(RuntimeError, "test body failed"):
                with harness.collector({"PATH": "/bin", "BRIS_COLLECTOR_CONFIG": "/not-used",
                                        "BRIS_COLLECTOR_BEARER_TOKEN": "not-used"}, root) as (port, token):
                    self.assertEqual(port, 12345)
                    self.assertEqual(len(token), 48)
                    command = popen.call_args.args[0]
                    self.assertEqual(command, [str(harness.ROOT / "target/debug/bris_collector"), "serve"])
                    env = popen.call_args.kwargs["env"]
                    self.assertNotIn("BRIS_COLLECTOR_CONFIG", env)
                    self.assertEqual(env["BRIS_COLLECTOR_DATA_ROOT"], str(root / "data"))
                    self.assertEqual(env["BRIS_COLLECTOR_BIND"], "127.0.0.1:12345")
                    self.assertEqual(env["BRIS_COLLECTOR_BEARER_TOKEN"], token)
                    raise RuntimeError("test body failed")
            process.terminate.assert_called_once()
            process.wait.assert_called_once_with(timeout=10)

    def test_startup_failure_is_not_accepted(self):
        process = MagicMock()
        process.poll.return_value = 1
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(harness.subprocess, "Popen", return_value=process):
            with self.assertRaisesRegex(RuntimeError, "Collector exited"):
                with harness.collector({}, Path(directory)):
                    self.fail("Exited collector must not run instrumentation")
            process.terminate.assert_not_called()

    def test_command_log_does_not_print_token(self):
        output = io.StringIO()
        args = ["gradlew", "-Pandroid.testInstrumentationRunnerArguments.collectorToken=secret"]
        with redirect_stdout(output), patch.object(harness.subprocess, "run") as run:
            harness.run(args, {})
        self.assertNotIn("secret", output.getvalue())
        self.assertIn("<redacted>", output.getvalue())
        self.assertEqual(run.call_args.args[0], args)


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
