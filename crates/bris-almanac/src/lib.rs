//! Ephemeris and time scale conversions.
//!
//! Truncated VSOP87 (Sun, planets), Meeus Ch. 47 truncated lunar series
//! (Moon), Yale Bright Star Catalogue with Hipparcos cross-reference for
//! the navigational stars. Precession, nutation, aberration, and
//! refraction applied at runtime.
//!
//! See `plan.org` Phase 1 for the full design.

pub mod ephemeris;
pub mod frame;
pub mod lunar;

pub use ephemeris::{heliocentric, sun_geocentric, Body, Heliocentric};
pub use frame::{mean_obliquity, nutation, precession_angles, NutationAngles, PrecessionAngles};
pub use lunar::{lunar_position, LunarPosition};
