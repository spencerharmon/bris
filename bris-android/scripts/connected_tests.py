#!/usr/bin/env python3
"""Provision and run real Android instrumentation on an isolated Linux/KVM AVD."""

import fcntl
import json
import os
import secrets
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET


ANDROID = Path(__file__).resolve().parents[1]
ROOT = ANDROID.parent
IMAGE = "system-images;android-34;aosp_atd;x86_64"
PACKAGES = ("platform-tools", "platforms;android-37.0", "build-tools;37.0.0",
            "ndk;29.0.14206865", "emulator", IMAGE)
REQUIRED_CLASSES = (
    "io.github.spencerharmon.bris.engine.CalibrationStoreManifestTest",
    "io.github.spencerharmon.bris.ui.ConfidenceEllipseOverlayTest",
    "io.github.spencerharmon.bris.upload.AutoUploadAcceptanceTest",
)


def run(args, env, **kwargs):
    print("+ " + " ".join(map(str, args)), flush=True)
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
    if not all(counts.values()):
        raise RuntimeError(f"Required instrumentation classes did not execute: {counts}")
    print(f"Instrumentation verified: {total} tests passed; {counts}", flush=True)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def verify_restart_result(output, method):
    if "OK (1 test)" not in output or "FAILURES!!!" in output or "INSTRUMENTATION_FAILED" in output:
        raise RuntimeError(f"Restart instrumentation {method} did not pass:\n{output}")
    if f"test={method}" not in output or "INSTRUMENTATION_STATUS_CODE: 0" not in output:
        raise RuntimeError(f"Required restart method {method} did not execute:\n{output}")


def restart_acceptance(adb, env, port, token, state):
    package = "io.github.spencerharmon.bris"
    run([ANDROID / "gradlew", "-p", ANDROID, "installDebug", "installDebugAndroidTest", "--no-daemon"], env)
    run([adb, "shell", "pm", "clear", package], env)
    for method in ("preparePendingRestart", "reopenPendingRestart"):
        run([adb, "shell", "am", "force-stop", package], env)
        stopped = subprocess.run([str(adb), "shell", "pidof", package], env=env, capture_output=True, text=True)
        if stopped.returncode != 1 or stopped.stdout.strip():
            raise RuntimeError(f"App process did not stop: {stopped.stdout} {stopped.stderr}")
        result = run([adb, "shell", "am", "instrument", "-w", "-r",
                      "-e", "class", REQUIRED_CLASSES[-1] + "#" + method,
                      "-e", "brisCollectorUrl", f"http://10.0.2.2:{port}",
                      "-e", "brisCollectorAdminToken", token,
                      package + ".test/androidx.test.runner.AndroidJUnitRunner"],
                     env, capture_output=True, text=True, timeout=360)
        output = result.stdout + result.stderr
        (state / f"{method}.log").write_text(output)
        verify_restart_result(output, method)
        print(f"Restart instrumentation verified: {method}: 1 passed", flush=True)


def cargo_bin(name, package, env):
    """Build a workspace binary and return cargo's reported executable path."""
    out = subprocess.run(["cargo", "build", "--quiet", "--message-format=json", "-p", package,
                          "--bin", name], env=env, cwd=ROOT, check=True, capture_output=True, text=True)
    for line in out.stdout.splitlines():
        msg = json.loads(line)
        if msg.get("reason") == "compiler-artifact" and msg["target"]["name"] == name \
                and msg.get("executable"):
            return msg["executable"]
    raise RuntimeError(f"cargo did not report an executable for {name}")


def start_collector(env, data_root, log):
    """Run the real bris-collector on host loopback with temporary durable storage.

    The emulator reaches host 127.0.0.1 as 10.0.2.2. The admin token is
    ephemeral and only enrolls the app (which then receives a per-device token).
    """
    binary = cargo_bin("bris_collector", "bris-collector", env)
    port = free_port()
    token = secrets.token_hex(24)
    collector_env = dict(env, BRIS_COLLECTOR_DATA_ROOT=str(data_root),
                         BRIS_COLLECTOR_BIND=f"127.0.0.1:{port}",
                         BRIS_COLLECTOR_BEARER_TOKEN=token)
    collector_env.pop("BRIS_COLLECTOR_CONFIG", None)
    process = subprocess.Popen([binary], env=collector_env, stdout=log, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"bris-collector exited with {process.returncode}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return process, port, token
        except OSError:
            time.sleep(0.2)
    process.terminate()
    raise RuntimeError("bris-collector did not start listening")


def replay_stored_submissions(env, data_root):
    """Every bundle the app uploaded must replay IN PLACE from the collector store."""
    bris = cargo_bin("bris", "bris-cli", env)
    media = sorted(p.parent for p in (data_root / "submissions").glob("*/*/*/*/manifest.json"))
    if not media:
        raise RuntimeError("collector stored no submissions from the device")
    replayed = 0
    no_frames = 0
    for submission in media:
        result = subprocess.run([bris, "replay", "--bundle", str(submission / "media"),
                                 "--disable-store"], env=dict(env, NO_COLOR="1"),
                                capture_output=True, text=True)
        text = result.stdout + result.stderr
        bundle = json.loads((submission / "media/bundle.json").read_text())
        if bundle["capture"]["frame_count"] == 0:
            # Debug-off no-fix recordings retain diagnostics, not camera frames.
            # Replay must report that honestly rather than invent a successful run.
            if result.returncode == 0 or "no frames found in bundle" not in text:
                raise RuntimeError(f"Expected explicit no-frame replay refusal for {submission}:\n{text}")
            print(f"Verified no-frame replay refusal: {submission.name}", flush=True)
            no_frames += 1
        elif result.returncode != 0 or "mode complete" not in text:
            raise RuntimeError(f"bris replay failed in place for {submission}:\n{text}")
        else:
            replayed += 1
    if replayed < 2:
        raise RuntimeError(f"Expected at least two frame-bearing replay fixtures, got {replayed}")
    print(f"Replayed {replayed} stored device submissions in place; "
          f"verified {no_frames} diagnostic-only no-frame refusals", flush=True)


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
    collector = None
    log_path = state / "emulator.log"
    collector_root = Path(tempfile.mkdtemp(prefix="collector-", dir=state))
    try:
        with (state / "collector.log").open("w") as collector_log:
            collector, collector_port, admin_token = start_collector(env, collector_root, collector_log)
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
            if reports.exists():
                shutil.rmtree(reports)  # Old XML must never satisfy this run's assertion.
            run([ANDROID / "gradlew", *args, "--no-daemon",
                 "-Pandroid.testInstrumentationRunnerArguments.class=" + ",".join([
                     *REQUIRED_CLASSES[:-1], REQUIRED_CLASSES[-1] + "#automaticUploadIsSilentDurableVisibleAndRevocable"]),
                 f"-Pandroid.testInstrumentationRunnerArguments.brisCollectorUrl=http://10.0.2.2:{collector_port}",
                 f"-Pandroid.testInstrumentationRunnerArguments.brisCollectorAdminToken={admin_token}"], env)
            verify_results(reports)
            restart_acceptance(adb, env, collector_port, admin_token, state)
            replay_stored_submissions(env, collector_root)
    finally:
        if collector is not None and collector.poll() is None:
            collector.terminate()
            try:
                collector.wait(timeout=20)
            except subprocess.TimeoutExpired:
                collector.kill()
                collector.wait()
        shutil.rmtree(collector_root, ignore_errors=True)
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
