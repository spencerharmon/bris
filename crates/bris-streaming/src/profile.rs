//! Use-case profile dispatch.
//!
//! [`apply_profile`] mutates an [`EngineConfig`] in place
//! according to a [`UseCaseProfile`] picked by the operator
//! (typically from `session.json`, optionally overridden by
//! `bris-cli replay --profile`).
//!
//! # Application order
//!
//! The full overlay sequence used by `bris-cli` is:
//!
//! 1. **Engine defaults** — [`EngineConfig::new`] gives the
//!    design-doc baseline.
//! 2. **`session.json` overrides** — `apply_session_overlay`
//!    pulls operator-set fields (retention, kinematics) into
//!    the config.
//! 3. **Profile defaults** — *this* function applies the
//!    per-profile preset, **but only to fields the operator
//!    did not already move off the engine default**. A field
//!    that step (2) moved off the engine default is treated
//!    as operator-set and is left alone.
//! 4. **CLI flag overrides** — explicit `--horizon-providers`,
//!    `--max-position-sigma-nm`, etc. win last.
//!
//! Rule (3) is the load-bearing one: the profile is a fallback
//! set of opinionated defaults, never a silent override of an
//! explicit operator choice. AGENTS.md rule zero: the operator's
//! input is authoritative.
//!
//! `Custom` is a no-op.
//!
//! Per-profile picks are documented per-field below; see
//! `docs/design/testing_strategy.md` § `UseCaseProfile` for
//! the rationale.

use bris_bundle::UseCaseProfile;

use crate::config::{EngineConfig, HorizonProviderSet, PublicationGateConfig};

/// Apply the named [`UseCaseProfile`] to `cfg` in place.
///
/// See the module docs for the application-order rules. `Custom`
/// is a no-op. Each non-`Custom` variant adjusts a small,
/// documented set of fields and only writes when the current
/// field value is still at the engine default (i.e. the operator
/// did not override it via `session.json`).
pub fn apply_profile(cfg: &mut EngineConfig, profile: UseCaseProfile) {
    match profile {
        UseCaseProfile::Custom => {}
        UseCaseProfile::Marine => apply_marine(cfg),
        UseCaseProfile::Aeronautical => apply_aeronautical(cfg),
        UseCaseProfile::LandBased => apply_land_based(cfg),
        UseCaseProfile::Urban => apply_urban(cfg),
    }
}

// ---------------------------------------------------------------
// Per-profile preset tables. Each entry documents *what* it
// changes from the engine default and *why*.

/// Marine: open-water sight-taking. Default-honest provider set;
/// segmentation kept on for hazy / low-contrast horizons;
/// ML-gravity available; cold-start enabled so the operator can
/// publish a fix without seeding an AP.
fn apply_marine(cfg: &mut EngineConfig) {
    // Provider set: the full marine-friendly subset. Vertical-line
    // and vanishing-point stay off — marine scenes rarely have
    // useful Manhattan-world geometry, and a near-vertical line
    // on a moving deck is noise, not signal.
    set_horizon_provider_set_if_default(
        cfg,
        HorizonProviderSet {
            gradient: true,
            sky_region: true,
            night: true,
            night_textured: true,
            segmentation: true,
            reflection_pair: true,
            vertical_line: false,
            vanishing_point: false,
            ml_gravity: true,
        },
    );
    // Cold-start is the open-water happy path: an operator
    // stepping on deck for a first fix has no usable AP.
    set_cold_start_enabled_if_default(cfg, true);
}

/// Aeronautical: airborne, fast cross-track sweep. Tolerates a
/// narrower azimuth spread (~20°) because aircraft motion gives
/// natural look-angle coverage. Drops night detectors (a fast-
/// moving cabin sky window rarely lets the night pipeline lock).
/// Drops reflection-pair (no water surface). Defaults the speed
/// gate to 250 kn when the operator did not declare kinematics.
fn apply_aeronautical(cfg: &mut EngineConfig) {
    set_horizon_provider_set_if_default(
        cfg,
        HorizonProviderSet {
            gradient: true,
            sky_region: true,
            night: false,
            night_textured: false,
            segmentation: true,
            reflection_pair: false,
            vertical_line: false,
            vanishing_point: false,
            ml_gravity: true,
        },
    );
    // 20° spread tolerance. Aircraft track changes give the LSQ
    // a usable cross-look-angle baseline faster than a vessel.
    set_min_azimuth_spread_if_default(cfg, 20.0_f64.to_radians());
    // 250 kn covers most general-aviation transport regimes.
    // Only set when the operator did not declare kinematics
    // (i.e. session.json left assumed_max_speed_kn at the
    // engine default of 0.0).
    set_assumed_max_speed_if_default(cfg, 250.0);
}

/// Land-based: stationary terrestrial observer (sight-taking
/// from a known point — boundary survey, calibration site,
/// shoreline). Full provider set; speed gate explicitly 0 kn
/// (matches engine default, but the call makes the intent
/// explicit and survives if engine defaults shift).
fn apply_land_based(cfg: &mut EngineConfig) {
    set_horizon_provider_set_if_default(cfg, HorizonProviderSet::default());
    set_assumed_max_speed_if_default(cfg, 0.0);
}

/// Urban: cluttered scene, partial-sky views, Manhattan-world
/// geometry dominant. Vanishing-point on (parallel building
/// edges give a strong horizon constraint). Sky-region off
/// (sky is rarely contiguous between buildings). Reflection-pair
/// off (puddles / windows confuse the pair test). Night
/// detectors stay on (urban night-sky observations are exactly
/// the operator-targeted case).
fn apply_urban(cfg: &mut EngineConfig) {
    set_horizon_provider_set_if_default(
        cfg,
        HorizonProviderSet {
            gradient: true,
            sky_region: false,
            night: true,
            night_textured: true,
            segmentation: true,
            reflection_pair: false,
            vertical_line: false,
            vanishing_point: true,
            ml_gravity: true,
        },
    );
}

// ---------------------------------------------------------------
// "Only-if-default" setters. Each compares the current field
// against what `EngineConfig::new` would produce; the profile
// writes only when they match, so a prior step (session.json
// overlay) that moved the field is treated as operator-set.

fn engine_default() -> EngineConfig {
    EngineConfig::new(bris_almanac::Observer::default_dev())
}

fn set_horizon_provider_set_if_default(cfg: &mut EngineConfig, value: HorizonProviderSet) {
    let default = engine_default().horizon_provider_set;
    if horizon_provider_set_eq(cfg.horizon_provider_set, default) {
        cfg.horizon_provider_set = value;
    }
}

fn set_min_azimuth_spread_if_default(cfg: &mut EngineConfig, value_rad: f64) {
    let default = PublicationGateConfig::default().min_azimuth_spread_rad;
    if (cfg.publication_gate.min_azimuth_spread_rad - default).abs() < f64::EPSILON {
        cfg.publication_gate.min_azimuth_spread_rad = value_rad;
    }
}

fn set_assumed_max_speed_if_default(cfg: &mut EngineConfig, value_kn: f64) {
    let default = PublicationGateConfig::default().assumed_max_speed_kn;
    if (cfg.publication_gate.assumed_max_speed_kn - default).abs() < f64::EPSILON {
        cfg.publication_gate.assumed_max_speed_kn = value_kn;
    }
}

fn set_cold_start_enabled_if_default(cfg: &mut EngineConfig, value: bool) {
    // Engine default in EngineConfig::new is `true`; the
    // ColdStartEngineConfig::default impl is `false` for
    // historical reasons. Compare against the engine-level
    // default we actually ship.
    let default = engine_default().cold_start.enabled;
    if cfg.cold_start.enabled == default {
        cfg.cold_start.enabled = value;
    }
}

fn horizon_provider_set_eq(a: HorizonProviderSet, b: HorizonProviderSet) -> bool {
    a.gradient == b.gradient
        && a.sky_region == b.sky_region
        && a.night == b.night
        && a.night_textured == b.night_textured
        && a.segmentation == b.segmentation
        && a.reflection_pair == b.reflection_pair
        && a.vertical_line == b.vertical_line
        && a.vanishing_point == b.vanishing_point
        && a.ml_gravity == b.ml_gravity
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_almanac::Observer;

    fn cfg() -> EngineConfig {
        EngineConfig::new(Observer::default_dev())
    }

    #[test]
    fn custom_is_noop() {
        let mut c = cfg();
        let before_providers = c.horizon_provider_set;
        let before_spread = c.publication_gate.min_azimuth_spread_rad;
        let before_speed = c.publication_gate.assumed_max_speed_kn;
        apply_profile(&mut c, UseCaseProfile::Custom);
        assert!(horizon_provider_set_eq(
            c.horizon_provider_set,
            before_providers
        ));
        assert!((c.publication_gate.min_azimuth_spread_rad - before_spread).abs() < f64::EPSILON);
        assert!((c.publication_gate.assumed_max_speed_kn - before_speed).abs() < f64::EPSILON);
    }

    #[test]
    fn marine_sets_expected_provider_set() {
        let mut c = cfg();
        apply_profile(&mut c, UseCaseProfile::Marine);
        let p = c.horizon_provider_set;
        assert!(p.gradient);
        assert!(p.sky_region);
        assert!(p.night);
        assert!(p.night_textured);
        assert!(p.segmentation);
        assert!(p.reflection_pair);
        assert!(!p.vertical_line);
        assert!(!p.vanishing_point);
        assert!(p.ml_gravity);
        assert!(c.cold_start.enabled);
    }

    #[test]
    fn aeronautical_sets_expected_provider_set_and_gates() {
        let mut c = cfg();
        apply_profile(&mut c, UseCaseProfile::Aeronautical);
        let p = c.horizon_provider_set;
        assert!(p.gradient);
        assert!(p.sky_region);
        assert!(!p.night);
        assert!(!p.night_textured);
        assert!(p.segmentation);
        assert!(!p.reflection_pair);
        assert!(!p.vertical_line);
        assert!(!p.vanishing_point);
        assert!(p.ml_gravity);
        assert!((c.publication_gate.min_azimuth_spread_rad - 20.0_f64.to_radians()).abs() < 1e-12);
        assert!((c.publication_gate.assumed_max_speed_kn - 250.0).abs() < f64::EPSILON);
    }

    #[test]
    fn land_based_keeps_full_provider_set() {
        let mut c = cfg();
        apply_profile(&mut c, UseCaseProfile::LandBased);
        assert!(horizon_provider_set_eq(
            c.horizon_provider_set,
            HorizonProviderSet::default()
        ));
        assert!((c.publication_gate.assumed_max_speed_kn - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn urban_sets_expected_provider_set() {
        let mut c = cfg();
        apply_profile(&mut c, UseCaseProfile::Urban);
        let p = c.horizon_provider_set;
        assert!(p.gradient);
        assert!(!p.sky_region);
        assert!(p.night);
        assert!(p.night_textured);
        assert!(p.segmentation);
        assert!(!p.reflection_pair);
        assert!(!p.vertical_line);
        assert!(p.vanishing_point);
        assert!(p.ml_gravity);
    }

    #[test]
    fn profile_does_not_clobber_operator_set_provider_set() {
        let mut c = cfg();
        // Simulate a CLI / session override that flipped the
        // provider set to something other than the engine
        // default before apply_profile ran.
        let custom_set = HorizonProviderSet {
            gradient: true,
            sky_region: false,
            night: false,
            night_textured: false,
            segmentation: false,
            reflection_pair: false,
            vertical_line: false,
            vanishing_point: false,
            ml_gravity: false,
        };
        c.horizon_provider_set = custom_set;
        apply_profile(&mut c, UseCaseProfile::Marine);
        assert!(horizon_provider_set_eq(c.horizon_provider_set, custom_set));
    }

    #[test]
    fn profile_does_not_clobber_operator_set_speed() {
        let mut c = cfg();
        c.publication_gate.assumed_max_speed_kn = 12.0;
        apply_profile(&mut c, UseCaseProfile::Aeronautical);
        assert!((c.publication_gate.assumed_max_speed_kn - 12.0).abs() < f64::EPSILON);
    }

    #[test]
    fn profile_does_not_clobber_operator_set_spread() {
        let mut c = cfg();
        c.publication_gate.min_azimuth_spread_rad = 5.0_f64.to_radians();
        apply_profile(&mut c, UseCaseProfile::Aeronautical);
        assert!((c.publication_gate.min_azimuth_spread_rad - 5.0_f64.to_radians()).abs() < 1e-12);
    }
}
