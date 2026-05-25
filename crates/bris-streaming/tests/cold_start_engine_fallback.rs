//! Engine-level cold-start fallback wiring smoke test.
//!
//! Deep behavior of the cold-start path is exercised by unit
//! tests inside `crates/bris-streaming/src/pipeline/stage_e.rs`
//! (`cold_start_path_publishes_when_multi_sight_fix_singular`,
//! `cold_start_ambiguous_without_hint_skips_publication`).
//!
//! This test confirms the public-API surface is wired:
//!
//! - [`ColdStartEngineConfig`] is reachable from
//!   [`EngineConfig`] and honoured at construction.
//! - [`FixProvenance`] has the three variants documented in
//!   `docs/design/circle_of_position.md`.
//! - [`StreamingEngine`] constructs with a configured
//!   `cold_start.coarse_hemisphere` hint.
//! - Cold-start diagnostics counters are exposed on
//!   [`bris_streaming::EngineDiagnostics`] and start at zero.
//!
//! A full end-to-end "synthetic 3-sight scene that triggers
//! the fallback through `push_frame`" needs synthesised frames
//! that flow through the detection stages; that scaffolding
//! lives only in `moonlight_pond_lop.rs` against the real
//! corpus. Once a synthetic Stage-A-through-D harness lands,
//! upgrade this test to assert a `FixProvenance::ColdStart`
//! `PublishedFix` arrives at the receiver.

use bris_almanac::Observer;
use bris_core::Hemisphere;
use bris_streaming::{ColdStartEngineConfig, EngineConfig, FixProvenance, StreamingEngine};

#[test]
fn cold_start_config_default_is_enabled_no_hint() {
    let cfg = EngineConfig::new(Observer::default_dev());
    assert!(cfg.cold_start.enabled);
    assert!(cfg.cold_start.coarse_hemisphere.is_none());
}

#[test]
fn cold_start_config_can_be_constructed_with_hemisphere_hint() {
    let cfg = ColdStartEngineConfig {
        enabled: true,
        coarse_hemisphere: Some(Hemisphere::North),
    };
    assert_eq!(cfg.coarse_hemisphere, Some(Hemisphere::North));
}

#[test]
fn fix_provenance_variants_have_stable_labels() {
    assert_eq!(FixProvenance::SaintHilaire.label(), "saint_hilaire");
    assert_eq!(FixProvenance::ColdStart.label(), "cold_start");
    assert_eq!(
        FixProvenance::ColdStartAmbiguous.label(),
        "cold_start_ambiguous"
    );
}

#[test]
fn engine_constructs_with_cold_start_hemisphere_hint() {
    let mut cfg = EngineConfig::new(Observer::default_dev());
    cfg.store.enabled = false;
    cfg.cold_start.coarse_hemisphere = Some(Hemisphere::South);
    let engine = StreamingEngine::new(cfg);
    let diag = engine.diagnostics();
    // Counters all start at zero.
    assert_eq!(diag.cold_start_attempts, 0);
    assert_eq!(diag.cold_start_published, 0);
    assert_eq!(diag.cold_start_ambiguous_skipped, 0);
    assert_eq!(diag.cold_start_inconsistent_count, 0);
    assert_eq!(diag.cold_start_disjoint_count, 0);
}
