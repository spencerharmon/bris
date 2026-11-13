# Bris Android shell

Spike-grade Android app for the Bris celestial-navigation system.

The shell consumes the Rust streaming engine via UniFFI-generated
Kotlin bindings. See `docs/design/diagnostic_collection.md` for
the design rationale and `AGENTS.md` for the conventions.

## Prerequisites

```sh
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk
# Android SDK + NDK installed; ANDROID_HOME / ANDROID_NDK_HOME set.
```

## Build

```sh
./gradlew :app:assembleDebug
```

This runs the following sub-tasks in order:

1. `cargoBuildArm64v8a` / `cargoBuildX8664` — cross-compile
   `bris-ffi` for each ABI.
2. `stageJniLibs*` — copy the resulting `libbris_ffi.so` into
   `app/src/main/jniLibs/<abi>/`.
3. `uniffiBindgen` — generate Kotlin bindings from the built
   shared library into `app/build/generated/source/uniffi/kotlin/`.
4. Android build proper, with the generated bindings on the
   Kotlin source path.

Neither the staged native libraries nor the generated bindings
are committed to the repo; they are produced fresh on each
build.

## Configuration

Nothing about the collector is compiled into the APK. Settings has a
**Collector** section, independent of Debug mode:

- **Base URL** and **Token** (masked), persisted in an app-private
  DataStore (`bris_collector`; app backup is disabled).
- **Save & register** saves both, then calls
  `POST /v1/devices/register` with the token. If the collector
  issues a per-device token it is stored and used for every
  upload (with the `X-Bris-Device-Uuid` header); a 401/403 means
  the token is not an enrollment token and it is used as-is.
  Changing URL or token drops a previously-issued device token.
- **Automatic upload**, persisted and OFF by default, silently uploads each
  completed capture when ON. No review or Debug prerequisite. URL and token
  are required to send; errors remain in Settings, never a prompt.
  Without credentials, completed bundles stay queued locally until configured.
- Queue status (pending / rejected) and the last upload error.

## Uploading a capture

Sight log -> session capture -> **Submit to collector...** (Debug
mode only) shows the one-screen pre-upload review produced by
`Submitter.review`. Nothing is queued until **Approve & upload**
(`Submitter.approve_scoped`); a network-constrained,
exponential-backoff WorkManager job (`FlushWorker`) then flushes
the durable queue under `files/submission-queue/`, retrying
while entries remain pending. **Upload now** in Settings
re-schedules the flush.

Automatic completion uses `Submitter.enqueue_automatic` on that same bundle
and queue, including zero-frame/no-fix captures. Debug affects retained frames
and GPS truth, not automatic-upload authorization. Neither path derives AP
from GPS truth. Changing destination/credentials invalidates pending approvals;
old queued data is never redirected to the new collector. Turning automatic
upload OFF cancels its pending and in-flight native HTTP; Debug OFF cancels only
manual work. Revoked entries appear in rejected counts and cannot be revived by
toggling back on. Data already received by the collector cannot be recalled.

Registration is serialized and generation-checked so delayed responses cannot
install credentials for a superseded destination. Entered device tokens also
carry the installation UUID. Native drains serialize across queue handles;
WorkManager cancellation reaches the HTTP future instead of abandoning a
blocking thread. App restart restores scopes and schedules pending work.

## Tests

`./gradlew testDebugUnitTest` runs JVM unit tests only; the
Android cross-compile (`cargo ndk`) is attached to the jniLibs
merge. Native upload integration tests use host JNA and the host Rust library,
including actual recorder stop/timeout, canonical session review, HTTP retries
after DataStore reopen, and blocked-request cancellation.

### Headless instrumentation

On an Android CI worker (or an explicitly authorized Android test host), from
the repository root:

```sh
./bris-android/gradlew -p bris-android connectedDebugAndroidTest
```

The Linux wrapper routes this exact task (also `:app:connectedDebugAndroidTest`)
through `scripts/connected_tests.py`. It requires an x86_64 Linux host with
read/write `/dev/kvm`, JDK 17, Python 3, Rust/rustup, Android command-line tools
under `$ANDROID_HOME/cmdline-tools/latest`, network access, and sufficient disk
space for the SDK image and both native builds. It accepts the SDK package
licenses noninteractively, installs missing SDK packages using `sdkmanager`, and
installs both Android Rust targets on the repository's pinned toolchain plus
`cargo-ndk` 4.1.2. This does not change the normal CI-only APK production policy.

The harness uses a checkout-local SDK overlay under `.connected-harness/`,
reuses installed SDK packages without modifying them, and provisions a Pixel 6
API 34 AOSP ATD x86_64 AVD. It boots with KVM, software graphics, no window/audio
or snapshots, and a fresh 2 GiB data partition. Its private adb server, port pair, and
AVD state do not reuse or stop any existing devices. Boot has a five-minute
timeout; the emulator and private adb server are stopped on success or failure,
and the invocation's temporary AVD is removed. SIGTERM is handled; SIGKILL cannot
be cleaned up, but a subsequent run uses a distinct AVD. Runs in the same checkout
are serialized. State survives Gradle `clean`. An existing `local.properties` is
refused rather than letting its `sdk.dir` silently override the isolated SDK;
move that file aside or use the existing-device opt-out below.

Gradle's existing tasks build **both arm64-v8a and x86_64** native libraries and
generate UniFFI Kotlin bindings into the APK. No native build is skipped.
After Gradle succeeds, the harness parses fresh instrumentation XML and refuses
zero tests, skipped/failing tests, or missing `CalibrationStoreManifestTest` /
`ConfidenceEllipseOverlayTest` / `CaptureUploadTest` execution. Reports are under
`app/build/reports/androidTests/connected/`; XML is under
`app/build/outputs/androidTest-results/connected/`. Boot diagnostics are in
`.connected-harness/emulator.log`.

The harness also builds and starts this checkout's Rust collector on a random
loopback port, with an ephemeral token and isolated data root. It forwards the
port with adb reverse and supplies instrumentation arguments `collectorBaseUrl`
and `collectorToken`. Android Test Orchestrator clears app data between cases.
Capture tests configure Automatic upload through Settings, then exercise both
first Start (without visiting Sessions) and explicit-session Start/Stop through
MainActivity. They compare canonical bundle/frame/log bytes with the native
queue and collector, assert exactly one submission per capture, and reject
orphan captures. The host checks both submissions and stored checksums too.
Collector evidence remains in `.connected-harness/collector-*/`. Only debug
builds permit cleartext loopback traffic; release network policy is unchanged.

To deliberately use an already-connected device instead of provisioning one:

```sh
BRIS_CONNECTED_DEVICE=existing ./bris-android/gradlew -p bris-android connectedDebugAndroidTest
```

That opt-out uses ordinary Gradle behavior and requires SDK/NDK, Rust targets,
and cargo-ndk to be installed already. Supply `collectorBaseUrl` and
`collectorToken` runner arguments pointing at an isolated real collector when
running the capture tests this way. The result-validator regression tests
do not require Android tooling:

```sh
python3 -m unittest discover -s bris-android/scripts -p 'test_*.py' -v
```
