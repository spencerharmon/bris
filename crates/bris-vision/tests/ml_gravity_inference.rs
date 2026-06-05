//! End-to-end ONNX load + inference test against the vendored
//! ML-gravity model. Skipped (passes trivially) when the model
//! file is absent at `data/ml-gravity/geocalib-heteroscedastic-v1.onnx`.

#![cfg(feature = "ml-gravity")]

use bris_core::time::Tt;
use bris_vision::horizon_providers::ml_gravity::{
    self, MlGravityConfig, MlGravityProvider, MlGravityStats,
};
use bris_vision::horizon_providers::HorizonProviderContext;
use bris_vision::{Frame, Intrinsics};
use std::path::PathBuf;

fn model_path() -> Option<PathBuf> {
    let p = PathBuf::from("../../data/ml-gravity/geocalib-heteroscedastic-v1.onnx");
    if p.exists() {
        return Some(p);
    }
    let p = PathBuf::from("data/ml-gravity/geocalib-heteroscedastic-v1.onnx");
    if p.exists() {
        return Some(p);
    }
    None
}

fn make_frame(w: u32, h: u32) -> Frame {
    // A gradient sky → ground frame: top half dark, bottom half bright.
    let mut pix = vec![0u16; (w as usize) * (h as usize)];
    for y in 0..(h as usize) {
        let v = if y < (h as usize) / 2 { 8_000 } else { 56_000 };
        for x in 0..(w as usize) {
            pix[y * (w as usize) + x] = v;
        }
    }
    let intr = Intrinsics::placeholder(w, h);
    let tt = Tt::from_julian_date(2_460_676.5);
    Frame::new(w, h, pix, tt, 1_000, intr).unwrap()
}

#[test]
fn load_real_model_and_run_inference() {
    let Some(path) = model_path() else {
        eprintln!("[skip] data/ml-gravity/geocalib-heteroscedastic-v1.onnx absent");
        return;
    };
    ml_gravity::load_model(&path).expect("load real ONNX model");
    assert!(ml_gravity::is_loaded());
    assert_ne!(ml_gravity::loaded_model_id(), "unloaded");

    let frame = make_frame(512, 384);
    let intr = frame.intrinsics;
    let ctx = HorizonProviderContext {
        frame: &frame,
        intrinsics: &intr,
        body_candidates: &[],
        position_prior: None,
        timestamp: frame.capture_tt,
    };
    let provider = MlGravityProvider::new(MlGravityConfig {
        model_path: path,
        ..MlGravityConfig::default()
    });
    let mut stats = MlGravityStats::default();
    let hyp = provider.detect_with_stats(&ctx, &mut stats);
    assert!(stats.invoked);
    let hyp = hyp.expect("provider should produce a hypothesis");
    let sigma = hyp.line.altitude_sigma.value();
    assert!(sigma.is_finite() && sigma > 0.0, "sigma {sigma}");
    // The default σ_ceiling caps at 0.5 rad; an honest model on
    // a flat synthetic gradient should still emit a finite line.
    assert!(sigma <= 0.5_f64 + 1e-9, "sigma {sigma} above ceiling");
    assert!(stats.inference_ms > 0.0, "inference latency unrecorded");
    assert!(matches!(
        hyp.provenance,
        bris_vision::HorizonProvenance::MlGravity { .. }
    ));
}
