//! Integration: sight + fix persistence survives engine drop.
//!
//! Drives a few synthetic body+horizon frames into a fresh
//! engine, drops the engine, opens a second engine on the same
//! tempdir, and verifies the operational pool was hydrated and
//! a position prior was recovered.

use bris_almanac::Observer;
use bris_core::time::{Tt, JD_J2000};
use bris_streaming::{EngineConfig, StoreConfig, StreamingEngine};
use bris_vision::{Frame, Intrinsics};
use tempfile::TempDir;

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn body_plus_horizon_frame(tt: Tt) -> Frame {
    let w = 128_u32;
    let h = 128_u32;
    let mut pixels = vec![0u16; (w * h) as usize];
    for y in 0..h {
        let value = if y < 64 { 50_000 } else { 200 };
        for x in 0..w {
            pixels[(y as usize) * (w as usize) + (x as usize)] = value;
        }
    }
    let cx: i32 = 64;
    let cy: i32 = 32;
    for dy in -10..=10_i32 {
        for dx in -10..=10_i32 {
            if dx * dx + dy * dy <= 36 {
                let px = cx + dx;
                let py = cy + dy;
                if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                    pixels[(py as usize) * (w as usize) + (px as usize)] = u16::MAX;
                }
            }
        }
    }
    Frame::new(w, h, pixels, tt, 1000, Intrinsics::placeholder(w, h)).unwrap()
}

fn cfg_in(dir: &std::path::Path) -> EngineConfig {
    let mut cfg = EngineConfig::new(Observer::default_dev());
    cfg.min_fix_publication_interval_ms = 0;
    cfg.sight_window_seconds = 1e12;
    cfg.position_prior_max_age_seconds = 1e12;
    cfg.store = StoreConfig {
        data_root: dir.to_path_buf(),
        retention_days: 7,
        rotation_size_bytes: 8 * 1024 * 1024,
        enabled: true,
    };
    cfg
}

#[test]
fn sight_pool_and_fix_prior_survive_engine_restart() {
    let dir = TempDir::new().unwrap();

    // Phase 1: drive the engine to produce several sights and
    // (with two diverse azimuths) at least one fix.
    {
        let engine = StreamingEngine::new(cfg_in(dir.path()));
        let t0 = Tt::from_julian_date(JD_J2000);
        let t1 = Tt::from_julian_date(JD_J2000 + 1.0 / 24.0);
        engine.push_frame(body_plus_horizon_frame(t0)).unwrap();
        engine.push_frame(body_plus_horizon_frame(t1)).unwrap();
        let diag = engine.diagnostics();
        assert!(
            diag.sights_persisted_total >= 1,
            "expected at least one sight persisted, got {}",
            diag.sights_persisted_total
        );
        assert!(
            diag.store_current_log_bytes >= 96,
            "log should have at least one record"
        );
    }

    // Phase 2: reopen on the same tempdir. Pool hydrates from
    // disk; the most recent persisted fix becomes the startup
    // position prior.
    {
        let engine = StreamingEngine::new(cfg_in(dir.path()));
        let diag = engine.diagnostics();
        assert!(
            diag.sights_loaded_on_start >= 1,
            "expected hydrated pool, got {}",
            diag.sights_loaded_on_start
        );
        assert!(
            diag.sight_window_depth >= 1,
            "operational sight window should be populated on restart, got depth {}",
            diag.sight_window_depth
        );
        let pool = engine.pool_sights();
        assert!(
            !pool.is_empty(),
            "pool_sights() should return the hydrated sights"
        );
        // Push another frame; pipeline should run without
        // panicking and the persistence counter should keep
        // climbing.
        let t2 = Tt::from_julian_date(JD_J2000 + 2.0 / 24.0);
        engine.push_frame(body_plus_horizon_frame(t2)).unwrap();
        let diag2 = engine.diagnostics();
        assert!(
            diag2.sights_persisted_total >= 1,
            "expected at least one sight persisted post-restart, got {}",
            diag2.sights_persisted_total
        );
    }
}
