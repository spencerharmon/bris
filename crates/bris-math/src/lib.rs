//! Pure-math primitives shared across Bris crates.
//!
//! Lives below `bris-vision` and `bris-platesolve` in the
//! workspace dependency graph so both can share implementations
//! that have no I/O, no astronomy, no platform code — just
//! linear algebra primitives a couple of higher-level crates
//! happen to need.
//!
//! Today: just the Kabsch optimal-rotation solver and the small
//! 3×3 Jacobi SVD it depends on. Both used by:
//!
//! - `bris-platesolve` for camera-attitude recovery from
//!   identified-star pairs (catalog ray ↔ camera ray).
//! - `bris-vision::track::track_rotation` for camera-space
//!   stitching: feature-matched pixel pairs lifted to ray pairs
//!   and Kabsch-fitted into a rotation matrix.
//!
//! Before this crate existed, the same Kabsch implementation was
//! duplicated in both crates because the dependency direction
//! (`bris-platesolve` → `bris-vision`) made direct sharing
//! impossible. Extracting both copies into a leaf crate breaks
//! the cycle and lets bug-fixes converge.

pub mod kabsch;

pub use kabsch::{kabsch_rotation, rotate_vec, KabschError};
