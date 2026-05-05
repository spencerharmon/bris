//! Bris reference CLI: desktop and embedded Linux frontend.
//!
//! Subcommands (per `plan.org` Phase 6): `capture`, `calibrate`, `fix`,
//! `serve`, `replay`, `log`, `update`, `demo`. Most are not yet
//! implemented; `demo` runs the end-to-end synthetic pipeline so you
//! can see a debug log of a fix without any camera hardware.

use bris_almanac::{body_apparent_place, Atmosphere, Observer, SolarSystemBody};
use bris_core::time::utc_to_tt;
use bris_core::{Latitude, Longitude, Sigma, Uncertain};
use bris_nav::{line_of_position, multi_sight_fix, screen_sights, ScreeningConfig};
use bris_nmea::{
    gpgga, gpgll, gpgst, gprmc, pbris_full, ErrorCounters, QualityThresholds, TimeDiagnostic,
    UncertaintyBudget,
};
use chrono::{TimeZone, Utc};
use clap::{Parser, Subcommand};
use tracing::{info, info_span};

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
    /// Compute a one-shot fix from a frame source (stub).
    Fix,
    /// Run the continuous engine and serve NMEA output (stub).
    Serve,
    /// Re-derive a fix from saved frames (stub).
    Replay,
    /// Sight log management (list/show/delete/restore/export) (stub).
    Log,
    /// Download and apply almanac/catalog/leap-second updates (stub).
    Update,
    /// Run the end-to-end synthetic pipeline and print a debug log
    /// of a sample fix. Useful for verifying the build and watching
    /// the NMEA stream that would go on the wire.
    Demo,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Demo => run_demo(),
        Command::Capture
        | Command::Calibrate
        | Command::Fix
        | Command::Serve
        | Command::Replay
        | Command::Log
        | Command::Update => {
            anyhow::bail!("not yet implemented; see plan.org for the development roadmap");
        }
    }
}

/// Run a synthetic end-to-end pipeline:
/// 1. Compute the Sun's apparent place from an assumed observer position.
/// 2. Pretend the vision pipeline observed it 1 arcmin higher than computed
///    (a +1 nm intercept toward the Sun).
/// 3. Repeat for the Moon at a different azimuth.
/// 4. Reduce both sights to a single fix.
/// 5. Emit the full NMEA stream (`$GP*` + `$PBRIS,*`) at debug level.
///
/// The output is what would go on the wire for a real fix at the same
/// time and place — the stack is exercised end-to-end without any
/// camera hardware.
#[allow(clippy::too_many_lines, clippy::single_match_else)]
fn run_demo() -> anyhow::Result<()> {
    let span = info_span!("bris_demo");
    let _enter = span.enter();

    let utc = Utc
        .with_ymd_and_hms(2024, 6, 21, 18, 0, 0)
        .single()
        .unwrap();
    let tt = utc_to_tt(utc)?;
    let jd_ut1 = utc_to_jd_utc(utc); // ΔUT1 = 0 approximation
    let observer = Observer {
        latitude: Latitude::from_degrees(47.6).unwrap(),
        longitude: Longitude::from_degrees(-122.3).unwrap(),
        eye_height_m: 5.0,
        eye_height_sigma_m: 0.5,
        atmosphere: Atmosphere::STANDARD,
    };

    info!(
        utc = %utc,
        observer_lat_deg = observer.latitude.degrees(),
        observer_lon_deg = observer.longitude.degrees(),
        "demo: starting synthetic end-to-end fix"
    );

    // Sun apparent place at the assumed observer position.
    let sun = body_apparent_place(SolarSystemBody::Sun, tt, jd_ut1, observer)?;
    info!(
        body = "Sun",
        altitude_deg = sun.direction.altitude.to_degrees(),
        azimuth_deg = sun.direction.azimuth.to_degrees(),
        sigma_arcsec = sun.altitude_sigma.value().to_degrees() * 3600.0,
        "demo: computed apparent place"
    );

    // Moon apparent place — note: lunar topocentric parallax is the
    // documented Phase 1 follow-up. The altitude here is geocentric
    // and may be off by ~1° for low altitudes; sufficient for a demo
    // of the wiring. The Moon may be below the horizon at this
    // synthetic time/place, in which case we fall back to a synthetic
    // second sight so the multi-sight LSQ has the geometry it needs.
    let second_sight = match body_apparent_place(SolarSystemBody::Moon, tt, jd_ut1, observer) {
        Ok(moon) => {
            info!(
                body = "Moon",
                altitude_deg = moon.direction.altitude.to_degrees(),
                azimuth_deg = moon.direction.azimuth.to_degrees(),
                sigma_arcsec = moon.altitude_sigma.value().to_degrees() * 3600.0,
                "demo: computed apparent place"
            );
            ("Moon".to_string(), moon)
        }
        Err(_) => {
            // Synthesize a second body at azimuth perpendicular
            // to the Sun and a moderate altitude. Names it
            // "Synthetic2" so the operator-facing log makes it
            // clear this is not a real observation.
            tracing::warn!(
                "demo: Moon below horizon at this UTC; \
                     synthesizing a second body 90° from the Sun for \
                     demo geometry"
            );
            let synth = bris_almanac::ApparentPlace {
                direction: bris_almanac::Horizontal {
                    altitude: 30.0_f64.to_radians(),
                    azimuth: (sun.direction.azimuth + std::f64::consts::FRAC_PI_2)
                        .rem_euclid(std::f64::consts::TAU),
                },
                altitude_sigma: sun.altitude_sigma,
            };
            ("Synthetic2".to_string(), synth)
        }
    };
    let (second_name, second_ap) = second_sight;

    // Synthesize observed altitudes 1 arcmin higher than computed for
    // each body (so each LOP has a +1 nm intercept toward its body).
    let arcmin_rad = 1.0_f64.to_radians() / 60.0;
    let observed_sun = Uncertain::new(
        sun.direction.altitude + arcmin_rad,
        Sigma::new(arcmin_rad * 0.5).unwrap(),
    );
    let observed_second = Uncertain::new(
        second_ap.direction.altitude + arcmin_rad,
        Sigma::new(arcmin_rad * 0.5).unwrap(),
    );
    let computed_sun = Uncertain::new(sun.direction.altitude, sun.altitude_sigma);
    let computed_second = Uncertain::new(second_ap.direction.altitude, second_ap.altitude_sigma);

    let sun_lop = line_of_position(
        observer.latitude,
        observer.longitude,
        observed_sun,
        computed_sun,
        sun.direction.azimuth,
    )?;
    let second_lop = line_of_position(
        observer.latitude,
        observer.longitude,
        observed_second,
        computed_second,
        second_ap.direction.azimuth,
    )?;
    info!(
        sun_intercept_nm = sun_lop.intercept_nm,
        sun_sigma_nm = sun_lop.intercept_sigma_nm.value(),
        second_body = %second_name,
        second_intercept_nm = second_lop.intercept_nm,
        second_sigma_nm = second_lop.intercept_sigma_nm.value(),
        "demo: per-sight LOPs"
    );

    // Screen for blunders, then solve.
    let screened = screen_sights(&[sun_lop, second_lop], ScreeningConfig::default());
    if !screened.rejected.is_empty() {
        for (idx, _, reason) in &screened.rejected {
            tracing::warn!(idx = idx, reason = %reason, "demo: sight rejected");
        }
    }
    let fix = multi_sight_fix(&screened.kept)?;
    info!(
        lat_deg = fix.lat.degrees(),
        lon_deg = fix.lon.degrees(),
        sigma_major_nm = fix.sigma_major_nm,
        sigma_minor_nm = fix.sigma_minor_nm,
        sigma_nm = fix.sigma_nm().value(),
        sight_count = fix.sight_count,
        "demo: fix"
    );

    // Emit NMEA. Each formatter logs at debug level via tracing::debug!,
    // which under our default subscriber level (debug) goes to stderr.
    info!("demo: emitting NMEA sentences (each one logs at debug level)");
    let quality = QualityThresholds::default().classify(fix.sigma_nm().value());
    let _ = gpgll(&fix, utc, quality);
    let _ = gprmc(&fix, utc, quality);
    let _ = gpgga(&fix, utc, quality);
    let _ = gpgst(&fix, utc);

    let budget = UncertaintyBudget {
        centroid_nm: 0.05,
        horizon_nm: 0.10,
        calibration_nm: 0.20,
        stitching_nm: 0.0,
        refraction_nm: 0.05,
        dip_nm: 0.05,
        timing_nm: 0.0,
    };
    let time_diag = TimeDiagnostic {
        seconds_since_sync: Some(60),
        drift_ppm: None,
        step_detected: false,
    };
    let counters = ErrorCounters::default();
    let sights_for_pbris: Vec<(String, f64, f64, _)> = vec![
        (
            "Sun".to_string(),
            sun.direction.altitude,
            sun.direction.azimuth,
            sun_lop,
        ),
        (
            second_name,
            second_ap.direction.altitude,
            second_ap.direction.azimuth,
            second_lop,
        ),
    ];
    let _ = pbris_full(
        utc,
        &fix,
        &time_diag,
        &budget,
        &sights_for_pbris,
        &counters,
        true,
    );

    info!(
        "demo: end-to-end synthetic fix complete. \
         A real deployment would write the NMEA bytes to TCP/UDP/serial; \
         for this demo we discard them but the debug logs above show \
         exactly what was emitted."
    );
    Ok(())
}

fn utc_to_jd_utc(utc: chrono::DateTime<Utc>) -> f64 {
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
