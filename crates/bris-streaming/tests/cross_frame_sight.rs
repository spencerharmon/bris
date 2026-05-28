//! Integration: Stage E cross-frame execution.
//!
//! Drives two synthetic frames into the engine where frame A
//! has a body but no detectable horizon and frame B has a
//! horizon (and shared corner features for `track_rotation`).
//! Confirms that a cross-frame sight is emitted and that the
//! diagnostic counter increments.

use bris_almanac::Observer;
use bris_core::time::{Tt, JD_J2000};
use bris_streaming::{EngineConfig, StoreConfig, StreamingEngine};
use bris_vision::{Frame, Intrinsics};
use tempfile::TempDir;

const W: u32 = 320;
const H: u32 = 240;

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn sprinkle_markers(pixels: &mut [u16]) {
    let wi = W as i32;
    let hi = H as i32;
    for (cx, cy) in [
        (50, 30),
        (120, 50),
        (200, 40),
        (270, 60),
        (90, 80),
        (180, 90),
    ] {
        for dy in -3_i32..=3 {
            for dx in -3_i32..=3 {
                let x = cx + dx;
                let y = cy + dy;
                if x < 0 || y < 0 || x >= wi || y >= hi {
                    continue;
                }
                pixels[(y as usize) * (W as usize) + (x as usize)] = 65_000;
            }
        }
    }
}

/// Frame A: bright body, uniform sky background (no horizon).
fn body_only_frame(tt: Tt) -> Frame {
    let mut pixels = vec![45_000u16; (W * H) as usize];
    // Body disk near top.
    let (bx, by, r) = (160.0_f64, 50.0_f64, 12.0_f64);
    for y in 0..H {
        for x in 0..W {
            let dx = f64::from(x) - bx;
            let dy = f64::from(y) - by;
            if dx * dx + dy * dy <= r * r {
                pixels[(y as usize) * (W as usize) + (x as usize)] = u16::MAX;
            }
        }
    }
    sprinkle_markers(&mut pixels);
    Frame::new(W, H, pixels, tt, 1000, Intrinsics::placeholder(W, H)).unwrap()
}

/// Frame B: horizon (bright sky over dark sea) + the same
/// marker pattern (no body).
fn horizon_only_frame(tt: Tt) -> Frame {
    let mut pixels = vec![0u16; (W * H) as usize];
    for y in 0..H {
        let v = if y < 120 { 50_000 } else { 5_000 };
        for x in 0..W {
            pixels[(y as usize) * (W as usize) + (x as usize)] = v;
        }
    }
    sprinkle_markers(&mut pixels);
    Frame::new(W, H, pixels, tt, 1000, Intrinsics::placeholder(W, H)).unwrap()
}

fn cfg_in(dir: &std::path::Path) -> EngineConfig {
    let mut cfg = EngineConfig::new(Observer::default_dev());
    cfg.min_fix_publication_interval_ms = 0;
    cfg.store = StoreConfig {
        data_root: dir.to_path_buf(),
        retention_days: 7,
        rotation_size_bytes: 8 * 1024 * 1024,
        enabled: false,
    };
    cfg
}

#[test]
fn cross_frame_sight_emits_and_counter_increments() {
    let dir = TempDir::new().unwrap();
    let engine = StreamingEngine::new(cfg_in(dir.path()));

    let t0 = Tt::from_julian_date(JD_J2000);
    let t1 = Tt::from_julian_date(JD_J2000 + 0.1 / 86_400.0);

    engine.push_frame(body_only_frame(t0)).unwrap();
    engine.push_frame(horizon_only_frame(t1)).unwrap();

    let diag = engine.diagnostics();
    assert_eq!(
        diag.cross_frame_sights_emitted, 1,
        "expected exactly one cross-frame sight, diag={diag:?}"
    );
    assert!(
        diag.sights_inserted_total >= diag.cross_frame_sights_emitted,
        "total sights ({}) must be >= cross-frame sights ({})",
        diag.sights_inserted_total,
        diag.cross_frame_sights_emitted
    );
}
