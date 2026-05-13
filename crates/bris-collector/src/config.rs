//! Collector configuration.
//!
//! The collector reads configuration from environment variables
//! at startup. No TOML/YAML config file in the spike — the
//! settings are few enough that env vars are clearer than a
//! file.
//!
//! - `BRIS_COLLECTOR_DATA_ROOT` — base directory for the
//!   filesystem store. Required. Created if it does not exist.
//! - `BRIS_COLLECTOR_BIND` — `host:port` to bind to. Default
//!   `0.0.0.0:8443`.
//! - `BRIS_COLLECTOR_BEARER_TOKEN` — shared bearer token
//!   accepted on submissions. Required in production; tests
//!   bypass auth by constructing the [`Config`] directly.
//! - `BRIS_COLLECTOR_MAX_SUBMISSION_BYTES` — request body
//!   ceiling, in bytes. Default 512 MiB.

use std::path::PathBuf;

/// Effective collector configuration after env / argument
/// parsing.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base directory of the filesystem store. Submissions live
    /// under `<data_root>/submissions/<yyyy>/<mm>/<dd>/<ulid>/`.
    pub data_root: PathBuf,
    /// Bind address.
    pub bind: String,
    /// Shared bearer token expected in `Authorization: Bearer
    /// <token>`. Empty disables auth (test-only; the binary
    /// refuses to start if empty).
    pub bearer_token: String,
    /// Maximum request body size, in bytes.
    pub max_submission_bytes: usize,
}

impl Config {
    /// Build a config from process environment variables.
    ///
    /// # Errors
    ///
    /// Returns a string describing the first missing or
    /// malformed variable.
    pub fn from_env() -> Result<Self, String> {
        let data_root = std::env::var("BRIS_COLLECTOR_DATA_ROOT")
            .map_err(|_| "BRIS_COLLECTOR_DATA_ROOT is required".to_owned())?
            .into();
        let bind =
            std::env::var("BRIS_COLLECTOR_BIND").unwrap_or_else(|_| "0.0.0.0:8443".to_owned());
        let bearer_token = std::env::var("BRIS_COLLECTOR_BEARER_TOKEN").unwrap_or_default();
        let max_submission_bytes = match std::env::var("BRIS_COLLECTOR_MAX_SUBMISSION_BYTES") {
            Ok(s) => s
                .parse::<usize>()
                .map_err(|e| format!("BRIS_COLLECTOR_MAX_SUBMISSION_BYTES: {e}"))?,
            Err(_) => 512 * 1024 * 1024,
        };
        Ok(Self {
            data_root,
            bind,
            bearer_token,
            max_submission_bytes,
        })
    }
}
