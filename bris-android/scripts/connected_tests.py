#!/usr/bin/env python3
"""Provision and run real Android instrumentation on an isolated Linux/KVM AVD."""

import fcntl
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import shutil
import secrets
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET


ANDROID = Path(__file__).resolve().parents[1]
ROOT = ANDROID.parent
IMAGE = "system-images;android-34;aosp_atd;x86_64"
PACKAGES = ("platform-tools", "platforms;android-37.0", "build-tools;37.0.0",
            "ndk;29.0.14206865", "emulator", IMAGE)
REQUIRED_CLASSES = (
    "io.github.spencerharmon.bris.engine.CalibrationStoreManifestTest",
    "io.github.spencerharmon.bris.ui.ConfidenceEllipseOverlayTest",
    "io.github.spencerharmon.bris.ui.FirstCaptureUploadTest",
    "io.github.spencerharmon.bris.ui.ExplicitSessionUploadTest",
)
APP = "io.github.spencerharmon.bris"
MINIMUM_TESTS = dict(zip(REQUIRED_CLASSES, (4, 3, 1, 1)))


def run(args, env, **kwargs):
    print("+ " + " ".join(
        str(arg).split("=", 1)[0] + "=<redacted>"
        if str(arg).startswith("-Pandroid.testInstrumentationRunnerArguments.collectorToken=")
        else str(arg) for arg in args), flush=True)
    return subprocess.run(list(map(str, args)), env=env, check=True, **kwargs)


def verify_results(directory):
    """Reject empty, skipped, failing, or incomplete instrumentation runs."""
    counts = dict.fromkeys(REQUIRED_CLASSES, 0)
    total = 0
    for report in directory.rglob("TEST-*.xml"):
        for case in ET.parse(report).iter("testcase"):
            if any(case.find(tag) is not None for tag in ("failure", "error", "skipped")):
                raise RuntimeError(f"Non-passing test in {report}: {case.attrib}")
            total += 1
            name = case.get("classname")
            if name in counts:
                counts[name] += 1
    if any(counts[name] < minimum for name, minimum in MINIMUM_TESTS.items()):
        raise RuntimeError(f"Required instrumentation classes did not execute: {counts}")
    print(f"Instrumentation verified: {total} tests passed; {counts}", flush=True)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextmanager
def collector(env, directory):
    """Run this checkout's actual authenticated collector, never an HTTP stand-in."""
    port = free_port()
    token = secrets.token_hex(24)
    child_env = {key: value for key, value in env.items()
                 if not key.startswith("BRIS_COLLECTOR_")}
    child_env.update(BRIS_COLLECTOR_DATA_ROOT=str(directory / "data"),
                     BRIS_COLLECTOR_BIND=f"127.0.0.1:{port}",
                     BRIS_COLLECTOR_BEARER_TOKEN=token)
    with (directory / "collector.log").open("w") as log:
        process = subprocess.Popen([str(ROOT / "target/debug/bris_collector"), "serve"],
                                   cwd=ROOT, env=child_env, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(f"Collector exited; see {directory / 'collector.log'}")
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=1) as response:
                        if response.status == 200:
                            break
                except (urllib.error.URLError, TimeoutError):
                    time.sleep(0.1)
            else:
                raise RuntimeError("Collector readiness timed out")
            yield port, token
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def verify_collector(data, device):
    """Cross-check actual stored multipart bytes against the canonical device tree."""
    sessions = list(device.glob("sessions/*/session.json"))
    manifests = list(data.glob("submissions/*/*/*/*/manifest.json"))
    if len(sessions) != 1 or len(manifests) != 2:
        raise RuntimeError(f"Expected one session and two submissions: {sessions}, {manifests}")
    session = json.loads(sessions[0].read_bytes())
    session_id = sessions[0].parent.name
    ids = session["ordered_capture_ids"]
    captures = sessions[0].parent / "captures"
    if (session["session_id"] != session_id or len(ids) != 2 or len(set(ids)) != 2
            or set(ids) != {path.name for path in captures.iterdir()}):
        raise RuntimeError("Canonical session identity/capture index mismatch")
    if any(path.is_file() for path in (device / "sights").rglob("*")):
        raise RuntimeError("Legacy orphan capture artifacts found")
    received_ids = []
    for path in manifests:
        manifest = json.loads(path.read_bytes())
        bundle_bytes = (path.parent / "media/bundle.json").read_bytes()
        bundle = json.loads(bundle_bytes)
        capture_id = bundle["bundle_id"]
        if bundle.get("session_id") != session_id or capture_id not in ids:
            raise RuntimeError("Collector received an unbound or unexpected capture")
        received_ids.append(capture_id)
        capture = captures / capture_id
        for name in ("manifest.json", "bundle.json", "pbris.log"):
            if not (capture / name).is_file():
                raise RuntimeError(f"Missing canonical artifact: {capture_id}/{name}")
        if bundle.get("gps_truth") is not None:
            raise RuntimeError("Debug OFF capture contains GPS truth")
        for sidecar in (capture / "frames").glob("*.json"):
            if json.loads(sidecar.read_bytes()).get("retention") != "fix_frame":
                raise RuntimeError("Debug OFF capture contains non-fix frames")
        # The production multipart contract excludes the sight-log manifest and
        # session.json; those are checked locally, not fabricated into the upload.
        expected = {"bundle.json": capture / "bundle.json", "pbris.log": capture / "pbris.log"}
        if (capture / "index.jsonl").exists():
            expected["index.jsonl"] = capture / "index.jsonl"
        for frame in (capture / "frames").glob("*"):
            if frame.is_file():
                expected[frame.name] = frame
        media = manifest["media"]
        if len(media) != len(expected) or {item["filename"] for item in media} != set(expected):
            raise RuntimeError("Collector media set differs from canonical capture")
        for item in media:
            name = item["filename"]
            received = path.parent / ("pbris.log" if item.get("role") == "pbris_log" else f"media/{name}")
            original = expected[name].read_bytes()
            if (received.read_bytes() != original or item["size_bytes"] != len(original)
                    or item["checksum_sha256"] != hashlib.sha256(original).hexdigest()):
                raise RuntimeError(f"Collector changed canonical bytes: {capture_id}/{name}")
    if sorted(received_ids) != sorted(ids):
        raise RuntimeError("Completion duplicated or omitted a capture submission")
    print(f"Real collector verified: {session_id}, captures {ids}, byte-identical media", flush=True)


def emulator_port():
    for port in range(5554, 5682, 2):
        with socket.socket() as console, socket.socket() as adb:
            try:
                console.bind(("127.0.0.1", port))
                adb.bind(("127.0.0.1", port + 1))
                return port
            except OSError:
                continue
    raise RuntimeError("No free emulator port pair")


def main(args):
    if sys.platform != "linux" or not os.access("/dev/kvm", os.R_OK | os.W_OK):
        raise RuntimeError("This harness requires Linux x86_64 with read/write /dev/kvm")
    if os.uname().machine != "x86_64":
        raise RuntimeError("The API 34 test image requires an x86_64 host")

    if (ANDROID / "local.properties").exists():
        raise RuntimeError("The isolated harness requires no bris-android/local.properties; "
                           "move it aside or use BRIS_CONNECTED_DEVICE=existing")
    state = ANDROID / ".connected-harness"
    state.mkdir(parents=True, exist_ok=True)
    # Serialize this checkout only; other checkouts use their own SDK/AVD/adb.
    with (state / "lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        # A killed worker can leave an emulator alive. Never overwrite its AVD.
        with tempfile.TemporaryDirectory(prefix="avd-", dir=state) as avd_home:
            execute(args, state, avd_home)


def execute(args, state, avd_home):
    env = os.environ.copy()
    source = Path(env.get("ANDROID_HOME") or env.get("ANDROID_SDK_ROOT", ""))
    sdkmanager = source / "cmdline-tools/latest/bin/sdkmanager"
    if not sdkmanager.is_file():
        found = shutil.which("sdkmanager")
        if not found:
            raise RuntimeError("Install Android command-line tools and set ANDROID_HOME")
        sdkmanager = Path(found).resolve()
        source = sdkmanager.parents[3]

    sdk = state / "sdk"
    sdk.mkdir(exist_ok=True)
    # Reuse installed packages read-only, installing missing ones into our overlay.
    # Never ask sdkmanager to update packages in the shared/system SDK.
    tools = sdk / "cmdline-tools/latest"
    if not tools.exists():
        shutil.copytree(sdkmanager.parents[1], tools)
    sdkmanager = tools / "bin/sdkmanager"
    for package in PACKAGES:
        relative = Path(*package.split(";"))
        destination = sdk / relative
        if not destination.exists() and (source / relative / "package.xml").is_file():
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.symlink_to((source / relative).resolve(), target_is_directory=True)
    if (source / "licenses").is_dir() and not (sdk / "licenses").exists():
        shutil.copytree(source / "licenses", sdk / "licenses")

    env.update(ANDROID_HOME=str(sdk), ANDROID_SDK_ROOT=str(sdk),
               ANDROID_NDK_HOME=str(sdk / "ndk/29.0.14206865"),
               ANDROID_NDK_ROOT=str(sdk / "ndk/29.0.14206865"),
               ANDROID_AVD_HOME=avd_home,
               ANDROID_USER_HOME=str(state / "android-user"),
               ANDROID_EMULATOR_HOME=str(state / "android-user"),
               ANDROID_ADB_SERVER_PORT=str(free_port()),
               BRIS_CONNECTED_DEVICE="existing")
    for key in ("ANDROID_AVD_HOME", "ANDROID_USER_HOME"):
        Path(env[key]).mkdir(exist_ok=True)
    # A caller's adb socket must not override our private server port.
    env.pop("ADB_SERVER_SOCKET", None)
    env["PATH"] = str(sdk / "platform-tools") + os.pathsep + env["PATH"]
    missing = [package for package in PACKAGES
               if not (sdk / Path(*package.split(";")) / "package.xml").exists()]
    if missing:
        run([sdkmanager, f"--sdk_root={sdk}", *missing], env, input="y\n" * 100, text=True)
    run(["rustup", "target", "add", "aarch64-linux-android", "x86_64-linux-android"],
        env, cwd=ROOT)
    run(["cargo", "install", "cargo-ndk", "--version", "4.1.2", "--locked"], env, cwd=ROOT)

    avdmanager = sdkmanager.with_name("avdmanager")
    run([avdmanager, "create", "avd", "--force", "--name", "pixel6Api34",
         "--package", IMAGE, "--device", "pixel_6"], env, input="no\n", text=True)
    adb = sdk / "platform-tools/adb"
    port = emulator_port()
    env["ANDROID_SERIAL"] = f"emulator-{port}"
    emulator = None
    log_path = state / "emulator.log"
    try:
        run([adb, "start-server"], env)
        with log_path.open("w") as log:
            emulator = subprocess.Popen(
                [str(sdk / "emulator/emulator"), "-avd", "pixel6Api34", "-port", str(port),
                 "-no-window", "-no-audio", "-no-metrics", "-no-snapshot", "-wipe-data", "-accel", "on",
                 "-gpu", "swiftshader_indirect", "-memory", "2048", "-cores", "2"],
                env=env, stdout=log, stderr=subprocess.STDOUT)
            deadline = time.monotonic() + 300
            while time.monotonic() < deadline:
                if emulator.poll() is not None:
                    raise RuntimeError(f"Emulator exited with {emulator.returncode}; see {log_path}")
                boot = subprocess.run([str(adb), "shell", "getprop", "sys.boot_completed"],
                                      env=env, capture_output=True, text=True, timeout=15)
                if boot.returncode == 0 and boot.stdout.strip() == "1":
                    break
                time.sleep(2)
            else:
                raise RuntimeError(f"Emulator boot timed out; see {log_path}")

            reports = ANDROID / "app/build/outputs/androidTest-results/connected"
            evidence = Path(tempfile.mkdtemp(prefix="results-", dir=state))
            print(f"Connected evidence: {evidence}", flush=True)
            run(["cargo", "build", "--locked", "--target-dir", ROOT / "target",
                 "-p", "bris-collector", "--bin", "bris_collector"],
                env, cwd=ROOT)
            failures = []
            # Separate invocations are deliberate: pm clear also resets process-wide
            # SessionHolder/Upload/DataStore singletons, unlike deleting test files.
            groups = [REQUIRED_CLASSES[:2], *[(name,) for name in REQUIRED_CLASSES[2:]]]
            for number, classes in enumerate(groups):
                group = evidence / str(number)
                group.mkdir()
                if reports.exists():
                    shutil.rmtree(reports)  # Never accept stale XML from an earlier group.
                selection = (f"notClass={','.join(REQUIRED_CLASSES[2:])}" if number == 0
                             else f"class={','.join(classes)}")
                extra = [f"-Pandroid.testInstrumentationRunnerArguments.{selection}",
                         "-PbrisConnectedCollector=true"]
                try:
                    if number == 0:
                        run([ANDROID / "gradlew", *args, *extra, "--no-daemon"], env)
                    else:
                        installed = run([adb, "shell", "pm", "list", "packages", APP], env,
                                        capture_output=True, text=True)
                        if f"package:{APP}" in installed.stdout.splitlines():
                            cleared = run([adb, "shell", "pm", "clear", APP], env,
                                          capture_output=True, text=True)
                            if cleared.stdout.strip() != "Success":
                                raise RuntimeError(f"Could not clear app data: {cleared.stdout}")
                        with collector(env, group) as (collector_port, token):
                            run([adb, "reverse", f"tcp:{collector_port}", f"tcp:{collector_port}"], env)
                            try:
                                extra += [f"-Pandroid.testInstrumentationRunnerArguments.collectorUrl=http://127.0.0.1:{collector_port}",
                                          f"-Pandroid.testInstrumentationRunnerArguments.collectorToken={token}"]
                                run([ANDROID / "gradlew", *args, *extra, "--no-daemon"], env)
                                run([adb, "pull", f"/data/local/tmp/{classes[0].rsplit('.', 1)[-1]}",
                                     group / "device"], env)
                                verify_collector(group / "data", group / "device")
                            finally:
                                run([adb, "reverse", "--remove", f"tcp:{collector_port}"], env)
                except (RuntimeError, OSError, ValueError, subprocess.SubprocessError) as error:
                    # CalledProcessError includes argv, which contains the ephemeral token.
                    detail = (f"command exited {error.returncode}" if isinstance(error, subprocess.CalledProcessError)
                              else str(error))
                    failures.append(f"{classes}: {detail}")
                finally:
                    if reports.exists():
                        shutil.copytree(reports, group / "reports")
                    with (group / "logcat.txt").open("w") as logcat:
                        run([adb, "logcat", "-d"], env, stdout=logcat)
            if failures:
                raise RuntimeError(f"Connected checks failed; evidence {evidence}:\n" + "\n".join(failures))
            verify_results(evidence)
    finally:
        # Only terminate processes owned by this invocation, never a user's device/server.
        if emulator is not None and emulator.poll() is None:
            emulator.terminate()
            try:
                emulator.wait(timeout=20)
            except subprocess.TimeoutExpired:
                emulator.kill()
                emulator.wait()
        run([adb, "kill-server"], env, timeout=20)


def interrupted(signum, frame):
    raise KeyboardInterrupt(f"Interrupted by signal {signum}")


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main(sys.argv[1:] or ["-p", str(ANDROID), "connectedDebugAndroidTest"])
    except (RuntimeError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
    except KeyboardInterrupt:
        sys.exit(130)
