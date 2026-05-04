//! Core types and traits shared across the Bris workspace.
//!
//! This crate is intentionally minimal and dependency-light. It defines
//! the vocabulary (units, coordinate types, uncertainty newtypes) used
//! by every other crate so they can compose without each pulling in
//! the others' implementation details.
//!
//! See `plan.org` Phase 0 / Phase 1 for the design rationale.

#![cfg_attr(not(feature = "std"), no_std)]
