//! Uncertainty as a first-class type.
//!
//! Every measurement in Bris carries a 1σ uncertainty alongside its value.
//! [`Uncertain<T>`] is the carrier; [`Sigma`] is a non-negative scalar
//! magnitude in the same unit as `T`.
//!
//! # Why a separate type?
//!
//! Threading `(value, sigma)` tuples through the codebase makes it easy
//! to lose one or accidentally swap them. A wrapper type forces every
//! producer of a measurement to attach an uncertainty and every consumer
//! to acknowledge it. The type system does the bookkeeping.
//!
//! # Combination
//!
//! Independent uncertainties combine in quadrature ([`Sigma::combine`]).
//! Bris assumes independence between most error sources for simplicity;
//! see `plan.org` Phase 4 for the limits of this assumption and the
//! optional inflation factor used in validation.

use core::fmt;
use core::ops::{Add, Mul};

/// A 1σ uncertainty magnitude.
///
/// Always non-negative and finite. Construct via [`Sigma::new`].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Sigma(f64);

impl Sigma {
    /// Zero uncertainty (a perfectly known value, often a placeholder).
    pub const ZERO: Self = Self(0.0);

    /// Construct from a non-negative finite scalar.
    ///
    /// # Errors
    ///
    /// Returns [`UncertaintyError::NotFinite`] for NaN or infinity,
    /// [`UncertaintyError::Negative`] for negative values.
    pub fn new(value: f64) -> Result<Self, UncertaintyError> {
        if !value.is_finite() {
            return Err(UncertaintyError::NotFinite);
        }
        if value < 0.0 {
            return Err(UncertaintyError::Negative);
        }
        Ok(Self(value))
    }

    /// The 1σ value as a scalar.
    pub const fn value(self) -> f64 {
        self.0
    }

    /// Combine two independent 1σ uncertainties in quadrature:
    /// `sqrt(a² + b²)`.
    #[must_use]
    pub fn combine(self, other: Self) -> Self {
        Self((self.0 * self.0 + other.0 * other.0).sqrt())
    }

    /// Scale by a positive factor (e.g. uncertainty inflation).
    ///
    /// # Errors
    ///
    /// Returns [`UncertaintyError::Negative`] if `factor < 0` or
    /// [`UncertaintyError::NotFinite`] if `factor` is non-finite.
    pub fn scale(self, factor: f64) -> Result<Self, UncertaintyError> {
        Self::new(self.0 * factor)
    }
}

impl fmt::Display for Sigma {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "σ={:.4}", self.0)
    }
}

impl Add for Sigma {
    type Output = Self;
    /// Quadrature sum, equivalent to [`Sigma::combine`].
    fn add(self, other: Self) -> Self {
        self.combine(other)
    }
}

impl Mul<f64> for Sigma {
    type Output = Self;
    /// Scale; panics on negative or non-finite factor (use [`Sigma::scale`]
    /// for a fallible variant).
    fn mul(self, factor: f64) -> Self {
        self.scale(factor)
            .expect("Sigma * factor: factor must be non-negative and finite")
    }
}

/// A measurement of `T` with an associated 1σ uncertainty.
///
/// `T` is typically [`Angle`](crate::Angle), [`Latitude`](crate::Latitude),
/// or any other measured quantity. The `sigma` field's unit is the same as
/// `T`'s natural unit (e.g. radians for angles); it is the responsibility
/// of code that produces an `Uncertain<T>` to ensure unit consistency.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Uncertain<T> {
    /// The point-estimate value.
    pub value: T,
    /// The 1σ uncertainty around `value`, in the same unit as `T`.
    pub sigma: Sigma,
}

impl<T> Uncertain<T> {
    /// Pair a value with its uncertainty.
    pub const fn new(value: T, sigma: Sigma) -> Self {
        Self { value, sigma }
    }

    /// Pair a value with zero uncertainty.
    ///
    /// Use this only for values that are known by definition (e.g. constants
    /// from a standard) or as a deliberate placeholder during pipeline
    /// development. Real measurements should always carry a real sigma.
    pub const fn exact(value: T) -> Self {
        Self {
            value,
            sigma: Sigma::ZERO,
        }
    }
}

impl<T: fmt::Display> fmt::Display for Uncertain<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ± {:.4}", self.value, self.sigma.value())
    }
}

/// Errors constructing an uncertainty value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UncertaintyError {
    /// Input was NaN or infinite.
    #[error("uncertainty is not finite")]
    NotFinite,
    /// Input was negative; uncertainties are non-negative by definition.
    #[error("uncertainty must be non-negative")]
    Negative,
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use proptest::prelude::*;

    #[test]
    fn sigma_rejects_negative() {
        assert_eq!(Sigma::new(-0.1), Err(UncertaintyError::Negative));
    }

    #[test]
    fn sigma_rejects_non_finite() {
        assert_eq!(Sigma::new(f64::NAN), Err(UncertaintyError::NotFinite));
        assert_eq!(Sigma::new(f64::INFINITY), Err(UncertaintyError::NotFinite));
    }

    #[test]
    fn sigma_combine_quadrature() {
        let a = Sigma::new(3.0).unwrap();
        let b = Sigma::new(4.0).unwrap();
        assert_relative_eq!(a.combine(b).value(), 5.0);
    }

    #[test]
    fn sigma_scale() {
        let s = Sigma::new(2.0).unwrap().scale(1.5).unwrap();
        assert_relative_eq!(s.value(), 3.0);
        assert_eq!(
            Sigma::new(1.0).unwrap().scale(-1.0),
            Err(UncertaintyError::Negative)
        );
    }

    #[test]
    fn sigma_zero_constant_is_zero() {
        assert_relative_eq!(Sigma::ZERO.value(), 0.0);
    }

    #[test]
    fn sigma_scale_by_zero_yields_zero() {
        let s = Sigma::new(7.5).unwrap().scale(0.0).unwrap();
        assert_relative_eq!(s.value(), 0.0);
    }

    #[test]
    fn sigma_scale_rejects_non_finite_factor() {
        assert_eq!(
            Sigma::new(1.0).unwrap().scale(f64::NAN),
            Err(UncertaintyError::NotFinite)
        );
        assert_eq!(
            Sigma::new(1.0).unwrap().scale(f64::INFINITY),
            Err(UncertaintyError::NotFinite)
        );
    }

    #[test]
    fn sigma_add_operator_matches_combine() {
        let a = Sigma::new(3.0).unwrap();
        let b = Sigma::new(4.0).unwrap();
        assert_relative_eq!((a + b).value(), a.combine(b).value());
        assert_relative_eq!((a + b).value(), 5.0);
    }

    #[test]
    fn sigma_mul_operator_matches_scale() {
        let s = Sigma::new(2.0).unwrap();
        assert_relative_eq!((s * 1.5).value(), 3.0);
        // Multiplication by zero is allowed.
        assert_relative_eq!((s * 0.0).value(), 0.0);
    }

    #[test]
    #[should_panic(expected = "Sigma * factor")]
    fn sigma_mul_by_negative_panics() {
        let _ = Sigma::new(1.0).unwrap() * -1.0;
    }

    #[test]
    #[should_panic(expected = "Sigma * factor")]
    fn sigma_mul_by_nan_panics() {
        let _ = Sigma::new(1.0).unwrap() * f64::NAN;
    }

    #[test]
    fn sigma_display_format() {
        let s = Sigma::new(0.123_456_789).unwrap();
        assert_eq!(format!("{s}"), "σ=0.1235");
    }

    #[test]
    fn uncertain_new_pairs_value_and_sigma() {
        let s = Sigma::new(0.25).unwrap();
        let u: Uncertain<f64> = Uncertain::new(1.5, s);
        assert_relative_eq!(u.value, 1.5);
        assert_eq!(u.sigma, s);
    }

    #[test]
    fn uncertain_exact_has_zero_sigma() {
        let u: Uncertain<f64> = Uncertain::exact(42.0);
        assert_relative_eq!(u.value, 42.0);
        assert_eq!(u.sigma, Sigma::ZERO);
        assert_relative_eq!(u.sigma.value(), 0.0);
    }

    #[test]
    fn uncertain_display_includes_value_and_sigma() {
        // T must implement Display; use f64 (formats as "1").
        let u: Uncertain<f64> = Uncertain::new(1.0, Sigma::new(0.5).unwrap());
        assert_eq!(format!("{u}"), "1 ± 0.5000");
    }

    proptest! {
        #[test]
        fn combine_commutative(a in 0.0_f64..1e9, b in 0.0_f64..1e9) {
            let sa = Sigma::new(a).unwrap();
            let sb = Sigma::new(b).unwrap();
            prop_assert!((sa.combine(sb).value() - sb.combine(sa).value()).abs() < 1e-9);
        }

        #[test]
        fn combine_with_zero_is_identity(a in 0.0_f64..1e9) {
            let sa = Sigma::new(a).unwrap();
            prop_assert!((sa.combine(Sigma::ZERO).value() - sa.value()).abs() < 1e-9);
        }

        #[test]
        fn combine_grows_or_equal(a in 0.0_f64..1e6, b in 0.0_f64..1e6) {
            let sa = Sigma::new(a).unwrap();
            let sb = Sigma::new(b).unwrap();
            let combined = sa.combine(sb).value();
            prop_assert!(combined >= sa.value() - 1e-9);
            prop_assert!(combined >= sb.value() - 1e-9);
        }
    }
}
