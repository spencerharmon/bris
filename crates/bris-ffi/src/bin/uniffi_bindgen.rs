//! `uniffi-bindgen` entry point.
//!
//! Built only when the `bindgen` feature is enabled. The Android
//! Gradle build invokes:
//!
//! ```sh
//! cargo run -p bris-ffi --features bindgen \
//!     --bin uniffi-bindgen -- generate \
//!     --library target/<abi>/release/libbris_ffi.so \
//!     --language kotlin \
//!     --out-dir <gradle-generated>/source/uniffi/
//! ```
//!
//! to produce Kotlin bindings against the proc-macro-exported
//! Rust types. The same binary supports `--language swift` for
//! the eventual iOS shell.

fn main() {
    uniffi::uniffi_bindgen_main();
}
