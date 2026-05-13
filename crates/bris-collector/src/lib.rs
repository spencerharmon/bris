//! Diagnostic submission collector for Bris.
//!
//! `bris-collector` is the HTTP receiver for operator-initiated
//! diagnostic submissions from Bris devices. It stores
//! everything on the local filesystem (no database server) and
//! mirrors a minimal index in SQLite for the review UI's
//! list/filter needs.
//!
//! See [`docs/design/diagnostic_collection.md`](../../../docs/design/diagnostic_collection.md)
//! for the design rationale, manifest schema, on-disk layout,
//! and security posture.
//!
//! # Binary vs library
//!
//! The crate compiles both as a library (so integration tests
//! can run the router in-process) and as a binary
//! (`bris-collector`) suitable for `docker run` or a systemd
//! unit. The library entry point is [`build_app`]; the binary
//! lives in `src/bin/bris_collector.rs`.

#![allow(
    clippy::missing_const_for_fn,
    clippy::module_name_repetitions,
    // Some response builders use `axum::Json` which doesn't
    // benefit from `#[must_use]` annotations.
    clippy::must_use_candidate,
    // Proper nouns (SQLite, Bris, axum) recur throughout the
    // crate docs; backticking each occurrence makes the prose
    // harder to read, not easier.
    clippy::doc_markdown,
    // The content-type guesser deliberately compares against
    // already-lowercased extensions; clippy can't see that the
    // input was lowercased upstream.
    clippy::case_sensitive_file_extension_comparisons
)]

pub mod auth;
pub mod config;
pub mod manifest;
pub mod routes;
pub mod store;

pub use config::Config;
pub use routes::build_app;
