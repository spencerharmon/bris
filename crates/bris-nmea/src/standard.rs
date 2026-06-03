//! Standard NMEA 0183 sentence formatters: `$GPGLL`, `$GPRMC`,
//! `$GPGGA`, `$GPGST`.
//!
//! Each function takes a typed `Fix` (and a UTC instant for the
//! sentences that need a timestamp) and returns the formatted sentence
//! ready for transport. Every emission logs at `debug` level via
//! [`tracing`] so deployments can observe exactly what's going on the
//! wire — under `RUST_LOG=bris_nmea=debug` or via the journald
//! subscriber on the embedded image.
//!
//! # Quality field policy
//!
//! Per `plan.org` Phase 5, the `$GPGGA` quality field and the
//! `$GPRMC` status field degrade based on the *combined* fix
//! uncertainty (not just one source). The thresholds are taken
//! from a [`QualityThresholds`] struct so callers can override
//! based on user configuration.
//!
//! # Sign conventions
//!
//! NMEA 0183 expresses latitude and longitude as
//! `DDMM.MMMM,N|S` (lat) and `DDDMM.MMMM,E|W` (lon). The hemisphere
//! letter encodes the sign; the numeric magnitude is always positive.

use crate::checksum::format_sentence;
use bris_core::{Latitude, Longitude};
use bris_nav::Fix;
use chrono::{DateTime, Datelike, Timelike, Utc};
use tracing::debug;

/// The standard `$GPGGA` quality indicator.
///
/// Bris uses three values:
/// - `1` — valid GPS-equivalent fix.
/// - `6` — dead-reckoning / degraded fix (chartplotter shows DR mode).
/// - `0` — invalid fix (chartplotter raises "GPS lost" alarm).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixQuality {
    /// `1` — valid fix.
    Valid,
    /// `6` — fix produced but uncertainty exceeds the user's
    /// "acceptable" threshold; chartplotter shows DR mode.
    Degraded,
    /// `0` — fix exceeds the hard-invalid threshold; chartplotter
    /// raises "GPS lost".
    Invalid,
}

impl FixQuality {
    fn gga_digit(self) -> char {
        match self {
            Self::Valid => '1',
            Self::Degraded => '6',
            Self::Invalid => '0',
        }
    }

    fn rmc_status(self) -> char {
        match self {
            Self::Valid | Self::Degraded => 'A',
            Self::Invalid => 'V',
        }
    }
}

/// Thresholds for [`FixQuality`] degradation, in nautical miles of 1σ
/// position uncertainty.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityThresholds {
    /// Below this, fix is reported as valid (GGA quality `1`).
    pub valid_max_nm: f64,
    /// Between `valid_max_nm` and `invalid_min_nm`, fix is reported as
    /// degraded (GGA quality `6`, RMC status `A`).
    pub invalid_min_nm: f64,
}

impl Default for QualityThresholds {
    /// Conservative defaults: valid below 1 nm, invalid above 10 nm.
    /// Real deployments override based on the user's configured
    /// "target accuracy" slider.
    fn default() -> Self {
        Self {
            valid_max_nm: 1.0,
            invalid_min_nm: 10.0,
        }
    }
}

impl QualityThresholds {
    /// Classify the given fix's σ.
    #[must_use]
    pub fn classify(self, sigma_nm: f64) -> FixQuality {
        if sigma_nm.is_nan() || sigma_nm >= self.invalid_min_nm {
            FixQuality::Invalid
        } else if sigma_nm <= self.valid_max_nm {
            FixQuality::Valid
        } else {
            FixQuality::Degraded
        }
    }
}

/// Emit `$GPGLL` (geographic position) for the given fix and time.
///
/// Format: `$GPGLL,llll.ll,a,yyyyy.yy,a,hhmmss.ss,A,A*XX`
///   - lat `llll.ll,a` (DDMM.MM, hemisphere)
///   - lon `yyyyy.yy,a` (DDDMM.MM, hemisphere)
///   - UTC time `hhmmss.ss`
///   - Status (`A`/`V`)
///   - FAA mode indicator (NMEA 2.3+); we emit `A` for "Autonomous"
///     (or `N` for "Not valid" when status is `V`).
#[must_use]
pub fn gpgll(fix: &Fix, utc: DateTime<Utc>, quality: FixQuality) -> String {
    let body = format!(
        "GPGLL,{},{},{},{},{},{},{}",
        format_lat(fix.lat),
        if fix.lat.degrees() >= 0.0 { 'N' } else { 'S' },
        format_lon(fix.lon),
        if fix.lon.degrees() >= 0.0 { 'E' } else { 'W' },
        format_hms(utc),
        quality.rmc_status(),
        if quality == FixQuality::Invalid {
            'N'
        } else {
            'A'
        },
    );
    let s = format_sentence(&body);
    debug!(
        sentence = "$GPGLL",
        sigma_nm = fix.sigma_nm().value(),
        bytes = s.trim_end_matches("\r\n"),
        "emitted NMEA"
    );
    s
}

/// Emit `$GPRMC` (recommended minimum) for the given fix and time.
///
/// Format: `$GPRMC,hhmmss.ss,A,llll.ll,a,yyyyy.yy,a,sss.s,ccc.c,ddmmyy,,,A*XX`
/// We emit empty fields for SOG/COG (Bris does not track them) and an
/// empty magnetic variation (we don't compute it). The FAA mode
/// indicator at the end is `A` for valid/degraded, `N` for invalid.
#[must_use]
pub fn gprmc(fix: &Fix, utc: DateTime<Utc>, quality: FixQuality) -> String {
    let body = format!(
        // time, status, lat, N/S, lon, E/W, sog, cog (empty),
        // date, mvar (empty), mvar_dir (empty), faa_mode
        "GPRMC,{},{},{},{},{},{},{},,{},,,{}",
        format_hms(utc),
        quality.rmc_status(),
        format_lat(fix.lat),
        if fix.lat.degrees() >= 0.0 { 'N' } else { 'S' },
        format_lon(fix.lon),
        if fix.lon.degrees() >= 0.0 { 'E' } else { 'W' },
        "", // SOG knots — not tracked
        format_dmy(utc),
        if quality == FixQuality::Invalid {
            'N'
        } else {
            'A'
        },
    );
    let s = format_sentence(&body);
    debug!(
        sentence = "$GPRMC",
        sigma_nm = fix.sigma_nm().value(),
        bytes = s.trim_end_matches("\r\n"),
        "emitted NMEA"
    );
    s
}

/// Emit `$GPGGA` (fix data) for the given fix and time.
///
/// Format: `$GPGGA,hhmmss.ss,llll.ll,a,yyyyy.yy,a,q,nn,h.h,a.a,M,g.g,M,,*XX`
/// Bris emits:
///   - q (quality): 1/6/0 from [`FixQuality`].
///   - nn (number of sights used): from `fix.sight_count`.
///   - h.h (HDOP): set to a coarse approximation derived from the
///     ellipse axes (HDOP ≈ `σ_major` / 1 nm, conservative).
///   - a.a (altitude): 0.0 (sea-level convention).
///   - g.g (geoid separation): empty.
///   - DGPS station / age: empty.
#[must_use]
pub fn gpgga(fix: &Fix, utc: DateTime<Utc>, quality: FixQuality) -> String {
    let hdop_approx = fix.sigma_major_nm.max(0.1);
    let body = format!(
        "GPGGA,{},{},{},{},{},{},{:02},{:.1},{:.1},M,,,,,",
        format_hms(utc),
        format_lat(fix.lat),
        if fix.lat.degrees() >= 0.0 { 'N' } else { 'S' },
        format_lon(fix.lon),
        if fix.lon.degrees() >= 0.0 { 'E' } else { 'W' },
        quality.gga_digit(),
        fix.sight_count.min(99),
        hdop_approx,
        0.0_f64, // altitude (sea level)
    );
    let s = format_sentence(&body);
    debug!(
        sentence = "$GPGGA",
        sigma_nm = fix.sigma_nm().value(),
        sight_count = fix.sight_count,
        bytes = s.trim_end_matches("\r\n"),
        "emitted NMEA"
    );
    s
}

/// Emit `$GPGST` (pseudorange error statistics) for the given fix.
///
/// This is the standards-compliant uncertainty channel. `OpenCPN` parses
/// it; many fixed-mount chartplotters ignore it. Format:
/// `$GPGST,hhmmss.ss,r.r,a,b,c,d.d,e.e,f.f*XX`
///   - `r.r`: total RMS standard deviation (m). We use `σ_total = √(σ_major²+σ_minor²) / √2` ≈ geometric mean × √2.
///   - `a`: σ semi-major (m).
///   - `b`: σ semi-minor (m).
///   - `c`: orientation of major axis (degrees from north).
///   - `d.d`: `σ_lat` (m).
///   - `e.e`: `σ_lon` (m).
///   - `f.f`: `σ_alt` (m). Bris doesn't compute altitude uncertainty;
///     emit empty.
#[must_use]
#[allow(clippy::similar_names)] // sigma_n_m, sigma_e_m, sigma_major_m, sigma_minor_m
pub fn gpgst(fix: &Fix, utc: DateTime<Utc>) -> String {
    let nm_to_m = 1852.0;
    let sigma_major_m = fix.sigma_major_nm * nm_to_m;
    let sigma_minor_m = fix.sigma_minor_nm * nm_to_m;
    let sigma_n_m = fix.covariance_nm2[0][0].max(0.0).sqrt() * nm_to_m;
    let sigma_e_m = fix.covariance_nm2[1][1].max(0.0).sqrt() * nm_to_m;
    let total_rms_m = (sigma_n_m * sigma_n_m + sigma_e_m * sigma_e_m).sqrt();
    let body = format!(
        "GPGST,{},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},",
        format_hms(utc),
        total_rms_m,
        sigma_major_m,
        sigma_minor_m,
        fix.orientation_rad.to_degrees(),
        sigma_n_m,
        sigma_e_m,
    );
    let s = format_sentence(&body);
    debug!(
        sentence = "$GPGST",
        sigma_nm = fix.sigma_nm().value(),
        sigma_major_nm = fix.sigma_major_nm,
        sigma_minor_nm = fix.sigma_minor_nm,
        bytes = s.trim_end_matches("\r\n"),
        "emitted NMEA"
    );
    s
}

/// Format latitude as `DDMM.MMMM`, magnitude only (hemisphere is
/// a separate field).
fn format_lat(lat: Latitude) -> String {
    let deg = lat.degrees().abs();
    let d = deg.trunc();
    let m = (deg - d) * 60.0;
    format!("{d:02}{m:07.4}")
}

/// Format longitude as `DDDMM.MMMM`, magnitude only.
fn format_lon(lon: Longitude) -> String {
    let deg = lon.degrees().abs();
    let d = deg.trunc();
    let m = (deg - d) * 60.0;
    format!("{d:03}{m:07.4}")
}

/// Format UTC time as `hhmmss.ss`.
fn format_hms(utc: DateTime<Utc>) -> String {
    let h = utc.hour();
    let m = utc.minute();
    let s = f64::from(utc.second()) + f64::from(utc.nanosecond()) * 1e-9;
    format!("{h:02}{m:02}{s:05.2}")
}

/// Format date as `ddmmyy`.
fn format_dmy(utc: DateTime<Utc>) -> String {
    let d = utc.day();
    let m = utc.month();
    let y = utc.year() % 100;
    format!("{d:02}{m:02}{y:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_core::Sigma;
    use chrono::TimeZone;

    fn sample_fix(sigma_nm: f64) -> Fix {
        let _ = Sigma::new(sigma_nm);
        Fix {
            lat: Latitude::from_degrees(47.6062).unwrap(),
            lon: Longitude::from_degrees(-122.3321).unwrap(),
            covariance_nm2: [[sigma_nm * sigma_nm, 0.0], [0.0, sigma_nm * sigma_nm]],
            sigma_major_nm: sigma_nm,
            sigma_minor_nm: sigma_nm,
            orientation_rad: 0.0,
            sight_count: 3,
            chi_square: None,
        }
    }

    fn sample_utc() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 6, 15, 12, 34, 56).unwrap()
    }

    #[test]
    fn quality_thresholds_classify() {
        let t = QualityThresholds::default();
        assert_eq!(t.classify(0.5), FixQuality::Valid);
        assert_eq!(t.classify(5.0), FixQuality::Degraded);
        assert_eq!(t.classify(20.0), FixQuality::Invalid);
        assert_eq!(t.classify(f64::NAN), FixQuality::Invalid);
    }

    #[test]
    fn gpgll_format_and_checksum_valid() {
        let fix = sample_fix(0.5);
        let s = gpgll(&fix, sample_utc(), FixQuality::Valid);
        // Should start with $GPGLL, end with *XX\r\n, and contain the
        // hemisphere letters.
        assert!(s.starts_with("$GPGLL,"));
        assert!(s.ends_with("\r\n"));
        assert!(s.contains(",N,"));
        assert!(s.contains(",W,"));
        // Self-checksum: parse out the body and verify.
        let body = &s[1..s.len() - 5];
        let checksum_str = &s[s.len() - 4..s.len() - 2];
        let computed = crate::checksum::checksum(body);
        let parsed = u8::from_str_radix(checksum_str, 16).unwrap();
        assert_eq!(computed, parsed);
    }

    #[test]
    fn gprmc_status_v_when_invalid() {
        let fix = sample_fix(20.0);
        let s = gprmc(&fix, sample_utc(), FixQuality::Invalid);
        assert!(s.contains(",V,"));
        // Date should be 150624 for 2024-06-15.
        assert!(s.contains(",150624,"));
    }

    #[test]
    fn gpgga_quality_digit_reflects_classification() {
        let fix = sample_fix(0.5);
        let v = gpgga(&fix, sample_utc(), FixQuality::Valid);
        let d = gpgga(&fix, sample_utc(), FixQuality::Degraded);
        let i = gpgga(&fix, sample_utc(), FixQuality::Invalid);
        // Quality field is the 7th comma-separated field.
        let extract = |s: &str| -> char {
            let body = &s[1..s.len() - 5];
            body.split(',').nth(6).unwrap().chars().next().unwrap()
        };
        assert_eq!(extract(&v), '1');
        assert_eq!(extract(&d), '6');
        assert_eq!(extract(&i), '0');
    }

    #[test]
    fn gpgst_emits_uncertainty_in_meters() {
        let fix = sample_fix(1.0);
        let s = gpgst(&fix, sample_utc());
        // 1 nm = 1852 m. The semi-major / semi-minor fields should
        // be near 1852.0.
        assert!(s.contains("1852.0"));
        assert!(s.starts_with("$GPGST,"));
    }

    #[test]
    fn lat_lon_formatting_round_trips_known_values() {
        // 47.6062°N → 47°36.372'N → "4736.3720"
        let lat = Latitude::from_degrees(47.6062).unwrap();
        assert_eq!(format_lat(lat), "4736.3720");
        // -122.3321°W → 122°19.926'W → "12219.9260"
        let lon = Longitude::from_degrees(-122.3321).unwrap();
        assert_eq!(format_lon(lon), "12219.9260");
    }

    #[test]
    fn format_hms_known_value() {
        let s = format_hms(sample_utc());
        assert_eq!(s, "123456.00");
    }

    #[test]
    fn debug_logging_includes_sentence_bytes() {
        // Capture tracing output and verify the debug log fired.
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct VecWriter(Arc<Mutex<Vec<u8>>>);
        impl<'a> MakeWriter<'a> for VecWriter {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        impl std::io::Write for VecWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = VecWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let fix = sample_fix(0.5);
            let _ = gpgll(&fix, sample_utc(), FixQuality::Valid);
        });

        let logged = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            logged.contains("emitted NMEA"),
            "expected debug log; got: {logged}"
        );
        assert!(
            logged.contains("$GPGLL"),
            "expected sentence type in log; got: {logged}"
        );
    }
}
