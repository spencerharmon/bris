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
    "io.github.spencerharmon.bris.ui.CaptureUploadTest",
)
CAPTURE_TESTS = {"firstStartWithoutSessionsUploadsOnce", "explicitSessionUploadsOnce"}


def run(args, env, **kwargs):
    print("+ " + " ".join(map(str, args)), flush=True)
    return subprocess.run(list(map(str, args)), env=env, check=True, **kwargs)


def verify_results(directory):
    """Reject empty, skipped, failing, or incomplete instrumentation runs."""
    counts = dict.fromkeys(REQUIRED_CLASSES, 0)
    captures = set()
    total = 0
    for report in directory.rglob("TEST-*.xml"):
        for case in ET.parse(report).iter("testcase"):
            if any(case.find(tag) is not None for tag in ("failure", "error", "skipped")):
                raise RuntimeError(f"Non-passing test in {report}: {case.attrib}")
            total += 1
            name = case.get("classname")
            if name in counts:
                counts[name] += 1
            if name == REQUIRED_CLASSES[-1]:
                captures.add(case.get("name"))
    if not all(counts.values()):
        raise RuntimeError(f"Required instrumentation classes did not execute: {counts}")
    if not CAPTURE_TESTS <= captures:
        raise RuntimeError(f"Required capture scenarios did not execute: {captures}")
    print(f"Instrumentation verified: {total} tests passed; {counts}", flush=True)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextmanager
def real_collector(state, env):
    """Build this checkout's collector; retain its isolated store/log for diagnosis."""
    run(["cargo", "build", "--locked", "-p", "bris-collector", "--bin", "bris_collector",
         "--target-dir", ROOT / "target"], env, cwd=ROOT)
    directory = Path(tempfile.mkdtemp(prefix="collector-", dir=state))
    port, token = free_port(), secrets.token_hex(32)
    collector_env = {key: value for key, value in env.items()
                     if not key.startswith("BRIS_COLLECTOR_")}
    collector_env.update(BRIS_COLLECTOR_DATA_ROOT=str(directory / "data"),
                         BRIS_COLLECTOR_BIND=f"127.0.0.1:{port}",
                         BRIS_COLLECTOR_BEARER_TOKEN=token)
    log_path = directory / "collector.log"
    print(f"Real collector evidence: {directory}", flush=True)
    with log_path.open("w") as log:
        process = subprocess.Popen([str(ROOT / "target/debug/bris_collector")],
                                   env=collector_env, cwd=ROOT, stdout=log,
                                   stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(f"Collector exited with {process.returncode}; see {log_path}")
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=1) as response:
                        if response.status == 200 and response.read() == b"ok":
                            break
                except (urllib.error.URLError, TimeoutError):
                    pass
                time.sleep(0.1)
            else:
                raise RuntimeError(f"Collector readiness timed out; see {log_path}")
            yield port, token, directory / "data"
            if process.poll() is not None:
                raise RuntimeError(f"Collector died during instrumentation; see {log_path}")
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def verify_collector(data):
    """Complement on-device byte comparisons with real-store count/checksum checks."""
    manifests = list((data / "submissions").glob("*/*/*/*/manifest.json"))
    if len(manifests) != len(CAPTURE_TESTS):
        raise RuntimeError(f"Expected {len(CAPTURE_TESTS)} real submissions, got {len(manifests)}")
    devices = set()
    for path in manifests:
        manifest = json.loads(path.read_text())
        devices.add(manifest["device"]["uuid"])
        media = manifest["media"]
        files = [file for file in path.parent.rglob("*") if file.is_file() and file != path]
        if len(files) != len(media):
            raise RuntimeError(f"Collector artifact count mismatch: {path}")
        by_name = {file.name: file for file in files}
        names = [item["filename"] for item in media]
        if len(set(names)) != len(names) or set(names) != set(by_name) or "bundle.json" not in names:
            raise RuntimeError(f"Collector artifact names mismatch: {path}")
        for item in media:
            payload = by_name[item["filename"]].read_bytes()
            if (len(payload) != item["size_bytes"] or
                    hashlib.sha256(payload).hexdigest() != item["checksum_sha256"]):
                raise RuntimeError(f"Collector artifact bytes mismatch: {by_name[item['filename']]}")
    if len(devices) != len(CAPTURE_TESTS):
        raise RuntimeError("Capture scenarios did not use independent fresh app data")
    print(f"Real collector verified: {len(manifests)} submissions and all artifact checksums", flush=True)


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
                  "-gpu", "swiftshader_indirect", "-memory", "2048", "-cores", "2",
                  "-partition-size", "2048",
                 "-camera-back", "emulated", "-camera-front", "none"],
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
            if reports.exists():
                shutil.rmtree(reports)  # Old XML must never satisfy this run's assertion.
            with real_collector(state, env) as (collector_port, token, data):
                run([adb, "reverse", f"tcp:{collector_port}", f"tcp:{collector_port}"], env)
                run([ANDROID / "gradlew", *args,
                     f"-Pandroid.testInstrumentationRunnerArguments.collectorBaseUrl=http://127.0.0.1:{collector_port}",
                     f"-Pandroid.testInstrumentationRunnerArguments.collectorToken={token}",
                     "--no-daemon"], env)
                verify_results(reports)
                verify_collector(data)
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
