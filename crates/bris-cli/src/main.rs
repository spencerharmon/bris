//! Bris reference CLI: desktop and embedded Linux frontend.
//!
//! Subcommands (per `plan.org` Phase 6):
//! - `capture` — record frames from a V4L2 camera to disk.
//!   *Implemented* against the YUYV format on Linux.
//! - `calibrate` — lens calibration workflow (stub).
//! - `fix` — one-shot fix from a webcam (stub; the streaming
//!   engine in `serve` supersedes this).
//! - `serve` — continuous engine + NMEA serving. *Implemented*
//!   for the V4L2 → engine → published-fix path; NMEA
//!   transport (TCP/serial) is a follow-up.
//! - `replay` — process saved frames through the full pipeline.
//!   *Implemented* as the validation path before live capture.
//! - `log` — sight log management (stub).
//! - `update` — refresh almanac/catalog/leap-seconds (stub).

use anyhow::{bail, Context};
use bris_almanac::{body_apparent_place, ApparentPlace, Atmosphere, Observer, SolarSystemBody};
use bris_capture::{
    run_capture_loop, run_capture_loop_with, CaptureLoopAction, V4l2Capture, V4l2Config,
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
    /// Run the lens calibration workflow (stub).
    Calibrate,
    /// Compute a one-shot fix from a webcam (stub; the
    /// streaming engine in `serve` supersedes this).
    Fix,
    /// Run the continuous streaming engine against a V4L2
    /// camera, logging each published fix.
    ///
    /// NMEA transport (TCP server, serial port) is a
    /// follow-up; for now fixes are reported via `tracing` at
    /// info level.
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
    /// Path to the V4L2 device node. Default `/dev/video0`.
    #[arg(long, default_value = "/dev/video0")]
    device: PathBuf,
    /// Capture width (pixels). Default 640.
    #[arg(long, default_value_t = 640)]
    width: u32,
    /// Capture height (pixels). Default 480.
    #[arg(long, default_value_t = 480)]
    height: u32,
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
    /// microseconds. Used as the per-frame mid-exposure
    /// offset; see `bris_capture::V4l2Config::exposure_us`.
    /// Default 10000 (10 ms — typical daylight).
    #[arg(long, default_value_t = 10_000)]
    exposure_us: u32,
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// Path to the V4L2 device node. Default `/dev/video0`.
    #[arg(long, default_value = "/dev/video0")]
    device: PathBuf,
    /// Capture width (pixels). Default 640.
    #[arg(long, default_value_t = 640)]
    width: u32,
    /// Capture height (pixels). Default 480.
    #[arg(long, default_value_t = 480)]
    height: u32,
    /// Camera exposure for the timestamp correction, in
    /// microseconds. Default 10000.
    #[arg(long, default_value_t = 10_000)]
    exposure_us: u32,
    /// Observer latitude in degrees (north positive). The
    /// engine uses this for almanac apparent-place
    /// computations and for the assumed position in sight
    /// reduction. The fix it publishes is a refinement of
    /// this assumed position; an error of a few hundred nm
    /// in the assumed position introduces a few-arcmin
    /// linearization error in the fix, which is in the
    /// noise for typical sights but matters offshore. Use
    /// the most-recent known fix (DR or GNSS) when
    /// available.
    #[arg(long, allow_hyphen_values = true)]
    assumed_lat: f64,
    /// Observer longitude in degrees (east positive). See
    /// `--assumed-lat` for accuracy requirements.
    #[arg(long, allow_hyphen_values = true)]
    assumed_lon: f64,
    /// Eye height above sea level, meters. Default 2.0.
    #[arg(long, default_value_t = 2.0)]
    eye_height_m: f64,
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
    match cli.command {
        Command::Replay(args) => run_replay(&args),
        Command::Capture(args) => run_capture(&args),
        Command::Serve(args) => run_serve(&args),
        Command::Calibrate | Command::Fix | Command::Log | Command::Update => {
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

fn run_capture(args: &CaptureArgs) -> anyhow::Result<()> {
    fs::create_dir_all(&args.output)
        .with_context(|| format!("create output dir {}", args.output.display()))?;

    let v4l_config = V4l2Config {
        device_path: args.device.clone(),
        width: args.width,
        height: args.height,
        buffer_count: 4,
        exposure_us: args.exposure_us,
    };
    let intrinsics = Intrinsics::placeholder(args.width, args.height);
    let capture =
        V4l2Capture::open(v4l_config, intrinsics).context("open V4L2 device")?;
    info!(
        device = %args.device.display(),
        width = args.width,
        height = args.height,
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

fn run_serve(args: &ServeArgs) -> anyhow::Result<()> {
    let observer = Observer {
        latitude: Latitude::from_degrees(args.assumed_lat)
            .context("assumed_lat out of [-90, 90]")?,
        longitude: Longitude::from_degrees(args.assumed_lon).context("assumed_lon")?,
        eye_height_m: args.eye_height_m,
        eye_height_sigma_m: 0.5,
        atmosphere: Atmosphere::STANDARD,
    };
    let engine_config = EngineConfig::new(observer);
    let engine = Arc::new(StreamingEngine::new(engine_config));

    // Subscribe before the capture thread starts so we never
    // miss the first publication.
    let fix_rx = engine
        .fix_stream()
        .map_err(|e| anyhow::anyhow!("fix_stream: {e}"))?;

    let v4l_config = V4l2Config {
        device_path: args.device.clone(),
        width: args.width,
        height: args.height,
        buffer_count: 4,
        exposure_us: args.exposure_us,
    };
    let intrinsics = Intrinsics::placeholder(args.width, args.height);
    let capture =
        V4l2Capture::open(v4l_config, intrinsics).context("open V4L2 device")?;
    info!(
        device = %args.device.display(),
        width = args.width,
        height = args.height,
        observer_lat = args.assumed_lat,
        observer_lon = args.assumed_lon,
        "bris serve: starting"
    );
    warn!(
        "bris serve: using placeholder camera intrinsics (fx=fy=1000); \
         published fixes will be wrong by the calibration error \
         (potentially tens of nm). Run `bris calibrate` once that \
         workflow lands to fit per-device intrinsics."
    );

    let shutdown = install_ctrlc_handler()?;
    let engine_thread = engine.clone();
    let shutdown_thread = shutdown.clone();
    let capture_handle = std::thread::Builder::new()
        .name("bris-capture".to_string())
        .spawn(move || run_capture_loop(capture, engine_thread, shutdown_thread))
        .context("spawn capture thread")?;

    // Main thread drains the fix stream, logging each
    // published fix until shutdown is signalled. NMEA
    // transport (TCP / serial) is the next follow-up;
    // until then operators see fixes in the tracing log.
    info!("bris serve: draining fix stream (Ctrl-C to stop)");
    while !shutdown.load(Ordering::Relaxed) {
        match fix_rx.try_recv() {
            Ok(Some(fix)) => {
                info!(
                    lat_deg = fix.fix.lat.degrees(),
                    lon_deg = fix.fix.lon.degrees(),
                    sigma_nm = fix.fix.sigma_nm().value(),
                    n_sights = fix.n_sights,
                    azimuth_spread_deg = fix.azimuth_spread_rad.to_degrees(),
                    oldest_sight_age_s = fix.oldest_sight_age_seconds,
                    "bris serve: published fix"
                );
            }
            Ok(None) => {
                // No fix available right now; sleep briefly to
                // avoid busy-spinning. 100ms matches the
                // engine's default min_fix_publication_interval_ms.
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(()) => {
                // Channel closed — engine is gone (unexpected
                // since we hold an Arc; defensive log).
                warn!("bris serve: fix stream channel closed");
                break;
            }
        }
    }

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
