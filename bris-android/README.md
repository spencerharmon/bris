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

Nothing about the collector is compiled into the APK. Settings ->
Debug mode -> on reveals a **Collector** section:

- **Base URL** and **Token** (masked), persisted in an app-private
  DataStore (`bris_collector`; app backup is disabled).
- **Save & register** saves both, then calls
  `POST /v1/devices/register` with the token. If the collector
  issues a per-device token it is stored and used for every
  upload (with the `X-Bris-Device-Uuid` header); a 401/403 means
  the token is not an enrollment token and it is used as-is.
  Changing URL or token drops a previously-issued device token.
- Queue status (pending / rejected) and the last upload error.

## Uploading a capture

Sight log -> open a capture -> **Submit to collector...** (Debug
mode only) shows the one-screen pre-upload review produced by
`Submitter.review`. Nothing is queued until **Approve & upload**
(`Submitter.approve_and_enqueue`); a network-constrained,
exponential-backoff WorkManager job (`FlushWorker`) then flushes
the durable queue under `files/submission-queue/`, retrying
while entries remain pending. **Upload now** in Settings
re-schedules the flush.

## Tests

`./gradlew testDebugUnitTest` runs JVM unit tests only; the
Android cross-compile (`cargo ndk`) is attached to the jniLibs
merge, so unit tests need only a host Rust toolchain.
