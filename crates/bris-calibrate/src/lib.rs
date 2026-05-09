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
//! See `docs/calibration.md` for the operator-facing guide:
//! target choice, capture procedure, expected residuals,
//! troubleshooting.
//!
//! # Workflow
//!
//! 1. **Capture** ~30 frames of a printed checkerboard at
//!    varied positions, distances, and tilts. The standard
//!    `bris capture` subcommand records frames to disk.
//! 2. **Detect** chessboard corners in each frame via
//!    [`detect::detect_corners_in_directory`]. Skips frames
//!    where the board can't be found cleanly.
//! 3. **Solve** with [`solve::calibrate`]: Zhang's
//!    closed-form planar method initializes; non-linear
//!    Levenberg-Marquardt refines. Returns intrinsics +
//!    quality summary.
//! 4. **Persist** via [`persist::write_intrinsics`] —
//!    a TOML file that `bris serve` and `bris capture`
//!    load on startup.
//! 5. **Inspect** quality via [`doctor::diagnose`] before
//!    trusting the result. The diagnostic flags common
//!    failure modes (insufficient views, high RMS, sign
//!    inversions, principal point off-center).
//!
//! # Library structure
//!
//! - [`target`] — checkerboard configuration (rows × cols,
//!   square size in meters).
//! - [`detect`] — corner detection from on-disk frames,
//!   wrapping the `chess-corners` and `calib-targets`
//!   crates.
//! - [`solve`] — intrinsics fit, wrapping `vision-calibration`'s
//!   planar-intrinsics workflow.
//! - [`persist`] — TOML serialization of a calibration
//!   result.
//! - [`doctor`] — quality assessment of a result.
//!
//! # Dependencies
//!
//! Calibration brings in a substantial dependency stack
//! (`chess-corners`, `calib-targets`, `vision-calibration`,
//! and their transitive deps including `nalgebra`). This
//! crate is *not* a default dependency of `bris-cli` — it
//! lives behind the `bris calibrate` subcommand. The
//! streaming engine itself doesn't depend on it; only the
//! CLI's calibrate path does.

pub mod detect;
pub mod doctor;
pub mod persist;
pub mod solve;
pub mod target;

pub use detect::{detect_corners_in_directory, DetectError, DetectedView};
pub use doctor::{diagnose, Diagnosis, DiagnosisIssue, DiagnosisLevel};
pub use persist::{
    default_intrinsics_path, read_intrinsics, write_intrinsics, PersistError, PersistedIntrinsics,
};
pub use solve::{calibrate, CalibrationResult, SolveError};
pub use target::CheckerboardTarget;
