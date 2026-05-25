//! Sight reduction, multi-sight fix computation with full uncertainty
//! propagation, and the continuous-operation engine that drives Bris's
//! streaming pipeline.
//!
//! See `plan.org` Phase 3.5 and Phase 4.

pub mod circle_of_position;
pub mod fix;
pub mod screen;
pub mod sight;

pub use circle_of_position::{
    cold_start_fix, CircleOfPosition, ColdStartConfig, ColdStartError, ColdStartResult,
    FixCandidate,
};
pub use fix::{multi_sight_fix, Fix, FixError};
pub use screen::{screen_sights, RejectionReason, ScreeningConfig, ScreeningResult};
pub use sight::{line_of_position, LineOfPosition, LopError, NM_PER_ARCMIN};
