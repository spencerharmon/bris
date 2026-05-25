//! Pluggable horizon-providers.
//!
//! A [`HorizonProvider`] is any source of a horizon line:
//! classical gradient / sky-region detectors, ML segmentation,
//! auto-detected artificial horizons (reflection pairs, plumb
//! lines, vanishing points), and eventually IMU-derived
//! gravity. Each provider observes per-frame (and eventually
//! cross-frame) evidence and produces a [`HorizonHypothesis`]
//! or declines.
//!
//! The trait is the engine-internal seam that lets new providers
//! drop into the streaming pipeline's horizon dispatch without
//! restructuring the surrounding stages. Phase 1 lands the seam
//! plus the first auto-horizon provider
//! ([`reflection_pair::ReflectionPairProvider`]); the existing
//! optical detectors are wrapped by trivial trait impls in
//! `bris-streaming::pipeline::horizon_providers`.
//!
//! Design and roadmap: `docs/design/horizon_autodetect.md`.

use crate::frame::{Frame, Intrinsics};
use crate::horizon::HorizonLine;
use bris_core::time::Tt;
use bris_core::Uncertain;

pub mod reflection_pair;

pub use reflection_pair::{ReflectionPairConfig, ReflectionPairProvider};

/// Temporal scope a provider operates over.
///
/// Phase 1 implementations all return [`TemporalScope::IntraFrame`].
/// `Window` is reserved for the cross-frame registration work
/// outlined in `docs/design/horizon_autodetect.md` §11; the
/// trait shape carries it so the later phase is additive rather
/// than a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalScope {
    /// Provider consumes only the current frame's evidence.
    IntraFrame,
    /// Provider needs a window of registered frames (cross-frame
    /// pose chain available via the streaming engine). Not used
    /// in Phase 1.
    Window,
}

/// Narrow read-only view of a single body candidate, exposed to
/// horizon providers.
///
/// Constructed by the streaming engine from
/// `BodyDetection::Day(Centroid)` (single candidate) or
/// `BodyDetection::Night(Vec<Peak>)` (one per peak). Keeps
/// `BodyDetection` itself private to `bris-streaming`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyCandidate {
    /// Sub-pixel image coordinates.
    pub pixel: (f64, f64),
    /// Raw background-subtracted brightness in u16-scale.
    /// Higher is brighter; reflection-pair Test 2 uses this.
    pub brightness: f64,
    /// 1σ pixel-position uncertainty.
    pub position_sigma_px: f64,
}

/// Position prior threaded into a horizon-provider context.
///
/// Sourced from the engine's last successful published fix.
/// Phase 1 does **not** project this forward via DR — if the
/// fix is stale beyond the provider's window the provider
/// should treat the prior as absent (cold start). DR projection
/// is a Phase 2 followup.
#[derive(Debug, Clone, Copy)]
pub struct PositionPrior {
    /// Latitude, radians.
    pub lat_rad: f64,
    /// Longitude, radians.
    pub lon_rad: f64,
    /// 1σ horizontal position uncertainty, metres.
    pub sigma_position_m: f64,
    /// TT instant the fix was published; used for staleness.
    pub timestamp: Tt,
}

/// Read-only context handed to a [`HorizonProvider`].
///
/// Holds the working analysis frame, intrinsics, the per-frame
/// body candidates (a narrow view of `BodyDetection`), an
/// optional position prior, and the frame's capture instant.
#[derive(Debug, Clone, Copy)]
pub struct HorizonProviderContext<'a> {
    /// Working-resolution analysis frame.
    pub frame: &'a Frame,
    /// Camera intrinsics matching `frame`.
    pub intrinsics: &'a Intrinsics,
    /// Body candidates available this frame. Empty for
    /// classifications where Stage B produced nothing.
    pub body_candidates: &'a [BodyCandidate],
    /// Position prior, if available; `None` on cold start.
    pub position_prior: Option<PositionPrior>,
    /// Capture instant of the frame.
    pub timestamp: Tt,
}

/// A horizon hypothesis produced by a provider.
#[derive(Debug, Clone, Copy)]
pub struct HorizonHypothesis {
    /// The horizon line, in image (pixel) coordinates.
    pub line: HorizonLine,
    /// Where this hypothesis came from. Carried into engine
    /// diagnostics.
    pub provenance: HorizonProvenance,
    /// Optional direct sight emitted from the same evidence
    /// (e.g. reflection pair's `Ho = θ/2`). The sight-combination
    /// stage in `bris-nav` de-duplicates per-body sights in a
    /// window so the same body's two sights (the direct one and
    /// the horizon-derived one) do not both contribute.
    pub direct_sight: Option<DirectSight>,
}

/// Direct sight produced alongside a horizon hypothesis.
#[derive(Debug, Clone, Copy)]
pub struct DirectSight {
    /// Image-pixel coordinates of the body candidate this sight
    /// identifies. The streaming engine uses this to attribute
    /// the sight to a body record.
    pub body_pixel: (f64, f64),
    /// Observed altitude (radians) with 1σ. For reflection
    /// pairs this is `θ/2` where θ is the angle between the
    /// direct and reflected body rays.
    pub observed_altitude: Uncertain<f64>,
}

/// Origin of a [`HorizonHypothesis`].
///
/// The optical-detector variant carries a `u8` discriminant
/// rather than the streaming-engine's `HorizonDetector` enum so
/// `bris-vision` does not depend on `bris-streaming`. The
/// streaming engine maps the discriminant back to its own enum
/// at the call site (a 1:1 mapping documented in
/// `bris-streaming::pipeline::horizon_providers`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizonProvenance {
    /// Classical optical detector. The `kind` discriminant
    /// is decoded by the streaming engine.
    Optical(OpticalKind),
    /// Auto-detected reflection pair.
    ReflectionPair {
        /// Number of surviving pairs in the winning cluster.
        pair_count: usize,
        /// Whether the position-prior catalog test (Test 3) was
        /// applied (vs cold-start with the stricter Test 4
        /// threshold).
        used_position_prior: bool,
    },
}

/// Discriminator for the five classical optical horizon
/// detectors. Kept in `bris-vision` so the trait + provenance
/// are self-contained; the streaming engine maps to its own
/// `HorizonDetector` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpticalKind {
    /// Day gradient detector.
    Gradient,
    /// Day sky-region detector.
    SkyRegion,
    /// Night brightness-boundary detector.
    Night,
    /// Night textured-boundary detector.
    NightTextured,
    /// ML segmentation (feature-gated; constructible only
    /// when the `segmentation` feature is enabled by the
    /// streaming engine).
    Segmentation,
}

/// Common interface for any source of a horizon line.
pub trait HorizonProvider {
    /// Short stable name, suitable for tracing / diagnostics.
    fn name(&self) -> &'static str;

    /// Whether the provider needs only the current frame
    /// ([`TemporalScope::IntraFrame`]) or a window of
    /// registered frames ([`TemporalScope::Window`]). All
    /// Phase 1 implementations return `IntraFrame`.
    fn temporal_scope(&self) -> TemporalScope;

    /// Run the provider against the given evidence.
    ///
    /// Returns `None` when the provider declines (no evidence,
    /// failing tests, etc.) — never silent fallback. Callers
    /// merge the surviving hypotheses across providers via a
    /// best-σ rule.
    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis>;
}
