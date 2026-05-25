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
use bris_almanac::{body_apparent_place, ApparentPlace, Atmosphere, Observer, SolarSystemBody};
use bris_calibrate::{
    calibrate, coverage, default_intrinsics_path, detect_corners_in_directory_with_progress,
    diagnose, write_intrinsics, CheckerboardTarget, CoverageConfig, DiagnosisLevel, FrameDetection,
    FrameOutcome,
};
use bris_capture::{
    max_yuyv_resolution, run_capture_loop, run_capture_loop_with, CaptureLoopAction, V4l2Capture,
    V4l2Config,
};
use bris_core::time::{utc_to_tt, Tt};
use bris_core::{Latitude, Longitude, Sigma, Uncertain};
use bris_nav::{line_of_position, multi_sight_fix, screen_sights, Fix, ScreeningConfig};
use bris_nmea::{
    gpgga, gpgll, gpgst, gprmc, pbris_full, ErrorCounters, QualityThresholds, TimeDiagnostic,
    UncertaintyBudget,
};
use bris_streaming::{EngineConfig, StreamingEngine};
use bris_vision::{
    centroid_brightest_body, detect_horizon, detect_horizon_via_segmentation,
    detect_horizon_via_sky_region, load_frame_from_path, load_model, measure_altitude,
    panorama_altitude_with_detector, save_frame_as_png, CentroidConfig, Frame, HorizonConfig,
    HorizonLine, Intrinsics, TrackConfig,
};
use chrono::{DateTime, Utc};
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
struct ReplayArgs {
    /// Directory containing captured frames (PNG/JPEG/PPM).
    #[arg(long)]
    frames: PathBuf,
    /// Assumed observer latitude in degrees (north positive).
    #[arg(long, allow_hyphen_values = true)]
    assumed_lat: f64,
    /// Assumed observer longitude in degrees (east positive).
    #[arg(long, allow_hyphen_values = true)]
    assumed_lon: f64,
    /// Eye height above sea level, meters. Default 2.0.
    #[arg(long, default_value_t = 2.0)]
    eye_height_m: f64,
    /// Body the camera was pointed at.
    #[arg(long, value_enum, default_value_t = BodyArg::Sun)]
    body: BodyArg,
    /// Override capture time as ISO-8601 UTC (e.g. 2024-06-21T18:00:00Z).
    /// Defaults to the file modification time of the first frame.
    #[arg(long)]
    capture_utc: Option<String>,
    /// Horizon detection method.
    ///
    /// `gradient` is the original RANSAC-on-column-gradients detector;
    /// best for open-ocean scenes. `sky-region` finds the bright sky's
    /// lower boundary; better for cluttered shipboard scenes where the
    /// deck or sail dominates the lower half of the frame.
    /// `segmentation` runs a pretrained semantic-segmentation model to
    /// classify sky/boat/other and uses the per-column sky→sea
    /// transitions (skipping vessel-occluded columns) as horizon
    /// candidates. Most robust on cluttered scenes; ~180ms per frame
    /// on `x86_64` (slower on Pi-class hardware). Requires the
    /// `segmentation` feature flag (on by default).
    #[arg(long, value_enum, default_value_t = HorizonMethod::SkyRegion)]
    horizon_method: HorizonMethod,
    /// Path to the segmentation ONNX model (only used with
    /// `--horizon-method segmentation`). Defaults to the vendored
    /// `crates/bris-vision/data/segmentation.onnx`.
    #[arg(long)]
    segmentation_model: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum HorizonMethod {
    Gradient,
    SkyRegion,
    Segmentation,
}

impl HorizonMethod {
    fn detect(
        self,
        frame: &Frame,
        cfg: HorizonConfig,
        seg_model_path: Option<&Path>,
    ) -> Result<HorizonLine, anyhow::Error> {
        match self {
            Self::Gradient => detect_horizon(frame, cfg).map_err(anyhow::Error::from),
            Self::SkyRegion => {
                detect_horizon_via_sky_region(frame, cfg).map_err(anyhow::Error::from)
            }
            Self::Segmentation => {
                let path = seg_model_path
                    .map_or_else(default_segmentation_model_path, std::path::PathBuf::from);
                load_model(&path).map_err(|e| anyhow::anyhow!("load segmentation model: {e}"))?;
                detect_horizon_via_segmentation(frame, cfg)
                    .map_err(|e| anyhow::anyhow!("segmentation horizon detection: {e}"))
            }
        }
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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum BodyArg {
    Sun,
    Moon,
    Mercury,
    Venus,
    Mars,
    Jupiter,
    Saturn,
}

impl BodyArg {
    fn to_solar_system_body(self) -> SolarSystemBody {
        match self {
            Self::Sun => SolarSystemBody::Sun,
            Self::Moon => SolarSystemBody::Moon,
            Self::Mercury => SolarSystemBody::Planet(bris_almanac::Body::Mercury),
            Self::Venus => SolarSystemBody::Planet(bris_almanac::Body::Venus),
            Self::Mars => SolarSystemBody::Planet(bris_almanac::Body::Mars),
            Self::Jupiter => SolarSystemBody::Planet(bris_almanac::Body::Jupiter),
            Self::Saturn => SolarSystemBody::Planet(bris_almanac::Body::Saturn),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Sun => "Sun",
            Self::Moon => "Moon",
            Self::Mercury => "Mercury",
            Self::Venus => "Venus",
            Self::Mars => "Mars",
            Self::Jupiter => "Jupiter",
            Self::Saturn => "Saturn",
        }
    }
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

fn run_replay(args: &ReplayArgs) -> anyhow::Result<()> {
    let observer = Observer {
        latitude: Latitude::from_degrees(args.assumed_lat)
            .context("assumed_lat out of [-90, 90]")?,
        longitude: Longitude::from_degrees(args.assumed_lon).context("assumed_lon")?,
        eye_height_m: args.eye_height_m,
        eye_height_sigma_m: 0.5,
        atmosphere: Atmosphere::STANDARD,
    };

    let frame_paths = list_frames(&args.frames)?;
    if frame_paths.is_empty() {
        bail!("no frames found in {}", args.frames.display());
    }
    info!(
        frame_count = frame_paths.len(),
        body = args.body.name(),
        observer_lat = args.assumed_lat,
        observer_lon = args.assumed_lon,
        "replay: loaded frames"
    );

    let utc = parse_or_infer_utc(args)?;
    let tt = utc_to_tt(utc).context("convert capture time to TT")?;
    info!(utc = %utc, "replay: capture time");

    let frames = load_all_frames(&frame_paths, tt)?;

    // Run the panorama-stitching path. The vision pipeline reports
    // per-frame failures inside; if every frame fails this returns
    // an error and we surface it cleanly.
    let horizon_method = args.horizon_method;
    let seg_model_path = args.segmentation_model.as_deref();
    let observed_altitude = match panorama_altitude_with_detector(
        &frames,
        HorizonConfig::default(),
        CentroidConfig::default(),
        TrackConfig::default(),
        |frame, cfg| horizon_method.detect(frame, cfg, seg_model_path),
    ) {
        Ok(alt) => {
            info!(
                altitude_deg = alt.value.to_degrees(),
                sigma_arcmin = alt.sigma.value().to_degrees() * 60.0,
                "replay: panorama-stitching produced an observed altitude"
            );
            alt
        }
        Err(e) => {
            warn!(error = %e, "replay: panorama failed; trying single-frame measurement");
            single_frame_fallback(&frames, horizon_method, seg_model_path)?
        }
    };

    // Compute the body's apparent place and reduce the sight.
    let jd_ut1 = utc_to_jd_utc(utc); // ΔUT1 ≈ 0 approximation
    let body = args.body.to_solar_system_body();
    let apparent: ApparentPlace = body_apparent_place(body, tt, jd_ut1, observer)
        .context("apparent-place computation failed")?;
    info!(
        body = args.body.name(),
        computed_altitude_deg = apparent.direction.altitude.to_degrees(),
        computed_azimuth_deg = apparent.direction.azimuth.to_degrees(),
        computed_sigma_arcsec = apparent.altitude_sigma.value().to_degrees() * 3600.0,
        "replay: body apparent place at assumed observer"
    );

    let computed = Uncertain::new(apparent.direction.altitude, apparent.altitude_sigma);
    let lop = line_of_position(
        observer.latitude,
        observer.longitude,
        observed_altitude,
        computed,
        apparent.direction.azimuth,
    )
    .context("line_of_position failed")?;
    info!(
        intercept_nm = lop.intercept_nm,
        intercept_sigma_nm = lop.intercept_sigma_nm.value(),
        "replay: line of position"
    );

    // Single-LOP "fix" is along the line; we report it as a 1-LOP
    // result with the observer's assumed position adjusted toward the
    // body by the intercept. A true fix needs ≥ 2 bodies.
    warn!(
        "replay: single-LOP result is a line, not a 2D fix. \
         The 'fix' below is the assumed position shifted by the intercept \
         along the body azimuth — useful as a sanity check, not as a \
         navigational fix. A true fix requires ≥ 2 bodies (plate solving \
         in Phase 3 will handle that automatically at night)."
    );

    // Fake a second LOP perpendicular to the first with zero intercept,
    // so multi_sight_fix has the geometry it needs. Mark the resulting
    // fix as advisory in the log.
    let fake_perp = bris_nav::LineOfPosition {
        assumed_lat: observer.latitude,
        assumed_lon: observer.longitude,
        azimuth_rad: (apparent.direction.azimuth + std::f64::consts::FRAC_PI_2)
            .rem_euclid(std::f64::consts::TAU),
        intercept_nm: 0.0,
        intercept_sigma_nm: Sigma::new(lop.intercept_sigma_nm.value().max(0.5))
            .unwrap_or(Sigma::ZERO),
    };
    let screened = screen_sights(&[lop, fake_perp], ScreeningConfig::default());
    let fix = multi_sight_fix(&screened.kept).context("multi_sight_fix failed")?;
    info!(
        lat_deg = fix.lat.degrees(),
        lon_deg = fix.lon.degrees(),
        sigma_major_nm = fix.sigma_major_nm,
        sigma_minor_nm = fix.sigma_minor_nm,
        sigma_nm = fix.sigma_nm().value(),
        "replay: advisory fix (single-body LOP + perpendicular zero-intercept anchor)"
    );

    emit_nmea(&fix, utc, args, &lop, &apparent);

    Ok(())
}

fn list_frames(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()).is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "ppm" | "pgm"
                )
            })
        })
        .collect();
    paths.sort();
    Ok(paths)
}

fn load_all_frames(paths: &[PathBuf], tt: Tt) -> anyhow::Result<Vec<Frame>> {
    let mut frames = Vec::with_capacity(paths.len());
    for path in paths {
        // Use placeholder intrinsics until calibration workflow lands.
        // The first frame determines the dimensions.
        let dims = image::image_dimensions(path)
            .with_context(|| format!("read dimensions of {}", path.display()))?;
        let intrinsics = Intrinsics::placeholder(dims.0, dims.1);
        let frame = load_frame_from_path(path, tt, 1000, intrinsics)
            .with_context(|| format!("load {}", path.display()))?
            .with_source_path(path.clone());
        frames.push(frame);
    }
    Ok(frames)
}

fn parse_or_infer_utc(args: &ReplayArgs) -> anyhow::Result<DateTime<Utc>> {
    if let Some(s) = &args.capture_utc {
        return DateTime::parse_from_rfc3339(s)
            .with_context(|| format!("parse capture_utc {s:?}"))
            .map(|dt| dt.with_timezone(&Utc));
    }
    // Infer from the first frame's mtime.
    let first = list_frames(&args.frames)?
        .into_iter()
        .next()
        .context("no frames to infer time from")?;
    let meta = fs::metadata(&first).with_context(|| format!("stat {}", first.display()))?;
    let mtime = meta
        .modified()
        .with_context(|| format!("modified time of {}", first.display()))?;
    let secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .context("system time before unix epoch")?
        .as_secs();
    let dt = DateTime::<Utc>::from_timestamp(
        i64::try_from(secs).context("first-frame mtime exceeds i64 range")?,
        0,
    )
    .context("first-frame mtime out of representable range")?;
    warn!(
        utc = %dt,
        "replay: --capture-utc not given; using first frame's mtime"
    );
    Ok(dt)
}

fn single_frame_fallback(
    frames: &[Frame],
    horizon_method: HorizonMethod,
    seg_model_path: Option<&Path>,
) -> anyhow::Result<Uncertain<f64>> {
    // Try each frame individually. The first one that yields both a
    // horizon and a centroid wins.
    for (i, frame) in frames.iter().enumerate() {
        let Ok(horizon) = horizon_method.detect(frame, HorizonConfig::default(), seg_model_path)
        else {
            continue;
        };
        let Ok(centroid) = centroid_brightest_body(frame, CentroidConfig::default()) else {
            continue;
        };
        let Ok(altitude) = measure_altitude(frame.intrinsics, horizon, centroid) else {
            continue;
        };
        info!(
            frame_index = i,
            altitude_deg = altitude.value.to_degrees(),
            "replay: single-frame fallback succeeded"
        );
        return Ok(altitude);
    }
    bail!("no frame contained both a horizon and a body centroid; cannot measure altitude");
}

fn emit_nmea(
    fix: &Fix,
    utc: DateTime<Utc>,
    args: &ReplayArgs,
    lop: &bris_nav::LineOfPosition,
    apparent: &ApparentPlace,
) {
    let quality = QualityThresholds::default().classify(fix.sigma_nm().value());
    let _ = gpgll(fix, utc, quality);
    let _ = gprmc(fix, utc, quality);
    let _ = gpgga(fix, utc, quality);
    let _ = gpgst(fix, utc);

    let budget = UncertaintyBudget {
        centroid_nm: 0.0,
        horizon_nm: 0.0,
        calibration_nm: 0.0,
        stitching_nm: 0.0,
        refraction_nm: 0.0,
        dip_nm: 0.0,
        timing_nm: 0.0,
    };
    let time_diag = TimeDiagnostic {
        seconds_since_sync: None,
        drift_ppm: None,
        step_detected: false,
    };
    let counters = ErrorCounters::default();
    let sights = vec![(
        args.body.name().to_string(),
        apparent.direction.altitude,
        apparent.direction.azimuth,
        *lop,
    )];
    let _ = pbris_full(utc, fix, &time_diag, &budget, &sights, &counters, true);
}

fn utc_to_jd_utc(utc: DateTime<Utc>) -> f64 {
    use chrono::Datelike;
    use chrono::Timelike;
    let mut y = utc.year();
    let mut m = i32::try_from(utc.month()).unwrap();
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    let a = y.div_euclid(100);
    let b = 2 - a + a.div_euclid(4);
    let day_fraction =
        (f64::from(utc.hour()) * 3600.0 + f64::from(utc.minute()) * 60.0 + f64::from(utc.second()))
            / 86_400.0;
    let jd_int = (365.25 * f64::from(y + 4716)).floor()
        + (30.6001 * f64::from(m + 1)).floor()
        + f64::from(utc.day())
        + f64::from(b)
        - 1524.5;
    jd_int + day_fraction
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
