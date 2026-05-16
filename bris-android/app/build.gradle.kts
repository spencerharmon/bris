// Kotlin app module for the Bris Android shell.
//
// Three notable pieces beyond a stock Compose app:
//
// 1. A `cargoBuild` task per Android ABI that cross-compiles
//    `bris-ffi` and stages `libbris_ffi.so` into
//    `app/src/main/jniLibs/<abi>/`.
// 2. A `uniffiBindgen` task that runs the `uniffi-bindgen`
//    binary against the just-built shared library to produce
//    Kotlin bindings under
//    `app/build/generated/source/uniffi/`.
// 3. Source-set wiring so the generated bindings are picked up
//    by the Kotlin compiler without committing them.
//
// Neither bindings nor native libs are committed to the repo.

import org.gradle.api.tasks.Exec

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "io.github.spencerharmon.bris"
    compileSdk = 34
    // Pin build-tools so AGP doesn't try to fetch a different
    // version than the one installed under the merged SDK at
    // local.properties' sdk.dir. The chaotic-aur
    // android-sdk-build-tools package ships 37.0.0.
    buildToolsVersion = "37.0.0"
    // Pin NDK version to the installed r29. AGP 8.13's bundled
    // default is r27; declaring the actual installed version
    // here suppresses the CXX1104 mismatch warnings without
    // affecting our pure-Rust cross-build (cargo-ndk handles
    // the toolchain selection independently of AGP).
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "io.github.spencerharmon.bris"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"
        // Restrict packaged ABIs to the two we actually cross-
        // compile bris-ffi for: arm64-v8a (real devices) and
        // x86_64 (emulator).
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }

        // Bearer token used by the diagnostic-collection
        // submitter. Spike-grade: a single shared token built
        // into every APK. Override with
        //   `-PbrisCollectorToken=<token>`
        // on the Gradle command line for a real build; the
        // default below is a placeholder that the collector
        // will not accept.
        val token = (project.findProperty("brisCollectorToken") as String?)
            ?: "spike-shared-token-replace-me"
        buildConfigField("String", "BRIS_COLLECTOR_BEARER_TOKEN", "\"$token\"")
        buildConfigField("String", "BRIS_APP_VERSION", "\"0.1.0\"")
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }

    // Pick up the generated UniFFI bindings as Kotlin sources.
    sourceSets {
        getByName("main") {
            kotlin.srcDir(layout.buildDirectory.dir("generated/source/uniffi/kotlin"))
            jniLibs.srcDir(layout.projectDirectory.dir("src/main/jniLibs"))
        }
    }

    buildTypes {
        debug {
            // Use the debug Rust build for faster iteration on
            // the FFI surface. Switch to release builds when
            // measuring engine performance.
            isMinifyEnabled = false
        }
        release {
            isMinifyEnabled = false
        }
    }
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.10.01")
    implementation(composeBom)
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.navigation:navigation-compose:2.8.4")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    // Provides the modern androidx.lifecycle.compose.LocalLifecycleOwner
    // (the androidx.compose.ui.platform.LocalLifecycleOwner is
    // deprecated and known to return null on certain
    // Compose-BOM + lifecycle-runtime version pairings,
    // crashing CameraX's bindToLifecycle on real devices).
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation("androidx.datastore:datastore-preferences:1.1.1")

    // CameraX
    val cameraxVersion = "1.4.0"
    implementation("androidx.camera:camera-core:$cameraxVersion")
    implementation("androidx.camera:camera-camera2:$cameraxVersion")
    implementation("androidx.camera:camera-lifecycle:$cameraxVersion")
    implementation("androidx.camera:camera-view:$cameraxVersion")

    // UniFFI runtime support — JNA is what UniFFI's Kotlin
    // bindings dispatch through.
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")

    // HTTP submission to the collector.
    implementation("com.squareup.okhttp3:okhttp:4.12.0")

    debugImplementation("androidx.compose.ui:ui-tooling")
}

// ---------------------------------------------------------------------------
// Cross-compile bris-ffi for each Android ABI and stage the shared object
// into the app's jniLibs directory.
//
// Prerequisite (one-time):
//   rustup target add aarch64-linux-android x86_64-linux-android
//   cargo install cargo-ndk
//
// The cargo-ndk crate wraps `cargo build` with the right linker
// configuration for Android targets. Without it, the build
// requires manual NDK toolchain plumbing.
// ---------------------------------------------------------------------------

val rustRoot = layout.projectDirectory.dir("../..")
val ndkDir = providers.gradleProperty("brisNdkDir")
    .orElse(provider { System.getenv("ANDROID_NDK_HOME") ?: "" })

val abiToTarget = mapOf(
    "arm64-v8a" to "aarch64-linux-android",
    "x86_64" to "x86_64-linux-android",
)

val cargoBuildAll = tasks.register("cargoBuildAll") {
    group = "rust"
    description = "Cross-compile bris-ffi for all packaged ABIs."
}

abiToTarget.forEach { (abi, target) ->
    val cargoTask = tasks.register<Exec>("cargoBuild${abi.replace("-", "").replaceFirstChar { it.uppercase() }}") {
        group = "rust"
        description = "Cross-compile bris-ffi for $abi ($target)."
        workingDir = rustRoot.asFile
        environment("ANDROID_NDK_HOME", ndkDir.get())
        commandLine(
            "cargo", "ndk",
            "--target", target,
            "--platform", "26",
            "--",
            "build",
            "--release",
            "-p", "bris-ffi"
        )
    }
    val stageTask = tasks.register<Copy>("stageJniLibs${abi.replace("-", "").replaceFirstChar { it.uppercase() }}") {
        group = "rust"
        description = "Stage libbris_ffi.so for $abi into jniLibs."
        dependsOn(cargoTask)
        from(rustRoot.dir("target/$target/release"))
        include("libbris_ffi.so")
        into(layout.projectDirectory.dir("src/main/jniLibs/$abi"))
    }
    cargoBuildAll.configure { dependsOn(stageTask) }
}

// ---------------------------------------------------------------------------
// Generate Kotlin bindings from the *host* cdylib of bris-ffi.
//
// UniFFI 0.28's library-mode bindgen has an issue extracting
// symbols from cross-compiled cdylibs (returns silently with no
// generated files); the host build does not have this issue.
// We compile bris-ffi for the host as a debug cdylib (fast),
// run uniffi-bindgen against that .so, and let the cross-built
// release .so files supply the actual runtime libraries via
// jniLibs.
// ---------------------------------------------------------------------------

val cargoHostBuild = tasks.register<Exec>("cargoHostBuild") {
    group = "rust"
    description = "Host (x86_64-unknown-linux-gnu) debug cdylib for bindgen."
    workingDir = rustRoot.asFile
    commandLine("cargo", "build", "-p", "bris-ffi")
}

val uniffiBindgen = tasks.register<Exec>("uniffiBindgen") {
    group = "rust"
    description = "Generate Kotlin bindings from the host libbris_ffi.so."
    dependsOn(cargoHostBuild)
    workingDir = rustRoot.asFile
    val outDir = layout.buildDirectory.dir("generated/source/uniffi/kotlin").get().asFile
    doFirst { outDir.mkdirs() }
    commandLine(
        "cargo", "run",
        "-p", "bris-ffi",
        "--features", "bindgen",
        "--bin", "uniffi-bindgen",
        "--",
        "generate",
        "--library", "target/debug/libbris_ffi.so",
        "--language", "kotlin",
        "--no-format",
        "--out-dir", outDir.absolutePath
    )
}

tasks.named("preBuild") {
    dependsOn(uniffiBindgen)
    dependsOn(cargoBuildAll)
}
