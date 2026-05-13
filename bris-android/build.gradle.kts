// Top-level Gradle build file for the Bris Android shell.
//
// See docs/design/diagnostic_collection.md for the design
// context. The shell consumes the Rust streaming engine via
// UniFFI-generated Kotlin bindings; the cargo cross-build and
// the bindings-generation steps are wired in `app/build.gradle.kts`.

plugins {
    id("com.android.application") version "8.13.0" apply false
    id("org.jetbrains.kotlin.android") version "2.0.21" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.0.21" apply false
}
