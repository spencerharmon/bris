//! Build script: generate `UniFFI` scaffolding.
//!
//! In proc-macro mode, the only build-time step needed is the
//! scaffolding macro setup. `uniffi::generate_scaffolding` is
//! the entry; without an external `.udl` file there's nothing
//! else for build-time codegen to do.

fn main() {
    // No-op for proc-macro mode: the `#[uniffi::export]` macros
    // in src/lib.rs do the work at compile time. Kept as a
    // build script so that switching to UDL-mode later (if we
    // ever wanted Swift's strict-type checking against a UDL)
    // is a one-line change here, not a Cargo.toml rewrite.
    println!("cargo:rerun-if-changed=build.rs");
}
