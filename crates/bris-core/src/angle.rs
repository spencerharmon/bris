//! Angle types.
//!
//! [`Angle`] is the general-purpose type stored in radians. Domain-specific
//! types ([`Latitude`], [`Longitude`]) wrap it with constraints appropriate
//! to their meaning (range and normalization). All conversions go through
//! the explicit constructors and accessors; there is no `From<f64>` for
//! these types because the unit must be stated at the call site.
//!
//! # Conventions
//!
//! - Internal storage is always radians (`f64`).
//! - Latitude is in `[-π/2, π/2]`; out-of-range values return an error.
//! - Longitude is normalized to `(-π, π]` (east positive, west negative).
//! - Generic angles are not normalized; callers normalize when meaningful.
//!
//! # Why hand-rolled?
//!
//! See `docs/design/index.md`. Briefly: the `uom` crate is excellent but
//! we have a small enough domain that explicit newtypes give clearer
//! error messages, zero dependencies in this crate, and room for
//! domain-specific behavior (e.g. RA wrap conventions) without fighting
//! a generic API.

use core::f64::consts::{PI, TAU};
use core::fmt;

/// Two π, the period of a full turn in radians.
const TWO_PI: f64 = TAU;

/// Half π, the magnitude bound on latitude in radians.
const HALF_PI: f64 = PI / 2.0;

/// A general-purpose angle, stored in radians.
///
/// `Angle` is intentionally unconstrained: it does not normalize on
/// construction and accepts any finite `f64`. Domain-specific wrappers
/// ([`Latitude`], [`Longitude`]) impose ranges where they apply.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Angle(f64);

impl Angle {
    /// Zero radians.
    pub const ZERO: Self = Self(0.0);

    /// One full turn (2π radians).
    pub const FULL_TURN: Self = Self(TWO_PI);

    /// Construct from radians. No normalization is performed.
    ///
    /// # Errors
    ///
    /// Returns [`AngleError::NotFinite`] if `radians` is NaN or infinite.
    pub fn from_radians(radians: f64) -> Result<Self, AngleError> {
        if radians.is_finite() {
            Ok(Self(radians))
        } else {
            Err(AngleError::NotFinite)
        }
    }

    /// Construct from degrees.
    ///
    /// # Errors
    ///
    /// Returns [`AngleError::NotFinite`] if `degrees` is NaN or infinite.
    pub fn from_degrees(degrees: f64) -> Result<Self, AngleError> {
        Self::from_radians(degrees.to_radians())
    }

    /// Construct from arcminutes.
    ///
    /// # Errors
    ///
    /// Returns [`AngleError::NotFinite`] if `arcmin` is NaN or infinite.
    pub fn from_arcminutes(arcmin: f64) -> Result<Self, AngleError> {
        Self::from_degrees(arcmin / 60.0)
    }

    /// Construct from arcseconds.
    ///
    /// # Errors
    ///
    /// Returns [`AngleError::NotFinite`] if `arcsec` is NaN or infinite.
    pub fn from_arcseconds(arcsec: f64) -> Result<Self, AngleError> {
        Self::from_degrees(arcsec / 3600.0)
    }

    /// The angle in radians.
    pub const fn radians(self) -> f64 {
        self.0
    }

    /// The angle in degrees.
    pub fn degrees(self) -> f64 {
        self.0.to_degrees()
    }

    /// The angle in arcminutes.
    pub fn arcminutes(self) -> f64 {
        self.degrees() * 60.0
    }

    /// The angle in arcseconds.
    pub fn arcseconds(self) -> f64 {
        self.degrees() * 3600.0
    }

    /// Normalize to the half-open range `(-π, π]`.
    #[must_use]
    pub fn normalized_signed(self) -> Self {
        // Reduce to (-2π, 2π), then to (-π, π].
        let mut x = self.0 % TWO_PI;
        if x > PI {
            x -= TWO_PI;
        } else if x <= -PI {
            x += TWO_PI;
        }
        Self(x)
    }

    /// Normalize to the half-open range `[0, 2π)`.
    #[must_use]
    pub fn normalized_unsigned(self) -> Self {
        let x = self.0.rem_euclid(TWO_PI);
        Self(x)
    }
}

impl fmt::Display for Angle {
    /// Formats as decimal degrees with five fractional places (≈4 mas).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.5}°", self.degrees())
    }
}

/// A latitude in `[-π/2, π/2]`.
///
/// North is positive, south is negative.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Latitude(f64);

impl Latitude {
    /// The equator.
    pub const EQUATOR: Self = Self(0.0);

    /// Construct from radians.
    ///
    /// # Errors
    ///
    /// Returns [`AngleError::NotFinite`] for non-finite input,
    /// [`AngleError::OutOfRange`] if `|radians| > π/2`.
    pub fn from_radians(radians: f64) -> Result<Self, AngleError> {
        if !radians.is_finite() {
            return Err(AngleError::NotFinite);
        }
        if radians.abs() > HALF_PI {
            return Err(AngleError::OutOfRange);
        }
        Ok(Self(radians))
    }

    /// Construct from degrees.
    ///
    /// # Errors
    ///
    /// As [`Latitude::from_radians`].
    pub fn from_degrees(degrees: f64) -> Result<Self, AngleError> {
        Self::from_radians(degrees.to_radians())
    }

    /// The latitude in radians.
    pub const fn radians(self) -> f64 {
        self.0
    }

    /// The latitude in degrees.
    pub fn degrees(self) -> f64 {
        self.0.to_degrees()
    }

    /// View as a generic [`Angle`].
    pub const fn as_angle(self) -> Angle {
        Angle(self.0)
    }
}

impl fmt::Display for Latitude {
    /// Formats as decimal degrees with hemisphere suffix (`N`/`S`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.degrees().abs();
        let h = if self.0 >= 0.0 { 'N' } else { 'S' };
        write!(f, "{d:.5}°{h}")
    }
}

/// A longitude, normalized to `(-π, π]`.
///
/// East is positive, west is negative.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Longitude(f64);

impl Longitude {
    /// The prime meridian.
    pub const PRIME_MERIDIAN: Self = Self(0.0);

    /// Construct from radians, normalizing to `(-π, π]`.
    ///
    /// # Errors
    ///
    /// Returns [`AngleError::NotFinite`] for non-finite input.
    pub fn from_radians(radians: f64) -> Result<Self, AngleError> {
        let normalized = Angle::from_radians(radians)?.normalized_signed();
        Ok(Self(normalized.0))
    }

    /// Construct from degrees, normalizing to `(-180°, 180°]`.
    ///
    /// # Errors
    ///
    /// As [`Longitude::from_radians`].
    pub fn from_degrees(degrees: f64) -> Result<Self, AngleError> {
        Self::from_radians(degrees.to_radians())
    }

    /// The longitude in radians, in `(-π, π]`.
    pub const fn radians(self) -> f64 {
        self.0
    }

    /// The longitude in degrees, in `(-180°, 180°]`.
    pub fn degrees(self) -> f64 {
        self.0.to_degrees()
    }

    /// View as a generic [`Angle`].
    pub const fn as_angle(self) -> Angle {
        Angle(self.0)
    }
}

impl fmt::Display for Longitude {
    /// Formats as decimal degrees with hemisphere suffix (`E`/`W`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.degrees().abs();
        let h = if self.0 >= 0.0 { 'E' } else { 'W' };
        write!(f, "{d:.5}°{h}")
    }
}

/// Errors constructing an angle-typed value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AngleError {
    /// Input was NaN or infinite.
    #[error("angle is not finite")]
    NotFinite,
    /// Input was out of the type's allowed range
    /// (e.g. latitude outside `[-90°, 90°]`).
    #[error("angle is out of range for this type")]
    OutOfRange,
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use proptest::prelude::*;

    #[test]
    fn angle_round_trips_radians() {
        let a = Angle::from_radians(1.234).unwrap();
        assert_relative_eq!(a.radians(), 1.234);
    }

    #[test]
    fn angle_unit_conversions_consistent() {
        let a = Angle::from_degrees(90.0).unwrap();
        assert_relative_eq!(a.radians(), HALF_PI);
        assert_relative_eq!(a.arcminutes(), 90.0 * 60.0);
        assert_relative_eq!(a.arcseconds(), 90.0 * 3600.0);
    }

    #[test]
    fn angle_rejects_non_finite() {
        assert_eq!(Angle::from_radians(f64::NAN), Err(AngleError::NotFinite));
        assert_eq!(
            Angle::from_radians(f64::INFINITY),
            Err(AngleError::NotFinite)
        );
        assert_eq!(Angle::from_degrees(f64::NAN), Err(AngleError::NotFinite));
    }

    #[test]
    fn normalized_signed_brings_pi_into_range() {
        let pi = Angle::from_radians(PI).unwrap().normalized_signed();
        assert_relative_eq!(pi.radians(), PI);
        let just_over = Angle::from_radians(PI + 0.1).unwrap().normalized_signed();
        assert_relative_eq!(just_over.radians(), -PI + 0.1);
    }

    #[test]
    fn normalized_unsigned_in_range() {
        let a = Angle::from_radians(-0.5).unwrap().normalized_unsigned();
        assert!(a.radians() >= 0.0);
        assert!(a.radians() < TWO_PI);
        assert_relative_eq!(a.radians(), TWO_PI - 0.5);
    }

    #[test]
    fn latitude_rejects_out_of_range() {
        assert_eq!(Latitude::from_degrees(91.0), Err(AngleError::OutOfRange));
        assert_eq!(Latitude::from_degrees(-91.0), Err(AngleError::OutOfRange));
        assert!(Latitude::from_degrees(90.0).is_ok());
        assert!(Latitude::from_degrees(-90.0).is_ok());
    }

    #[test]
    fn longitude_normalizes() {
        let east = Longitude::from_degrees(190.0).unwrap();
        assert_relative_eq!(east.degrees(), -170.0);
        let west = Longitude::from_degrees(-180.0).unwrap();
        // -180° is exclusive; +180° is inclusive.
        assert_relative_eq!(west.degrees(), 180.0);
    }

    #[test]
    fn display_includes_hemisphere() {
        let lat = Latitude::from_degrees(47.6).unwrap();
        assert_eq!(format!("{lat}"), "47.60000°N");
        let lat = Latitude::from_degrees(-47.6).unwrap();
        assert_eq!(format!("{lat}"), "47.60000°S");
        let lon = Longitude::from_degrees(-122.3).unwrap();
        assert_eq!(format!("{lon}"), "122.30000°W");
    }

    #[test]
    fn angle_zero_and_full_turn_constants() {
        assert_eq!(Angle::ZERO.radians(), 0.0);
        assert_relative_eq!(Angle::FULL_TURN.radians(), TWO_PI);
        assert_relative_eq!(Angle::FULL_TURN.degrees(), 360.0);
    }

    #[test]
    fn arcminutes_round_trip() {
        let a = Angle::from_arcminutes(1234.5).unwrap();
        assert_relative_eq!(a.arcminutes(), 1234.5, epsilon = 1e-9);
    }

    #[test]
    fn arcseconds_round_trip() {
        let a = Angle::from_arcseconds(36_000.0).unwrap();
        // 36000 arcsec = 10 degrees.
        assert_relative_eq!(a.degrees(), 10.0, epsilon = 1e-9);
        assert_relative_eq!(a.arcseconds(), 36_000.0, epsilon = 1e-6);
    }

    #[test]
    fn arcminutes_and_arcseconds_reject_non_finite() {
        assert_eq!(
            Angle::from_arcminutes(f64::NAN),
            Err(AngleError::NotFinite)
        );
        assert_eq!(
            Angle::from_arcseconds(f64::INFINITY),
            Err(AngleError::NotFinite)
        );
    }

    #[test]
    fn angle_display_uses_degrees_with_five_places() {
        let a = Angle::from_degrees(12.345_678_9).unwrap();
        assert_eq!(format!("{a}"), "12.34568°");
        let neg = Angle::from_degrees(-0.5).unwrap();
        assert_eq!(format!("{neg}"), "-0.50000°");
    }

    #[test]
    fn latitude_equator_constant_is_zero() {
        assert_eq!(Latitude::EQUATOR.radians(), 0.0);
        assert_eq!(Latitude::EQUATOR.degrees(), 0.0);
    }

    #[test]
    fn longitude_prime_meridian_constant_is_zero() {
        assert_eq!(Longitude::PRIME_MERIDIAN.radians(), 0.0);
        assert_eq!(Longitude::PRIME_MERIDIAN.degrees(), 0.0);
    }

    #[test]
    fn latitude_rejects_non_finite() {
        assert_eq!(
            Latitude::from_radians(f64::NAN),
            Err(AngleError::NotFinite)
        );
        assert_eq!(
            Latitude::from_radians(f64::INFINITY),
            Err(AngleError::NotFinite)
        );
        assert_eq!(
            Latitude::from_degrees(f64::NEG_INFINITY),
            Err(AngleError::NotFinite)
        );
    }

    #[test]
    fn longitude_rejects_non_finite() {
        assert_eq!(
            Longitude::from_radians(f64::NAN),
            Err(AngleError::NotFinite)
        );
        assert_eq!(
            Longitude::from_degrees(f64::INFINITY),
            Err(AngleError::NotFinite)
        );
    }

    #[test]
    fn latitude_as_angle_preserves_radians() {
        let lat = Latitude::from_degrees(42.0).unwrap();
        let a = lat.as_angle();
        assert_relative_eq!(a.radians(), lat.radians());
        assert_relative_eq!(a.degrees(), 42.0);
    }

    #[test]
    fn longitude_as_angle_preserves_radians() {
        // Pick a longitude that is normalized so the round-trip is exact.
        let lon = Longitude::from_degrees(-45.0).unwrap();
        let a = lon.as_angle();
        assert_relative_eq!(a.radians(), lon.radians());
        assert_relative_eq!(a.degrees(), -45.0);
    }

    #[test]
    fn longitude_zero_and_positive_one_eighty_normalize_consistently() {
        // +180° is in range and stays +180°; -180° normalizes to +180°
        // (the half-open convention `(-π, π]`).
        assert_relative_eq!(Longitude::from_degrees(180.0).unwrap().degrees(), 180.0);
        assert_relative_eq!(Longitude::from_degrees(-180.0).unwrap().degrees(), 180.0);
        assert_relative_eq!(Longitude::from_degrees(0.0).unwrap().degrees(), 0.0);
    }

    proptest! {
        #[test]
        fn radians_degrees_round_trip(deg in -1e6_f64..1e6_f64) {
            let a = Angle::from_degrees(deg).unwrap();
            prop_assert!((a.degrees() - deg).abs() < 1e-9);
        }

        #[test]
        fn normalized_signed_idempotent(r in -1e6_f64..1e6_f64) {
            let a = Angle::from_radians(r).unwrap().normalized_signed();
            let b = a.normalized_signed();
            prop_assert!((a.radians() - b.radians()).abs() < 1e-12);
            prop_assert!(a.radians() > -PI && a.radians() <= PI);
        }

        #[test]
        fn normalized_unsigned_idempotent(r in -1e6_f64..1e6_f64) {
            let a = Angle::from_radians(r).unwrap().normalized_unsigned();
            let b = a.normalized_unsigned();
            prop_assert!((a.radians() - b.radians()).abs() < 1e-12);
            prop_assert!(a.radians() >= 0.0 && a.radians() < TWO_PI);
        }

        #[test]
        fn valid_latitude_round_trips(deg in -90.0_f64..=90.0_f64) {
            let lat = Latitude::from_degrees(deg).unwrap();
            prop_assert!((lat.degrees() - deg).abs() < 1e-9);
        }

        #[test]
        fn longitude_normalization_in_range(deg in -1e6_f64..1e6_f64) {
            let lon = Longitude::from_degrees(deg).unwrap();
            // Result must be in (-180, 180].
            prop_assert!(lon.degrees() > -180.0 && lon.degrees() <= 180.0 + 1e-9);
        }
    }
}
