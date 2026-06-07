//! Configuration of the streaming engine.
//!
//! [`EngineConfig`] is the single struct passed to
//! [`crate::StreamingEngine::new`]. It bundles the observer
//! geometry needed by the almanac, the timing knobs that govern
//! the ring buffer and sight window, the publication interval,
//! and the choice of when to build the plate-solving hash
//! database.
//!
//! All defaults come from `docs/design/frame_scheduling.md`'s
//! summary recommendations:
//!
//! - 2-second stitching window (= 60 frames at 30 fps; the upper
//!   end of useful frame separation for cross-frame stitching).
//! - 7200-second (2-hour) sight window with linear age-weighting
//!   constant 600 s. Operator-targeted opportunistic flows often
//!   take same-body sights 30+ minutes apart (cold-start `CoP`
//!   needs ≥ 2 same-body LOPs on different time-shifted GP
//!   circles); 2 h covers that with margin. See
//!   `docs/design/sight_persistence.md` and
//!   `docs/design/circle_of_position.md`.
//! - Cap of 50 sights in the window, replace-worst on insertion.
//! - Minimum 1-second interval between fix publications.
//! - Single worker thread.
//! - Plate-solving database built lazily on first night frame.

use bris_almanac::Observer;
use bris_core::{Hemisphere, Sigma};
use bris_platesolve::{PlateSolveConfig, StarHashDbConfig};
use bris_vision::{
    ConditionConfig, HorizonConfig, HorizonFusionConfig, NightHorizonConfig, PeakConfig,
    SaturatedBodyConfig, TexturedHorizonConfig,
};
use std::path::PathBuf;

use crate::store::StoreConfig;

/// Top-level engine configuration.
///
/// Per-provider Stage C dispatch enables. Default: all `true`
/// (full dispatch matches pre-existing behaviour). Operators
/// flip entries off to inspect a constrained provider set
/// during diagnostic replays; the corpus explorer renders the
/// surviving provider's hypothesis without interference.
///
/// `ml_gravity` here is the dispatch enable inside the
/// provider set; the overall ML-gravity gate is the
/// conjunction of [`EngineConfig::enable_ml_gravity`] AND
/// `horizon_provider_set.ml_gravity`. This lets a session
/// have ML loaded but temporarily run in (e.g.) gradient-only
/// mode for an A/B comparison.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // one bool per provider; enum-set would obscure the dispatch
pub struct HorizonProviderSet {
    /// Day gradient detector.
    pub gradient: bool,
    /// Day sky-region detector.
    pub sky_region: bool,
    /// Night brightness-boundary detector.
    pub night: bool,
    /// Night textured-boundary detector.
    pub night_textured: bool,
    /// Segmentation horizon detector
    /// (cfg-gated by `segmentation` feature).
    pub segmentation: bool,
    /// Reflection-pair provider (post-Stage-B).
    pub reflection_pair: bool,
    /// Vertical-line provider (off by default at the engine
    /// level via [`EngineConfig::enable_vertical_line_provider`];
    /// this entry only matters when that flag is also set).
    pub vertical_line: bool,
    /// Vanishing-point provider.
    pub vanishing_point: bool,
    /// ML-gravity provider (cfg-gated by `ml-gravity`
    /// feature; also requires
    /// [`EngineConfig::enable_ml_gravity = true`] AND a
    /// loaded model).
    pub ml_gravity: bool,
}

impl Default for HorizonProviderSet {
    fn default() -> Self {
        Self {
            gradient: true,
            sky_region: true,
            night: true,
            night_textured: true,
            segmentation: true,
            reflection_pair: true,
            vertical_line: true,
            vanishing_point: true,
            ml_gravity: true,
        }
    }
}

impl HorizonProviderSet {
    /// Construct an all-false set; callers OR in the providers
    /// they want. Use [`Default::default`] for the full set.
    #[must_use]
    pub fn none() -> Self {
        Self {
            gradient: false,
            sky_region: false,
            night: false,
            night_textured: false,
            segmentation: false,
            reflection_pair: false,
            vertical_line: false,
            vanishing_point: false,
            ml_gravity: false,
        }
    }
}

/// Construct with [`EngineConfig::new`] passing the observer
/// position; override individual fields directly. The defaults are
/// chosen to match the design-doc recommendations; nearly every
/// real deployment will want to override at least the
/// [`Observer`].
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct EngineConfig {
    /// Observer position, eye height, and atmospheric model.
    /// Drives almanac apparent-place computations and horizon-dip
    /// uncertainty.
    pub observer: Observer,

    /// Maximum age difference, in seconds, between two frames the
    /// engine will consider for cross-frame stitching. Default 2.0.
    ///
    /// At 30 fps, a 2-second window holds ~60 frames; at 60 fps,
    /// ~120. Memory scales linearly: ~few hundred MB for HD
    /// frames at 60 frames buffered, manageable on Pi-class
    /// hardware. Increasing this to (say) 5 seconds permits
    /// more-stale stitching pairs at the cost of additional buffer
    /// pressure and a higher per-stitch alignment σ.
    pub stitching_window_seconds: f64,

    /// Maximum age, in seconds, of a sight retained in the active
    /// sight window. Older sights are evicted regardless of σ.
    /// Default 600.0 (10 minutes).
    ///
    /// Without a course/speed input the observer may have moved
    /// in the interval; the linear age-weighting (controlled by
    /// [`Self::sight_age_weight_time_constant_s`]) downweights
    /// older sights smoothly within this window, and the
    /// publication-time motion-staleness gate (see
    /// [`PublicationGateConfig::assumed_max_speed_kn`] and
    /// `docs/design/observer_motion_staleness.md`) refuses to
    /// publish a fix whose σ inflated by motion exceeds
    /// [`PublicationGateConfig::max_position_sigma_nm`].
    ///
    /// Default 7200 (2 hours): covers the multi-capture /
    /// same-body-30-min-apart opportunistic flow the operator
    /// targets. See `docs/design/sight_persistence.md` and
    /// `docs/design/circle_of_position.md`.
    pub sight_window_seconds: f64,

    /// Time constant of the linear age-weighting applied to sights
    /// within the active window, in seconds. A sight of age
    /// `t` contributes with weight `max(0, 1 - t / constant)`.
    /// Default 600.0 (10 minutes), matching the sight window;
    /// the oldest retained sight contributes ~0 weight.
    pub sight_age_weight_time_constant_s: f64,

    /// Maximum number of sights kept in the active window. When
    /// the window is full, an inserted sight replaces the worst-σ
    /// existing entry rather than the oldest. Default 50: the
    /// operator's multi-body opportunistic-scan flow can easily
    /// produce tens of sights across a 2-hour window; the
    /// diminishing-returns inflection on a *single* fix is still
    /// near N≈5, but capacity here governs the pool of
    /// candidates the publication LSQ chooses from, not the
    /// number it must combine.
    pub sight_window_capacity: usize,

    /// Minimum interval, in milliseconds, between fix publications
    /// even if the active sight window changes meaningfully more
    /// often. Default 1000 (1 Hz). Acts as a publication-rate cap
    /// to avoid swamping NMEA consumers when many frames produce
    /// new sights in quick succession.
    pub min_fix_publication_interval_ms: u64,

    /// Number of worker threads processing the input ring buffer.
    /// Default 1 — the design-doc baseline. Increase only when
    /// measurement demands it.
    pub max_concurrent_pipeline_workers: usize,

    /// Capacity of the input ring buffer of *raw* frames awaiting
    /// processing. When full, [`crate::StreamingEngine::push_frame`]
    /// silently drops the incoming frame (backpressure). Default
    /// 120 — covers a 2-second stitching window at 60 fps with
    /// modest headroom; tune if processing routinely lags.
    pub input_ring_capacity: usize,

    /// When and how to build the plate-solver hash database.
    /// Default [`PlateSolverInit::Lazy`] — defers the ~10-30
    /// second build cost to the first night frame. Use
    /// [`PlateSolverInit::AtStartup`] to absorb the cost during
    /// engine construction (preferable for marine deployments
    /// where the operator starts the engine well before
    /// twilight).
    pub plate_solver_init: PlateSolverInit,

    /// Minimum number of frames the day/night classifier must
    /// agree on before the engine switches its method set.
    /// Default 90 (~3 seconds at 30 fps). Prevents single-frame
    /// transients (cloud transit, exposure adjustment) from
    /// flipping the engine between day and night detector
    /// pipelines.
    pub classifier_hysteresis_frames: u32,

    /// Stage A configuration: knobs for the day/night/twilight
    /// classifier. See [`bris_vision::ConditionConfig`] for the
    /// individual fields. Defaults are tuned for typical 8-bit-
    /// widened-to-u16 imagery; override only when running with
    /// unusual exposure or sensor characteristics.
    pub condition_cfg: ConditionConfig,

    /// Stage B day-path configuration: thresholds for the
    /// saturated-body centroider used when the classifier
    /// reports `Day` (and as the first attempt under
    /// `Twilight`). See [`bris_vision::SaturatedBodyConfig`].
    pub saturated_body_cfg: SaturatedBodyConfig,

    /// Stage B night-path configuration: thresholds for the
    /// peak detector used when the classifier reports `Night`
    /// (and as the fallback under `Twilight`). See
    /// [`bris_vision::PeakConfig`].
    pub peak_cfg: PeakConfig,

    /// Safety margin (pixels) above the Stage C horizon line
    /// that the night peak detector treats as off-limits.
    /// Excluded so the horizon line's own gradient and any
    /// shipboard structure silhouetted against the sky just
    /// above it (rigging, antennas, deck superstructure) don't
    /// produce spurious "peaks." Default 5; raise on cluttered
    /// shipboard captures, set to 0 to disable. Has no effect
    /// when Stage C produced no horizon (peak detection runs
    /// unmasked).
    pub peak_horizon_margin_px: u32,

    /// Stage C day-path horizon configuration: shared by the
    /// gradient ([`bris_vision::detect_horizon`]) and sky-region
    /// ([`bris_vision::detect_horizon_via_sky_region`]) and
    /// segmentation ([`bris_vision::detect_horizon_via_segmentation`])
    /// detectors. See [`bris_vision::HorizonConfig`].
    pub horizon_cfg: HorizonConfig,

    /// Stage C night-path horizon configuration for the
    /// mean-gradient night detector
    /// ([`bris_vision::detect_horizon_night`]). See
    /// [`bris_vision::NightHorizonConfig`].
    pub night_horizon_cfg: NightHorizonConfig,

    /// Stage C night-path horizon configuration for the
    /// textured night detector
    /// ([`bris_vision::detect_horizon_night_textured`]). See
    /// [`bris_vision::TexturedHorizonConfig`].
    pub textured_horizon_cfg: TexturedHorizonConfig,

    /// Early-termination threshold on horizon σ, in radians.
    /// Once Stage C has produced a horizon line whose
    /// `altitude_sigma` is at or below this value, no further
    /// (more-expensive) horizon detectors are tried for the
    /// frame. Default `1 arcmin ≈ 2.91e-4 rad` — the noise
    /// floor of a clean sea horizon, below which the cost of
    /// running segmentation is rarely justified.
    ///
    /// Set to `f64::INFINITY` to always try every detector
    /// (useful for benchmarking; not for production where the
    /// segmentation cost dominates).
    pub horizon_early_termination_sigma_rad: f64,

    /// Optional analysis resolution for Stage C (horizon
    /// detection). When `Some((w, h))`, the engine asks the
    /// frame's [`bris_vision::FramePyramid`] for a
    /// downsampled level at that resolution and runs every
    /// horizon detector against the downsampled level instead
    /// of the source frame.
    ///
    /// `None` (default) preserves the historical behavior:
    /// every detector receives the source frame. The
    /// underlying detectors (gradient, sky-region, etc.)
    /// already downsample internally to
    /// their preferred working size; setting this knob lets
    /// the engine factor that downsample out of every detector
    /// so they share a single decimation instead of each doing
    /// downsampling on the same source.
    ///
    /// The chosen `(w, h)` must preserve the source frame's
    /// aspect ratio (within
    /// [`bris_vision::Intrinsics::scaled_to`]'s tolerance) and
    /// must be ≤ source dimensions (no upsampling). Mismatch
    /// degrades gracefully: the stage logs a debug message
    /// and falls back to the source frame.
    ///
    /// Mutually exclusive with
    /// [`Self::horizon_analysis_max_long_edge_px`]; setting
    /// both is a config error caught at engine construction.
    pub horizon_analysis_size: Option<(u32, u32)>,

    /// Aspect-ratio-agnostic alternative to
    /// [`Self::horizon_analysis_size`]: cap the long edge of
    /// the horizon-analysis frame at this many pixels and
    /// derive the matching short edge from the source's actual
    /// aspect ratio. When `Some(n)`, Stage C asks the pyramid
    /// for a level sized so `max(level_w, level_h) ≤ n` and
    /// the level preserves the source aspect exactly.
    ///
    /// Preferred over `horizon_analysis_size` when the engine
    /// may run against sources of varying aspect ratios (phone
    /// sensors are commonly 4:3, machine-vision sensors often
    /// 16:9 or 3:2, embedded sensors sometimes squarish). The
    /// pixel-pair form refuses to downsample anything whose
    /// aspect doesn't match the configured `(w, h)`; the
    /// long-edge form keeps working.
    ///
    /// Default: `Some(1280)`. Horizon detectors saturate well
    /// below 1280 px on the long edge — gradient SNR is set by
    /// the sky-sea transition contrast, not the pixel grid,
    /// and segmentation models get *worse* above their
    /// training resolution. Capping the horizon stage at this
    /// size keeps it cheap even when capture runs at 4K+, and
    /// the per-stage architecture exists exactly so this
    /// trade-off is invisible to higher-resolution stages
    /// (centroiding, plate-solve).
    ///
    /// Sources at or below the cap pass through unchanged (the
    /// pyramid declines to upsample).
    ///
    /// Mutually exclusive with [`Self::horizon_analysis_size`];
    /// setting both is a config error caught at engine
    /// construction.
    pub horizon_analysis_max_long_edge_px: Option<u32>,

    /// Path to the ONNX segmentation model used by the
    /// last-resort horizon detector
    /// ([`bris_vision::detect_horizon_via_segmentation`]).
    /// `None` (default) disables the segmentation detector
    /// entirely — the engine never tries it regardless of how
    /// poorly the cheap detectors performed. Set to `Some(path)`
    /// to opt in. The model is loaded lazily on first use and
    /// cached in [`bris_vision`]'s global `MODEL`.
    ///
    /// Even when set, the segmentation detector is gated on the
    /// `segmentation` feature flag (default-on); building
    /// without it makes this field inert.
    pub segmentation_model_path: Option<PathBuf>,

    /// Stage D: dispatch gate for the plate solver. Even when
    /// Stage B produced a `Night` peak payload, the engine
    /// only invokes [`bris_platesolve::plate_solve`] when this
    /// policy admits the frame. Default
    /// [`StageDDispatchPolicy::WhenStarsExpected`]: skip Stage
    /// D on Day-classified frames (no peaks) and on
    /// Night-classified frames with fewer than 3 peaks (the
    /// geometric-hash matcher needs at least three peaks to
    /// form a triangle pattern; below that it cannot succeed).
    ///
    /// On indoor / no-stars-visible scenes (the bedroom-moon
    /// corpus is the canonical case) the gate eliminates the
    /// ~30–50 ms per-frame plate-solve cost that's structurally
    /// guaranteed to find nothing.
    ///
    /// Set to [`StageDDispatchPolicy::Always`] to recover the
    /// pre-gate behaviour (every Night payload is dispatched
    /// regardless of peak count); [`StageDDispatchPolicy::Never`]
    /// disables Stage D entirely.
    pub stage_d_dispatch_policy: StageDDispatchPolicy,

    /// Stage D: configuration for the geometric-hash plate
    /// solver database
    /// ([`bris_platesolve::StarHashDb::build`]). Defaults
    /// match the platesolve crate's recommendations: 60° max
    /// pattern diameter, magnitude cutoff 5.5, 50 hash bins,
    /// 20 neighbors per anchor. Override only when targeting
    /// a non-typical FOV or stellar density.
    pub star_hash_db_cfg: StarHashDbConfig,

    /// Stage D: configuration for individual plate-solve
    /// attempts ([`bris_platesolve::plate_solve`]). Defaults
    /// match the platesolve crate's recommendations.
    pub plate_solve_cfg: PlateSolveConfig,

    /// Stage D: per-identified-star angular σ added in
    /// quadrature to the horizon σ when computing per-star
    /// altitudes (see [`bris_platesolve::star_altitudes`]).
    /// Reasonable values come from the plate-solve refinement
    /// RMS (typically a few arcseconds with calibrated
    /// intrinsics). Default 30 arcsec ≈ 1.45e-4 rad — matches
    /// `PlateSolveConfig::default()`'s `max_rms_residual_rad`,
    /// so a successful solve is by construction at or below
    /// this σ.
    pub per_star_sigma: Sigma,

    /// Maximum age (seconds) of a published fix before the
    /// engine treats it as stale and stops surfacing it as a
    /// [`bris_vision::PositionPrior`] to horizon providers
    /// (notably the reflection-pair provider's catalog
    /// consistency test). DR projection of stale fixes is a
    /// Phase 2 followup; see
    /// `docs/handoff/reflection-pair-phase1.md`.
    pub position_prior_max_age_seconds: f64,

    /// Vertical-line provider tunables. See
    /// [`bris_vision::VerticalLineConfig`]. Defaults are
    /// sensible (±20° from vertical, min 50 px line,
    /// 1e-3 rad floor); operator may tune.
    ///
    /// These tunables only take effect when
    /// [`Self::enable_vertical_line_provider`] is `true`. As
    /// of the disable-by-default change the provider is off
    /// in Stage C by default; see that field's docs.
    pub vertical_line_provider_config: bris_vision::VerticalLineConfig,

    /// Whether the [`bris_vision::VerticalLineProvider`] is
    /// dispatched by Stage C. Defaults to `false`.
    ///
    /// The provider infers camera-frame gravity from a single
    /// near-vertical line via the approximation
    /// `gravity ≈ r_bot - r_top` (image-space endpoint
    /// difference). That approximation is only valid for
    /// *short* lines *centered on the principal point*; for
    /// full-height lines on tilted cameras — the common
    /// hand-held capture geometry — the inferred gravity is
    /// wrong by 20–40°, and the synthesised horizon is
    /// confidently wrong. The operator diagnosed this on the
    /// bedroom-moon corpus.
    ///
    /// The provider stays in the codebase (other consumers
    /// reference it, the unit tests still exercise it) but is
    /// off by default in the streaming engine pending the
    /// ML-gravity provider described in
    /// `docs/design/ml_gravity.md`. Operators with a true
    /// plumb-string rig — short string centered in the FOV —
    /// may set this to `true` to opt back in.
    pub enable_vertical_line_provider: bool,

    /// Per-provider Stage C enable flags. Defaults: all true
    /// (preserves the current dispatch). Operators (or the
    /// `bris-cli replay --horizon-providers ...` flag) flip
    /// individual entries off to constrain Stage C to a
    /// subset — useful for adversarial / diagnostic replays
    /// where the operator wants to inspect one provider in
    /// isolation. Production captures should leave these
    /// alone; the publication-gate + fusion layer already
    /// handles dishonest providers in the typical case.
    pub horizon_provider_set: HorizonProviderSet,

    /// Path to the heteroscedastic ML-gravity ONNX model.
    /// `None` (default) disables the [`bris_vision::MlGravityProvider`]
    /// dispatch entirely. The provider is *also* gated on
    /// [`Self::enable_ml_gravity`]; both must be set for
    /// Stage C to invoke it. See
    /// `docs/design/ml_gravity.md`.
    ///
    /// Even when set, the provider is gated on the `ml-gravity`
    /// cargo feature (default-off in `bris-vision`); building
    /// without it makes this field inert.
    pub ml_gravity_model_path: Option<PathBuf>,

    /// Whether the [`bris_vision::MlGravityProvider`] is
    /// dispatched by Stage C. Defaults to `false`.
    ///
    /// When `true` AND [`Self::ml_gravity_model_path`] is
    /// `Some(_)` AND the `ml-gravity` cargo feature is on, the
    /// provider runs *last* in the Stage C dispatch and
    /// contributes a low-priority hypothesis (geometric
    /// providers always win on real horizons because their σ
    /// floor is tighter; the ML provider is the silent winner
    /// when nothing else fires).
    pub enable_ml_gravity: bool,

    /// Tunables for the ML-gravity provider (σ floor / ceiling,
    /// model-path override). Default takes the path from
    /// [`Self::ml_gravity_model_path`].
    #[cfg(feature = "ml-gravity")]
    pub ml_gravity_provider_config: bris_vision::MlGravityConfig,

    /// Configuration for the vanishing-point horizon provider
    /// (`bris_vision::VanishingPointProvider`). The most
    /// expensive of the auto-horizon providers; dispatched
    /// only when cheaper providers fail to clear the
    /// early-termination threshold.
    pub vanishing_point_provider_config: bris_vision::VanishingPointConfig,

    /// Stage C: multi-source horizon fusion configuration.
    /// When two or more providers produce concordant
    /// hypotheses on the same frame the fuser combines them
    /// for a tighter σ; when they disagree the fuser falls
    /// back to the lowest-σ singleton and increments the
    /// `horizon_fusion_discordant_frames` diagnostic.
    /// `enabled = false` reverts to the pre-fusion best-σ
    /// behavior.
    pub horizon_fusion: HorizonFusionConfig,

    /// On-disk persistence of the sight + fix pool. See
    /// [`crate::StoreConfig`] for the knobs. Default is
    /// `enabled: true` rooted at `"."`; tests that don't
    /// want persistence override with `enabled: false`.
    pub store: StoreConfig,

    /// Cold-start fix fallback configuration. See
    /// [`ColdStartEngineConfig`].
    pub cold_start: ColdStartEngineConfig,

    /// Geometric-diversity and motion-staleness gate applied
    /// *after* `multi_sight_fix` succeeds and *before* the fix
    /// goes out on the publication channel. Honest-but-misleading
    /// fixes (degenerate azimuth spread, huge ellipses, sights
    /// stale relative to operator motion) are rejected here so
    /// downstream consumers never see them. See
    /// [`PublicationGateConfig`] and
    /// `docs/design/observer_motion_staleness.md`.
    pub publication_gate: PublicationGateConfig,

    /// Enable the ephemeris-driven Stage E cross-frame stitch
    /// fallback. When the Harris+NCC stitcher
    /// ([`bris_vision::panorama_altitude_for_pair`]) declines
    /// on a cross-frame pair, Stage E falls back to the
    /// ephemeris predictor in
    /// `pipeline::ephemeris_stitch`: if a body candidate
    /// detected in the horizon frame lies within 3·σ of the
    /// ephemeris-predicted displacement from the body frame's
    /// centroid, Stage E accepts the correspondence under an
    /// identity-rotation (stationary-camera) assumption and
    /// emits a sight whose stitch σ is the *angular* prediction
    /// σ (≈ `STITCH_SIGMA_PER_SECOND_RAD × Δt`). Default `true`;
    /// operators may set to `false` for A/B comparison against
    /// the Harris+NCC-only baseline.
    pub enable_ephemeris_stitch_fallback: bool,

    /// **Diagnostic-only.** When `true`, the engine accepts the
    /// `ApInput` baked into its initial [`Observer`] and refuses
    /// to re-derive the assumed position from any other source for
    /// the rest of its life:
    ///
    /// - prior-published fixes do **not** flow back into horizon
    ///   detection as a [`bris_vision::PositionPrior`];
    /// - the cold-start circle-of-position solver is **not** run
    ///   (neither on `multi_sight_fix` failure nor on the
    ///   stale-prior trigger);
    /// - the stale-prior trigger inside `try_publish` is a no-op.
    ///
    /// Every suppressed re-derivation increments
    /// [`crate::EngineDiagnostics::ap_rederive_suppressed_count`]
    /// so an operator can see how often the lock fired.
    ///
    /// The flag exists **solely** to let `bris-cli replay
    /// --ap-lock-truth` bisect the celestial error budget by
    /// holding the AP fixed at a known-good (e.g. GPS-truth)
    /// position across a whole replay. Production code paths
    /// must leave this `false`.
    pub lock_ap_for_replay: bool,
}

/// Cold-start fix fallback knobs.
///
/// The cold-start solver (`bris_nav::cold_start_fix`) runs in
/// Stage E when [`bris_nav::multi_sight_fix`] returns
/// `SingularGeometry` (or no position prior is available at
/// all). See `docs/design/circle_of_position.md`,
/// "Engine integration".
#[derive(Debug, Clone, Copy)]
pub struct ColdStartEngineConfig {
    /// Master switch. When `false`, Stage E never falls back to
    /// the cold-start solver; a Saint-Hilaire failure simply
    /// publishes nothing. Default `true`.
    pub enabled: bool,
    /// Optional hemisphere hint for resolving two-candidate
    /// cold-start results. When `Some`, Stage E picks the
    /// candidate whose latitude lies in the configured
    /// hemisphere and publishes with
    /// [`crate::FixProvenance::ColdStartAmbiguous`]. When
    /// `None`, two-candidate results are not published (the
    /// operator-prompt FFI channel is a follow-up). Default
    /// `None`.
    pub coarse_hemisphere: Option<Hemisphere>,
    /// When Saint-Hilaire publishes a fix whose max |intercept|
    /// exceeds this threshold, the assumed position is likely
    /// so wrong that cold-start may produce a tighter answer;
    /// cold-start runs as a comparison and is preferred when
    /// it converges with tighter `σ_major`. Default 60 nm = 1°
    /// intercept = AP is more than 60 nm off-position.
    pub stale_prior_intercept_threshold_nm: f64,
}

/// Pre-publication gate on a freshly-solved fix.
///
/// `multi_sight_fix` reports the LSQ residual covariance
/// honestly. When the underlying geometry is degenerate (all
/// sights clustered at one azimuth, or a single body's `CoP`
/// pair with poor intersection geometry) the resulting
/// ellipse is huge — honest, but operationally useless. This
/// gate refuses to publish such fixes; the engine still
/// retains the sights for a later, better-conditioned
/// combination.
///
/// The gate is additive to (not a replacement for)
/// `multi_sight_fix`'s own singular-geometry rejection: the
/// LSQ refuses when the normal matrix is uninvertible; this
/// gate refuses when the resulting ellipse is wide enough
/// (in axis ratio or absolute σ) to be operationally useless.
///
/// Setting any threshold to `f64::INFINITY` disables that
/// individual check. Setting all to infinity (and
/// `min_azimuth_spread_rad = 0.0`) recovers the pre-gate
/// publish-everything-the-LSQ-accepts behavior.
#[derive(Debug, Clone, Copy)]
pub struct PublicationGateConfig {
    /// Minimum azimuth spread, in radians, across the
    /// contributing sights. Default `30° ≈ 0.524 rad`. Below
    /// this the LOPs are nearly parallel and the fix is
    /// "good along track, weak across track" — usually not
    /// what the operator wants surfaced as a navigation fix.
    pub min_azimuth_spread_rad: f64,
    /// Maximum allowed ratio of the position ellipse's major
    /// to minor axis. Default 10.0. A ratio above this is
    /// a sign of degenerate geometry that the absolute σ
    /// gate alone might miss (small absolute σ with extreme
    /// elongation is still a sliver-fix).
    pub max_ellipse_axis_ratio: f64,
    /// Maximum allowed σ along the ellipse's major axis, in
    /// nautical miles. Default 50.0. Above this the fix is
    /// useless for celestial-grade navigation regardless of
    /// how the σ accumulated.
    pub max_position_sigma_nm: f64,
    /// Assumed worst-case observer speed, in knots, between
    /// the oldest sight and the publication instant. Used
    /// only by the motion-staleness gate: a sight of age `t`
    /// seconds inflates its σ-contribution to the fix's
    /// major-axis σ by `assumed_max_speed_kn * t / 3600` nm
    /// (RSS). Default 0.0 (stationary use): the gate is
    /// inert. Set higher for mobile use (5-10 kn for a
    /// sailing yacht, ~30 kn for a power vessel). See
    /// `docs/design/observer_motion_staleness.md`.
    pub assumed_max_speed_kn: f64,
}

impl Default for ColdStartEngineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            coarse_hemisphere: None,
            stale_prior_intercept_threshold_nm: 60.0,
        }
    }
}

impl Default for PublicationGateConfig {
    fn default() -> Self {
        Self {
            min_azimuth_spread_rad: 30.0_f64.to_radians(),
            max_ellipse_axis_ratio: 10.0,
            max_position_sigma_nm: 50.0,
            assumed_max_speed_kn: 0.0,
        }
    }
}

impl EngineConfig {
    /// Construct an engine config with the given observer and the
    /// design-doc defaults for everything else.
    #[must_use]
    pub fn new(observer: Observer) -> Self {
        Self {
            observer,
            stitching_window_seconds: 2.0,
            sight_window_seconds: 7200.0,
            sight_age_weight_time_constant_s: 600.0,
            sight_window_capacity: 50,
            min_fix_publication_interval_ms: 1_000,
            max_concurrent_pipeline_workers: 1,
            input_ring_capacity: 120,
            plate_solver_init: PlateSolverInit::Lazy,
            stage_d_dispatch_policy: StageDDispatchPolicy::default(),
            classifier_hysteresis_frames: 90,
            condition_cfg: ConditionConfig::default(),
            saturated_body_cfg: SaturatedBodyConfig {
                // 95% of u16::MAX = 62258, matching the
                // bris-vision "saturation" convention. The
                // outer cast back to u16 is bounded by 100/100
                // and is therefore safe.
                #[allow(clippy::cast_possible_truncation)]
                saturation_threshold: (u32::from(u16::MAX) * 95 / 100) as u16,
                min_area_px: 50,
            },
            peak_cfg: PeakConfig::default(),
            peak_horizon_margin_px: 5,
            horizon_cfg: HorizonConfig::default(),
            night_horizon_cfg: NightHorizonConfig::default(),
            textured_horizon_cfg: TexturedHorizonConfig::default(),
            // 1 arcmin = π / (60 × 180) rad ≈ 2.909e-4. Below
            // this we don't bother running segmentation; clean
            // sea horizons hit it routinely.
            horizon_early_termination_sigma_rad: std::f64::consts::PI / (60.0 * 180.0),
            horizon_analysis_size: None,
            horizon_analysis_max_long_edge_px: Some(1280),
            segmentation_model_path: None,
            star_hash_db_cfg: StarHashDbConfig::default(),
            plate_solve_cfg: PlateSolveConfig::default(),
            // 30 arcsec → radians.
            per_star_sigma: Sigma::new(30.0 * std::f64::consts::PI / (180.0 * 3600.0))
                .expect("30 arcsec is a valid Sigma"),
            position_prior_max_age_seconds: 30.0,
            vertical_line_provider_config: bris_vision::VerticalLineConfig::default(),
            // Disabled by default; see field doc + docs/design/ml_gravity.md
            // for the gravity-math bug that motivated the change.
            enable_vertical_line_provider: false,
            horizon_provider_set: HorizonProviderSet::default(),
            ml_gravity_model_path: None,
            enable_ml_gravity: false,
            #[cfg(feature = "ml-gravity")]
            ml_gravity_provider_config: bris_vision::MlGravityConfig::default(),
            vanishing_point_provider_config: bris_vision::VanishingPointConfig::default(),
            horizon_fusion: HorizonFusionConfig::default(),
            store: StoreConfig::default(),
            cold_start: ColdStartEngineConfig {
                enabled: true,
                coarse_hemisphere: None,
                stale_prior_intercept_threshold_nm: 60.0,
            },
            publication_gate: PublicationGateConfig::default(),
            enable_ephemeris_stitch_fallback: true,
            // Production default: never lock. See the field docs;
            // only `bris-cli replay --ap-lock-truth` flips this on.
            lock_ap_for_replay: false,
        }
    }

    /// Resolve the effective horizon-analysis resolution for a
    /// source frame of size `(source_w, source_h)`.
    ///
    /// Returns:
    ///
    /// - `Some((w, h))` to ask the pyramid for a level at that
    ///   resolution. The aspect ratio matches the source
    ///   (within `Intrinsics::scaled_to` tolerance).
    /// - `None` to run horizon detection on the source frame
    ///   unchanged.
    ///
    /// Precedence: `horizon_analysis_size` (explicit pair)
    /// wins over `horizon_analysis_max_long_edge_px`
    /// (aspect-derived) when both are set; setting both is a
    /// configuration mistake that the FFI layer rejects, but
    /// in-process callers using the Rust API directly are
    /// trusted, so the engine just picks one and proceeds.
    ///
    /// The long-edge form derives `(w, h)` by scaling the
    /// source dimensions uniformly so `max(w, h) == cap` (or
    /// the source as-is when it already fits under the cap),
    /// then rounding to the nearest even integer per axis to
    /// keep the YUYV / 2×2 box-downsample invariants happy.
    /// Returns `None` if the derivation would yield a degenerate
    /// dimension (zero or larger than source).
    #[must_use]
    pub fn resolved_horizon_analysis_size(
        &self,
        source_w: u32,
        source_h: u32,
    ) -> Option<(u32, u32)> {
        if let Some(pair) = self.horizon_analysis_size {
            return Some(pair);
        }
        let cap = self.horizon_analysis_max_long_edge_px?;
        let long_edge = source_w.max(source_h);
        if cap == 0 || long_edge == 0 {
            return None;
        }
        if cap >= long_edge {
            // Source already fits under the cap; let the pyramid
            // hand the source back unchanged via its
            // "asking for >= source returns full" branch.
            return Some((source_w, source_h));
        }
        // Uniform scale so the long edge lands at `cap`. Round
        // each axis down to the nearest even integer so YUYV
        // even-width and 2×2 box-downsample invariants hold.
        let scale = f64::from(cap) / f64::from(long_edge);
        // scale ∈ (0, 1] by construction (cap < long_edge in
        // this branch), so scale * source_w and scale * source_h
        // are both in (0, source dim]. The cast cannot truncate
        // a meaningful value or lose a sign that wasn't already
        // checked against zero below.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let w = ((f64::from(source_w) * scale) as u32) & !1;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let h = ((f64::from(source_h) * scale) as u32) & !1;
        if w == 0 || h == 0 || w > source_w || h > source_h {
            return None;
        }
        Some((w, h))
    }
}

/// Dispatch gate for Stage D (plate solve).
///
/// See [`EngineConfig::stage_d_dispatch_policy`] for the
/// per-frame rule. `Default` is
/// [`StageDDispatchPolicy::WhenStarsExpected`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StageDDispatchPolicy {
    /// Always invoke Stage D when Stage B produced a `Night`
    /// peak payload, regardless of how few peaks were
    /// detected. Recovers the pre-gate behaviour; preferred
    /// only for diagnostic replays where the operator wants
    /// to record every solve attempt even on payloads that
    /// are guaranteed to fail.
    Always,
    /// Invoke Stage D only when the dispatched classifier
    /// verdict is `Night` AND Stage B produced ≥ 3 peaks (the
    /// geometric-hash matcher's structural floor). Default.
    #[default]
    WhenStarsExpected,
    /// Never invoke Stage D. Stage D is uniformly skipped and
    /// every Night payload remains as `BodyDetection::Night(peaks)`
    /// downstream. Useful for measuring the rest-of-pipeline
    /// cost in isolation.
    Never,
}

/// When and how the plate-solving hash database is built.
///
/// The database build is ~10-30 seconds in release. The choice
/// trades startup latency for first-night-fix latency.
///
/// See `docs/design/frame_scheduling.md` "Open questions → Plate
/// solving's database build cost" for the rationale.
#[derive(Debug, Clone)]
pub enum PlateSolverInit {
    /// Build the hash database during
    /// [`crate::StreamingEngine::new`]. Engine construction
    /// blocks for the full build cost (~10-30 s release). After
    /// that, the first night frame plate-solves at ~10-50 ms.
    /// Recommended for marine deployments where the operator
    /// starts the engine well before twilight.
    AtStartup,
    /// Build the database the first time a night/twilight frame
    /// arrives. Engine construction is fast; the *first* night
    /// frame's plate solve blocks for the full build cost
    /// (~10-30 s release). Subsequent night frames solve at
    /// ~10-50 ms. Default.
    Lazy,
    /// Load a pre-built database from disk. **Not yet
    /// implemented**: the on-disk format hasn't been defined.
    /// The variant is reserved so that switching to it later
    /// doesn't break callers.
    Cached(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design_doc_recommendations() {
        let cfg = EngineConfig::new(Observer::default_dev());
        assert!((cfg.stitching_window_seconds - 2.0).abs() < f64::EPSILON);
        assert!((cfg.sight_window_seconds - 7200.0).abs() < f64::EPSILON);
        assert_eq!(cfg.sight_window_capacity, 50);
        let gate = cfg.publication_gate;
        assert!((gate.min_azimuth_spread_rad - 30.0_f64.to_radians()).abs() < 1e-12);
        assert!((gate.max_ellipse_axis_ratio - 10.0).abs() < f64::EPSILON);
        assert!((gate.max_position_sigma_nm - 50.0).abs() < f64::EPSILON);
        assert!((gate.assumed_max_speed_kn - 0.0).abs() < f64::EPSILON);
        assert_eq!(cfg.min_fix_publication_interval_ms, 1_000);
        assert_eq!(cfg.max_concurrent_pipeline_workers, 1);
        assert_eq!(cfg.classifier_hysteresis_frames, 90);
        assert!(matches!(cfg.plate_solver_init, PlateSolverInit::Lazy));
        assert_eq!(
            cfg.stage_d_dispatch_policy,
            StageDDispatchPolicy::WhenStarsExpected,
        );
        // The long-edge cap defaults on so horizon detection
        // doesn't waste cycles at 4K capture resolutions.
        assert_eq!(cfg.horizon_analysis_max_long_edge_px, Some(1280));
        assert_eq!(cfg.horizon_analysis_size, None);
    }

    #[test]
    fn resolved_horizon_analysis_size_uses_explicit_pair_when_set() {
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.horizon_analysis_size = Some((640, 480));
        cfg.horizon_analysis_max_long_edge_px = Some(1280);
        // Explicit pair wins regardless of source dims and
        // regardless of the long-edge cap.
        assert_eq!(
            cfg.resolved_horizon_analysis_size(4032, 3024),
            Some((640, 480))
        );
    }

    #[test]
    fn resolved_horizon_analysis_size_scales_4_3_source() {
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.horizon_analysis_size = None;
        cfg.horizon_analysis_max_long_edge_px = Some(1280);
        // 4032 × 3024 source (4:3, typical phone main camera).
        // Cap 1280 → scale 1280/4032 ≈ 0.3175.
        // 4032 * 0.3175 ≈ 1280 (capped); 3024 * 0.3175 ≈ 960.
        // Both rounded to even.
        let (w, h) = cfg.resolved_horizon_analysis_size(4032, 3024).unwrap();
        assert_eq!(w, 1280);
        assert_eq!(h, 960);
        // Aspect preserved within rounding.
        let src_ratio = 4032.0_f64 / 3024.0;
        let dst_ratio = f64::from(w) / f64::from(h);
        assert!(
            (src_ratio - dst_ratio).abs() < 0.01,
            "aspect ratio mismatch: src={src_ratio} dst={dst_ratio}"
        );
    }

    #[test]
    fn resolved_horizon_analysis_size_scales_16_9_source() {
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.horizon_analysis_size = None;
        cfg.horizon_analysis_max_long_edge_px = Some(1280);
        // 3840 × 2160 (16:9 UHD). Cap 1280 → scale 1280/3840 = 1/3.
        let (w, h) = cfg.resolved_horizon_analysis_size(3840, 2160).unwrap();
        assert_eq!(w, 1280);
        assert_eq!(h, 720);
    }

    #[test]
    fn resolved_horizon_analysis_size_portrait_source() {
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.horizon_analysis_size = None;
        cfg.horizon_analysis_max_long_edge_px = Some(1280);
        // 3024 × 4032 (portrait 3:4). Long edge is height now.
        // Cap 1280 → 1280/4032 ≈ 0.3175; 3024*0.3175 ≈ 960, 4032*0.3175 ≈ 1280.
        let (w, h) = cfg.resolved_horizon_analysis_size(3024, 4032).unwrap();
        assert_eq!(w, 960);
        assert_eq!(h, 1280);
    }

    #[test]
    fn resolved_horizon_analysis_size_source_already_under_cap() {
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.horizon_analysis_size = None;
        cfg.horizon_analysis_max_long_edge_px = Some(1280);
        // 800 × 600 source (long edge 800 < cap 1280).
        // Resolver returns the source dimensions; the pyramid
        // then declines to upsample and hands back the source
        // frame as-is.
        assert_eq!(
            cfg.resolved_horizon_analysis_size(800, 600),
            Some((800, 600))
        );
    }

    #[test]
    fn resolved_horizon_analysis_size_both_none_disables_downsampling() {
        let mut cfg = EngineConfig::new(Observer::default_dev());
        cfg.horizon_analysis_size = None;
        cfg.horizon_analysis_max_long_edge_px = None;
        assert_eq!(cfg.resolved_horizon_analysis_size(4032, 3024), None);
    }
}
