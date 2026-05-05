//! Vision pipeline: capture-agnostic image processing, lens calibration,
//! horizon detection, body centroiding, and multi-frame stitching.
//!
//! All algorithms are classical (no ML inference runtime). See `plan.org`
//! Phase 2 for design.

pub mod frame;
pub mod horizon;
pub mod lens;

pub use frame::{Frame, FrameError, Intrinsics};
pub use horizon::{detect_horizon, HorizonConfig, HorizonError, HorizonLine};
pub use lens::{distort_normalized, pixel_ray_direction, project_pinhole, undistort_pixel};
