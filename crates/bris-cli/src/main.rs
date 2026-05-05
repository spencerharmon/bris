//! Bris reference CLI: desktop and embedded Linux frontend.
//!
//! Subcommands (per `plan.org` Phase 6):
//! - `capture` — capture frames from the camera (stub).
//! - `calibrate` — lens calibration workflow (stub).
//! - `fix` — one-shot fix from a webcam (stub; pending V4L2 wiring).
//! - `serve` — continuous engine + NMEA serving (stub).
//! - `replay` — process saved frames through the full pipeline.
//!   *Implemented* as the validation path before live capture.
//! - `log` — sight log management (stub).
//! - `update` — refresh almanac/catalog/leap-seconds (stub).

use anyhow::{bail, Context};
use bris_almanac::{body_apparent_place, ApparentPlace, Atmosphere, Observer, SolarSystemBody};
use bris_core::time::{utc_to_tt, Tt};
use bris_core::{Latitude, Longitude, Sigma, Uncertain};
use bris_nav::{line_of_position, multi_sight_fix, screen_sights, Fix, ScreeningConfig};
use bris_nmea::{
    gpgga, gpgll, gpgst, gprmc, pbris_full, ErrorCounters, QualityThresholds, TimeDiagnostic,
    UncertaintyBudget,
};
use bris_vision::{
    centroid_brightest_body, detect_horizon, detect_horizon_via_sky_region, load_frame_from_path,
    measure_altitude, panorama_altitude_with_detector, CentroidConfig, Frame, HorizonConfig,
    HorizonError, HorizonLine, Intrinsics, TrackConfig,
};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
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
    /// Capture frames from the camera (stub).
    Capture,
    /// Run the lens calibration workflow (stub).
    Calibrate,
    /// Compute a one-shot fix from a webcam (stub; pending V4L2 wiring).
    Fix,
    /// Run the continuous engine and serve NMEA output (stub).
    Serve,
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
    #[arg(long, value_enum, default_value_t = HorizonMethod::SkyRegion)]
    horizon_method: HorizonMethod,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum HorizonMethod {
    Gradient,
    SkyRegion,
}

impl HorizonMethod {
    fn detect(self, frame: &Frame, cfg: HorizonConfig) -> Result<HorizonLine, HorizonError> {
        match self {
            Self::Gradient => detect_horizon(frame, cfg),
            Self::SkyRegion => detect_horizon_via_sky_region(frame, cfg),
        }
    }
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
        Command::Capture
        | Command::Calibrate
        | Command::Fix
        | Command::Serve
        | Command::Log
        | Command::Update => {
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
    let observed_altitude = match panorama_altitude_with_detector(
        &frames,
        HorizonConfig::default(),
        CentroidConfig::default(),
        TrackConfig::default(),
        |frame, cfg| horizon_method.detect(frame, cfg),
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
            single_frame_fallback(&frames, horizon_method)?
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
            .with_context(|| format!("load {}", path.display()))?;
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
) -> anyhow::Result<Uncertain<f64>> {
    // Try each frame individually. The first one that yields both a
    // horizon and a centroid wins.
    for (i, frame) in frames.iter().enumerate() {
        let Ok(horizon) = horizon_method.detect(frame, HorizonConfig::default()) else {
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
