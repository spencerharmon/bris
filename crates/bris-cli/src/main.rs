//! Bris reference CLI: desktop and embedded Linux frontend.
//!
//! Subcommands (per `plan.org` Phase 6):
//! - `capture` — record frames from a V4L2 camera to disk.
//!   *Implemented* against the YUYV format on Linux.
//! - `calibrate` — lens calibration workflow (stub).
//! - `fix` — one-shot fix from a webcam (stub; the streaming
//!   engine in `serve` supersedes this).
//! - `serve` — continuous engine + NMEA serving. *Implemented*
//!   for the V4L2 → engine → published-fix path with NMEA
//!   stdout and TCP server transports. Serial-port and
//!   UDP-broadcast sinks are follow-ups.
//! - `replay` — process saved frames through the full pipeline.
//!   *Implemented* as the validation path before live capture.
//! - `log` — sight log management (stub).
//! - `update` — refresh almanac/catalog/leap-seconds (stub).

mod config;
mod nmea_transport;

use anyhow::{bail, Context};
use bris_almanac::{refraction::Atmosphere, Observer};
use bris_bundle::{
    enumerate_frames, verify_first_frame_checksum, ApInput, ApProvenance, BundleManifest,
    CaptureInfo, DeviceInfo, Distortion, FramePathPair, GpsTruth, IntrinsicsRecord,
};
use bris_calibrate::{
    calibrate, coverage, default_intrinsics_path, detect_corners_in_directory_with_progress,
    diagnose, write_intrinsics, CheckerboardTarget, CoverageConfig, DiagnosisLevel, FrameDetection,
    FrameOutcome,
};
use bris_capture::{
    max_yuyv_resolution, run_capture_loop, run_capture_loop_with, CaptureLoopAction, V4l2Capture,
    V4l2Config,
};
use bris_core::time::utc_to_tt;
use bris_core::{Latitude, Longitude, SensorGain};
use bris_nmea::QualityThresholds;
use bris_streaming::{format_fix_as_nmea, EngineConfig, PublishedFix, StreamingEngine};
use bris_vision::{load_frame_from_path_with_rotation, save_frame_as_png, Intrinsics, Rotation};
use chrono::{TimeZone, Utc};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "bris",
    version,
    about = "Bris: digital sextant",
    long_about = "Continuous celestial navigation from a camera. \
                  See https://github.com/anomalyco/bris."
)]
struct Cli {
    /// Path to a TOML configuration file. Default search
    /// location: `$XDG_CONFIG_HOME/bris/config.toml` (falling
    /// back to `~/.config/bris/config.toml`). When the
    /// default path doesn't exist, missing values must be
    /// supplied via subcommand flags. See
    /// `crates/bris-cli/src/config.rs` for the schema.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Record frames from a V4L2 camera to disk.
    ///
    /// The captured frames are saved as 16-bit grayscale PNGs
    /// suitable for re-processing via `bris replay`. Use this
    /// to (a) sanity-check that the camera is talking, (b)
    /// build a local corpus for offline development, (c)
    /// gather data for lens calibration.
    Capture(CaptureArgs),
    /// Fit camera intrinsics from captured frames of a
    /// printed checkerboard. See `docs/operator/calibration.md` for
    /// the operator workflow.
    Calibrate(CalibrateArgs),
    /// Compute a one-shot fix from a webcam (stub; the
    /// streaming engine in `serve` supersedes this).
    Fix,
    /// Run the continuous streaming engine against a V4L2
    /// camera, logging each published fix and emitting NMEA
    /// to the configured sinks (stdout, TCP server).
    ///
    /// Serial-port and UDP-broadcast sinks are not yet
    /// implemented.
    Serve(ServeArgs),
    /// Re-derive a fix from a directory of saved frames.
    ///
    /// Frames are processed in lexicographic filename order. Each
    /// frame's capture time defaults to its file modification time;
    /// override via a sidecar `frames.csv` (planned).
    ///
    /// Honest limitations: a single-body sight produces a line of
    /// position, not a true 2D fix. Without plate solving (Phase 3),
    /// you must specify which body the camera was pointed at and an
    /// approximate observer position; the output is the LOP refined
    /// from your assumed position.
    Replay(ReplayArgs),
    /// Sight log management (stub).
    Log,
    /// Download almanac/catalog/leap-second updates (stub).
    Update,
}

#[derive(Debug, clap::Args)]
struct CaptureArgs {
    /// Path to the V4L2 device node. Defaults to the
    /// config-file value if set, then `/dev/video0`.
    #[arg(long)]
    device: Option<PathBuf>,
    /// Capture width (pixels). Defaults to the config-file
    /// value if set, then 640.
    #[arg(long)]
    width: Option<u32>,
    /// Capture height (pixels). Defaults to the config-file
    /// value if set, then 480.
    #[arg(long)]
    height: Option<u32>,
    /// Output directory for captured PNG frames. Created if
    /// it doesn't exist. Existing files in the directory are
    /// not modified, but new captures may overwrite same-
    /// numbered files from a prior run.
    #[arg(long)]
    output: PathBuf,
    /// Maximum number of frames to capture. The capture
    /// stops at whichever of `--frames` and `--duration`
    /// is reached first; if neither is given, runs until
    /// Ctrl-C.
    #[arg(long)]
    frames: Option<u32>,
    /// Maximum capture duration in seconds. See
    /// `--frames` for stop semantics.
    #[arg(long)]
    duration: Option<f64>,
    /// Camera exposure for the timestamp correction, in
    /// microseconds. Defaults to the config-file value if
    /// set, then 10000 (10 ms — typical daylight).
    #[arg(long)]
    exposure_us: Option<u32>,
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// Path to the V4L2 device node. Defaults to the
    /// config-file value if set, then `/dev/video0`.
    #[arg(long)]
    device: Option<PathBuf>,
    /// Capture width (pixels). Defaults to the config-file
    /// value if set, then 640.
    #[arg(long)]
    width: Option<u32>,
    /// Capture height (pixels). Defaults to the config-file
    /// value if set, then 480.
    #[arg(long)]
    height: Option<u32>,
    /// Camera exposure for the timestamp correction, in
    /// microseconds. Defaults to the config-file value if
    /// set, then 10000.
    #[arg(long)]
    exposure_us: Option<u32>,
    /// Observer latitude in degrees (north positive). The
    /// engine uses this for almanac apparent-place
    /// computations and for the assumed position in sight
    /// reduction. The fix it publishes is a refinement of
    /// this assumed position; an error of a few hundred nm
    /// in the assumed position introduces a few-arcmin
    /// linearization error in the fix, which is in the
    /// noise for typical sights but matters offshore. Use
    /// the most-recent known fix (DR or GNSS) when
    /// available. Required: must be set via this flag or
    /// the config file.
    #[arg(long, allow_hyphen_values = true)]
    assumed_lat: Option<f64>,
    /// Observer longitude in degrees (east positive).
    /// Required: must be set via this flag or the config
    /// file. See `--assumed-lat` for accuracy requirements.
    #[arg(long, allow_hyphen_values = true)]
    assumed_lon: Option<f64>,
    /// Eye height above sea level, meters. Defaults to the
    /// config-file value if set, then 2.0.
    #[arg(long)]
    eye_height_m: Option<f64>,
    /// Emit NMEA sentences to stdout in addition to any
    /// `[[nmea]]` sinks defined in the config file. Off by
    /// default; useful for piping into another tool
    /// (`bris serve --nmea-stdout | gpsd`) or for debugging
    /// without editing the config file.
    #[arg(long, default_value_t = false)]
    nmea_stdout: bool,
    /// Bind a TCP server on this address and broadcast NMEA
    /// sentences to every connected client. Adds to any
    /// `[[nmea]]` sinks defined in the config file. Use
    /// `0.0.0.0:10110` for the `OpenCPN` convention.
    #[arg(long)]
    nmea_tcp: Option<std::net::SocketAddr>,
    /// Path to a calibration intrinsics TOML file written
    /// by `bris calibrate`. Overrides the
    /// `[camera] intrinsics` config-file value. When neither
    /// is set, falls back to placeholder intrinsics
    /// (fx = fy = 1000) with a loud warning — fixes will be
    /// off by the calibration error (potentially tens of
    /// nautical miles).
    #[arg(long)]
    intrinsics: Option<PathBuf>,
    /// Root directory for on-disk sight + fix persistence.
    /// Defaults to `~/.bris/`. Set to a directory the
    /// process can create files in; lives under your
    /// configured user data.
    #[arg(long)]
    data_root: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct CalibrateArgs {
    /// Directory containing captured calibration frames
    /// (PNG/JPEG). Use `bris capture --output <dir> --frames N`
    /// to record them, or capture with any other tool that
    /// produces grayscale or color images.
    #[arg(long)]
    frames: PathBuf,
    /// Number of *inner* corners along the short side of
    /// the checkerboard. Default 7 (an 8-square-tall board).
    #[arg(long, default_value_t = 7)]
    rows: u32,
    /// Number of *inner* corners along the long side of
    /// the checkerboard. Default 11 (a 12-square-wide
    /// board).
    #[arg(long, default_value_t = 11)]
    cols: u32,
    /// Square edge length in millimeters. Default 25.
    /// Measure your printed board carefully — printer
    /// scaling is a common source of millimeter-scale error.
    #[arg(long, default_value_t = 25.0)]
    square_size_mm: f64,
    /// Where to write the resulting intrinsics TOML file.
    /// Default: `$XDG_DATA_HOME/bris/intrinsics.toml`.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)]
struct ReplayArgs {
    /// Path to a debug bundle directory. Loads `bundle.json`
    /// plus the frames + sidecars from the directory. Preferred
    /// over `--frames`; CLI overrides may still be applied on
    /// top.
    #[arg(long)]
    bundle: Option<PathBuf>,
    /// Directory of raw frames (legacy orphan-corpus path). Only
    /// honored when `--bundle` is absent; requires the manifest
    /// values to be supplied via CLI flags below.
    #[arg(long, conflicts_with = "bundle")]
    frames: Option<PathBuf>,

    /// Assumed-position latitude override (degrees, N positive).
    #[arg(long, allow_hyphen_values = true)]
    ap_lat: Option<f64>,
    /// Assumed-position longitude override (degrees, E positive).
    #[arg(long, allow_hyphen_values = true)]
    ap_lon: Option<f64>,
    /// Eye-height override (metres).
    #[arg(long)]
    eye_height_m: Option<f64>,
    /// GPS-truth latitude override (degrees, N positive). Used
    /// only by scoring; never silently substituted for `ap_lat`.
    #[arg(long, allow_hyphen_values = true)]
    gps_truth_lat: Option<f64>,
    /// GPS-truth longitude override (degrees, E positive).
    #[arg(long, allow_hyphen_values = true)]
    gps_truth_lon: Option<f64>,
    /// JSON file matching the `IntrinsicsRecord` schema.
    #[arg(long)]
    intrinsics: Option<PathBuf>,
    /// Source-rotation override.
    #[arg(long, value_enum)]
    source_rotation: Option<RotationArg>,
    /// Fallback capture-UTC for `--frames` use (ISO-8601). Per-
    /// frame sidecars always win when present.
    #[arg(long)]
    capture_utc: Option<String>,

    /// Default mode: AP comes from the bundle's `ap_input` (may
    /// be null → cold-start).
    #[arg(long, group = "ap_mode")]
    ap_seed_truth: bool,
    /// Seed AP from `gps_truth`; engine may still re-derive.
    #[arg(long, group = "ap_mode")]
    ap_lock_truth: bool,
    /// Seed AP from `gps_truth` AND lock it (engine cannot
    /// re-derive). Diagnostic-only.
    #[arg(long, group = "ap_mode")]
    no_ap: bool,
    /// Run every mode the bundle's data supports and print a
    /// side-by-side summary at the end.
    #[arg(long, group = "ap_mode")]
    all_modes: bool,

    /// Override the engine's segmentation-model path.
    #[arg(long)]
    segmentation_model: Option<PathBuf>,
    /// Engine sight/fix store root. Defaults to a temp dir per
    /// run so replays don't pollute the operator's `.bris/`.
    #[arg(long)]
    data_root: Option<PathBuf>,
    /// Disable the on-disk sight/fix store entirely.
    #[arg(long)]
    disable_store: bool,
    /// Emit NMEA sentences for every published fix to stdout.
    #[arg(long)]
    nmea_stdout: bool,
}

/// CLI-facing rotation enum.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum RotationArg {
    /// 0°.
    Deg0,
    /// 90° clockwise.
    Deg90,
    /// 180°.
    Deg180,
    /// 270° clockwise.
    Deg270,
}

impl RotationArg {
    fn to_rotation(self) -> Rotation {
        match self {
            Self::Deg0 => Rotation::Deg0,
            Self::Deg90 => Rotation::Deg90,
            Self::Deg180 => Rotation::Deg180,
            Self::Deg270 => Rotation::Deg270,
        }
    }
    fn degrees(self) -> u16 {
        self.to_rotation().degrees()
    }
}

fn rotation_from_degrees(deg: u16) -> anyhow::Result<Rotation> {
    match deg {
        0 => Ok(Rotation::Deg0),
        90 => Ok(Rotation::Deg90),
        180 => Ok(Rotation::Deg180),
        270 => Ok(Rotation::Deg270),
        other => bail!("unsupported source_rotation_deg={other} (expected 0|90|180|270)"),
    }
}

/// Default on-disk root for the sight + fix store. `$HOME/.bris/`
/// on Unix; current directory as a last-resort fallback.
fn default_data_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map_or_else(|| PathBuf::from(".bris"), |h| h.join(".bris"))
}

fn default_segmentation_model_path() -> std::path::PathBuf {
    // The model file lives next to the source for the bris-vision
    // crate. We resolve it relative to the cargo manifest of bris-cli
    // for development; for shipped binaries the user must pass an
    // explicit --segmentation-model path or set up bris-data with the
    // file in a known location (TBD).
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("bris-vision")
        .join("data")
        .join("segmentation.onnx")
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,bris_nmea=debug")),
        )
        .init();

    let cli = Cli::parse();
    let raw_config = config::load_config(cli.config.as_deref()).context("load configuration")?;
    match cli.command {
        Command::Replay(args) => run_replay(&args),
        Command::Capture(args) => run_capture(&args, &raw_config),
        Command::Serve(args) => run_serve(&args, &raw_config),
        Command::Calibrate(args) => run_calibrate(&args),
        Command::Fix | Command::Log | Command::Update => {
            bail!("not yet implemented; see plan.org for the development roadmap");
        }
    }
}

// ---------------------------------------------------------
// Replay: debug-bundle / raw-frames → streaming engine.
// ---------------------------------------------------------

/// Which AP source to feed the engine for one replay run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayMode {
    /// AP comes from `manifest.ap_input` (may be absent → cold-start).
    Default,
    /// AP seeded from `gps_truth`; engine may still re-derive.
    ApSeedTruth,
    /// AP seeded from `gps_truth` AND locked (engine cannot re-derive).
    ApLockTruth,
    /// No AP fed in at all; rely on cold-start.
    NoAp,
}

impl ReplayMode {
    fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::ApSeedTruth => "ap_seed_truth",
            Self::ApLockTruth => "ap_lock_truth",
            Self::NoAp => "no_ap",
        }
    }
}

/// Resolved per-mode AP, with provenance label.
#[derive(Debug, Clone, Copy)]
struct ResolvedAp {
    lat: f64,
    lon: f64,
    eye_height_m: f64,
    source: &'static str,
}

#[derive(Debug)]
struct ModeResult {
    mode: ReplayMode,
    ap_used: Option<ResolvedAp>,
    fixes: Vec<PublishedFix>,
    suppressed: u64,
    frames_pushed: u64,
}

fn run_replay(args: &ReplayArgs) -> anyhow::Result<()> {
    // 1. Resolve the manifest (from --bundle or synthesized from --frames + flags).
    let (mut manifest, bundle_dir) = resolve_manifest(args)?;
    apply_cli_overrides(&mut manifest, args)?;

    // 2. Checksum verification (only if recorded).
    if manifest.capture.first_frame_blake3.is_some() {
        verify_first_frame_checksum(&manifest, &bundle_dir)
            .context("first-frame checksum verification")?;
        info!("replay: first-frame BLAKE3 checksum verified");
    }

    // 3. Enumerate frames once; sorted by sidecar captured_unix_ms.
    let frames = enumerate_frames(&bundle_dir).context("enumerate bundle frames")?;
    if frames.is_empty() {
        bail!("no frames found in bundle {}", bundle_dir.display());
    }
    info!(
        bundle = %bundle_dir.display(),
        frame_count = frames.len(),
        rotation_deg = manifest.capture.source_rotation_deg,
        "replay: bundle resolved"
    );

    // 4. Determine modes.
    let modes = select_modes(args, &manifest);
    if modes.is_empty() {
        bail!("no replay modes selected; pass one of --ap-seed-truth / --ap-lock-truth / --no-ap / --all-modes (or omit for Default)");
    }

    // 5. Run each mode.
    let mut results = Vec::new();
    for mode in modes {
        info!(mode = mode.label(), "replay: running mode");
        let result = run_one_mode(mode, args, &manifest, &frames)?;
        log_mode_result(&result, &manifest);
        results.push(result);
    }

    // 6. Summary table for --all-modes.
    if args.all_modes {
        print_summary(&results, &manifest);
    }

    Ok(())
}

/// Resolve a `BundleManifest` for the run, returning it plus the
/// directory it lives in. For `--frames` the manifest is
/// synthesized from CLI flags only.
fn resolve_manifest(args: &ReplayArgs) -> anyhow::Result<(BundleManifest, PathBuf)> {
    if let Some(bundle) = &args.bundle {
        let manifest = BundleManifest::load_from_dir(bundle)
            .with_context(|| format!("load bundle.json from {}", bundle.display()))?;
        return Ok((manifest, bundle.clone()));
    }
    let frames = args
        .frames
        .as_ref()
        .context("either --bundle or --frames must be supplied")?;
    let rotation = args.source_rotation.map_or(0, RotationArg::degrees);
    let intrinsics = args
        .intrinsics
        .as_ref()
        .map(|p| load_intrinsics_record(p.as_path()))
        .transpose()?
        .context(
            "--frames mode requires --intrinsics PATH (JSON matching the IntrinsicsRecord schema)",
        )?;
    // Synthesize a minimal manifest. `enumerate_frames` will read
    // the frame timestamps; we leave started/ended at 0 and let
    // the engine drive off the per-frame TT.
    let manifest = BundleManifest {
        schema_version: bris_bundle::SCHEMA_VERSION,
        bundle_id: frames
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("orphan-frames")
            .to_string(),
        device: DeviceInfo {
            model: "synthetic".into(),
            os: None,
            app_version: None,
        },
        capture: CaptureInfo {
            source_rotation_deg: rotation,
            pre_rotation_was_deg: None,
            frame_count: 0,
            started_unix_ms: 0,
            ended_unix_ms: 0,
            first_frame_blake3: None,
        },
        intrinsics,
        ap_input: args.ap_lat.zip(args.ap_lon).map(|(lat, lon)| ApInput {
            lat,
            lon,
            eye_height_m: args.eye_height_m.unwrap_or(2.0),
            provenance: ApProvenance::OperatorEntered,
        }),
        ap_derivation_trace: None,
        gps_truth: args
            .gps_truth_lat
            .zip(args.gps_truth_lon)
            .map(|(lat, lon)| GpsTruth {
                lat,
                lon,
                lat_sigma_m: 5.0,
                lon_sigma_m: 5.0,
                altitude_m: None,
                altitude_sigma_m: None,
                captured_unix_ms: 0,
                source: "cli_override".into(),
                satellites_used: None,
            }),
        atmosphere_hint: None,
        notes: String::new(),
    };
    Ok((manifest, frames.clone()))
}

/// Apply CLI overrides on top of the loaded manifest, warning
/// loudly so operators know the bundle wasn't reproduced
/// verbatim.
fn apply_cli_overrides(manifest: &mut BundleManifest, args: &ReplayArgs) -> anyhow::Result<()> {
    if let (Some(lat), Some(lon)) = (args.ap_lat, args.ap_lon) {
        warn!(
            lat,
            lon, "replay: --ap-lat/--ap-lon overriding manifest ap_input"
        );
        let eye = args
            .eye_height_m
            .or_else(|| manifest.ap_input.as_ref().map(|a| a.eye_height_m))
            .unwrap_or(2.0);
        manifest.ap_input = Some(ApInput {
            lat,
            lon,
            eye_height_m: eye,
            provenance: ApProvenance::Other {
                detail: "cli_override".into(),
            },
        });
    }
    if let Some(eye) = args.eye_height_m {
        if let Some(ap) = manifest.ap_input.as_mut() {
            if (ap.eye_height_m - eye).abs() > f64::EPSILON {
                warn!(eye, "replay: --eye-height-m overriding manifest eye height");
                ap.eye_height_m = eye;
            }
        }
    }
    if let (Some(lat), Some(lon)) = (args.gps_truth_lat, args.gps_truth_lon) {
        warn!(
            lat,
            lon, "replay: --gps-truth-* overriding manifest gps_truth"
        );
        manifest.gps_truth = Some(GpsTruth {
            lat,
            lon,
            lat_sigma_m: 5.0,
            lon_sigma_m: 5.0,
            altitude_m: None,
            altitude_sigma_m: None,
            captured_unix_ms: 0,
            source: "cli_override".into(),
            satellites_used: None,
        });
    }
    if let Some(rot) = args.source_rotation {
        let deg = rot.degrees();
        if manifest.capture.source_rotation_deg != deg {
            warn!(
                from = manifest.capture.source_rotation_deg,
                to = deg,
                "replay: --source-rotation overriding manifest"
            );
            manifest.capture.source_rotation_deg = deg;
        }
    }
    if let Some(path) = &args.intrinsics {
        warn!(path = %path.display(), "replay: --intrinsics overriding manifest intrinsics");
        manifest.intrinsics = load_intrinsics_record(path)?;
    }
    Ok(())
}

fn load_intrinsics_record(path: &Path) -> anyhow::Result<IntrinsicsRecord> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let rec: IntrinsicsRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse IntrinsicsRecord JSON at {}", path.display()))?;
    Ok(rec)
}

fn intrinsics_from_record(rec: &IntrinsicsRecord) -> Intrinsics {
    let (k1, k2, k3, p1, p2) = match &rec.distortion {
        Distortion::BrownConrady { k1, k2, k3, p1, p2 } => (*k1, *k2, *k3, *p1, *p2),
        Distortion::FisheyeEquidistant { .. } => {
            warn!(
                "replay: bundle declares FisheyeEquidistant distortion; \
                 bris_vision::Intrinsics is pinhole+Brown-Conrady only, \
                 dropping fisheye coefficients (TODO: extend Intrinsics)."
            );
            (0.0, 0.0, 0.0, 0.0, 0.0)
        }
        Distortion::None => (0.0, 0.0, 0.0, 0.0, 0.0),
    };
    Intrinsics {
        fx: rec.fx,
        fy: rec.fy,
        cx: rec.cx,
        cy: rec.cy,
        k1,
        k2,
        k3,
        p1,
        p2,
    }
}

fn select_modes(args: &ReplayArgs, manifest: &BundleManifest) -> Vec<ReplayMode> {
    if args.all_modes {
        let mut modes = vec![ReplayMode::Default];
        if manifest.gps_truth.is_some() {
            modes.push(ReplayMode::ApSeedTruth);
            modes.push(ReplayMode::ApLockTruth);
        } else {
            warn!(
                "replay --all-modes: skipping ApSeedTruth / ApLockTruth (no gps_truth in bundle)"
            );
        }
        modes.push(ReplayMode::NoAp);
        return modes;
    }
    let mode = if args.ap_seed_truth {
        ReplayMode::ApSeedTruth
    } else if args.ap_lock_truth {
        ReplayMode::ApLockTruth
    } else if args.no_ap {
        ReplayMode::NoAp
    } else {
        ReplayMode::Default
    };
    if matches!(mode, ReplayMode::ApSeedTruth | ReplayMode::ApLockTruth)
        && manifest.gps_truth.is_none()
    {
        warn!(
            "replay: mode {} requires gps_truth in the bundle; falling back to Default",
            mode.label()
        );
        return vec![ReplayMode::Default];
    }
    vec![mode]
}

fn resolve_ap(mode: ReplayMode, manifest: &BundleManifest) -> Option<ResolvedAp> {
    let eye_height = manifest.ap_input.as_ref().map_or(2.0, |a| a.eye_height_m);
    match mode {
        ReplayMode::Default => manifest.ap_input.as_ref().map(|a| ResolvedAp {
            lat: a.lat,
            lon: a.lon,
            eye_height_m: a.eye_height_m,
            source: "manifest.ap_input",
        }),
        ReplayMode::ApSeedTruth | ReplayMode::ApLockTruth => {
            manifest.gps_truth.as_ref().map(|g| ResolvedAp {
                lat: g.lat,
                lon: g.lon,
                eye_height_m: eye_height,
                source: "manifest.gps_truth",
            })
        }
        ReplayMode::NoAp => None,
    }
}

fn build_engine_config(
    mode: ReplayMode,
    ap: Option<ResolvedAp>,
    manifest: &BundleManifest,
    args: &ReplayArgs,
) -> anyhow::Result<EngineConfig> {
    let atmosphere = manifest
        .atmosphere_hint
        .as_ref()
        .map_or(Atmosphere::STANDARD, |h| {
            // The bundle records temperature/pressure/humidity;
            // bris-almanac's Atmosphere is pressure_mbar +
            // temperature_k (no humidity term yet). Convert
            // Pa→mbar, drop humidity for now (TODO: humidity in
            // refraction model).
            Atmosphere {
                pressure_mbar: h.pressure_pa / 100.0,
                temperature_k: h.temperature_k,
            }
        });
    // When no AP is available (NoAp mode, or Default with no
    // ap_input), seed the observer at (0, 0); the engine treats
    // the seeded observer as the cold-start anchor — Saint-
    // Hilaire intercepts won't converge but cold-start CoP may.
    let (lat, lon, eye) = ap.map_or((0.0, 0.0, 2.0), |a| (a.lat, a.lon, a.eye_height_m));
    let observer = Observer {
        latitude: Latitude::from_degrees(lat).context("AP lat out of range")?,
        longitude: Longitude::from_degrees(lon).context("AP lon")?,
        eye_height_m: eye,
        eye_height_sigma_m: 0.5,
        atmosphere,
    };
    let mut cfg = EngineConfig::new(observer);
    cfg.lock_ap_for_replay = matches!(mode, ReplayMode::ApLockTruth);
    cfg.store.enabled = !args.disable_store;
    cfg.store.data_root = args
        .data_root
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("bris-replay-{}", mode.label())));
    cfg.segmentation_model_path = args.segmentation_model.clone().or_else(|| {
        let p = default_segmentation_model_path();
        p.exists().then_some(p)
    });
    Ok(cfg)
}

#[allow(clippy::too_many_lines)]
fn run_one_mode(
    mode: ReplayMode,
    args: &ReplayArgs,
    manifest: &BundleManifest,
    frames: &[FramePathPair],
) -> anyhow::Result<ModeResult> {
    let ap = resolve_ap(mode, manifest);
    let cfg = build_engine_config(mode, ap, manifest, args)?;
    let engine = Arc::new(StreamingEngine::new(cfg));
    let fix_rx = engine
        .fix_stream()
        .map_err(|e| anyhow::anyhow!("fix_stream: {e}"))?;

    let intrinsics = intrinsics_from_record(&manifest.intrinsics);
    let rotation = rotation_from_degrees(manifest.capture.source_rotation_deg)?;
    let frames_owned: Vec<FramePathPair> = frames.to_vec();
    let engine_feed = engine.clone();
    let mode_label = mode.label().to_string();
    let feeder = std::thread::Builder::new()
        .name(format!("bris-replay-feed-{mode_label}"))
        .spawn(move || -> anyhow::Result<u64> {
            let mut pushed = 0u64;
            for pair in &frames_owned {
                let s = &pair.sidecar_data;
                let utc = Utc
                    .timestamp_millis_opt(s.captured_unix_ms)
                    .single()
                    .with_context(|| {
                        format!("captured_unix_ms {} out of range", s.captured_unix_ms)
                    })?;
                let tt = utc_to_tt(utc).context("utc_to_tt")?;
                let exposure_us = s.exposure_us_or(1000);
                let gain = SensorGain::new(s.sensor_gain_or(1.0));
                let frame = load_frame_from_path_with_rotation(
                    &pair.pgm,
                    tt,
                    exposure_us,
                    intrinsics,
                    rotation,
                )
                .with_context(|| format!("load {}", pair.pgm.display()))?
                .with_sensor_gain(gain)
                .with_source_path(pair.pgm.clone());
                if let Err(e) = engine_feed.push_frame(frame) {
                    warn!(error = ?e, frame = %pair.pgm.display(), "replay: push_frame failed");
                } else {
                    pushed += 1;
                }
            }
            Ok(pushed)
        })
        .context("spawn replay feeder thread")?;

    // Main thread drains the fix stream with a short timeout
    // until the feeder joins.
    let mut collected: Vec<PublishedFix> = Vec::new();
    loop {
        match fix_rx.try_recv() {
            Ok(Some(fix)) => {
                if args.nmea_stdout {
                    let s = format_fix_as_nmea(&fix, Utc::now(), QualityThresholds::default());
                    print!("[mode={mode_label}] {s}");
                }
                collected.push(fix);
            }
            Ok(None) => {
                if feeder.is_finished() {
                    // Drain remaining, then exit.
                    while let Ok(Some(f)) = fix_rx.try_recv() {
                        if args.nmea_stdout {
                            let s =
                                format_fix_as_nmea(&f, Utc::now(), QualityThresholds::default());
                            print!("[mode={mode_label}] {s}");
                        }
                        collected.push(f);
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(()) => break,
        }
    }
    let pushed = feeder
        .join()
        .map_err(|_| anyhow::anyhow!("feeder thread panicked"))??;
    let diag = engine.diagnostics();
    info!(
        mode = mode_label,
        frames_pushed = diag.frames_pushed,
        frames_dropped = diag.frames_dropped,
        body_queue_depth = diag.body_queue_depth,
        horizon_queue_depth = diag.horizon_queue_depth,
        sight_window_depth = diag.sight_window_depth,
        last_raw_classification = ?diag.last_raw_classification,
        last_dispatched_condition = ?diag.last_dispatched_condition,
        fixes_published_total = diag.fixes_published_total,
        fix_publish_attempts = diag.fix_publish_attempts,
        singular_geometry_rejections = diag.singular_geometry_rejections,
        publication_gate_rejections = diag.publication_gate_rejections,
        cold_start_attempts = diag.cold_start_attempts,
        cold_start_published = diag.cold_start_published,
        ap_rederive_suppressed_count = diag.ap_rederive_suppressed_count,
        "replay: engine diagnostics"
    );
    Ok(ModeResult {
        mode,
        ap_used: ap,
        fixes: collected,
        suppressed: diag.ap_rederive_suppressed_count,
        frames_pushed: pushed,
    })
}

fn log_mode_result(result: &ModeResult, manifest: &BundleManifest) {
    info!(
        mode = result.mode.label(),
        frames_pushed = result.frames_pushed,
        fixes = result.fixes.len(),
        suppressed = result.suppressed,
        "replay: mode complete"
    );
    if result.fixes.is_empty() {
        info!(
            mode = result.mode.label(),
            "replay: no fix published (honest silence; see diagnostics above)"
        );
        return;
    }
    for fix in &result.fixes {
        let lat = fix.fix.lat.degrees();
        let lon = fix.fix.lon.degrees();
        info!(
            mode = result.mode.label(),
            lat_deg = lat,
            lon_deg = lon,
            sigma_major_nm = fix.fix.sigma_major_nm,
            sigma_minor_nm = fix.fix.sigma_minor_nm,
            "replay: published_fix"
        );
        if let Some(ap) = result.ap_used {
            info!(
                mode = result.mode.label(),
                ap_lat = ap.lat,
                ap_lon = ap.lon,
                ap_source = ap.source,
                "replay: ap_used"
            );
        } else {
            info!(
                mode = result.mode.label(),
                "replay: ap_used = none (cold-start)"
            );
        }
        if let Some(gt) = manifest.gps_truth.as_ref() {
            let (nm, brg) = great_circle_nm_and_bearing(lat, lon, gt.lat, gt.lon);
            let within = nm <= 2.0 * fix.fix.sigma_major_nm;
            info!(
                mode = result.mode.label(),
                error_nm = nm,
                bearing_deg = brg,
                within_2sigma = within,
                "replay: vs gps_truth"
            );
        } else {
            info!(mode = result.mode.label(), "replay: no gps_truth in bundle");
        }
    }
}

fn print_summary(results: &[ModeResult], manifest: &BundleManifest) {
    println!();
    println!("================= replay --all-modes summary =================");
    println!(
        "{:<14}  {:>6}  {:>6}  {:>10}  {:>10}  {:>11}  {:>11}",
        "mode", "frames", "fixes", "ap_lat", "ap_lon", "err_nm", "sig_maj_nm"
    );
    for r in results {
        let ap_str = r.ap_used.map_or_else(
            || ("-".to_string(), "-".to_string()),
            |a| (format!("{:.6}", a.lat), format!("{:.6}", a.lon)),
        );
        if r.fixes.is_empty() {
            println!(
                "{:<14}  {:>6}  {:>6}  {:>10}  {:>10}  {:>11}  {:>11}",
                r.mode.label(),
                r.frames_pushed,
                0,
                ap_str.0,
                ap_str.1,
                "-",
                "-"
            );
        } else {
            for fix in &r.fixes {
                let err = manifest.gps_truth.as_ref().map(|g| {
                    great_circle_nm_and_bearing(
                        fix.fix.lat.degrees(),
                        fix.fix.lon.degrees(),
                        g.lat,
                        g.lon,
                    )
                    .0
                });
                println!(
                    "{:<14}  {:>6}  {:>6}  {:>10}  {:>10}  {:>11}  {:>11.3}",
                    r.mode.label(),
                    r.frames_pushed,
                    r.fixes.len(),
                    ap_str.0,
                    ap_str.1,
                    err.map_or_else(|| "-".to_string(), |n| format!("{n:.3}")),
                    fix.fix.sigma_major_nm,
                );
            }
        }
    }
    println!("==============================================================");
}

/// Great-circle distance (nm) and forward bearing (deg, true)
/// from (lat1, lon1) to (lat2, lon2).
fn great_circle_nm_and_bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> (f64, f64) {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    let nm = c.to_degrees() * 60.0;
    let y = dlam.sin() * phi2.cos();
    let x = phi1.cos() * phi2.sin() - phi1.sin() * phi2.cos() * dlam.cos();
    let bearing = y.atan2(x).to_degrees().rem_euclid(360.0);
    (nm, bearing)
}

// ---------------------------------------------------------
// Capture: V4L2 → PNG files on disk.
// ---------------------------------------------------------

fn run_capture(args: &CaptureArgs, raw_config: &config::RawConfig) -> anyhow::Result<()> {
    let resolved = config::ResolvedCaptureConfig::resolve(
        raw_config,
        args.device.clone(),
        args.width,
        args.height,
        args.exposure_us,
    );

    fs::create_dir_all(&args.output)
        .with_context(|| format!("create output dir {}", args.output.display()))?;

    let (width, height) =
        resolve_capture_resolution(&resolved.device, resolved.width, resolved.height)?;
    let v4l_config = V4l2Config {
        device_path: resolved.device.clone(),
        width,
        height,
        buffer_count: 4,
        exposure_us: resolved.exposure_us,
    };
    let intrinsics = Intrinsics::placeholder(width, height);
    let capture = V4l2Capture::open(v4l_config, intrinsics).context("open V4L2 device")?;
    info!(
        device = %resolved.device.display(),
        width = width,
        height = height,
        output = %args.output.display(),
        "bris capture: starting"
    );
    info!(
        "bris capture: using placeholder camera intrinsics (fx=fy=1000); \
         this is fine for raw frame capture (the saved PNGs are not \
         intrinsics-dependent), but downstream processing of these \
         frames via `bris replay` or `bris serve` will be wrong by the \
         calibration error until `bris calibrate` lands."
    );

    let shutdown = install_ctrlc_handler()?;
    let start = Instant::now();
    let max_frames = args.frames;
    let max_duration = args.duration.map(Duration::from_secs_f64);
    let output_dir = args.output.clone();
    let mut counter: u32 = 0;

    let stats = run_capture_loop_with(capture, shutdown.clone(), |frame| {
        // Stop conditions in priority order:
        //   1. Ctrl-C (handled by the shutdown atomic; the
        //      loop polls it on each iteration).
        //   2. --frames cap reached.
        //   3. --duration cap reached.
        if let Some(max) = max_frames {
            if counter >= max {
                return CaptureLoopAction::Stop;
            }
        }
        if let Some(d) = max_duration {
            if start.elapsed() >= d {
                return CaptureLoopAction::Stop;
            }
        }
        let path = output_dir.join(format!("frame_{counter:08}.png"));
        match save_frame_as_png(&frame, &path) {
            Ok(()) => {
                counter += 1;
                if counter.is_multiple_of(30) {
                    info!(
                        frames_saved = counter,
                        elapsed_s = start.elapsed().as_secs_f64(),
                        "bris capture: progress"
                    );
                }
            }
            Err(e) => {
                warn!(error = %e, path = %path.display(), "bris capture: save failed");
            }
        }
        CaptureLoopAction::Continue
    })
    .context("V4L2 capture loop")?;

    info!(
        frames_saved = counter,
        frames_captured = stats.frames_captured,
        frames_dropped = stats.frames_dropped_at_capture,
        elapsed_s = start.elapsed().as_secs_f64(),
        "bris capture: done"
    );
    Ok(())
}

// ---------------------------------------------------------
// Serve: V4L2 → streaming engine → published fixes.
// ---------------------------------------------------------

#[allow(
    // The serve subcommand orchestrates the full pipeline:
    // resolve config, open camera, build engine, spawn
    // capture thread, drain fix stream, log everything,
    // join cleanly. Splitting it into helpers obscures the
    // top-down flow more than it helps.
    clippy::too_many_lines,
)]
fn run_serve(args: &ServeArgs, raw_config: &config::RawConfig) -> anyhow::Result<()> {
    let resolved = config::ResolvedServeConfig::resolve(
        raw_config,
        args.device.clone(),
        args.width,
        args.height,
        args.exposure_us,
        args.assumed_lat,
        args.assumed_lon,
        args.eye_height_m,
        args.nmea_stdout,
        args.nmea_tcp,
        args.intrinsics.clone(),
    )?;

    let observer = Observer {
        latitude: Latitude::from_degrees(resolved.assumed_lat)
            .context("assumed_lat out of [-90, 90]")?,
        longitude: Longitude::from_degrees(resolved.assumed_lon).context("assumed_lon")?,
        eye_height_m: resolved.eye_height_m,
        eye_height_sigma_m: 0.5,
        atmosphere: Atmosphere::STANDARD,
    };
    let engine_config = {
        let mut c = EngineConfig::new(observer);
        let data_root = args.data_root.clone().unwrap_or_else(default_data_root);
        c.store.data_root = data_root;
        c
    };
    let engine = Arc::new(StreamingEngine::new(engine_config));

    // Subscribe before the capture thread starts so we never
    // miss the first publication.
    let fix_rx = engine
        .fix_stream()
        .map_err(|e| anyhow::anyhow!("fix_stream: {e}"))?;

    let (width, height) =
        resolve_capture_resolution(&resolved.device, resolved.width, resolved.height)?;
    let v4l_config = V4l2Config {
        device_path: resolved.device.clone(),
        width,
        height,
        buffer_count: 4,
        exposure_us: resolved.exposure_us,
    };
    let (intrinsics, used_placeholder) =
        load_intrinsics(resolved.intrinsics.as_deref(), width, height)?;
    let capture = V4l2Capture::open(v4l_config, intrinsics).context("open V4L2 device")?;
    info!(
        device = %resolved.device.display(),
        width = width,
        height = height,
        observer_lat = resolved.assumed_lat,
        observer_lon = resolved.assumed_lon,
        "bris serve: starting"
    );
    if used_placeholder {
        warn!(
            "bris serve: using placeholder camera intrinsics (fx=fy=1000); \
             published fixes will be wrong by the calibration error \
             (potentially tens of nm). Run `bris calibrate --frames <dir>` to \
             fit per-device intrinsics, then point `bris serve` at the resulting \
             file via `[camera] intrinsics = ...` in your config or \
             `--intrinsics PATH` on the command line."
        );
    }

    // Materialize NMEA sinks from the resolved sink list
    // (file + CLI flags merged). Empty is fine: fixes still
    // publish via the structured `info!` log inside the
    // dispatch loop.
    let mut sinks: Vec<Box<dyn nmea_transport::NmeaSink>> = Vec::new();
    for sink_spec in &resolved.nmea_sinks {
        match sink_spec {
            config::RawNmea::Stdout => {
                sinks.push(Box::new(nmea_transport::StdoutSink));
            }
            config::RawNmea::Tcp { addr } => {
                let tcp = nmea_transport::TcpServerSink::bind(*addr)
                    .with_context(|| format!("bind NMEA TCP server on {addr}"))?;
                sinks.push(Box::new(tcp));
            }
        }
    }
    if sinks.is_empty() {
        info!(
            "bris serve: no NMEA sinks configured; fixes are visible via the \
             structured info! log only. Add [[nmea]] entries to the config \
             file or pass --nmea-stdout / --nmea-tcp ADDR to emit NMEA bytes."
        );
    }

    let shutdown = install_ctrlc_handler()?;
    let engine_thread = engine.clone();
    let shutdown_thread = shutdown.clone();
    let capture_handle = std::thread::Builder::new()
        .name("bris-capture".to_string())
        .spawn(move || run_capture_loop(capture, engine_thread, shutdown_thread))
        .context("spawn capture thread")?;

    // Main thread runs the NMEA dispatch loop, which drains
    // the fix stream and emits to all configured sinks plus
    // the structured info! log.
    nmea_transport::run_nmea_dispatch(
        fix_rx,
        sinks,
        shutdown.clone(),
        QualityThresholds::default(),
    );

    info!("bris serve: shutdown signalled, joining capture thread");
    let stats = capture_handle
        .join()
        .map_err(|_| anyhow::anyhow!("capture thread panicked"))?
        .context("capture loop")?;
    let diag = engine.diagnostics();
    info!(
        frames_captured = stats.frames_captured,
        frames_dropped_at_capture = stats.frames_dropped_at_capture,
        engine_frames_pushed = diag.frames_pushed,
        engine_frames_dropped = diag.frames_dropped,
        "bris serve: done"
    );
    Ok(())
}

/// Install a Ctrl-C handler that flips the returned atomic
/// from `false` to `true`. The handler runs once; subsequent
/// SIGINTs use the default (terminate) handler so the
/// operator can hard-kill if the graceful shutdown hangs.
fn install_ctrlc_handler() -> anyhow::Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_handler = shutdown.clone();
    ctrlc::set_handler(move || {
        info!("Ctrl-C received: shutting down");
        shutdown_for_handler.store(true, Ordering::Relaxed);
    })
    .context("install Ctrl-C handler")?;
    Ok(shutdown)
}

/// Resolve a possibly-unspecified capture resolution against
/// a V4L2 device's advertised YUYV frame sizes.
///
/// - If both `width` and `height` are `Some`, returns them
///   verbatim.
/// - If both are `None`, queries the device and returns its
///   largest advertised YUYV size. This is the operator-
///   preferred default (see `plan.org` per-stage-resolution
///   architecture): downstream stages downsample on their
///   own via `bris_vision::FramePyramid`, so feeding capture
///   the highest pixel count the sensor can deliver is the
///   right choice.
/// - Mixed `Some`/`None` is rejected — under-specifying one
///   axis is more likely a config mistake than an intent.
fn resolve_capture_resolution(
    device_path: &Path,
    width: Option<u32>,
    height: Option<u32>,
) -> anyhow::Result<(u32, u32)> {
    match (width, height) {
        (Some(w), Some(h)) => Ok((w, h)),
        (None, None) => {
            let (w, h) = max_yuyv_resolution(device_path).with_context(|| {
                format!("query max YUYV resolution from {}", device_path.display())
            })?;
            info!(
                device = %device_path.display(),
                width = w,
                height = h,
                "auto-selected device's largest YUYV resolution (per-stage \
                 downsampling handles smaller-grid stages)"
            );
            Ok((w, h))
        }
        _ => bail!(
            "capture width and height must both be specified or both omitted; \
             got width={width:?} height={height:?}"
        ),
    }
}

/// Load camera intrinsics from a calibration file when one
/// is configured, otherwise fall back to the placeholder.
///
/// Returns `(intrinsics, used_placeholder)`. The caller logs
/// a warning at `warn!` level when `used_placeholder` is
/// `true` so operators see the calibration shortfall in
/// every relevant subcommand.
///
/// Validates the file's recorded resolution against the
/// camera's actual capture resolution; mismatched
/// resolution silently produces wrong altitudes (focal
/// length scales with sensor crop / binning), so this
/// function errors loudly when they don't match.
fn load_intrinsics(
    intrinsics_path: Option<&Path>,
    capture_width: u32,
    capture_height: u32,
) -> anyhow::Result<(Intrinsics, bool)> {
    let Some(path) = intrinsics_path else {
        return Ok((Intrinsics::placeholder(capture_width, capture_height), true));
    };
    let persisted = bris_calibrate::read_intrinsics(path)
        .with_context(|| format!("read intrinsics from {}", path.display()))?;
    if persisted.intrinsics.image_width != capture_width
        || persisted.intrinsics.image_height != capture_height
    {
        bail!(
            "intrinsics file {} was calibrated against {}×{} but camera is producing {}×{}; \
             focal length scales with resolution and using these intrinsics would silently \
             produce wrong altitudes. Re-run `bris calibrate` at the camera's current \
             resolution.",
            path.display(),
            persisted.intrinsics.image_width,
            persisted.intrinsics.image_height,
            capture_width,
            capture_height,
        );
    }
    info!(
        path = %path.display(),
        rms_px = persisted.quality.mean_reproj_error_px,
        view_count = persisted.quality.view_count,
        "loaded camera intrinsics"
    );
    Ok((persisted.intrinsics(), false))
}

// ---------------------------------------------------------
// Calibrate: chessboard frames → camera intrinsics file.
// ---------------------------------------------------------

#[allow(
    // Calibrate orchestrates the full workflow: target
    // construction, detection with progress reporting,
    // solve with spinner, diagnostic, persistence, and the
    // final summary print. Splitting hurts readability;
    // the top-down flow is the documentation.
    clippy::too_many_lines,
)]
fn run_calibrate(args: &CalibrateArgs) -> anyhow::Result<()> {
    let target = CheckerboardTarget::new(args.rows, args.cols, args.square_size_mm / 1000.0)
        .with_context(|| {
            format!(
                "invalid checkerboard target ({}×{} inner corners, {} mm squares)",
                args.rows, args.cols, args.square_size_mm
            )
        })?;

    info!(
        frames = %args.frames.display(),
        rows = target.rows,
        cols = target.cols,
        square_size_mm = target.square_size_m * 1000.0,
        "bris calibrate: starting"
    );

    // Detection bar: one tick per candidate frame. The
    // total isn't known until the directory is enumerated
    // (which the library does first), but indicatif lets us
    // start with an unknown total and switch to a known one
    // on the first callback. We lean on the callback's
    // (current, total, &outcome) signature to do exactly
    // that, and to print a one-line per-frame note for
    // every non-success outcome so the operator can
    // immediately see *why* a frame was skipped instead of
    // discovering "23 of 30 frames were skipped" with no
    // detail at the end.
    let detect_bar = indicatif::ProgressBar::new(0).with_style(
        indicatif::ProgressStyle::with_template(
            "  detecting [{wide_bar}] {pos}/{len} frames • {elapsed_precise} • eta {eta}",
        )
        .expect("static template")
        .progress_chars("=> "),
    );
    let detect_bar_for_callback = detect_bar.clone();
    let mut on_progress = move |current: usize, total: usize, det: &FrameDetection| {
        if detect_bar_for_callback.length() != Some(total as u64) {
            detect_bar_for_callback.set_length(total as u64);
        }
        detect_bar_for_callback.set_position(current as u64 + 1);
        // Print non-success outcomes inline; success is the
        // norm and the running bar is enough feedback.
        match &det.outcome {
            FrameOutcome::Detected { .. } => {}
            FrameOutcome::NoBoardFound => {
                detect_bar_for_callback.println(format!(
                    "  · {}: no chessboard detected (motion blur, defocus, or board out of frame)",
                    det.path.display()
                ));
            }
            FrameOutcome::WrongGridSize {
                found_rows,
                found_cols,
                expected_rows,
                expected_cols,
            } => {
                detect_bar_for_callback.println(format!(
                    "  · {}: found {}×{} grid, expected {}×{} \
                     (partial occlusion or wrong --rows/--cols)",
                    det.path.display(),
                    found_rows,
                    found_cols,
                    expected_rows,
                    expected_cols,
                ));
            }
            FrameOutcome::DecodeFailed { reason } => {
                detect_bar_for_callback.println(format!(
                    "  · {}: decode failed: {}",
                    det.path.display(),
                    reason
                ));
            }
        }
    };
    let detect_result =
        detect_corners_in_directory_with_progress(&args.frames, target, &mut on_progress);
    detect_bar.finish_and_clear();
    let detection =
        detect_result.with_context(|| format!("detect corners in {}", args.frames.display()))?;
    let views = detection.views;
    let stats = detection.stats;
    info!(
        successful_views = views.len(),
        skipped_no_board = stats.skipped_no_board,
        skipped_wrong_size = stats.skipped_wrong_size,
        skipped_io = stats.skipped_io,
        tried = stats.tried,
        "bris calibrate: detection done"
    );
    eprintln!(
        "  detection: {}/{} frames produced usable views \
         ({} no-board, {} wrong-grid, {} decode-failed)",
        views.len(),
        stats.tried,
        stats.skipped_no_board,
        stats.skipped_wrong_size,
        stats.skipped_io,
    );
    if stats.skipped_no_board + stats.skipped_wrong_size > stats.tried / 3 {
        warn!(
            "more than a third of frames produced no usable detection; \
             consider re-shooting with sharper focus and a fully-visible board"
        );
    }

    // Coverage report: rendered as ASCII art so the
    // operator can see at a glance which image-plane
    // regions they over- or under-sampled. Useful before
    // the solve to decide whether to re-shoot, and after
    // it as part of the diagnostic.
    if let Some(cov) = coverage(&views, CoverageConfig::default()) {
        eprintln!();
        eprintln!(
            "  coverage: {}/{} grid cells sampled ({:.0}% of FOV) — pose-tilt diversity σ = {:.3}",
            (cov.cell_counts.iter().filter(|&&c| c > 0).count()),
            cov.cell_counts.len(),
            cov.covered_fraction * 100.0,
            cov.aspect_ratio_stddev,
        );
        for r in 0..cov.config.grid_rows {
            let mut line = String::from("    ");
            for c in 0..cov.config.grid_cols {
                let count = cov.cell_counts[(r * cov.config.grid_cols + c) as usize];
                let ch = match count {
                    0 => '.',
                    1..=2 => '·',
                    3..=5 => '+',
                    6..=10 => '#',
                    _ => '*',
                };
                line.push(ch);
                line.push(' ');
            }
            eprintln!("{line}");
        }
        eprintln!("    legend: . empty  · 1-2  + 3-5  # 6-10  * >10");
        if !cov.fully_covered() {
            eprintln!(
                "    {} cell(s) still empty; consider capturing additional frames \
                 with the board placed in those regions of the FOV.",
                cov.empty_cells,
            );
        }
    }

    // Solve bar: spinner only — the LM solve is one opaque
    // call from the CLI's perspective. The spinner gives
    // operators visible "still working" feedback during
    // the seconds-to-tens-of-seconds it takes; the
    // bris-calibrate solve emits its own info! lines on
    // completion so the spinner is purely visual.
    let solve_spinner =
        indicatif::ProgressBar::new_spinner().with_message("running bundle adjustment…");
    solve_spinner.enable_steady_tick(std::time::Duration::from_millis(120));
    let solve_result = calibrate(&views);
    solve_spinner.finish_and_clear();
    let result = solve_result.context("calibration solve")?;
    info!(
        rms_px = result.mean_reproj_error_px,
        fx = result.intrinsics.fx,
        fy = result.intrinsics.fy,
        cx = result.intrinsics.cx,
        cy = result.intrinsics.cy,
        k1 = result.intrinsics.k1,
        "bris calibrate: solve done"
    );

    let diagnosis = diagnose(&result);
    print_diagnosis(&diagnosis);
    print_per_view_residuals(&result);
    if matches!(diagnosis.overall, DiagnosisLevel::Error) {
        bail!(
            "calibration diagnostic flagged at least one error; not writing intrinsics. \
             Re-shoot frames or address the issues above and re-run."
        );
    }

    let output_path = match args.output.clone() {
        Some(p) => p,
        None => default_intrinsics_path().ok_or_else(|| {
            anyhow::anyhow!(
                "no --output specified and could not derive a default path \
                 (XDG_DATA_HOME and HOME both unset). Pass --output PATH explicitly."
            )
        })?,
    };
    write_intrinsics(&output_path, &result)
        .with_context(|| format!("write {}", output_path.display()))?;
    info!(
        path = %output_path.display(),
        rms_px = result.mean_reproj_error_px,
        view_count = result.view_count,
        "bris calibrate: wrote intrinsics"
    );

    eprintln!();
    eprintln!("Calibration written to: {}", output_path.display());
    eprintln!("  RMS reprojection: {:.3} px", result.mean_reproj_error_px);
    eprintln!("  Views used:       {}", result.view_count);
    eprintln!("  Observations:     {}", result.observation_count);
    eprintln!("  Diagnosis:        {}", diagnosis.overall.label());
    eprintln!();
    eprintln!(
        "Use the file with `bris serve` by setting `[camera] intrinsics = \"{}\"` \
         in your config file, or by passing `--intrinsics {}` on the command line.",
        output_path.display(),
        output_path.display(),
    );

    Ok(())
}

fn print_diagnosis(d: &bris_calibrate::Diagnosis) {
    if d.issues.is_empty() {
        eprintln!("Diagnosis: OK (no issues found)");
        return;
    }
    eprintln!();
    eprintln!(
        "Diagnosis: {} ({} issue{})",
        d.overall.label(),
        d.issues.len(),
        if d.issues.len() == 1 { "" } else { "s" }
    );
    for issue in &d.issues {
        eprintln!(
            "  [{}] {}: {}",
            issue.level.label(),
            issue.code,
            issue.message
        );
        eprintln!("       → {}", issue.remediation);
    }
}

/// Print the per-view residual table (top offenders).
///
/// Empty `per_view` (residual extraction failed) prints
/// nothing; the aggregate `mean_reproj_error_px` already
/// printed elsewhere is the headline number.
fn print_per_view_residuals(result: &bris_calibrate::CalibrationResult) {
    if result.per_view.is_empty() {
        return;
    }
    let mut sorted: Vec<&bris_calibrate::ViewResidual> = result.per_view.iter().collect();
    sorted.sort_by(|a, b| {
        b.rms_px
            .partial_cmp(&a.rms_px)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let n_show = sorted.len().min(5);
    eprintln!();
    eprintln!("Per-view residuals (worst {n_show} of {}):", sorted.len());
    for v in sorted.iter().take(n_show) {
        let name = v.source.file_name().map_or_else(
            || v.source.display().to_string(),
            |s| s.to_string_lossy().into_owned(),
        );
        eprintln!(
            "  {:<28} rms={:>6.3} px  max={:>6.3} px  ({} corners)",
            name, v.rms_px, v.max_px, v.n_corners,
        );
    }
    eprintln!(
        "  (median view rms = {:.3} px; consider deleting outliers > 2× median and re-running)",
        median(&result.per_view.iter().map(|v| v.rms_px).collect::<Vec<_>>()),
    );
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if v.is_empty() {
        return f64::NAN;
    }
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}
