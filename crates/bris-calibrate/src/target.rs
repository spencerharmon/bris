//! Checkerboard target description.
//!
//! Calibration uses a printed flat checkerboard. The Bris
//! convention follows the `OpenCV` / `calib-targets` standard:
//! the board is described by its **inner corner counts**
//! (rows × cols), not its square counts.
//!
//! For an 8×12 squares board, the inner corner grid is
//! 7 × 11. The calibration pipeline only sees the inner
//! corners — the outer edge of squares isn't a corner shared
//! between black and white cells.
//!
//! # Square size
//!
//! Square size sets the absolute scale. The math is:
//!
//! - With `square_size_m = S`, the checker grid is treated
//!   as 3D points at `(i*S, j*S, 0)`. The recovered focal
//!   lengths and distortion are scale-invariant in
//!   principle, but accurate `square_size_m` makes the
//!   reprojection residuals interpretable in pixels (the
//!   only thing they're checked against).
//! - Print at the largest size that still fits comfortably
//!   in the camera's FOV at typical calibration distances.
//!   30–40 mm squares on US Letter / A4 are common.
//!
//! # Default
//!
//! [`CheckerboardTarget::default`] is 7 × 11 inner corners
//! with 25 mm squares. Suitable for any printed
//! checkerboard from a calibration template generator
//! (`OpenCV gen_pattern.py`, calib.io, the printable PDFs
//! that ship with most camera-calibration tools).

/// Checkerboard target geometry.
#[derive(Debug, Clone, Copy)]
pub struct CheckerboardTarget {
    /// Number of *inner* corners along the short side
    /// (7 for an 8-square-tall board).
    pub rows: u32,
    /// Number of *inner* corners along the long side
    /// (11 for a 12-square-wide board).
    pub cols: u32,
    /// Square edge length, meters. Sets the absolute scale
    /// for the reprojection residual.
    pub square_size_m: f64,
}

impl CheckerboardTarget {
    /// Construct a checkerboard target.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `rows < 3`, `cols < 3`, or
    /// `square_size_m <= 0`. The `vision-calibration` solve
    /// requires at least 3×3 inner corners (Zhang's method
    /// fits a planar homography per view, which needs ≥ 4
    /// non-collinear points; 3×3 = 9 corners is the
    /// practical minimum).
    pub fn new(rows: u32, cols: u32, square_size_m: f64) -> Result<Self, TargetError> {
        if rows < 3 || cols < 3 {
            return Err(TargetError::TooSmall { rows, cols });
        }
        if !square_size_m.is_finite() || square_size_m <= 0.0 {
            return Err(TargetError::InvalidSquareSize(square_size_m));
        }
        Ok(Self {
            rows,
            cols,
            square_size_m,
        })
    }
}

impl Default for CheckerboardTarget {
    /// 7 × 11 inner corners with 25 mm squares — a typical
    /// printable template that fits on US Letter / A4.
    fn default() -> Self {
        Self {
            rows: 7,
            cols: 11,
            square_size_m: 0.025,
        }
    }
}

/// Errors constructing a [`CheckerboardTarget`].
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum TargetError {
    /// Inner-corner count too small. Zhang's planar fit
    /// needs ≥ 9 corners per view; 3×3 is the minimum.
    #[error("checkerboard {rows}×{cols} too small (minimum 3×3 inner corners)")]
    TooSmall {
        /// Inner-corner rows requested.
        rows: u32,
        /// Inner-corner cols requested.
        cols: u32,
    },
    /// Square size is non-positive or non-finite.
    #[error("checkerboard square size must be positive and finite, got {0}")]
    InvalidSquareSize(f64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_7x11_with_25mm_squares() {
        let t = CheckerboardTarget::default();
        assert_eq!(t.rows, 7);
        assert_eq!(t.cols, 11);
        assert!((t.square_size_m - 0.025).abs() < 1e-12);
    }

    #[test]
    fn rejects_too_small_grids() {
        assert!(matches!(
            CheckerboardTarget::new(2, 5, 0.025),
            Err(TargetError::TooSmall { .. })
        ));
        assert!(matches!(
            CheckerboardTarget::new(5, 2, 0.025),
            Err(TargetError::TooSmall { .. })
        ));
    }

    #[test]
    fn rejects_invalid_square_size() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(matches!(
                CheckerboardTarget::new(7, 11, bad),
                Err(TargetError::InvalidSquareSize(_))
            ));
        }
    }

    #[test]
    fn accepts_minimum_3x3() {
        let t = CheckerboardTarget::new(3, 3, 0.01).unwrap();
        assert_eq!(t.rows, 3);
        assert_eq!(t.cols, 3);
    }
}
