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

Settings → Debug mode → on. Then enter a collector base URL. The
shared bearer token is currently compiled into the APK build
config; in the spike this means rebuilding with a different
token to change it. Per-device tokens are tracked as a
follow-up.
