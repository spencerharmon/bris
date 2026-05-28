//! `lock_ap_for_replay` smoke test.
//!
//! The behavioural coverage lives in `pipeline::stage_e::tests::
//! ap_lock_for_replay_suppresses_cold_start` (a unit test that
//! drives Stage E directly with a stale-prior scenario). This
//! integration test only confirms that the public surface
//! exposes the lock flag and the suppression counter so the
//! `bris-cli replay --ap-lock-truth` caller can rely on both.

use bris_almanac::Observer;
use bris_streaming::{EngineConfig, StreamingEngine};

#[test]
fn default_engine_has_lock_disabled_and_zero_counter() {
    let cfg = EngineConfig::new(Observer::default_dev());
    assert!(!cfg.lock_ap_for_replay, "production default must be false");
    let engine = StreamingEngine::new(cfg);
    let d = engine.diagnostics();
    assert_eq!(d.ap_rederive_suppressed_count, 0);
}

#[test]
fn engine_accepts_lock_flag() {
    let mut cfg = EngineConfig::new(Observer::default_dev());
    cfg.lock_ap_for_replay = true;
    cfg.store.enabled = false;
    let engine = StreamingEngine::new(cfg);
    let d = engine.diagnostics();
    // No frames pushed → counter still zero, but the field is
    // exposed.
    assert_eq!(d.ap_rederive_suppressed_count, 0);
    assert_eq!(d.frames_pushed, 0);
}
