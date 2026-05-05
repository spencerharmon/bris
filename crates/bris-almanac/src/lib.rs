//! Ephemeris and time scale conversions.
//!
//! Truncated VSOP87 (Sun, planets), Meeus Ch. 47 truncated lunar series
//! (Moon), Yale Bright Star Catalogue with Hipparcos cross-reference for
//! the navigational stars. Precession, nutation, aberration, and
//! refraction applied at runtime.
//!
//! See `plan.org` Phase 1 for the full design.

pub mod apparent;
pub mod catalog;
pub mod coord;
pub mod ephemeris;
pub mod frame;
pub mod lunar;
pub mod observer;
pub mod refraction;

pub use apparent::{
    body_apparent_place, star_apparent_place, ApparentPlace, ApparentPlaceError, SolarSystemBody,
};
pub use catalog::{all_stars, by_hr, navigational_stars, position_at, StarPosition, StarRecord};
pub use coord::{
    ecliptic_to_equatorial, equatorial_to_horizontal, gmst_rad, last_rad, Ecliptic, Equatorial,
    Horizontal,
};
pub use ephemeris::{heliocentric, sun_geocentric, Body, Heliocentric};
pub use frame::{mean_obliquity, nutation, precession_angles, NutationAngles, PrecessionAngles};
pub use lunar::{lunar_position, LunarPosition};
pub use observer::Observer;
pub use refraction::{bennett, Atmosphere, RefractionError};
