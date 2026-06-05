//! ORT session wrapper for the heteroscedastic gravity model.
//!
//! Input  : NCHW float32 (1, 3, 256, 256), ImageNet-normalised.
//! Output : (1, 4) float32 = (roll, pitch, log_var_roll,
//!          log_var_pitch). Roll/pitch in radians; log_var
//!          is the natural log of the predicted variance per
//!          axis.

#![cfg(feature = "ml-gravity")]
#![allow(
    clippy::missing_errors_doc,
    clippy::doc_markdown,
    clippy::similar_names
)]

use ndarray::Array4;
use ort::session::{builder::GraphOptimizationLevel, Session as OrtSession};
use ort::value::Tensor;

/// Errors from model loading + inference.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// File missing, corrupt, or runtime rejected it; or
    /// convention self-test failed.
    #[error("failed to load ml-gravity model: {0}")]
    LoadFailed(String),
    /// Inference returned an error (shape mismatch, GPU OOM,
    /// etc).
    #[error("ml-gravity inference failed: {0}")]
    InferenceFailed(String),
}

/// Wrapper around an `ort` session. `Send` because the
/// underlying `ort::Session` is.
pub struct Session {
    inner: OrtSession,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ml_gravity::model::Session")
            .finish_non_exhaustive()
    }
}

/// Handle alias kept for API consistency with the design doc.
pub type ModelHandle = Session;

impl Session {
    /// Load from raw ONNX bytes. The bytes-in form keeps the
    /// caller in charge of file IO (fetch script / cache /
    /// LFS / build.rs).
    ///
    /// # Errors
    /// See [`ModelError::LoadFailed`].
    pub fn open_from_bytes(bytes: &[u8]) -> Result<Self, ModelError> {
        let inner = OrtSession::builder()
            .map_err(|e| ModelError::LoadFailed(e.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| ModelError::LoadFailed(e.to_string()))?
            .commit_from_memory(bytes)
            .map_err(|e| ModelError::LoadFailed(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Run inference on a preprocessed input tensor.
    ///
    /// # Errors
    /// See [`ModelError::InferenceFailed`]. Returns Err if the
    /// runtime fails OR the output shape isn't (1, 4) of f32.
    pub fn run(&mut self, input: &Array4<f32>) -> Result<ModelPrediction, ModelError> {
        let tensor = Tensor::from_array(input.clone())
            .map_err(|e| ModelError::InferenceFailed(e.to_string()))?;
        let outputs = self
            .inner
            .run(ort::inputs![tensor])
            .map_err(|e| ModelError::InferenceFailed(e.to_string()))?;
        let view = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|e| ModelError::InferenceFailed(e.to_string()))?;
        let shape: Vec<usize> = view.shape().to_vec();
        // Accept (1, 4) or (4,) — onnxsim may strip the batch dim
        // depending on opset.
        let flat: Vec<f32> = view.iter().copied().collect();
        if flat.len() != 4 {
            return Err(ModelError::InferenceFailed(format!(
                "expected 4 scalars, got shape {shape:?} ({} values)",
                flat.len()
            )));
        }
        Ok(ModelPrediction {
            roll: f64::from(flat[0]),
            pitch: f64::from(flat[1]),
            log_var_roll: f64::from(flat[2]),
            log_var_pitch: f64::from(flat[3]),
        })
    }
}

/// Raw model output. `roll` / `pitch` in radians; `log_var_*`
/// in natural log of variance (rad²).
#[derive(Debug, Clone, Copy)]
pub struct ModelPrediction {
    /// Roll about camera +z, radians.
    pub roll: f64,
    /// Pitch about camera +x, radians.
    pub pitch: f64,
    /// Log-variance for roll (natural log of σ² in rad²).
    pub log_var_roll: f64,
    /// Log-variance for pitch (natural log of σ² in rad²).
    pub log_var_pitch: f64,
}

impl ModelPrediction {
    /// True iff all four scalars are finite (no NaN / ±inf).
    #[must_use]
    pub fn is_finite(&self) -> bool {
        self.roll.is_finite()
            && self.pitch.is_finite()
            && self.log_var_roll.is_finite()
            && self.log_var_pitch.is_finite()
    }
}
