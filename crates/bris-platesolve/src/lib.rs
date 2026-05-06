//! Tetra3-style geometric-hash plate solving against the embedded
//! star catalog.
//!
//! # Pipeline
//!
//! 1. **Database build** ([`StarHashDb::build`]): enumerate every
//!    4-star combination from the catalog whose four stars all fit
//!    within a configurable maximum FOV and whose dimmest member
//!    is brighter than a magnitude cutoff. For each combination,
//!    compute a quantized hash from the pattern's pairwise
//!    distances. Insert into a `HashMap<PatternHash,
//!    Vec<CatalogPattern>>` for fast lookup.
//!
//! 2. **Solve** ([`plate_solve`]): given detected peaks (from
//!    [`bris_vision::detect_peaks`]) and camera intrinsics:
//!    - Map each peak to a unit ray in camera frame using the
//!      intrinsics.
//!    - For each 4-tuple of the brightest peaks, compute the hash
//!      from their pairwise angular distances.
//!    - Look up the hash; for each candidate catalog pattern, try
//!      the four 24 permutations of star ↔ peak assignment.
//!    - Solve Wahba's problem (Kabsch via SVD) to get the rotation
//!      matrix mapping celestial-frame star unit vectors to
//!      camera-frame peak rays.
//!    - Verify by projecting *additional* catalog stars within the
//!      candidate FOV into the camera frame and checking how many
//!      of them match additional detected peaks.
//!    - Return the highest-verification-count match if it exceeds
//!      a confidence threshold.
//!
//! # Limitations of the v1 implementation
//!
//! - **Camera intrinsics matter for absolute angular scale.** With
//!   placeholder intrinsics (fx = fy = 1000) the derived angular
//!   distances between peaks are off by the same factor across all
//!   pairs, so *ratios* of distances are preserved. The hash is
//!   built from ratios (largest distance normalizes the others), so
//!   matching still works. The recovered camera attitude has the
//!   right rotation. Per-star altitudes derived from the
//!   intrinsics-dependent pixel→ray mapping are wrong by the same
//!   factor as any other altitude measurement, which is why
//!   calibration is the dominant absolute-altitude error budget
//!   item.
//!
//! - **No light-time, no proper-motion advance to the observation
//!   epoch.** Catalog positions are J2000.0; we use them as-is for
//!   matching. Match accuracy is dominated by detection sigma at
//!   the pixel level (~0.1-1 px) which corresponds to arcminutes,
//!   far larger than the few-arcseconds of motion since J2000.0 for
//!   most stars. Once we need that accuracy, switch to
//!   [`bris_almanac::position_at`] which already does proper-motion
//!   advance.
//!
//! - **Hash database built lazily at first call**, not via build.rs
//!   serialization. For ~3000 stars to mag 5.5 and a 60° max FOV,
//!   the database has ~10^5 patterns and builds in well under a
//!   second on modern hardware. If startup latency becomes a
//!   problem we can serialize at build time later.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    // Numerical / linear-algebra code routinely uses single-letter
    // variable names (i, j, k for indices; u, v, s for SVD; a, b
    // for input vectors). Re-enable per-function if a particular
    // section grows enough to warrant longer names.
    clippy::many_single_char_names,
    clippy::similar_names,
    // Loop indices into arrays of paired indices (e.g. `for i in
    // 0..3 { for j in 0..3 { ... } }`) are clearer with the
    // numeric form than with iter().enumerate() in this code.
    clippy::needless_range_loop
)]

mod altitude;
mod hash;
mod kabsch;
mod solve;

pub use altitude::{star_altitude, star_altitudes, StarAltitude};
pub use hash::{
    pattern_hash, ra_dec_to_unit_vec, CatalogPattern, PatternHash, StarHashDb, StarHashDbConfig,
};
pub use kabsch::{kabsch_rotation, rotate_vec, KabschError};
pub use solve::{
    plate_solve, Attitude, IdentifiedStar, PlateSolveConfig, PlateSolveError, PlateSolveResult,
};
