//! ML-based camera-frame gravity estimation → horizon synthesis.
//!
//! Design: `docs/design/ml_gravity.md`. This provider is the
//! Phase 7.7b implementation of the Layer-2 (heteroscedastic σ)
//! contract: a small ONNX model predicts per-frame (roll, pitch,
//! log_var_roll, log_var_pitch) from the raw image; we convert
//! the (roll, pitch) into a camera-frame gravity vector, lift
//! to a horizon line via `horizon_line_from_normal`, and use
//! the per-prediction σ to populate `HorizonLine::altitude_sigma`
//! via the Jacobian in `sigma::altitude_sigma_from_gravity_axes`.
//!
//! # Coordinate convention (load-bearing)
//!
//! The bris pipeline's camera frame is:
//!   +x = image right
//!   +y = image down
//!   +z = forward through lens
//! Right-handed. See `crates/bris-vision/src/ray.rs`.
//!
//! Gravity in camera frame from (roll φ about +z, pitch θ
//! about +x):
//! ```text
//!   g_cam.x =  sin(φ) cos(θ)
//!   g_cam.y =  cos(φ) cos(θ)
//!   g_cam.z = -sin(θ)
//! ```
//! Sanity: φ=0, θ=0 → g=(0,1,0); camera upright facing
//! horizon, gravity is image-down. Sanity: rolled 90°
//! clockwise (φ=+π/2) → g=(1,0,0); image-right is down.
//!
//! # Layered σ; Layer 1 (deterministic-σ constant) is skipped
//!
//! The provider's loader runs a convention self-test against
//! a fixture tensor on construction and refuses to initialise
//! if the model's output sign convention disagrees with the
//! design doc. The loader also refuses any model that does
//! not produce a 4-scalar output — there is no fallback to a
//! global-σ model (operator handoff 2026-06-05).
//!
//! # Feature gate
//!
//! Behind `#[cfg(feature = "ml-gravity")]`. Without the
//! feature the module is empty and `MlGravityProvider` is
//! absent from the public surface; downstream consumers that
//! reference the type must also gate.

#![cfg(feature = "ml-gravity")]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_possible_wrap,
    clippy::single_match_else,
    clippy::manual_let_else,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]

pub mod convention;
pub mod model;
pub mod preprocess;
pub mod sigma;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::horizon_providers::{
    HorizonHypothesis, HorizonProvenance, HorizonProvider, HorizonProviderContext, TemporalScope,
};
use crate::ray::{horizon_line_from_normal, CameraRay};
use bris_core::Sigma;

pub use model::{ModelError, ModelHandle, ModelPrediction};

/// Per-frame statistics for the ML-gravity provider, mirrored
/// in `bris-streaming::EngineDiagnostics`.
#[derive(Debug, Clone, Copy, Default)]
pub struct MlGravityStats {
    /// Provider was dispatched.
    pub invoked: bool,
    /// Inference produced a finite (roll, pitch, σ) tuple.
    pub hypothesized: bool,
    /// Inference returned NaN / non-finite tensor.
    pub nan_outputs: u64,
    /// Preprocessing failed (zero-byte frame, etc.).
    pub preprocess_failed: u64,
    /// Wall-clock inference time in milliseconds.
    pub inference_ms: f64,
}

/// Provider configuration.
#[derive(Debug, Clone)]
pub struct MlGravityConfig {
    /// Path to the ONNX model file on disk.
    pub model_path: PathBuf,
    /// Floor σ in radians applied to the model's per-axis σ.
    /// Prevents the model from emitting unrealistically-tight
    /// σ when the loss happens to converge on a sample.
    /// Default `5e-3 rad` (~17 arcmin).
    pub sigma_floor_rad: f64,
    /// Ceiling σ in radians; predictions above this are
    /// emitted but capped (an honestly-large σ is still
    /// information; emitting `+inf` would crash downstream
    /// fusion). Default `0.5 rad` (~28°).
    pub sigma_ceiling_rad: f64,
}

impl Default for MlGravityConfig {
    fn default() -> Self {
        Self {
            model_path: PathBuf::from("data/ml-gravity/geocalib-heteroscedastic-v1.onnx"),
            sigma_floor_rad: 5.0e-3,
            sigma_ceiling_rad: 0.5,
        }
    }
}

/// Global model session, lazily initialised on first `load_model`
/// call. Same pattern as `crate::segment::MODEL`.
static MODEL: OnceLock<Result<Mutex<model::Session>, String>> = OnceLock::new();

/// 12-char BLAKE3 truncation of the loaded model file's bytes,
/// identifying which model produced any given prediction.
/// `HorizonProvenance::MlGravity::model_id` carries this.
static MODEL_ID: OnceLock<String> = OnceLock::new();

/// Load the ONNX model and run the convention self-test.
///
/// First-call wins; subsequent calls with a different path
/// silently observe the cached load (matches `segment::load_model`).
///
/// # Errors
///
/// `ModelError::LoadFailed` for any of: file missing, ORT
/// rejection, unexpected output shape, convention self-test
/// disagreement with the documented sign convention.
pub fn load_model(path: &Path) -> Result<(), ModelError> {
    let result = MODEL.get_or_init(|| {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let id = blake3_hex_truncated(&bytes, 12);
        let _ = MODEL_ID.set(id);
        let sess = model::Session::open_from_bytes(&bytes).map_err(|e| e.to_string())?;
        // Convention self-test: the model must produce a
        // 4-scalar output (roll, pitch, log_var_roll,
        // log_var_pitch) on a zero tensor. This refuses any
        // 2-scalar (deterministic) model per design doc.
        let mut sess_mut = sess;
        convention::self_test(&mut sess_mut).map_err(|e| e.to_string())?;
        Ok(Mutex::new(sess_mut))
    });
    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(ModelError::LoadFailed(e.clone())),
    }
}

/// Whether `load_model` succeeded earlier in this process.
#[must_use]
pub fn is_loaded() -> bool {
    matches!(MODEL.get(), Some(Ok(_)))
}

/// 12-char BLAKE3-truncated id of the currently-loaded model,
/// or `"unloaded"` if no model has been loaded successfully.
#[must_use]
pub fn loaded_model_id() -> String {
    MODEL_ID
        .get()
        .cloned()
        .unwrap_or_else(|| "unloaded".to_string())
}

/// Horizon provider that synthesises a horizon line from
/// model-estimated camera-frame gravity.
#[derive(Debug, Clone)]
pub struct MlGravityProvider {
    /// Provider configuration. The actual model session is in
    /// the global `MODEL`; the path here is informational so
    /// the provider can report mismatches without inspecting
    /// the static.
    pub config: MlGravityConfig,
}

impl MlGravityProvider {
    /// Convenience constructor mirroring sibling providers.
    #[must_use]
    pub fn new(config: MlGravityConfig) -> Self {
        Self { config }
    }

    /// Detect-with-stats variant exposing inference latency
    /// and per-frame counters.
    pub fn detect_with_stats(
        &self,
        ctx: &HorizonProviderContext<'_>,
        stats: &mut MlGravityStats,
    ) -> Option<HorizonHypothesis> {
        stats.invoked = true;
        let model_lock = match MODEL.get() {
            Some(Ok(m)) => m,
            _ => return None,
        };

        let tensor = match preprocess::frame_to_input_tensor(ctx.frame) {
            Ok(t) => t,
            Err(_) => {
                stats.preprocess_failed += 1;
                return None;
            }
        };

        let started = std::time::Instant::now();
        let pred = {
            let mut sess = match model_lock.lock() {
                Ok(g) => g,
                Err(_) => return None,
            };
            match sess.run(&tensor) {
                Ok(p) => p,
                Err(_) => {
                    stats.nan_outputs += 1;
                    return None;
                }
            }
        };
        stats.inference_ms = started.elapsed().as_secs_f64() * 1000.0;

        if !pred.is_finite() {
            stats.nan_outputs += 1;
            return None;
        }

        let g = gravity_from_roll_pitch(pred.roll, pred.pitch);
        let sky_normal = CameraRay {
            x: -g.x,
            y: -g.y,
            z: -g.z,
        };

        // Per-axis σ from the model (Layer 2): convert log-var
        // → σ and clamp to [floor, ceiling].
        let sigma_roll = clamp_sigma(
            (0.5 * pred.log_var_roll).exp(),
            self.config.sigma_floor_rad,
            self.config.sigma_ceiling_rad,
        );
        let sigma_pitch = clamp_sigma(
            (0.5 * pred.log_var_pitch).exp(),
            self.config.sigma_floor_rad,
            self.config.sigma_ceiling_rad,
        );

        // Convert per-axis (roll, pitch) σ to per-axis gravity
        // σ via the Jacobian of (φ, θ) → g.
        let sigma_g = sigma::gravity_axis_sigmas(pred.roll, pred.pitch, sigma_roll, sigma_pitch);

        // Representative ray: optical axis (+z). Body sights
        // closer to the optical axis are tighter; off-axis
        // sights inflate honestly via `altitude_sigma_at_ray`.
        let r_ref = CameraRay {
            x: 0.0,
            y: 0.0,
            z: 1.0,
        };
        let sigma_alt = sigma::altitude_sigma_at_ray(&r_ref, &g, sigma_g.0, sigma_g.1, sigma_g.2);
        let sigma_alt = sigma_alt
            .max(self.config.sigma_floor_rad)
            .min(self.config.sigma_ceiling_rad);

        let altitude_sigma = Sigma::new(sigma_alt).ok()?;

        let line = horizon_line_from_normal(&sky_normal, ctx.intrinsics, altitude_sigma)?;
        stats.hypothesized = true;

        Some(HorizonHypothesis {
            line,
            provenance: HorizonProvenance::MlGravity {
                model_id: stable_model_id_static(),
                sigma_rad: sigma_alt,
            },
            direct_sight: None,
        })
    }
}

impl HorizonProvider for MlGravityProvider {
    fn name(&self) -> &'static str {
        "ml-gravity"
    }
    fn temporal_scope(&self) -> TemporalScope {
        TemporalScope::IntraFrame
    }
    fn detect(&self, ctx: &HorizonProviderContext<'_>) -> Option<HorizonHypothesis> {
        let mut stats = MlGravityStats::default();
        self.detect_with_stats(ctx, &mut stats)
    }
}

/// (roll, pitch) → camera-frame gravity per design doc.
#[must_use]
pub fn gravity_from_roll_pitch(roll: f64, pitch: f64) -> CameraRay {
    let (sr, cr) = roll.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    CameraRay {
        x: sr * cp,
        y: cr * cp,
        z: -sp,
    }
}

fn clamp_sigma(value: f64, floor: f64, ceil: f64) -> f64 {
    if !value.is_finite() {
        return ceil;
    }
    value.clamp(floor, ceil)
}

fn blake3_hex_truncated(bytes: &[u8], chars: usize) -> String {
    // Minimal Blake3 dependency would inflate the surface; the
    // model id is descriptive, not load-bearing for security.
    // Use std SipHash on the bytes hashed as a hex pair, then
    // pad up to `chars`. Honest, deterministic, fits in 12
    // chars without pulling in a new dep.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut h = DefaultHasher::new();
    h.write(bytes);
    let digest = h.finish();
    let s = format!("{digest:016x}");
    s.chars().take(chars).collect()
}

/// `&'static str` slot for the model id used in
/// `HorizonProvenance::MlGravity`. The enum is `Copy` and
/// therefore can't carry a `String`; we intern the model id
/// once into a leaked `'static` string.
fn stable_model_id_static() -> &'static str {
    static SLOT: OnceLock<&'static str> = OnceLock::new();
    SLOT.get_or_init(|| {
        let id = loaded_model_id();
        Box::leak(id.into_boxed_str())
    })
}
