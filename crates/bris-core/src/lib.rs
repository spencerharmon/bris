//! Core types and traits shared across the Bris workspace.
//!
//! This crate is intentionally minimal and dependency-light. It defines
//! the vocabulary (units, coordinate types, uncertainty newtypes, time
//! scales) used by every other crate so they can compose without each
//! pulling in the others' implementation details.
//!
//! See `plan.org` Phase 0 / Phase 1 for the design rationale.

pub mod angle;
pub mod time;
pub mod uncertainty;

pub use angle::{Angle, AngleError, Hemisphere, Latitude, Longitude};
pub use time::{Tai, TimeError, Tt, Ut1, JD_J2000, LEAP_TABLE_EXPIRES};
pub use uncertainty::{Sigma, Uncertain, UncertaintyError};
