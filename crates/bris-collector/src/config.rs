//! Collector configuration.
//!
//! The collector's configuration surface is deliberately
//! infrastructure-*agnostic*: this crate owns the shape of the
//! settings (which knobs exist, their env-var and file names,
//! their defaults, and the validation that refuses to start
//! without a token), while the concrete values — real
//! hostnames, the admin token, the storage class, the mount
//! path — live only in the deployment layer (flux). A reference
//! config file and a reference k8s spec fragment ship under
//! [`deploy/reference/`](../../../deploy/reference/) using
//! RFC2606/RFC5737 placeholders so the flux consumer has a
//! documented contract to fill in.
//!
//! ## Precedence
//!
//! Settings are resolved by [`Config::load`] in this order,
//! last-wins:
//!
//! 1. built-in defaults,
//! 2. a TOML config file (if one is given, either explicitly or
//!    via `BRIS_COLLECTOR_CONFIG`),
//! 3. process environment variables.
//!
//! So a deployment can bake a base config file into the image
//! (or a mounted `ConfigMap`) and override any single value —
//! most importantly the secret token — from the environment (a
//! mounted `Secret`) without editing the file.
//!
//! ## Settings (env var / file key)
//!
//! - `BRIS_COLLECTOR_DATA_ROOT` / `data_root` — base directory
//!   for the filesystem store. **Required.** Created if it does
//!   not exist. In k8s this is the mount path of the submissions
//!   volume (see the reference spec fragment).
//! - `BRIS_COLLECTOR_BIND` / `bind` — `host:port` to bind to.
//!   Default `0.0.0.0:8443`. The port half is the container
//!   port the k8s `Service` targets.
//! - `BRIS_COLLECTOR_BEARER_TOKEN` / `bearer_token` — the
//!   admin/bootstrap token. Compiled into every device at flash
//!   time; a device uses it exactly once, to call
//!   `POST /v1/devices/register` and obtain its own per-device
//!   token, then authenticates every subsequent submission with
//!   that per-device token instead (see `bris_collector::auth`).
//!   The admin token also continues to authorize every
//!   review/admin endpoint (list, get manifest, get media) and
//!   remains accepted on `POST /v1/submissions` for operator
//!   tooling. **Required in production** (the binary refuses to
//!   start without it); tests bypass auth by constructing the
//!   [`Config`] directly. Supply it from a k8s `Secret`, never
//!   the config file.
//! - `BRIS_COLLECTOR_MAX_SUBMISSION_BYTES` / `max_submission_bytes`
//!   — request body ceiling, in bytes. Default 512 MiB.
//! - `BRIS_COLLECTOR_RETENTION_DAYS` / `retention_days` — the
//!   default retention window, in days, applied by the
//!   `retention-sweep` maintenance command to hard-delete
//!   submissions soft-deleted longer than this. Default 30. The
//!   sweep is never run automatically; this is only its default
//!   window (an explicit `--retention-days` still overrides it).

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Default bind address when none is configured.
const DEFAULT_BIND: &str = "0.0.0.0:8443";
/// Default request-body ceiling: 512 MiB.
const DEFAULT_MAX_SUBMISSION_BYTES: usize = 512 * 1024 * 1024;
/// Default retention window for the `retention-sweep` command.
const DEFAULT_RETENTION_DAYS: i64 = 30;

/// Effective collector configuration after file / env parsing.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base directory of the filesystem store. Submissions live
    /// under `<data_root>/submissions/<yyyy>/<mm>/<dd>/<ulid>/`.
    pub data_root: PathBuf,
    /// Bind address.
    pub bind: String,
    /// Admin/bootstrap token expected in `Authorization: Bearer
    /// <token>` on device registration, the review/admin
    /// endpoints, and (for backward-compatible operator tooling)
    /// submissions. Empty disables auth (test-only; the binary
    /// refuses to start if empty).
    pub bearer_token: String,
    /// Maximum request body size, in bytes.
    pub max_submission_bytes: usize,
    /// Default retention window (days) for the `retention-sweep`
    /// maintenance command. Not applied automatically.
    pub retention_days: i64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_root: PathBuf::new(),
            bind: DEFAULT_BIND.to_owned(),
            bearer_token: String::new(),
            max_submission_bytes: DEFAULT_MAX_SUBMISSION_BYTES,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

/// The on-disk TOML shape. Every field is optional so a config
/// file may set only the subset it cares about; the rest fall
/// back to defaults (or an environment override). `deny_unknown_fields`
/// turns a typo'd key into a hard error rather than a silently
/// ignored setting.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    data_root: Option<PathBuf>,
    bind: Option<String>,
    bearer_token: Option<String>,
    max_submission_bytes: Option<usize>,
    retention_days: Option<i64>,
}

impl Config {
    /// Resolve configuration from an optional file plus the
    /// process environment, applying defaults for anything unset.
    ///
    /// Precedence (last wins): defaults → file → environment.
    /// The file is the `config_path` argument if `Some`,
    /// otherwise the path named by `BRIS_COLLECTOR_CONFIG` if
    /// that env var is set, otherwise no file is read.
    ///
    /// # Errors
    ///
    /// Returns a string describing the first problem: an
    /// unreadable / malformed config file, a malformed numeric
    /// env var, or a missing required `data_root`.
    pub fn load(config_path: Option<&Path>) -> Result<Self, String> {
        let mut cfg = Self::default();

        // Layer 2: the config file, if any.
        let file_path: Option<PathBuf> = config_path
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("BRIS_COLLECTOR_CONFIG").map(PathBuf::from));
        if let Some(path) = file_path {
            cfg.apply_file(&path)?;
        }

        // Layer 3: environment overrides.
        cfg.apply_env()?;

        // Required-field validation (a token is validated by the
        // binary at startup, not here, so tests may build a
        // token-less Config directly).
        if cfg.data_root.as_os_str().is_empty() {
            return Err(
                "data_root is required (set BRIS_COLLECTOR_DATA_ROOT or `data_root` in the config file)"
                    .to_owned(),
            );
        }
        Ok(cfg)
    }

    /// Build a config from process environment variables only
    /// (no file). Equivalent to `load(None)` when
    /// `BRIS_COLLECTOR_CONFIG` is unset; retained as the binary's
    /// entry point and for backward compatibility.
    ///
    /// # Errors
    ///
    /// As [`Config::load`].
    pub fn from_env() -> Result<Self, String> {
        Self::load(None)
    }

    /// Read and merge a TOML config file over the current values.
    fn apply_file(&mut self, path: &Path) -> Result<(), String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading config file {}: {e}", path.display()))?;
        let file: FileConfig = toml::from_str(&text)
            .map_err(|e| format!("parsing config file {}: {e}", path.display()))?;
        if let Some(v) = file.data_root {
            self.data_root = v;
        }
        if let Some(v) = file.bind {
            self.bind = v;
        }
        if let Some(v) = file.bearer_token {
            self.bearer_token = v;
        }
        if let Some(v) = file.max_submission_bytes {
            self.max_submission_bytes = v;
        }
        if let Some(v) = file.retention_days {
            self.retention_days = v;
        }
        Ok(())
    }

    /// Overlay environment variables over the current values.
    fn apply_env(&mut self) -> Result<(), String> {
        if let Ok(v) = std::env::var("BRIS_COLLECTOR_DATA_ROOT") {
            self.data_root = v.into();
        }
        if let Ok(v) = std::env::var("BRIS_COLLECTOR_BIND") {
            self.bind = v;
        }
        if let Ok(v) = std::env::var("BRIS_COLLECTOR_BEARER_TOKEN") {
            self.bearer_token = v;
        }
        if let Ok(v) = std::env::var("BRIS_COLLECTOR_MAX_SUBMISSION_BYTES") {
            self.max_submission_bytes = v
                .parse::<usize>()
                .map_err(|e| format!("BRIS_COLLECTOR_MAX_SUBMISSION_BYTES: {e}"))?;
        }
        if let Ok(v) = std::env::var("BRIS_COLLECTOR_RETENTION_DAYS") {
            self.retention_days = v
                .parse::<i64>()
                .map_err(|e| format!("BRIS_COLLECTOR_RETENTION_DAYS: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Mutex, MutexGuard};

    /// The process environment is global mutable state; serialize
    /// every test that touches `BRIS_COLLECTOR_*` so they cannot
    /// clobber each other under the test runner's threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Clear every collector env var, returning the held lock.
    fn clean_env() -> MutexGuard<'static, ()> {
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for k in [
            "BRIS_COLLECTOR_DATA_ROOT",
            "BRIS_COLLECTOR_BIND",
            "BRIS_COLLECTOR_BEARER_TOKEN",
            "BRIS_COLLECTOR_MAX_SUBMISSION_BYTES",
            "BRIS_COLLECTOR_RETENTION_DAYS",
            "BRIS_COLLECTOR_CONFIG",
        ] {
            std::env::remove_var(k);
        }
        guard
    }

    #[test]
    fn env_only_loads_and_applies_defaults() {
        let _g = clean_env();
        std::env::set_var("BRIS_COLLECTOR_DATA_ROOT", "/srv/data");
        std::env::set_var("BRIS_COLLECTOR_BEARER_TOKEN", "s3cret");

        let cfg = Config::load(None).expect("load");
        assert_eq!(cfg.data_root, PathBuf::from("/srv/data"));
        assert_eq!(cfg.bearer_token, "s3cret");
        // Unset values take their documented defaults.
        assert_eq!(cfg.bind, DEFAULT_BIND);
        assert_eq!(cfg.max_submission_bytes, DEFAULT_MAX_SUBMISSION_BYTES);
        assert_eq!(cfg.retention_days, DEFAULT_RETENTION_DAYS);
    }

    #[test]
    fn missing_data_root_is_rejected() {
        let _g = clean_env();
        // No data_root anywhere → refuse.
        let err = Config::load(None).expect_err("must reject missing data_root");
        assert!(err.contains("data_root"), "unexpected error: {err}");
    }

    #[test]
    fn loads_from_file() {
        let _g = clean_env();
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        write!(
            f,
            r#"
data_root = "/var/lib/bris-collector"
bind = "0.0.0.0:9000"
bearer_token = "from-file"
max_submission_bytes = 1048576
retention_days = 7
"#
        )
        .expect("write");

        let cfg = Config::load(Some(f.path())).expect("load");
        assert_eq!(cfg.data_root, PathBuf::from("/var/lib/bris-collector"));
        assert_eq!(cfg.bind, "0.0.0.0:9000");
        assert_eq!(cfg.bearer_token, "from-file");
        assert_eq!(cfg.max_submission_bytes, 1_048_576);
        assert_eq!(cfg.retention_days, 7);
    }

    #[test]
    fn env_overrides_file() {
        let _g = clean_env();
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        write!(
            f,
            r#"
data_root = "/from/file"
bind = "0.0.0.0:9000"
bearer_token = "file-token"
retention_days = 90
"#
        )
        .expect("write");

        // Environment wins over the file, per-key.
        std::env::set_var("BRIS_COLLECTOR_BEARER_TOKEN", "env-token");
        std::env::set_var("BRIS_COLLECTOR_RETENTION_DAYS", "14");

        let cfg = Config::load(Some(f.path())).expect("load");
        // Overridden by env.
        assert_eq!(cfg.bearer_token, "env-token");
        assert_eq!(cfg.retention_days, 14);
        // Not overridden → file value survives.
        assert_eq!(cfg.data_root, PathBuf::from("/from/file"));
        assert_eq!(cfg.bind, "0.0.0.0:9000");
    }

    #[test]
    fn config_path_from_env_var() {
        let _g = clean_env();
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        writeln!(f, "data_root = \"/via/env/config\"").expect("write");
        std::env::set_var("BRIS_COLLECTOR_CONFIG", f.path());

        let cfg = Config::load(None).expect("load");
        assert_eq!(cfg.data_root, PathBuf::from("/via/env/config"));
    }

    #[test]
    fn unknown_file_key_is_rejected() {
        let _g = clean_env();
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        writeln!(f, "data_root = \"/x\"\nnonsense_key = 1").expect("write");
        let err = Config::load(Some(f.path())).expect_err("must reject unknown key");
        assert!(
            err.contains("parsing config file"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn malformed_numeric_env_is_rejected() {
        let _g = clean_env();
        std::env::set_var("BRIS_COLLECTOR_DATA_ROOT", "/x");
        std::env::set_var("BRIS_COLLECTOR_RETENTION_DAYS", "not-a-number");
        let err = Config::load(None).expect_err("must reject bad number");
        assert!(err.contains("RETENTION_DAYS"), "unexpected error: {err}");
    }

    /// The `bearer_token` empty check is enforced by the binary,
    /// not `Config::load` — so tests may construct a token-less
    /// config. Assert the loaded value is empty when none is set,
    /// which is exactly the condition the binary refuses to serve
    /// on.
    #[test]
    fn absent_token_loads_empty_for_the_binary_to_refuse() {
        let _g = clean_env();
        std::env::set_var("BRIS_COLLECTOR_DATA_ROOT", "/x");
        let cfg = Config::load(None).expect("load");
        assert!(
            cfg.bearer_token.is_empty(),
            "no token configured must surface as empty so the binary refuses to start"
        );
    }
}
