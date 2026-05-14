//! Format an engine [`PublishedFix`] as a complete NMEA 0183
//! sentence batch ready to ship over a transport.
//!
//! The streaming engine produces [`PublishedFix`] values on
//! its [`crate::StreamingEngine::fix_stream`] channel; this
//! module's [`format_fix_as_nmea`] turns one of those into
//! the bytes a chartplotter sees. Splitting the formatter
//! out from the transport lets the headless CLI, the FFI
//! shells, and tests share one well-tested formatting path.
//!
//! # Sentence set
//!
//! For each fix we emit, in canonical order:
//!
//! 1. `$GPGLL` — geographic position (lat/lon).
//! 2. `$GPRMC` — recommended minimum (lat/lon + status).
//! 3. `$GPGGA` — GPS-style fix data (lat/lon + quality).
//! 4. `$GPGST` — pseudorange error statistics (1σ ellipse).
//! 5. `$PBRIS,FIX` — engine summary (`n_sights`, azimuth
//!    spread, oldest-sight age, dominant σ source).
//!
//! Operators familiar with marine NMEA will recognize the
//! first four as the standard set `OpenCPN`, `MaxSea`, and
//! similar consumers expect. `$PBRIS,FIX` is Bris-specific
//! and ignored by standard consumers.
//!
//! Other `$PBRIS,*` subtypes (`UNC`, `TIME`, `ERR`, per-
//! `SIGHT`) carry per-source detail the engine's
//! [`PublishedFix`] doesn't yet expose. They land when the
//! engine starts populating its uncertainty budget breakdown
//! and per-sight LOPs on the published value.
//!
//! # Timestamping
//!
//! NMEA sentences carry a wall-clock timestamp. The engine's
//! [`PublishedFix::timestamp`] is in TT (the time the
//! contributing frames were captured); for NMEA we want
//! "now" as the consumer sees it. Callers pass `Utc::now()`
//! at emission time. This is the standard convention for
//! NMEA-emitting hardware: the timestamp is when the bytes
//! left the device, not when the underlying measurement was
//! taken.

use crate::PublishedFix;
use bris_nmea::{gpgga, gpgll, gpgst, gprmc, pbris_fix, QualityThresholds};
use chrono::{DateTime, Utc};

/// Format one [`PublishedFix`] as a concatenated batch of
/// NMEA 0183 sentences (each `$...*XX\r\n`).
///
/// The returned `String` contains the bytes a transport
/// (serial / TCP / UDP) writes to its sink. Each sentence is
/// terminated with `\r\n` per NMEA convention.
///
/// `utc` is the wall-clock instant at *emission*; callers
/// pass [`chrono::Utc::now()`] in production. Tests pass
/// fixed values for determinism.
///
/// `quality_thresholds` controls the red/yellow/green
/// classification that drives the `$GPGGA` quality byte and
/// the `$GPGLL` / `$GPRMC` status (`A` valid vs `V` void).
/// Pass [`QualityThresholds::default`] for the
/// design-doc-recommended cutoffs.
#[must_use]
pub fn format_fix_as_nmea(
    fix: &PublishedFix,
    utc: DateTime<Utc>,
    quality_thresholds: QualityThresholds,
) -> String {
    let quality = quality_thresholds.classify(fix.fix.sigma_nm().value());
    let mut out = String::new();
    out.push_str(&gpgll(&fix.fix, utc, quality));
    out.push_str(&gprmc(&fix.fix, utc, quality));
    out.push_str(&gpgga(&fix.fix, utc, quality));
    out.push_str(&gpgst(&fix.fix, utc));
    out.push_str(&pbris_fix(utc, &fix.to_pbris_fix_summary()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DominantSource;
    use bris_core::time::{Tt, JD_J2000};
    use bris_core::{Latitude, Longitude};
    use bris_nav::Fix;
    use chrono::TimeZone;

    fn sample_fix() -> PublishedFix {
        PublishedFix {
            fix: Fix {
                lat: Latitude::from_degrees(47.6).unwrap(),
                lon: Longitude::from_degrees(-122.3).unwrap(),
                covariance_nm2: [[0.25, 0.0], [0.0, 0.25]],
                sigma_major_nm: 0.5,
                sigma_minor_nm: 0.5,
                orientation_rad: 0.0,
                sight_count: 3,
            },
            n_sights: 3,
            azimuth_spread_rad: std::f64::consts::FRAC_PI_2,
            oldest_sight_age_seconds: 60.0,
            dominant_source: DominantSource::Horizon,
            timestamp: Tt::from_julian_date(JD_J2000),
        }
    }

    fn sample_utc() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 6, 15, 12, 34, 56).unwrap()
    }

    #[test]
    fn output_contains_all_five_sentences_in_order() {
        let s = format_fix_as_nmea(&sample_fix(), sample_utc(), QualityThresholds::default());
        let gll = s.find("$GPGLL").expect("missing $GPGLL");
        let rmc = s.find("$GPRMC").expect("missing $GPRMC");
        let gga = s.find("$GPGGA").expect("missing $GPGGA");
        let gst = s.find("$GPGST").expect("missing $GPGST");
        let pbris = s.find("$PBRIS,FIX").expect("missing $PBRIS,FIX");
        assert!(
            gll < rmc && rmc < gga && gga < gst && gst < pbris,
            "sentences out of canonical order in:\n{s}"
        );
    }

    #[test]
    fn each_sentence_terminates_with_crlf() {
        let s = format_fix_as_nmea(&sample_fix(), sample_utc(), QualityThresholds::default());
        // Five sentences = five \r\n.
        let crlf_count = s.matches("\r\n").count();
        assert_eq!(crlf_count, 5, "expected 5 \\r\\n terminators in:\n{s}");
    }

    #[test]
    fn pbris_fix_includes_engine_summary_fields() {
        let s = format_fix_as_nmea(&sample_fix(), sample_utc(), QualityThresholds::default());
        // n_sights=3, azimuth_spread π/2 ≈ 90°, dominant=horizon.
        assert!(s.contains(",3,"), "missing n_sights=3 in:\n{s}");
        assert!(s.contains(",90.00,"), "missing 90° spread in:\n{s}");
        assert!(s.contains(",horizon*"), "missing horizon dominant in:\n{s}");
    }
}
