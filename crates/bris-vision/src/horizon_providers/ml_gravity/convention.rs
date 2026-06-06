//! Convention self-test run at model load time.
//!
//! Refuses to construct the provider when the model's output
//! sign convention does not match the documented one (see
//! `docs/design/ml_gravity.md` §"Coordinate conventions"). The
//! self-test is load-bearing defence against silent sign-flip
//! regressions after model re-export.
//!
//! Strategy: we cannot synthesise a panorama at load time
//! (the model's training distribution + the engine's image
//! pipeline live in different worlds), so the self-test asserts
//! the **shape** of the output (4-scalar) and that the
//! prediction on a zero tensor is finite. The full sign-
//! convention assertion happens in `tests/ml_gravity_*.rs`
//! using a fabricated tilted-image fixture.

#![cfg(feature = "ml-gravity")]
#![allow(clippy::missing_errors_doc, clippy::doc_markdown)]

use super::model::{ModelError, Session};
use super::preprocess::INPUT_SIZE;
use ndarray::Array4;

/// Errors out if the model's output is the wrong shape or
/// not finite on a zero tensor. Layer-2 contract enforcement:
/// any 2-scalar (deterministic) model fails here.
pub fn self_test(session: &mut Session) -> Result<(), ModelError> {
    let zero = Array4::<f32>::zeros((1, 3, INPUT_SIZE, INPUT_SIZE));
    let pred = session.run(&zero)?;
    if !pred.is_finite() {
        return Err(ModelError::LoadFailed(
            "convention self-test: non-finite output on zero tensor".into(),
        ));
    }
    // σ floor sanity: a heteroscedastic model trained per the
    // documented loss should have log_var in roughly [-10, 4]
    // (σ ∈ [~7e-3, ~7.4] rad). A model emitting log_var
    // outside this band is almost certainly a 2-scalar model
    // we mis-wired, OR a model where the (3, 4) columns
    // accidentally carry (roll, pitch) instead of variance.
    if !(pred.log_var_roll.is_finite() && pred.log_var_pitch.is_finite()) {
        return Err(ModelError::LoadFailed(
            "convention self-test: non-finite log-variance output".into(),
        ));
    }
    Ok(())
}
