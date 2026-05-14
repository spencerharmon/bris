//! Configuration file parsing and CLI-flag merging for the
//! `bris` headless deployments.
//!
//! Operator workflow:
//!
//! 1. Write a config file (TOML) describing the camera,
//!    observer, and NMEA outputs. Default search location:
//!    `~/.config/bris/config.toml`. Override with
//!    `bris --config <path>`.
//! 2. Run `bris serve` (or `bris capture`); the subcommand
//!    reads the config and applies any CLI-flag overrides on
//!    top.
//!
//! All sections are optional in the file. Required values
//! must be supplied either by the file or by a CLI flag;
//! omissions surface as a clear error at startup rather than
//! a hardcoded default that silently misroutes (e.g.
//! "observer at the equator" when the user forgot to set
//! their position).
//!
//! # Example
//!
//! ```toml
//! [observer]
//! latitude = 47.6
//! longitude = -122.3
//! eye_height_m = 2.5
//!
//! [camera]
//! device = "/dev/video0"
//! width = 640
//! height = 480
//! exposure_us = 10000
//!
//! # Multiple [[nmea]] tables fan out the engine's fix
//! # publications to every configured sink.
//! [[nmea]]
//! type = "stdout"
//!
//! [[nmea]]
//! type = "tcp"
//! addr = "0.0.0.0:10110"
//! ```
//!
//! # Override semantics
//!
//! CLI flags **override** config-file values. Resolution is
//! "flag if Some, else file value, else error if required."
//! This means a config file shipping with sensible defaults
//! and a per-invocation override (e.g.
//! `bris serve --device /dev/video1`) just works.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// Raw deserialized config-file contents. Every field is
/// optional so the file can carry a partial spec (the rest
/// supplied by CLI flags).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawConfig {
    #[serde(default)]
    pub observer: Option<RawObserver>,
    #[serde(default)]
    pub camera: Option<RawCamera>,
    #[serde(default)]
    pub nmea: Vec<RawNmea>,
}

/// Observer position + eye height. All fields optional in
/// the file; required for `bris serve`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawObserver {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub eye_height_m: Option<f64>,
}

/// Camera capture parameters. All fields optional in the
/// file; sensible defaults apply if missing.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCamera {
    pub device: Option<PathBuf>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub exposure_us: Option<u32>,
    /// Path to a calibration intrinsics file written by
    /// `bris calibrate`. When `None`, both
    /// [`ResolvedServeConfig`] and [`ResolvedCaptureConfig`]
    /// fall back to [`bris_vision::Intrinsics::placeholder`]
    /// — the engine will still produce fixes but they'll be
    /// wrong by the calibration error (potentially tens of
    /// nautical miles).
    pub intrinsics: Option<PathBuf>,
}

/// One NMEA output sink. Discriminated by the `type` field.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum RawNmea {
    /// Write NMEA sentences to stdout.
    Stdout,
    /// Bind a TCP server on the given address; broadcast
    /// each sentence to all connected clients.
    Tcp {
        /// Listen address. Convention: `0.0.0.0:10110` for
        /// the `OpenCPN` default.
        addr: SocketAddr,
    },
}

/// Default location of the config file:
/// `${XDG_CONFIG_HOME:-$HOME/.config}/bris/config.toml`.
///
/// Returns `None` if neither `$XDG_CONFIG_HOME` nor `$HOME`
/// is set (very unusual; e.g. service contexts that strip
/// the environment).
pub(crate) fn default_config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("bris").join("config.toml"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("bris")
                .join("config.toml"),
        );
    }
    None
}

/// Load config from the supplied path, or from the default
/// location if `path` is `None`. Returns an empty config
/// (all defaults) if the default location doesn't exist —
/// no-config-file is a valid mode where the operator
/// supplies everything via CLI flags.
///
/// # Errors
///
/// - The file at the explicit `path` doesn't exist.
/// - The file exists but is malformed TOML.
/// - The file is well-formed TOML but contains unknown
///   fields, indicating a likely typo (`#[serde(deny_unknown_fields)]`
///   is set on every config struct).
pub(crate) fn load_config(path: Option<&Path>) -> Result<RawConfig> {
    let resolved_path = match path {
        Some(p) => Some(p.to_path_buf()),
        None => default_config_path(),
    };
    let Some(p) = resolved_path else {
        // No path given and no default available — return
        // empty config; the resolver will error if
        // required fields are missing.
        return Ok(RawConfig::default());
    };
    if !p.exists() {
        if path.is_some() {
            // Operator explicitly named a file that doesn't
            // exist: surface that loudly.
            bail!("config file {} does not exist", p.display());
        }
        // Default path doesn't exist: silently fall back to
        // empty.
        return Ok(RawConfig::default());
    }
    let text =
        std::fs::read_to_string(&p).with_context(|| format!("read config file {}", p.display()))?;
    let cfg: RawConfig =
        toml::from_str(&text).with_context(|| format!("parse config file {}", p.display()))?;
    Ok(cfg)
}

/// Resolved config for the `serve` subcommand. All required
/// values are present; defaults applied where appropriate.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedServeConfig {
    pub device: PathBuf,
    pub width: u32,
    pub height: u32,
    pub exposure_us: u32,
    pub assumed_lat: f64,
    pub assumed_lon: f64,
    pub eye_height_m: f64,
    pub nmea_sinks: Vec<RawNmea>,
    /// Path to a calibration intrinsics file. `None` means
    /// "fall back to placeholder intrinsics with a loud
    /// warning" — see the `bris_calibrate::PersistedIntrinsics`
    /// doc for what that means for fix accuracy.
    pub intrinsics: Option<PathBuf>,
}

/// Resolved config for the `capture` subcommand.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedCaptureConfig {
    pub device: PathBuf,
    pub width: u32,
    pub height: u32,
    pub exposure_us: u32,
}

impl ResolvedServeConfig {
    /// Merge the file config with the CLI-flag overrides
    /// (which are themselves `Option<T>` so we can tell
    /// "not specified" from "specified to default").
    ///
    /// # Errors
    ///
    /// Returns an error listing every missing required
    /// field (lat/lon at minimum). Listing them all in one
    /// message avoids the operator playing whack-a-mole.
    #[allow(
        // 10 named overrides is one for each tunable
        // ServeArgs field; bundling them into a struct
        // would just push the same parameter list one
        // call deeper.
        clippy::too_many_arguments,
    )]
    pub(crate) fn resolve(
        file: &RawConfig,
        cli_device: Option<PathBuf>,
        cli_width: Option<u32>,
        cli_height: Option<u32>,
        cli_exposure_us: Option<u32>,
        cli_assumed_lat: Option<f64>,
        cli_assumed_lon: Option<f64>,
        cli_eye_height_m: Option<f64>,
        cli_nmea_stdout: bool,
        cli_nmea_tcp: Option<SocketAddr>,
        cli_intrinsics: Option<PathBuf>,
    ) -> Result<Self> {
        let camera = file.camera.as_ref();
        let observer = file.observer.as_ref();
        let device = cli_device
            .or_else(|| camera.and_then(|c| c.device.clone()))
            .unwrap_or_else(|| PathBuf::from("/dev/video0"));
        let width = cli_width
            .or_else(|| camera.and_then(|c| c.width))
            .unwrap_or(640);
        let height = cli_height
            .or_else(|| camera.and_then(|c| c.height))
            .unwrap_or(480);
        let exposure_us = cli_exposure_us
            .or_else(|| camera.and_then(|c| c.exposure_us))
            .unwrap_or(10_000);
        let eye_height_m = cli_eye_height_m
            .or_else(|| observer.and_then(|o| o.eye_height_m))
            .unwrap_or(2.0);
        let intrinsics = cli_intrinsics.or_else(|| camera.and_then(|c| c.intrinsics.clone()));

        // Required: lat + lon. Collect both into one error
        // message if both are missing.
        let lat = cli_assumed_lat.or_else(|| observer.and_then(|o| o.latitude));
        let lon = cli_assumed_lon.or_else(|| observer.and_then(|o| o.longitude));
        let assumed_lat = lat.ok_or_else(|| {
            anyhow!(
                "observer latitude not set. Pass --assumed-lat or set \
                 [observer]\\nlatitude = ... in the config file."
            )
        })?;
        let assumed_lon = lon.ok_or_else(|| {
            anyhow!(
                "observer longitude not set. Pass --assumed-lon or set \
                 [observer]\\nlongitude = ... in the config file."
            )
        })?;

        // NMEA sinks: union of CLI flags and config-file
        // sinks. CLI flags take effect in addition to file
        // sinks rather than replacing them — that's the
        // useful semantics ("config has my normal TCP setup;
        // I want stdout too for this debug session").
        let mut nmea_sinks: Vec<RawNmea> = file.nmea.clone();
        if cli_nmea_stdout {
            nmea_sinks.push(RawNmea::Stdout);
        }
        if let Some(addr) = cli_nmea_tcp {
            nmea_sinks.push(RawNmea::Tcp { addr });
        }

        Ok(Self {
            device,
            width,
            height,
            exposure_us,
            assumed_lat,
            assumed_lon,
            eye_height_m,
            nmea_sinks,
            intrinsics,
        })
    }
}

impl ResolvedCaptureConfig {
    pub(crate) fn resolve(
        file: &RawConfig,
        cli_device: Option<PathBuf>,
        cli_width: Option<u32>,
        cli_height: Option<u32>,
        cli_exposure_us: Option<u32>,
    ) -> Self {
        let camera = file.camera.as_ref();
        let device = cli_device
            .or_else(|| camera.and_then(|c| c.device.clone()))
            .unwrap_or_else(|| PathBuf::from("/dev/video0"));
        let width = cli_width
            .or_else(|| camera.and_then(|c| c.width))
            .unwrap_or(640);
        let height = cli_height
            .or_else(|| camera.and_then(|c| c.height))
            .unwrap_or(480);
        let exposure_us = cli_exposure_us
            .or_else(|| camera.and_then(|c| c.exposure_us))
            .unwrap_or(10_000);
        Self {
            device,
            width,
            height,
            exposure_us,
        }
    }
}

#[cfg(test)]
mod tests {
    // The resolver tests assert_eq! on f64 fields that flow
    // straight from the config without arithmetic; exact
    // equality is the right check (and clippy::float_cmp's
    // standard advice "use approx" doesn't apply when the
    // value passed in is the value coming out).
    #![allow(clippy::float_cmp)]

    use super::*;

    #[test]
    fn empty_config_parses_to_all_defaults() {
        let cfg: RawConfig = toml::from_str("").unwrap();
        assert!(cfg.observer.is_none());
        assert!(cfg.camera.is_none());
        assert!(cfg.nmea.is_empty());
    }

    #[test]
    fn full_config_parses_correctly() {
        let text = r#"
[observer]
latitude = 47.6
longitude = -122.3
eye_height_m = 2.5

[camera]
device = "/dev/video0"
width = 1280
height = 720
exposure_us = 5000

[[nmea]]
type = "stdout"

[[nmea]]
type = "tcp"
addr = "0.0.0.0:10110"
"#;
        let cfg: RawConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.observer.as_ref().unwrap().latitude, Some(47.6));
        assert_eq!(cfg.observer.as_ref().unwrap().longitude, Some(-122.3));
        assert_eq!(cfg.observer.as_ref().unwrap().eye_height_m, Some(2.5));
        assert_eq!(cfg.camera.as_ref().unwrap().width, Some(1280));
        assert_eq!(cfg.nmea.len(), 2);
        assert!(matches!(cfg.nmea[0], RawNmea::Stdout));
        assert!(matches!(cfg.nmea[1], RawNmea::Tcp { .. }));
    }

    #[test]
    fn unknown_field_rejected() {
        let text = r"
[observer]
latitudo = 47.6  # typo
";
        let err = toml::from_str::<RawConfig>(text).unwrap_err();
        assert!(
            err.to_string().contains("latitudo") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn resolve_serve_uses_cli_when_present() {
        let file = RawConfig::default();
        let resolved = ResolvedServeConfig::resolve(
            &file,
            Some(PathBuf::from("/dev/video1")),
            Some(800),
            Some(600),
            Some(20_000),
            Some(45.0),
            Some(-90.0),
            Some(3.0),
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(resolved.device, PathBuf::from("/dev/video1"));
        assert_eq!(resolved.width, 800);
        assert_eq!(resolved.assumed_lat, 45.0);
        assert!(matches!(resolved.nmea_sinks.as_slice(), [RawNmea::Stdout]));
    }

    #[test]
    fn resolve_serve_uses_file_when_cli_absent() {
        let file: RawConfig = toml::from_str(
            r#"
[observer]
latitude = 47.6
longitude = -122.3

[camera]
device = "/dev/video2"
"#,
        )
        .unwrap();
        let resolved = ResolvedServeConfig::resolve(
            &file, None, None, None, None, None, None, None, false, None, None,
        )
        .unwrap();
        assert_eq!(resolved.device, PathBuf::from("/dev/video2"));
        assert_eq!(resolved.assumed_lat, 47.6);
        assert_eq!(resolved.assumed_lon, -122.3);
        // Defaults applied where neither file nor CLI specified.
        assert_eq!(resolved.width, 640);
        assert_eq!(resolved.eye_height_m, 2.0);
    }

    #[test]
    fn resolve_serve_errors_on_missing_required_lat() {
        let file = RawConfig::default();
        let err = ResolvedServeConfig::resolve(
            &file,
            None,
            None,
            None,
            None,
            None,
            Some(0.0),
            None,
            false,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("latitude"), "got: {err}");
    }

    #[test]
    fn resolve_serve_unions_file_and_cli_nmea_sinks() {
        let file: RawConfig = toml::from_str(
            r#"
[observer]
latitude = 0.0
longitude = 0.0

[[nmea]]
type = "tcp"
addr = "10.0.0.1:10110"
"#,
        )
        .unwrap();
        let resolved = ResolvedServeConfig::resolve(
            &file, None, None, None, None, None, None, None,
            true, // --nmea-stdout adds a second sink
            None, None,
        )
        .unwrap();
        assert_eq!(resolved.nmea_sinks.len(), 2);
    }

    #[test]
    fn load_config_errors_on_explicit_missing_path() {
        // Operator named a path that doesn't exist; surface
        // it as an error rather than silently using defaults.
        // (Silently falling back is the right behaviour for
        // the default path, which is tested by relying on
        // the dev environment not having ~/.config/bris/config.toml
        // — verified manually.)
        let bogus = std::path::PathBuf::from("/definitely/does/not/exist/bris-config-test.toml");
        let err = load_config(Some(&bogus)).unwrap_err();
        assert!(
            err.to_string().contains("does not exist"),
            "expected 'does not exist', got: {err}"
        );
    }
}
