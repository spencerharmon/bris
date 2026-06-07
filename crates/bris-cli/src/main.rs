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
mod replay_report;

use anyhow::{bail, Context};
use bris_almanac::{refraction::Atmosphere, Observer};
use bris_bundle::{
    enumerate_frames, verify_first_frame_checksum, ApInput, ApProvenance, BundleManifest,
    CaptureInfo, DeviceInfo, Distortion, FramePathPair, GpsTruth, IntrinsicsRecord,
    SessionKinematics, SessionManifest, UseCaseProfile,
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
use bris_streaming::{
    format_fix_as_nmea, EngineConfig, FixReceiver, PublishedFix, StreamingEngine,
};
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
    Replay(Box<ReplayArgs>),
    /// Manage sessions (capture groupings) on the local corpus.
    ///
    /// Sessions are the operator-facing grouping for captures
    /// (one Start/Stop window each). See
    /// `docs/design/testing_strategy.md` for the model. The
    /// CLI subcommands let a Linux workstation author and
    /// inspect sessions — the same authoring surface the
    /// Android app provides for on-device sessions.
    #[command(subcommand)]
    Session(SessionCommand),
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

#[derive(Debug, clap::Subcommand)]
enum SessionCommand {
    /// Create a new empty session and print its UUID.
    New(SessionNewArgs),
    /// List sessions in the corpus.
    List(SessionListArgs),
    /// Print one session's `session.json`.
    Show(SessionShowArgs),
    /// Attach an existing capture (bundle directory) to a
    /// session. Sets `bundle.session_id` and appends to
    /// `ordered_capture_ids`.
    Attach(SessionAttachArgs),
}

#[derive(Debug, clap::Args)]
struct SessionNewArgs {
    /// Human-readable title (shown in `bris session list`).
    #[arg(long)]
    title: String,
    /// Operator-entered AP at session create: latitude (deg N).
    #[arg(long, allow_hyphen_values = true, requires = "ap_lon")]
    ap_lat: Option<f64>,
    /// Operator-entered AP at session create: longitude (deg E).
    #[arg(long, allow_hyphen_values = true, requires = "ap_lat")]
    ap_lon: Option<f64>,
    /// Eye-height (m) accompanying the AP. Defaults to 2.0.
    #[arg(long)]
    ap_eye_height_m: Option<f64>,
    /// Kinematics: either `stationary` (default) or
    /// `max-speed-kn=<f64>`.
    #[arg(long, default_value = "stationary")]
    kinematics: KinematicsArg,
    /// Override `EngineConfig::sight_window_seconds` for
    /// captures in this session. Defaults to 7200 (2h).
    #[arg(long)]
    sight_retention_seconds: Option<u64>,
    /// Override `EngineConfig::sight_window_capacity` for
    /// captures in this session. Defaults to 50.
    #[arg(long)]
    sight_retention_capacity: Option<u32>,
    /// Use-case classification. Reserved (today only `custom`
    /// is wired); see `docs/design/testing_strategy.md`.
    #[arg(long, default_value = "custom")]
    profile: ProfileArg,
    /// Free-text notes.
    #[arg(long)]
    notes: Option<String>,
    /// Adversarial-corpus flag: "no fix is the correct answer".
    #[arg(long, default_value_t = false)]
    expected_to_fail: bool,
    /// Corpus root. Defaults to `./bris-corpus`.
    #[arg(long)]
    corpus: Option<PathBuf>,
}

#[derive(Debug, Clone)]
enum KinematicsArg {
    Stationary,
    MaxSpeedKn(f64),
}

impl std::str::FromStr for KinematicsArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("stationary") {
            return Ok(Self::Stationary);
        }
        if let Some(rest) = s.strip_prefix("max-speed-kn=") {
            let kn: f64 = rest
                .parse()
                .map_err(|e| format!("max-speed-kn=<f64>: {e}"))?;
            return Ok(Self::MaxSpeedKn(kn));
        }
        Err(format!(
            "expected `stationary` or `max-speed-kn=<f64>`, got `{s}`"
        ))
    }
}

#[derive(Debug, Clone, Copy)]
enum ProfileArg {
    Custom,
    Marine,
    Aeronautical,
    LandBased,
    Urban,
}

impl std::str::FromStr for ProfileArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "custom" => Self::Custom,
            "marine" => Self::Marine,
            "aeronautical" => Self::Aeronautical,
            "land-based" | "land_based" | "landbased" => Self::LandBased,
            "urban" => Self::Urban,
            other => return Err(format!("unknown profile `{other}`")),
        })
    }
}

impl From<ProfileArg> for UseCaseProfile {
    fn from(p: ProfileArg) -> Self {
        match p {
            ProfileArg::Custom => Self::Custom,
            ProfileArg::Marine => Self::Marine,
            ProfileArg::Aeronautical => Self::Aeronautical,
            ProfileArg::LandBased => Self::LandBased,
            ProfileArg::Urban => Self::Urban,
        }
    }
}

#[derive(Debug, clap::Args)]
struct SessionListArgs {
    /// Corpus root. Defaults to `./bris-corpus`.
    #[arg(long)]
    corpus: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct SessionShowArgs {
    /// Session UUID.
    session: uuid::Uuid,
    /// Corpus root. Defaults to `./bris-corpus`.
    #[arg(long)]
    corpus: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct SessionAttachArgs {
    /// Session UUID.
    #[arg(long)]
    session: uuid::Uuid,
    /// Path to the capture bundle directory (contains
    /// `bundle.json`). Will be moved under
    /// `<corpus>/sessions/<UUID>/captures/<bundle_id>/` and
    /// its `bundle.json` rewritten with the session back-ref.
    #[arg(long)]
    bundle: PathBuf,
    /// Corpus root. Defaults to `./bris-corpus`.
    #[arg(long)]
    corpus: Option<PathBuf>,
    /// Do not move the bundle; only set the back-reference
    /// and append to `ordered_capture_ids` (operator already
    /// arranged the directory).
    #[arg(long, default_value_t = false)]
    in_place: bool,
}

#[derive(Debug, Clone, clap::Args)]
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
    /// Replay every capture under
    /// `<corpus>/sessions/<UUID>/captures/` in chronological
    /// order, sharing one engine across the whole session.
    /// Matches the APK's `SessionHolder` lifetime (engine
    /// constructed when active session is acquired; reused
    /// across capture start/stop cycles). The cross-capture
    /// `SightWindow`, cold-start state, and last-published-
    /// fix continuity that the engine provides are what
    /// make a multi-capture fix possible.
    #[arg(long, conflicts_with_all = ["bundle", "frames"])]
    session: Option<uuid::Uuid>,
    /// Corpus root for `--session`. Defaults to `./bris-corpus`.
    #[arg(long)]
    corpus: Option<PathBuf>,

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
    /// Enable the ML-gravity horizon provider for this replay.
    /// Requires `--ml-gravity-model` to point at a heteroscedastic
    /// ONNX file. See `docs/design/ml_gravity.md`.
    #[arg(long)]
    ml_gravity: bool,
    /// Path to the ML-gravity ONNX model. Implies
    /// `--ml-gravity` even when that flag is not also passed.
    #[arg(long)]
    ml_gravity_model: Option<PathBuf>,
    /// Comma-separated subset of horizon providers to dispatch.
    /// Names: gradient, sky-region, night, night-textured,
    /// segmentation, reflection-pair, vertical-line,
    /// vanishing-point, ml-gravity. Default: all (preserves
    /// the current dispatch). Use `--horizon-providers
    /// ml-gravity` to inspect a single provider's hypothesis
    /// in the replay report without interference from the
    /// others winning Stage C fusion.
    #[arg(long, value_delimiter = ',')]
    horizon_providers: Option<Vec<String>>,
    /// Override publication-gate `max_position_sigma_nm`
    /// (default 50.0). Use `inf` to disable; large values
    /// permit a 'rough fix at honest σ' diagnostic on
    /// adversarial corpora.
    #[arg(long)]
    max_position_sigma_nm: Option<f64>,
    /// Override publication-gate `min_azimuth_spread_rad`
    /// (default 30° = 0.524 rad). 0 disables.
    #[arg(long)]
    min_azimuth_spread_rad: Option<f64>,
    /// Override publication-gate `max_ellipse_axis_ratio`
    /// (default 10.0). `inf` disables.
    #[arg(long)]
    max_ellipse_axis_ratio: Option<f64>,
    /// Cold-start hemisphere hint. Resolves the two-candidate
    /// ambiguity inherent in two-sight cold-start `CoP`
    /// intersections (the candidates are mirror-symmetric
    /// about the great-circle joining the two sub-points).
    /// Without a hint cold-start refuses to publish; setting
    /// `north` or `south` picks the candidate on that side
    /// of the equator. Honest only when the operator actually
    /// knows which hemisphere they're in.
    #[arg(long, value_parser = ["north", "south"])]
    coarse_hemisphere: Option<String>,
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
    /// Render an annotated PNG next to each input frame and
    /// emit a per-session `bris-replay-report.json` (and, when
    /// `--all-sessions` is in use, a corpus-root
    /// `index.json`). Slow on large corpora.
    #[arg(long)]
    render_frames: bool,
    /// Stage D dispatch policy override. Default unset =
    /// inherit `EngineConfig` default (`when-stars-expected`).
    /// `always` recovers the pre-gate behaviour (run Stage D
    /// on every Night-classified frame regardless of peak
    /// count); `never` disables Stage D entirely. See
    /// `docs/design/pipeline.md` §Stage D dispatch.
    #[arg(long, value_parser = ["always", "when-stars-expected", "never"])]
    stage_d_dispatch: Option<String>,
    /// Replay every session under `<corpus>/sessions/`. Each
    /// session is processed end-to-end; rendering and report
    /// generation are governed by `--render-frames`.
    #[arg(
        long,
        conflicts_with_all = ["bundle", "frames", "session"]
    )]
    all_sessions: bool,
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
        Command::Session(cmd) => run_session(cmd),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// Populated when `--render-frames` is on. One entry per
    /// processed frame, plus the running rejection histogram.
    /// `None` when rendering was off (no work done; no report).
    render: Option<RenderRunOutput>,
}

/// Per-mode rendering output: per-frame records and a
/// Stage E rejection histogram aggregated across the run.
#[derive(Debug, Default)]
struct RenderRunOutput {
    frames: Vec<replay_report::FrameReport>,
    rejection_counts: std::collections::BTreeMap<String, u64>,
}

/// Build a per-frame replay report and write the annotated
/// PNG.
///
/// `pgm_path` and `render_path` in the returned report are
/// **bundle-relative** strings; the session-level writer
/// promotes them to corpus-relative paths if needed.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_one_frame(
    frame: &bris_vision::Frame,
    pair: &FramePathPair,
    seq: usize,
    captured_utc: chrono::DateTime<Utc>,
    diag: &bris_streaming::EngineDiagnostics,
    bundle_dir: &Path,
    capture_id_short: &str,
    session_id_short: &str,
) -> anyhow::Result<replay_report::FrameReport> {
    use bris_streaming::StageEOutcomeSnapshot;

    let classification_label = diag
        .last_raw_classification
        .map_or_else(|| "unknown".into(), |c| format!("{c:?}"));

    let stem = pair
        .pgm
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("frame");
    let render_filename = format!("{stem}-render.png");
    let render_path_abs = pair.pgm.parent().map_or_else(
        || std::path::PathBuf::from(&render_filename),
        |d| d.join(&render_filename),
    );

    // Idempotent: skip the PNG encode when the cached base
    // image already exists. Multi-mode replays therefore pay
    // the per-frame PNG cost exactly once across all modes
    // (the overlay is rendered SVG-on-image client-side in
    // the corpus explorer, driven by the JSON we emit below).
    let metadata = if render_path_abs.exists() {
        // Re-derive metadata from the source frame so the
        // JSON stays correct; the file itself isn't re-encoded.
        let (out_w, out_h, scale) = bris_vision_canvas_dims(frame.width(), frame.height());
        bris_vision::RenderMetadata {
            source_width: frame.width(),
            source_height: frame.height(),
            canvas_width: out_w,
            canvas_height: out_h,
            scale,
        }
    } else {
        bris_vision::render_base_image(frame, &render_path_abs).context("render base image PNG")?
    };
    let _ = (capture_id_short, session_id_short, captured_utc);

    let pgm_rel = path_relative_to(&pair.pgm, bundle_dir);
    let render_rel = path_relative_to(&render_path_abs, bundle_dir);

    let horizon_report = diag.last_horizon_hypothesis.map(|h| {
        let model_id = match diag.last_horizon_provenance {
            Some(bris_vision::HorizonProvenance::MlGravity { model_id, .. }) => {
                Some(model_id.to_string())
            }
            _ => None,
        };
        replay_report::HorizonReport {
            provider: h.provider.to_string(),
            intercept_px: h.intercept,
            slope: h.slope,
            sigma_rad: h.altitude_sigma_rad,
            model_id,
        }
    });
    let centroid_report = diag
        .last_body_centroid
        .map(|c| replay_report::BodyCentroidReport {
            x: c.x,
            y: c.y,
            sigma_px: c.sigma_px,
            area_px: c.area_px,
            secondaries: c.secondaries,
        });
    let stage_e_report: Vec<replay_report::StageEAttemptReport> = diag
        .last_stage_e_outcomes
        .iter()
        .map(|o| match o {
            StageEOutcomeSnapshot::Ok {
                altitude_rad,
                sigma_rad,
            } => replay_report::StageEAttemptReport::Ok {
                altitude_rad: *altitude_rad,
                sigma_rad: *sigma_rad,
            },
            StageEOutcomeSnapshot::Err { kind } => replay_report::StageEAttemptReport::Err {
                error: kind.clone(),
            },
        })
        .collect();
    let sight_emitted = stage_e_report
        .iter()
        .any(|o| matches!(o, replay_report::StageEAttemptReport::Ok { .. }));

    Ok(replay_report::FrameReport {
        #[allow(clippy::cast_possible_truncation)]
        seq: seq as u32,
        captured_unix_ms: pair.sidecar_data.captured_unix_ms,
        render_path: Some(render_rel),
        pgm_path: pgm_rel,
        render_geometry: Some(replay_report::RenderGeometry {
            source_width: metadata.source_width,
            source_height: metadata.source_height,
            canvas_width: metadata.canvas_width,
            canvas_height: metadata.canvas_height,
            scale: metadata.scale,
        }),
        classification: classification_label,
        horizon: horizon_report,
        body_centroid: centroid_report,
        stage_e_outcomes: stage_e_report,
        sight_emitted,
    })
}

/// Mirror of `bris_vision`'s internal `scaled_dims`. Used when
/// the cached base PNG already exists and we want the metadata
/// without re-encoding.
fn bris_vision_canvas_dims(src_w: u32, src_h: u32) -> (u32, u32, f64) {
    let max_side = bris_vision::RENDER_MAX_SIDE_PX;
    let long = src_w.max(src_h).max(1);
    if long <= max_side {
        return (src_w, src_h, 1.0);
    }
    let s = f64::from(max_side) / f64::from(long);
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let out_w = ((f64::from(src_w) * s).round() as u32).max(1);
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let out_h = ((f64::from(src_h) * s).round() as u32).max(1);
    (out_w, out_h, s)
}

/// String path of `target` relative to `base`; falls back to
/// the absolute path when `target` is not a descendant of
/// `base`.
fn path_relative_to(target: &Path, base: &Path) -> String {
    target.strip_prefix(base).ok().map_or_else(
        || target.to_string_lossy().to_string(),
        |p| p.to_string_lossy().to_string(),
    )
}

fn run_replay(args: &ReplayArgs) -> anyhow::Result<()> {
    if args.all_sessions {
        return run_replay_all_sessions(args);
    }
    if let Some(session_id) = args.session {
        return run_replay_session(args, session_id);
    }
    let (_capture_report, _) = run_replay_capture(args)?;
    Ok(())
}

/// Run replay over one bundle (one capture). Returns the per-
/// capture report (when `--render-frames` was on) and the
/// loaded `BundleManifest` so callers can stitch reports
/// together at the session level.
fn run_replay_capture(
    args: &ReplayArgs,
) -> anyhow::Result<(Option<replay_report::CaptureReport>, BundleManifest)> {
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
        let bundle_dir_arg = args.bundle.as_deref();
        let result = run_one_mode(mode, args, &manifest, bundle_dir_arg, &frames)?;
        log_mode_result(&result, &manifest);
        results.push(result);
    }

    // 6. Summary table for --all-modes.
    if args.all_modes {
        print_summary(&results, &manifest);
    }

    // 7. Build the per-capture report from the Default-mode
    //    run (if rendering was on for that mode).
    let capture_report = build_capture_report(&results, &manifest, frames.len());
    Ok((capture_report, manifest))
}

/// Pick the Default-mode result (if present) and build a
/// per-capture report from it.
fn build_capture_report(
    results: &[ModeResult],
    manifest: &BundleManifest,
    frame_count: usize,
) -> Option<replay_report::CaptureReport> {
    let default_result = results.iter().find(|r| r.mode == ReplayMode::Default)?;
    let render = default_result.render.as_ref()?;
    Some(replay_report::CaptureReport {
        capture_id: manifest.bundle_id.clone(),
        bundle_dir: format!("captures/{}/", manifest.bundle_id),
        app_version: manifest.device.app_version.clone(),
        #[allow(clippy::cast_possible_truncation)]
        frame_count: frame_count as u32,
        frames_pushed: default_result.frames_pushed,
        fixes_published: default_result.fixes.len() as u64,
        sights_inserted_total: render.frames.iter().filter(|f| f.sight_emitted).count() as u64,
        stage_e_rejection_counts: render.rejection_counts.clone(),
        frames: render
            .frames
            .iter()
            .cloned()
            .map(|mut f| {
                // Promote the bundle-relative paths recorded
                // in `render_one_frame` to session-relative
                // paths.
                f.render_path = f
                    .render_path
                    .map(|p| format!("captures/{}/{p}", manifest.bundle_id));
                f.pgm_path = format!("captures/{}/{}", manifest.bundle_id, f.pgm_path);
                f
            })
            .collect(),
        fixes: default_result
            .fixes
            .iter()
            .map(|pf| published_fix_to_report(pf, manifest.gps_truth.as_ref()))
            .collect(),
    })
}

/// Resolve a `BundleManifest` for the run, returning it plus the
/// directory it lives in. For `--frames` the manifest is
/// synthesized from CLI flags only.
/// Convert a [`bris_streaming::PublishedFix`] to the wire-
/// shape used in the corpus replay report.
///
/// `gps_truth`, when present, lets the explorer compare each
/// published fix to a recorded ground truth without
/// re-deriving the great-circle math client-side.
fn published_fix_to_report(
    pf: &bris_streaming::PublishedFix,
    gps_truth: Option<&bris_bundle::GpsTruth>,
) -> replay_report::PublishedFixReport {
    let lat = pf.fix.lat.degrees();
    let lon = pf.fix.lon.degrees();
    let (err_nm, brg_deg) = if let Some(gt) = gps_truth {
        let (nm, brg) = great_circle_nm_and_bearing(lat, lon, gt.lat, gt.lon);
        (Some(nm), Some(brg))
    } else {
        (None, None)
    };
    replay_report::PublishedFixReport {
        timestamp_unix_ms: tt_to_unix_ms(pf.timestamp),
        lat_deg: lat,
        lon_deg: lon,
        sigma_major_nm: pf.fix.sigma_major_nm,
        sigma_minor_nm: pf.fix.sigma_minor_nm,
        orientation_rad: pf.fix.orientation_rad,
        sight_count: pf.fix.sight_count,
        chi_square: pf.fix.chi_square,
        gps_truth_error_nm: err_nm,
        gps_truth_bearing_deg: brg_deg,
    }
}

/// Approximate TT (Terrestrial Time) -> Unix-ms conversion
/// for display purposes. TT = TAI + 32.184 s; TAI - UTC is
/// the integer leap-second offset (37 s in 2026; bumps only
/// on rare announced leap-second days). We use a constant
/// 69.184 s offset here — honest for any time after
/// 2017-01-01 and good enough for chartplotter-grade
/// timestamps. The engine's authoritative time math stays
/// in Tt; this is purely a display path.
fn tt_to_unix_ms(tt: bris_core::time::Tt) -> i64 {
    const JD_UNIX_EPOCH: f64 = 2_440_587.5;
    const TT_MINUS_UTC_SECONDS: f64 = 69.184;
    let jd = tt.julian_date();
    let unix_secs_tt = (jd - JD_UNIX_EPOCH) * 86_400.0;
    let unix_secs_utc = unix_secs_tt - TT_MINUS_UTC_SECONDS;
    #[allow(clippy::cast_possible_truncation)]
    let ms = (unix_secs_utc * 1000.0).round() as i64;
    ms
}

/// Replay every capture in a session, chronological order.
///
/// All captures within one mode share a single engine
/// instance — matching what the APK does in production via
/// `SessionHolder` (one engine per active session UUID; held
/// across capture start/stop cycles). The cross-capture
/// `SightWindow`, cold-start state, and `last_published_fix`
/// continuity that the engine provides are what make a fix
/// possible across captures separated by tens of minutes.
#[allow(clippy::too_many_lines, clippy::items_after_statements)]
fn run_replay_session(args: &ReplayArgs, session_id: uuid::Uuid) -> anyhow::Result<()> {
    let corpus = args
        .corpus
        .clone()
        .unwrap_or_else(|| PathBuf::from("./bris-corpus"));
    let session_dir = corpus.join("sessions").join(session_id.to_string());
    let session = SessionManifest::load_from_dir(&session_dir)
        .with_context(|| format!("load session.json from {}", session_dir.display()))?;
    if session.ordered_capture_ids.is_empty() {
        bail!("session {session_id} has no captures yet (ordered_capture_ids is empty)");
    }
    info!(
        session_id = %session.session_id,
        captures = session.ordered_capture_ids.len(),
        "replay: session resolved"
    );

    // Resolve every capture's manifest + frame list up front
    // so the mode loop iterates already-validated inputs.
    struct ResolvedCapture {
        manifest: BundleManifest,
        bundle_dir: PathBuf,
        frames: Vec<FramePathPair>,
    }
    let mut resolved: Vec<ResolvedCapture> = Vec::new();
    for cap_id in &session.ordered_capture_ids {
        let bundle_dir = session_dir.join("captures").join(cap_id);
        if !bundle_dir.join("bundle.json").exists() {
            warn!(
                bundle = %bundle_dir.display(),
                "replay: capture missing bundle.json; skipping"
            );
            continue;
        }
        let mut capture_args = args.clone();
        capture_args.session = None;
        capture_args.all_sessions = false;
        capture_args.bundle = Some(bundle_dir.clone());
        let (mut manifest, _) = resolve_manifest(&capture_args)?;
        apply_cli_overrides(&mut manifest, &capture_args)?;
        if manifest.capture.first_frame_blake3.is_some() {
            verify_first_frame_checksum(&manifest, &bundle_dir).with_context(|| {
                format!(
                    "first-frame checksum verification ({})",
                    bundle_dir.display()
                )
            })?;
        }
        let frames = enumerate_frames(&bundle_dir)
            .with_context(|| format!("enumerate bundle frames at {}", bundle_dir.display()))?;
        if frames.is_empty() {
            warn!(bundle = %bundle_dir.display(), "replay: bundle has no frames; skipping");
            continue;
        }
        resolved.push(ResolvedCapture {
            manifest,
            bundle_dir,
            frames,
        });
    }
    if resolved.is_empty() {
        bail!("session {session_id} has no replayable captures");
    }

    // Pick the mode set from the first capture's manifest —
    // `--all-modes` requires gps_truth somewhere, but per-
    // capture mode selection is no longer coherent (one
    // engine per session-mode pair).
    let modes = select_modes(args, &resolved[0].manifest);
    if modes.is_empty() {
        bail!("no replay modes selected");
    }

    // For each mode, build one engine, feed every capture's
    // frames through it in chronological order, accumulate
    // per-capture reports.
    let mut per_mode_capture_reports: std::collections::BTreeMap<
        ReplayMode,
        Vec<replay_report::CaptureReport>,
    > = std::collections::BTreeMap::new();
    let mut per_mode_fix_total: std::collections::BTreeMap<ReplayMode, u64> =
        std::collections::BTreeMap::new();

    for mode in &modes {
        let mode = *mode;
        info!(mode = mode.label(), "replay: running mode (session)");
        // AP comes from the first capture's manifest. All
        // captures within a session share AP semantics by
        // design (the operator sets AP once at session
        // create); per-capture AP overrides aren't honoured
        // in the session-engine path.
        let ap = resolve_ap(mode, &resolved[0].manifest);
        let cfg = build_engine_config(
            mode,
            ap,
            &resolved[0].manifest,
            Some(&resolved[0].bundle_dir),
            args,
        )?;
        let engine = Arc::new(StreamingEngine::new(cfg));
        let fix_rx = engine
            .fix_stream()
            .map_err(|e| anyhow::anyhow!("fix_stream: {e}"))?;
        let mut session_fixes: Vec<PublishedFix> = Vec::new();
        let mut capture_reports: Vec<replay_report::CaptureReport> = Vec::new();
        for cap in &resolved {
            info!(
                capture_id = %cap.manifest.bundle_id,
                mode = mode.label(),
                "replay: feeding capture into session engine"
            );
            // Snapshot fix count + push frames; the delta is
            // attributed to this capture in the report.
            let pre_fix_count = session_fixes.len();
            let pre_frames_pushed = engine.diagnostics().frames_pushed;
            let render = feed_capture_through_engine(
                engine.clone(),
                &fix_rx,
                &mut session_fixes,
                &cap.manifest,
                Some(&cap.bundle_dir),
                &cap.frames,
                args,
                mode,
            )?;
            let capture_fix_count = session_fixes.len() - pre_fix_count;
            let capture_fixes_slice = &session_fixes[pre_fix_count..];
            let capture_frames_pushed = engine.diagnostics().frames_pushed - pre_frames_pushed;
            if let Some(render) = render {
                let report = replay_report::CaptureReport {
                    capture_id: cap.manifest.bundle_id.clone(),
                    bundle_dir: format!("captures/{}/", cap.manifest.bundle_id),
                    app_version: cap.manifest.device.app_version.clone(),
                    #[allow(clippy::cast_possible_truncation)]
                    frame_count: cap.frames.len() as u32,
                    frames_pushed: capture_frames_pushed,
                    fixes_published: capture_fix_count as u64,
                    sights_inserted_total: render.frames.iter().filter(|f| f.sight_emitted).count()
                        as u64,
                    stage_e_rejection_counts: render.rejection_counts.clone(),
                    frames: render
                        .frames
                        .into_iter()
                        .map(|mut f| {
                            f.render_path = f
                                .render_path
                                .map(|p| format!("captures/{}/{p}", cap.manifest.bundle_id));
                            f.pgm_path =
                                format!("captures/{}/{}", cap.manifest.bundle_id, f.pgm_path);
                            f
                        })
                        .collect(),
                    fixes: capture_fixes_slice
                        .iter()
                        .map(|pf| published_fix_to_report(pf, cap.manifest.gps_truth.as_ref()))
                        .collect(),
                };
                capture_reports.push(report);
            }
        }
        let diag = engine.diagnostics();
        info!(
            mode = mode.label(),
            captures = resolved.len(),
            session_fixes = session_fixes.len(),
            sight_window_depth = diag.sight_window_depth,
            fixes_published_total = diag.fixes_published_total,
            cold_start_attempts = diag.cold_start_attempts,
            cold_start_published = diag.cold_start_published,
            publication_gate_rejections = diag.publication_gate_rejections,
            "replay: session-engine mode complete"
        );
        for fix in &session_fixes {
            info!(
                mode = mode.label(),
                lat_deg = fix.fix.lat.degrees(),
                lon_deg = fix.fix.lon.degrees(),
                sigma_major_nm = fix.fix.sigma_major_nm,
                sigma_minor_nm = fix.fix.sigma_minor_nm,
                "replay: published_fix (session-engine)"
            );
        }
        per_mode_fix_total.insert(mode, session_fixes.len() as u64);
        per_mode_capture_reports.insert(mode, capture_reports);
    }

    if args.render_frames {
        // Choose which mode's reports get written. Prefer
        // Default; fall back to the first mode in the
        // dispatched list (e.g. --ap-seed-truth alone has
        // no Default to fall back to).
        let chosen_mode = if per_mode_capture_reports.contains_key(&ReplayMode::Default) {
            ReplayMode::Default
        } else {
            modes[0]
        };
        if let Some(reports) = per_mode_capture_reports.remove(&chosen_mode) {
            let report = replay_report::ReplaySessionReport {
                schema_version: replay_report::SCHEMA_VERSION,
                session_id: session.session_id.to_string(),
                session_title: session.title.clone(),
                generated_unix_ms: chrono::Utc::now().timestamp_millis(),
                engine_build: replay_report::EngineBuild::current(),
                captures: reports,
            };
            let path = replay_report::write_session_report(&session_dir, &report)
                .with_context(|| format!("write session report to {}", session_dir.display()))?;
            info!(report = %path.display(), chosen_mode = chosen_mode.label(), "replay: session report written");
        }
    }
    Ok(())
}

/// Replay every session under `<corpus>/sessions/`.
fn run_replay_all_sessions(args: &ReplayArgs) -> anyhow::Result<()> {
    let corpus = args
        .corpus
        .clone()
        .unwrap_or_else(|| PathBuf::from("./bris-corpus"));
    let sessions_root = corpus.join("sessions");
    let entries = fs::read_dir(&sessions_root)
        .with_context(|| format!("read sessions root {}", sessions_root.display()))?;
    let mut session_ids: Vec<uuid::Uuid> = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Ok(id) = uuid::Uuid::parse_str(&name) {
            if entry.path().join("session.json").exists() {
                session_ids.push(id);
            }
        }
    }
    session_ids.sort();
    info!(
        count = session_ids.len(),
        "replay --all-sessions: enumerated sessions"
    );
    let mut index_entries: Vec<replay_report::CorpusIndexEntry> = Vec::new();
    for id in &session_ids {
        let mut per_session_args = args.clone();
        per_session_args.all_sessions = false;
        per_session_args.session = Some(*id);
        per_session_args.corpus = Some(corpus.clone());
        if let Err(e) = run_replay_session(&per_session_args, *id) {
            warn!(session_id = %id, error = ?e, "replay --all-sessions: session failed; continuing");
            continue;
        }
        let session_dir = corpus.join("sessions").join(id.to_string());
        let report_path = session_dir.join(replay_report::SESSION_REPORT_FILENAME);
        if args.render_frames && report_path.exists() {
            let session = SessionManifest::load_from_dir(&session_dir).ok();
            let bytes = fs::read(&report_path).ok();
            let parsed: Option<replay_report::ReplaySessionReport> =
                bytes.and_then(|b| serde_json::from_slice(&b).ok());
            #[allow(clippy::cast_possible_truncation)]
            let capture_count = parsed.as_ref().map_or(0, |r| r.captures.len() as u32);
            index_entries.push(replay_report::CorpusIndexEntry {
                session_id: id.to_string(),
                session_title: session.map_or_else(|| id.to_string(), |s| s.title),
                report_path: format!("sessions/{id}/{}", replay_report::SESSION_REPORT_FILENAME),
                capture_count,
            });
        }
    }
    if args.render_frames {
        let index = replay_report::CorpusIndex {
            schema_version: replay_report::SCHEMA_VERSION,
            generated_unix_ms: chrono::Utc::now().timestamp_millis(),
            sessions: index_entries,
        };
        let path = replay_report::write_corpus_index(&corpus, &index)
            .with_context(|| format!("write corpus index to {}", corpus.display()))?;
        info!(index = %path.display(), "replay --all-sessions: corpus index written");
    }
    Ok(())
}

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
        build: None,
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
        session_id: None,
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

/// Locate the `SessionManifest` for a capture and overlay its
/// engine-relevant fields onto `cfg`. Looks for `session.json`
/// two directories above `bundle_dir`. No-op when missing.
fn apply_session_overlay(
    cfg: &mut EngineConfig,
    manifest: &BundleManifest,
    bundle_dir: Option<&Path>,
) {
    let Some(bundle_dir) = bundle_dir else {
        return;
    };
    let Some(session_dir) = bundle_dir.parent().and_then(Path::parent) else {
        return;
    };
    if !session_dir.join("session.json").exists() {
        return;
    }
    let session = match SessionManifest::load_from_dir(session_dir) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = ?e, dir = %session_dir.display(), "replay: failed to load session.json; ignoring overlay");
            return;
        }
    };
    if let Some(bundle_session_id) = manifest.session_id {
        if bundle_session_id != session.session_id {
            warn!(
                bundle_session_id = %bundle_session_id,
                disk_session_id = %session.session_id,
                "replay: bundle.session_id disagrees with session.json on disk; using session.json anyway"
            );
        }
    }
    #[allow(clippy::cast_precision_loss)]
    {
        cfg.sight_window_seconds = session.sight_retention_seconds as f64;
    }
    cfg.sight_window_capacity = session.sight_retention_capacity as usize;
    cfg.publication_gate.assumed_max_speed_kn = match session.kinematics {
        SessionKinematics::Stationary => 0.0,
        SessionKinematics::MaxSpeedKn { kn } => kn,
    };
    info!(
        session_id = %session.session_id,
        sight_window_seconds = cfg.sight_window_seconds,
        sight_window_capacity = cfg.sight_window_capacity,
        assumed_max_speed_kn = cfg.publication_gate.assumed_max_speed_kn,
        "replay: applied session.json overlay"
    );
}

fn build_engine_config(
    mode: ReplayMode,
    ap: Option<ResolvedAp>,
    manifest: &BundleManifest,
    bundle_dir: Option<&Path>,
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
    apply_session_overlay(&mut cfg, manifest, bundle_dir);
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
    let ml_gravity_path = args.ml_gravity_model.clone().or_else(|| {
        let p = std::path::PathBuf::from("data/ml-gravity/geocalib-heteroscedastic-v1.onnx");
        p.exists().then_some(p)
    });
    if args.ml_gravity || args.ml_gravity_model.is_some() {
        cfg.enable_ml_gravity = true;
    }
    cfg.ml_gravity_model_path = ml_gravity_path;
    if let Some(names) = args.horizon_providers.as_ref() {
        cfg.horizon_provider_set = parse_horizon_provider_set(names).map_err(anyhow::Error::msg)?;
    }
    if let Some(v) = args.max_position_sigma_nm {
        cfg.publication_gate.max_position_sigma_nm = v;
    }
    if let Some(v) = args.min_azimuth_spread_rad {
        cfg.publication_gate.min_azimuth_spread_rad = v;
    }
    if let Some(v) = args.max_ellipse_axis_ratio {
        cfg.publication_gate.max_ellipse_axis_ratio = v;
    }
    if let Some(p) = args.stage_d_dispatch.as_deref() {
        cfg.stage_d_dispatch_policy = match p {
            "always" => bris_streaming::StageDDispatchPolicy::Always,
            "when-stars-expected" => bris_streaming::StageDDispatchPolicy::WhenStarsExpected,
            "never" => bris_streaming::StageDDispatchPolicy::Never,
            _ => unreachable!("clap value_parser restricts the set"),
        };
    }
    if let Some(h) = args.coarse_hemisphere.as_deref() {
        cfg.cold_start.coarse_hemisphere = Some(match h {
            "north" => bris_core::Hemisphere::North,
            "south" => bris_core::Hemisphere::South,
            _ => unreachable!("clap value_parser restricts to north|south"),
        });
    }
    Ok(cfg)
}

/// Parse a list of provider names (`gradient`, `night`, ...)
/// into a [`HorizonProviderSet`] with only those entries on.
/// Unknown names are rejected; an empty list is rejected.
fn parse_horizon_provider_set(
    names: &[String],
) -> Result<bris_streaming::HorizonProviderSet, String> {
    let mut set = bris_streaming::HorizonProviderSet::none();
    let mut any = false;
    for raw in names {
        let name = raw.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        match name.as_str() {
            "gradient" => set.gradient = true,
            "sky-region" | "sky_region" | "skyregion" => set.sky_region = true,
            "night" | "night-gradient" => set.night = true,
            "night-textured" | "night_textured" => set.night_textured = true,
            "segmentation" => set.segmentation = true,
            "reflection-pair" | "reflection_pair" => set.reflection_pair = true,
            "vertical-line" | "vertical_line" => set.vertical_line = true,
            "vanishing-point" | "vanishing_point" => set.vanishing_point = true,
            "ml-gravity" | "ml_gravity" => set.ml_gravity = true,
            other => return Err(format!("unknown horizon provider name: {other}")),
        }
        any = true;
    }
    if !any {
        return Err("--horizon-providers must list at least one provider".into());
    }
    Ok(set)
}

#[allow(clippy::too_many_lines)]
fn run_one_mode(
    mode: ReplayMode,
    args: &ReplayArgs,
    manifest: &BundleManifest,
    bundle_dir: Option<&Path>,
    frames: &[FramePathPair],
) -> anyhow::Result<ModeResult> {
    let ap = resolve_ap(mode, manifest);
    let cfg = build_engine_config(mode, ap, manifest, bundle_dir, args)?;
    let engine = Arc::new(StreamingEngine::new(cfg));
    let fix_rx = engine
        .fix_stream()
        .map_err(|e| anyhow::anyhow!("fix_stream: {e}"))?;
    let mut collected: Vec<PublishedFix> = Vec::new();
    let render = feed_capture_through_engine(
        engine.clone(),
        &fix_rx,
        &mut collected,
        manifest,
        bundle_dir,
        frames,
        args,
        mode,
    )?;
    let diag = engine.diagnostics();
    let mode_label = mode.label().to_string();
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
        frames_pushed: diag.frames_pushed,
        render,
    })
}

/// Feed one capture's frames through an existing engine,
/// optionally rendering per-frame overlays + collecting
/// published fixes onto `collected`.
///
/// The engine is kept alive across the call; the caller may
/// reuse it to feed another capture in the same session
/// (preserving the engine's `SightWindow`, cold-start state,
/// and `last_published_fix` across captures — matching what
/// the APK does in production).
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::needless_pass_by_value
)]
fn feed_capture_through_engine(
    engine: Arc<StreamingEngine>,
    fix_rx: &FixReceiver,
    collected: &mut Vec<PublishedFix>,
    manifest: &BundleManifest,
    bundle_dir: Option<&Path>,
    frames: &[FramePathPair],
    args: &ReplayArgs,
    mode: ReplayMode,
) -> anyhow::Result<Option<RenderRunOutput>> {
    let intrinsics = intrinsics_from_record(&manifest.intrinsics);
    let rotation = rotation_from_degrees(manifest.capture.source_rotation_deg)?;
    let frames_owned: Vec<FramePathPair> = frames.to_vec();
    let engine_feed = engine.clone();
    let mode_label = mode.label().to_string();
    let render_enabled = args.render_frames && bundle_dir.is_some();
    let render_state: Option<Arc<std::sync::Mutex<RenderRunOutput>>> = if render_enabled {
        Some(Arc::new(std::sync::Mutex::new(RenderRunOutput::default())))
    } else {
        None
    };
    let render_state_feed = render_state.clone();
    let bundle_dir_owned = bundle_dir.map(Path::to_path_buf);
    let session_id_short = manifest
        .session_id
        .map(|s| s.to_string().chars().take(8).collect::<String>())
        .unwrap_or_default();
    let capture_id_short = manifest.bundle_id.chars().take(8).collect::<String>();
    let engine_diag_handle = engine.clone();
    let nmea_stdout = args.nmea_stdout;
    let feeder = std::thread::Builder::new()
        .name(format!("bris-replay-feed-{mode_label}"))
        .spawn(move || -> anyhow::Result<()> {
            for (idx, pair) in frames_owned.iter().enumerate() {
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
                let frame = if let Some([gx, gy, gz]) = s.gravity_camera_frame {
                    frame.with_gravity_camera_frame((gx, gy, gz))
                } else {
                    frame
                };
                let frame_for_render = if render_state_feed.is_some() {
                    Some(frame.clone())
                } else {
                    None
                };
                if let Err(e) = engine_feed.push_frame(frame) {
                    warn!(error = ?e, frame = %pair.pgm.display(), "replay: push_frame failed");
                    continue;
                }
                if let (Some(state), Some(frame), Some(bundle_dir)) = (
                    render_state_feed.as_ref(),
                    frame_for_render,
                    bundle_dir_owned.as_deref(),
                ) {
                    let diag = engine_diag_handle.diagnostics();
                    let report = render_one_frame(
                        &frame,
                        pair,
                        idx,
                        utc,
                        &diag,
                        bundle_dir,
                        &capture_id_short,
                        &session_id_short,
                    );
                    if let Ok(frame_report) = report {
                        let mut s = state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        for outcome in &frame_report.stage_e_outcomes {
                            if let replay_report::StageEAttemptReport::Err { error } = outcome {
                                *s.rejection_counts.entry(error.clone()).or_insert(0) += 1;
                            }
                        }
                        s.frames.push(frame_report);
                    }
                }
            }
            Ok(())
        })
        .context("spawn replay feeder thread")?;

    // Drain published fixes during + after the feed.
    loop {
        match fix_rx.try_recv() {
            Ok(Some(fix)) => {
                if nmea_stdout {
                    let s = format_fix_as_nmea(&fix, Utc::now(), QualityThresholds::default());
                    print!("[mode={mode_label}] {s}");
                }
                collected.push(fix);
            }
            Ok(None) => {
                if feeder.is_finished() {
                    while let Ok(Some(f)) = fix_rx.try_recv() {
                        if nmea_stdout {
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
    feeder
        .join()
        .map_err(|_| anyhow::anyhow!("feeder thread panicked"))??;
    Ok(render_state.map(|m| {
        Arc::try_unwrap(m)
            .map(|m| {
                m.into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .unwrap_or_default()
    }))
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

// -------------------------------------------------------------
// `bris session` subcommand
// -------------------------------------------------------------

fn default_corpus_root() -> PathBuf {
    PathBuf::from("./bris-corpus")
}

fn run_session(cmd: SessionCommand) -> anyhow::Result<()> {
    match cmd {
        SessionCommand::New(args) => run_session_new(args),
        SessionCommand::List(args) => run_session_list(args),
        SessionCommand::Show(args) => run_session_show(args),
        SessionCommand::Attach(args) => run_session_attach(args),
    }
}

fn run_session_new(args: SessionNewArgs) -> anyhow::Result<()> {
    let corpus = args.corpus.unwrap_or_else(default_corpus_root);
    let session_id = uuid::Uuid::new_v4();
    let session_dir = corpus.join("sessions").join(session_id.to_string());
    if session_dir.exists() {
        bail!(
            "session directory already exists (collision?): {}",
            session_dir.display()
        );
    }
    let created_unix_ms = chrono::Utc::now().timestamp_millis();
    // bris-cli is the writing device for this session. App
    // version is the bris-cli crate semver; OS is "linux" since
    // bris-capture only supports Linux today.
    let device = DeviceInfo {
        model: format!("bris-cli ({})", std::env::consts::ARCH),
        os: Some(std::env::consts::OS.into()),
        app_version: Some(env!("CARGO_PKG_VERSION").into()),
    };
    let mut s = SessionManifest::new(session_id, args.title, device, created_unix_ms);
    s.notes = args.notes;
    s.ap_seed = args.ap_lat.zip(args.ap_lon).map(|(lat, lon)| ApInput {
        lat,
        lon,
        eye_height_m: args.ap_eye_height_m.unwrap_or(2.0),
        provenance: ApProvenance::OperatorEntered,
    });
    s.kinematics = match args.kinematics {
        KinematicsArg::Stationary => SessionKinematics::Stationary,
        KinematicsArg::MaxSpeedKn(kn) => SessionKinematics::MaxSpeedKn { kn },
    };
    if let Some(seconds) = args.sight_retention_seconds {
        s.sight_retention_seconds = seconds;
    }
    if let Some(cap) = args.sight_retention_capacity {
        s.sight_retention_capacity = cap;
    }
    s.profile = args.profile.into();
    s.expected_to_fail = args.expected_to_fail;
    s.save_to_dir(&session_dir)
        .with_context(|| format!("write session.json to {}", session_dir.display()))?;
    info!(
        session_id = %session_id,
        dir = %session_dir.display(),
        "session created"
    );
    println!("{session_id}");
    Ok(())
}

fn run_session_list(args: SessionListArgs) -> anyhow::Result<()> {
    let corpus = args.corpus.unwrap_or_else(default_corpus_root);
    let sessions_root = corpus.join("sessions");
    if !sessions_root.exists() {
        println!("no sessions in {}", sessions_root.display());
        return Ok(());
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&sessions_root)
        .with_context(|| format!("read_dir {}", sessions_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        match SessionManifest::load_from_dir(&dir) {
            Ok(s) => found.push(s),
            Err(e) => warn!(dir = %dir.display(), error = ?e, "skipping non-session dir"),
        }
    }
    found.sort_by_key(|s| s.created_unix_ms);
    if found.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    println!("{:<36}  {:<6}  {:>3}  title", "session_id", "expect", "cap");
    for s in &found {
        let expect = if s.expected_to_fail { "FAIL" } else { "ok" };
        println!(
            "{:<36}  {:<6}  {:>3}  {}",
            s.session_id,
            expect,
            s.ordered_capture_ids.len(),
            s.title
        );
    }
    Ok(())
}

fn run_session_show(args: SessionShowArgs) -> anyhow::Result<()> {
    let corpus = args.corpus.unwrap_or_else(default_corpus_root);
    let dir = corpus.join("sessions").join(args.session.to_string());
    let s = SessionManifest::load_from_dir(&dir)
        .with_context(|| format!("load session.json from {}", dir.display()))?;
    let raw = serde_json::to_string_pretty(&s)?;
    println!("{raw}");
    Ok(())
}

fn run_session_attach(args: SessionAttachArgs) -> anyhow::Result<()> {
    let corpus = args.corpus.unwrap_or_else(default_corpus_root);
    let session_dir = corpus.join("sessions").join(args.session.to_string());
    let mut session = SessionManifest::load_from_dir(&session_dir)
        .with_context(|| format!("load session.json from {}", session_dir.display()))?;
    let mut manifest = BundleManifest::load_from_dir(&args.bundle)
        .with_context(|| format!("load bundle.json from {}", args.bundle.display()))?;
    let cap_id = manifest.bundle_id.clone();
    if session.ordered_capture_ids.iter().any(|c| c == &cap_id) {
        bail!(
            "capture {cap_id} already attached to session {}",
            args.session
        );
    }
    manifest.session_id = Some(session.session_id);
    let dest = if args.in_place {
        args.bundle.clone()
    } else {
        let dest = session_dir.join("captures").join(&cap_id);
        if dest.exists() {
            bail!(
                "destination already exists: {} (use --in-place to skip move)",
                dest.display()
            );
        }
        std::fs::create_dir_all(dest.parent().unwrap())?;
        std::fs::rename(&args.bundle, &dest)
            .with_context(|| format!("move {} -> {}", args.bundle.display(), dest.display()))?;
        dest
    };
    manifest
        .save_to_dir(&dest)
        .with_context(|| format!("rewrite bundle.json in {}", dest.display()))?;
    session.ordered_capture_ids.push(cap_id.clone());
    session
        .save_to_dir(&session_dir)
        .with_context(|| format!("rewrite session.json in {}", session_dir.display()))?;
    info!(
        session_id = %session.session_id,
        capture_id = %cap_id,
        "session attached"
    );
    Ok(())
}

#[cfg(test)]
mod session_cli_tests {
    use super::*;
    use tempfile::tempdir;

    fn args_new(corpus: &Path) -> SessionNewArgs {
        SessionNewArgs {
            title: "t".into(),
            ap_lat: None,
            ap_lon: None,
            ap_eye_height_m: None,
            kinematics: KinematicsArg::Stationary,
            sight_retention_seconds: None,
            sight_retention_capacity: None,
            profile: ProfileArg::Custom,
            notes: None,
            expected_to_fail: false,
            corpus: Some(corpus.to_path_buf()),
        }
    }

    #[test]
    fn new_writes_session_json() {
        let dir = tempdir().unwrap();
        run_session_new(args_new(dir.path())).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path().join("sessions"))
            .unwrap()
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn new_with_kinematics_max_speed() {
        let dir = tempdir().unwrap();
        let mut a = args_new(dir.path());
        a.kinematics = KinematicsArg::MaxSpeedKn(7.5);
        run_session_new(a).unwrap();
        let sub: PathBuf = std::fs::read_dir(dir.path().join("sessions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let s = SessionManifest::load_from_dir(&sub).unwrap();
        match s.kinematics {
            SessionKinematics::MaxSpeedKn { kn } => assert!((kn - 7.5).abs() < f64::EPSILON),
            SessionKinematics::Stationary => panic!("unexpected Stationary"),
        }
    }

    #[test]
    fn kinematics_arg_parse() {
        use std::str::FromStr;
        assert!(matches!(
            KinematicsArg::from_str("stationary").unwrap(),
            KinematicsArg::Stationary
        ));
        assert!(matches!(
            KinematicsArg::from_str("max-speed-kn=5.5").unwrap(),
            KinematicsArg::MaxSpeedKn(_)
        ));
        assert!(KinematicsArg::from_str("nope").is_err());
    }
}

#[cfg(test)]
mod session_overlay_tests {
    use super::*;
    use bris_almanac::{refraction::Atmosphere, Observer};
    use bris_bundle::{
        CaptureInfo, Distortion, IntrinsicsRecord, IntrinsicsSource, SessionKinematics,
        SessionManifest, UseCaseProfile,
    };
    use bris_core::angle::{Latitude, Longitude};
    use tempfile::tempdir;
    use uuid::Uuid;

    fn dummy_observer() -> Observer {
        Observer {
            latitude: Latitude::from_degrees(0.0).unwrap(),
            longitude: Longitude::from_degrees(0.0).unwrap(),
            eye_height_m: 2.0,
            eye_height_sigma_m: 0.5,
            atmosphere: Atmosphere::STANDARD,
        }
    }

    fn dummy_manifest(session_id: Uuid) -> BundleManifest {
        BundleManifest {
            schema_version: bris_bundle::SCHEMA_VERSION,
            bundle_id: "cap-test".into(),
            session_id: Some(session_id),
            device: DeviceInfo {
                model: "t".into(),
                os: None,
                app_version: None,
            },
            build: None,
            capture: CaptureInfo {
                source_rotation_deg: 0,
                pre_rotation_was_deg: None,
                frame_count: 0,
                started_unix_ms: 0,
                ended_unix_ms: 0,
                first_frame_blake3: None,
            },
            intrinsics: IntrinsicsRecord {
                source: IntrinsicsSource::Placeholder,
                profile_key: None,
                width: 1280,
                height: 720,
                fx: 1.0,
                fy: 1.0,
                cx: 0.5,
                cy: 0.5,
                distortion: Distortion::None,
                rms_px: None,
                solved_at_unix_ms: None,
                placeholder: Some(true),
            },
            ap_input: None,
            ap_derivation_trace: None,
            gps_truth: None,
            atmosphere_hint: None,
            notes: String::new(),
        }
    }

    #[test]
    fn overlay_applies_kinematics_and_retention() {
        let root = tempdir().unwrap();
        let sid = Uuid::new_v4();
        let session_dir = root.path().join("sessions").join(sid.to_string());
        let bundle_dir = session_dir.join("captures").join("cap-abc");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        let mut s = SessionManifest::new(
            sid,
            "t".into(),
            DeviceInfo {
                model: "m".into(),
                os: None,
                app_version: None,
            },
            0,
        );
        s.kinematics = SessionKinematics::MaxSpeedKn { kn: 12.0 };
        s.sight_retention_seconds = 86_400;
        s.sight_retention_capacity = 250;
        s.profile = UseCaseProfile::Marine;
        s.save_to_dir(&session_dir).unwrap();

        let mut cfg = EngineConfig::new(dummy_observer());
        apply_session_overlay(&mut cfg, &dummy_manifest(sid), Some(&bundle_dir));
        assert!((cfg.sight_window_seconds - 86_400.0).abs() < f64::EPSILON);
        assert_eq!(cfg.sight_window_capacity, 250);
        assert!((cfg.publication_gate.assumed_max_speed_kn - 12.0).abs() < f64::EPSILON);
    }

    #[test]
    fn overlay_no_session_json_is_noop() {
        let root = tempdir().unwrap();
        let bundle_dir = root
            .path()
            .join("sessions")
            .join("x")
            .join("captures")
            .join("y");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        let mut cfg = EngineConfig::new(dummy_observer());
        let before = cfg.sight_window_seconds;
        apply_session_overlay(&mut cfg, &dummy_manifest(Uuid::nil()), Some(&bundle_dir));
        assert!((cfg.sight_window_seconds - before).abs() < f64::EPSILON);
    }

    #[test]
    fn overlay_no_bundle_dir_is_noop() {
        let mut cfg = EngineConfig::new(dummy_observer());
        let before = cfg.sight_window_capacity;
        apply_session_overlay(&mut cfg, &dummy_manifest(Uuid::nil()), None);
        assert_eq!(cfg.sight_window_capacity, before);
    }
}
