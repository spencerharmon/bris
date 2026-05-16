//! Lens calibration for Bris.
//!
//! Bris's vision pipeline is built on a calibrated camera
//! model: the [`bris_vision::Intrinsics`] (focal lengths,
//! principal point, Brown-Conrady distortion coefficients)
//! map between sensor pixels and ray directions. Until the
//! calibration is correct, every measured altitude is wrong
//! by the calibration error — a few pixels of distortion at
//! the lens edge translates to several arcminutes at typical
//! FOVs, which is the dominant absolute-altitude error after
//! refraction.
//!
//! See `docs/operator/calibration.md` for the operator-facing guide:
//! target choice, capture procedure, expected residuals,
//! troubleshooting.
//!
//! # Workflow
//!
//! 1. **Capture** ~30 frames of a printed checkerboard at
//!    varied positions, distances, and tilts. The standard
//!    `bris capture` subcommand records frames to disk.
//!    Live operator UIs (Android) call
//!    [`detect::detect_corners_in_jpeg`] on each captured
//!    frame and surface the resulting [`detect::FrameOutcome`]
//!    so the operator gets immediate per-capture feedback
//!    instead of discovering a third of their captures were
//!    unusable when the solve runs.
//! 2. **Detect** chessboard corners in each frame via
//!    [`detect::detect_corners_in_directory`]. Reports
//!    per-frame outcomes alongside successful views.
//! 3. **Solve** with [`solve::calibrate`]: Zhang's
//!    closed-form planar method initializes; non-linear
//!    Levenberg-Marquardt refines. Returns intrinsics +
//!    quality summary including per-view RMS residuals.
//! 4. **Persist** via [`persist::write_intrinsics`] —
//!    a TOML file that `bris serve` and `bris capture`
//!    load on startup.
//! 5. **Inspect** quality via [`doctor::diagnose`] before
//!    trusting the result. The diagnostic flags common
//!    failure modes (insufficient views, high RMS, sign
//!    inversions, principal point off-center).
//!
//! [`coverage::coverage`] is an *interactive-session*
//! helper: given the views accumulated so far, report which
//! regions of the image plane have been sampled. The
//! Android operator UI calls it after every successful
//! capture; the CLI prints it before invoking the solve.
//!
//! # Library structure
//!
//! - [`target`] — checkerboard configuration (rows × cols,
//!   square size in meters).
//! - [`detect`] — corner detection from on-disk frames or
//!   in-memory buffers, wrapping the `chess-corners` and
//!   `calib-targets` crates.
//! - [`sharpness`] — Laplacian-variance blur estimator over
//!   a region of interest (the detected board's bbox).
//! - [`coverage`] — image-plane coverage of accumulated
//!   views, for live "where to point next" feedback.
//! - [`solve`] — intrinsics fit, wrapping `vision-calibration`'s
//!   planar-intrinsics workflow. Includes per-view residual
//!   extraction.
//! - [`persist`] — TOML serialization of a calibration
//!   result.
//! - [`doctor`] — quality assessment of a result.
//!
//! # Dependencies
//!
//! Calibration brings in a substantial dependency stack
//! (`chess-corners`, `calib-targets`, `vision-calibration`,
//! and their transitive deps including `nalgebra`). This
//! crate is *not* a default dependency of `bris-cli`; it
//! lives behind the `bris calibrate` subcommand. The
//! streaming engine itself doesn't depend on it; only the
//! CLI's calibrate path and the FFI's calibration entry
//! points do.

pub mod coverage;
pub mod detect;
pub mod doctor;
pub mod persist;
pub mod sharpness;
pub mod solve;
pub mod target;

pub use coverage::{coverage, CoverageConfig, CoverageReport};
pub use detect::{
    detect_corners_in_directory, detect_corners_in_directory_with_progress,
    detect_corners_in_image, detect_corners_in_jpeg, BoundingBox, DetectError, DetectedView,
    DetectionStats, DirectoryDetection, FrameDetection, FrameOutcome,
};
pub use doctor::{diagnose, Diagnosis, DiagnosisIssue, DiagnosisLevel};
pub use persist::{
    default_intrinsics_path, read_intrinsics, write_intrinsics, PersistError, PersistedIntrinsics,
};
pub use sharpness::laplacian_variance;
pub use solve::{calibrate, CalibrationResult, SolveError, ViewResidual};
pub use target::CheckerboardTarget;
