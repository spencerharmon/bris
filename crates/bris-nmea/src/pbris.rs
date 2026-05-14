//! Proprietary `$PBRIS,*` NMEA 0183 sentences for Bris diagnostics.
//!
//! NMEA 0183 reserves `$P<mfg>...` for vendor extensions. Standard
//! consumers ignore unknown sentences; Bris-aware tooling consumes
//! these to recover per-source uncertainty contributions, time-state
//! diagnostics, per-sight residuals, and the dominant-source field.
//!
//! NMEA 0183 caps each sentence at 82 characters, so `$PBRIS` is
//! split into typed subtypes. Downstream tools reassemble subtypes
//! by their shared timestamp.
//!
//! See `docs/protocol/pbris.md` for the canonical wire spec.
//!
//! Every emission logs at `debug` level via [`tracing`].

use crate::checksum::format_sentence;
use bris_nav::{Fix, LineOfPosition};
use chrono::{DateTime, Timelike, Utc};
use tracing::debug;

/// Schema version emitted in `$PBRIS,VER`. Bump when fields change.
pub const PBRIS_SCHEMA_VERSION: u32 = 1;

/// Emit `$PBRIS,VER` at session start so consumers can detect
/// schema changes.
///
/// Format: `$PBRIS,VER,<schema_version>*XX`.
#[must_use]
pub fn pbris_ver() -> String {
    let body = format!("PBRIS,VER,{PBRIS_SCHEMA_VERSION}");
    let s = format_sentence(&body);
    debug!(
        sentence = "$PBRIS,VER",
        schema = PBRIS_SCHEMA_VERSION,
        bytes = s.trim_end_matches("\r\n"),
        "emitted PBRIS"
    );
    s
}

/// Time-state diagnostic.
#[derive(Debug, Clone, Copy)]
pub struct TimeDiagnostic {
    /// Seconds since the most recent successful NTP sync.
    /// `None` if the system has never synced.
    pub seconds_since_sync: Option<u64>,
    /// Estimated local oscillator drift, parts per million.
    /// `None` if drift learning is disabled or has insufficient data.
    pub drift_ppm: Option<f64>,
    /// True if a clock step was detected since the last fix.
    pub step_detected: bool,
}

/// Emit `$PBRIS,TIME,...`.
///
/// Format: `$PBRIS,TIME,hhmmss.ss,<sec_since_sync>,<drift_ppm>,<step:0|1>*XX`
#[must_use]
pub fn pbris_time(utc: DateTime<Utc>, diag: &TimeDiagnostic) -> String {
    let sync_str = diag
        .seconds_since_sync
        .map(|s| s.to_string())
        .unwrap_or_default();
    let drift_str = diag
        .drift_ppm
        .map(|d| format!("{d:.3}"))
        .unwrap_or_default();
    let step = u8::from(diag.step_detected);
    let body = format!(
        "PBRIS,TIME,{},{},{},{}",
        format_hms(utc),
        sync_str,
        drift_str,
        step,
    );
    let s = format_sentence(&body);
    debug!(
        sentence = "$PBRIS,TIME",
        seconds_since_sync = ?diag.seconds_since_sync,
        drift_ppm = ?diag.drift_ppm,
        step_detected = diag.step_detected,
        bytes = s.trim_end_matches("\r\n"),
        "emitted PBRIS"
    );
    s
}

/// Per-source uncertainty contribution to the current fix, in nm.
///
/// Each field is the 1σ contribution from that error source. Quadrature
/// sum equals (approximately) the fix's overall σ. The field whose
/// magnitude is largest is the *dominant source* — the operator's
/// remediation guide.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_field_names)] // every field is in nm by design.
pub struct UncertaintyBudget {
    /// Body centroiding (vision pipeline).
    pub centroid_nm: f64,
    /// Horizon line fit (vision pipeline).
    pub horizon_nm: f64,
    /// Lens calibration residual (per-device intrinsics quality).
    pub calibration_nm: f64,
    /// Stitching alignment residual (cross-frame pose chain).
    pub stitching_nm: f64,
    /// Atmospheric refraction model.
    pub refraction_nm: f64,
    /// Horizon dip (eye-height uncertainty).
    pub dip_nm: f64,
    /// Timing (NTP staleness, drift, step events).
    pub timing_nm: f64,
}

impl UncertaintyBudget {
    /// Field name of the largest contributor.
    #[must_use]
    pub fn dominant_source(&self) -> &'static str {
        let entries = [
            ("centroid", self.centroid_nm),
            ("horizon", self.horizon_nm),
            ("calibration", self.calibration_nm),
            ("stitching", self.stitching_nm),
            ("refraction", self.refraction_nm),
            ("dip", self.dip_nm),
            ("timing", self.timing_nm),
        ];
        entries
            .iter()
            .copied()
            .filter(|(_, v)| v.is_finite())
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map_or("none", |(name, _)| name)
    }
}

/// Emit `$PBRIS,UNC,...`.
///
/// Format:
/// `$PBRIS,UNC,hhmmss.ss,<centroid>,<horizon>,<calibration>,<stitching>,<refraction>,<dip>,<timing>,<dominant>*XX`
/// All numeric fields are in nautical miles, fixed-point 4 decimal places.
#[must_use]
pub fn pbris_unc(utc: DateTime<Utc>, budget: &UncertaintyBudget) -> String {
    let body = format!(
        "PBRIS,UNC,{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{}",
        format_hms(utc),
        budget.centroid_nm,
        budget.horizon_nm,
        budget.calibration_nm,
        budget.stitching_nm,
        budget.refraction_nm,
        budget.dip_nm,
        budget.timing_nm,
        budget.dominant_source(),
    );
    let s = format_sentence(&body);
    debug!(
        sentence = "$PBRIS,UNC",
        dominant = budget.dominant_source(),
        bytes = s.trim_end_matches("\r\n"),
        "emitted PBRIS"
    );
    s
}

/// Engine-level per-fix summary: the operator-meaningful
/// "should I trust this fix?" diagnostics that don't fit on
/// the standard `$GP*` sentences and aren't naturally part of
/// `$PBRIS,UNC` (which is per-source, not per-fix). Carried
/// by the `$PBRIS,FIX` sentence.
#[derive(Debug, Clone, Copy)]
pub struct FixSummary {
    /// Number of sights contributing to the fix. ≤ 99 in
    /// practice given the sight-window cap of 10; 2-byte
    /// field width is generous for unforeseen overrides.
    pub n_sights: u32,
    /// Spread, in radians, between the maximum and minimum
    /// azimuth across the contributing sights, accounting
    /// for [0, 2π) wrap. A small spread (e.g. < 0.5 rad ≈
    /// 30°) means the position covariance is poorly
    /// conditioned — the fix is "good along one direction,
    /// weak across it."
    pub azimuth_spread_rad: f64,
    /// Age of the oldest sight contributing to the fix, in
    /// seconds. ≤ 99 999 by the sight window's 10-min cap
    /// in practice; 5-byte field width is generous.
    pub oldest_sight_age_s: u32,
    /// Stable string label of the per-source σ component
    /// dominating the fix uncertainty. The streaming engine
    /// computes this from its uncertainty budget; consumers
    /// of this sentence can map it back to the operator
    /// remediation guide ("centroid → longer exposure",
    /// "horizon → pan to a cleaner stretch", etc.).
    pub dominant_source: &'static str,
}

/// Emit `$PBRIS,FIX,...`.
///
/// Format:
/// `$PBRIS,FIX,hhmmss.ss,<n_sights>,<az_spread_deg>,<oldest_age_s>,<dominant>*XX`
///
/// Azimuth spread is emitted in degrees (not radians) for
/// operator readability; consumers convert to radians if they
/// need them. Two-decimal precision (max 360.00 → 6 chars).
///
/// Holds well under the NMEA 0183 82-char-per-sentence cap:
/// even at maximum field widths the body fits in ~50 chars.
#[must_use]
pub fn pbris_fix(utc: DateTime<Utc>, summary: &FixSummary) -> String {
    let body = format!(
        "PBRIS,FIX,{},{},{:.2},{},{}",
        format_hms(utc),
        summary.n_sights,
        summary.azimuth_spread_rad.to_degrees(),
        summary.oldest_sight_age_s,
        summary.dominant_source,
    );
    let s = format_sentence(&body);
    debug!(
        sentence = "$PBRIS,FIX",
        n_sights = summary.n_sights,
        azimuth_spread_deg = summary.azimuth_spread_rad.to_degrees(),
        oldest_sight_age_s = summary.oldest_sight_age_s,
        dominant_source = summary.dominant_source,
        bytes = s.trim_end_matches("\r\n"),
        "emitted PBRIS"
    );
    s
}

/// Emit one `$PBRIS,SIGHT,n,...` sentence per sight in the current fix.
///
/// Format:
/// `$PBRIS,SIGHT,<n>,<body_name>,<altitude_deg>,<azimuth_deg>,<intercept_nm>,<sigma_nm>*XX`
#[must_use]
pub fn pbris_sight(
    index: u32,
    body_name: &str,
    altitude_rad: f64,
    azimuth_rad: f64,
    lop: &LineOfPosition,
) -> String {
    let body = format!(
        "PBRIS,SIGHT,{},{},{:.4},{:.4},{:.3},{:.3}",
        index,
        body_name,
        altitude_rad.to_degrees(),
        azimuth_rad.to_degrees(),
        lop.intercept_nm,
        lop.intercept_sigma_nm.value(),
    );
    let s = format_sentence(&body);
    debug!(
        sentence = "$PBRIS,SIGHT",
        index = index,
        body = body_name,
        intercept_nm = lop.intercept_nm,
        bytes = s.trim_end_matches("\r\n"),
        "emitted PBRIS"
    );
    s
}

/// Counters for capture/processing errors since the previous fix.
#[derive(Debug, Clone, Copy, Default)]
pub struct ErrorCounters {
    /// Frames dropped at capture (camera, queue overflow).
    pub frames_dropped: u32,
    /// Horizon detections that failed (insufficient candidates / low
    /// confidence).
    pub horizon_failures: u32,
    /// Centroiding failures (no bright region / too small).
    pub centroid_failures: u32,
    /// Sights rejected by `bris_nav::screen_sights`.
    pub sights_rejected: u32,
}

/// Emit `$PBRIS,ERR,...`.
///
/// Format: `$PBRIS,ERR,hhmmss.ss,<frames_dropped>,<horizon_fails>,<centroid_fails>,<sights_rejected>*XX`
#[must_use]
pub fn pbris_err(utc: DateTime<Utc>, counters: &ErrorCounters) -> String {
    let body = format!(
        "PBRIS,ERR,{},{},{},{},{}",
        format_hms(utc),
        counters.frames_dropped,
        counters.horizon_failures,
        counters.centroid_failures,
        counters.sights_rejected,
    );
    let s = format_sentence(&body);
    debug!(
        sentence = "$PBRIS,ERR",
        frames_dropped = counters.frames_dropped,
        horizon_failures = counters.horizon_failures,
        centroid_failures = counters.centroid_failures,
        sights_rejected = counters.sights_rejected,
        bytes = s.trim_end_matches("\r\n"),
        "emitted PBRIS"
    );
    s
}

/// Emit the full `$PBRIS` set for one fix in canonical order.
///
/// Order: VER (only if requested via `include_ver`), TIME, UNC,
/// SIGHT × N, ERR. Returns the concatenated bytes ready for transport.
#[must_use]
pub fn pbris_full(
    utc: DateTime<Utc>,
    fix: &Fix,
    time_diag: &TimeDiagnostic,
    budget: &UncertaintyBudget,
    sights: &[(String, f64, f64, LineOfPosition)],
    counters: &ErrorCounters,
    include_ver: bool,
) -> String {
    let mut out = String::new();
    if include_ver {
        out.push_str(&pbris_ver());
    }
    out.push_str(&pbris_time(utc, time_diag));
    out.push_str(&pbris_unc(utc, budget));
    for (i, (body_name, alt, az, lop)) in sights.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        out.push_str(&pbris_sight(i as u32, body_name, *alt, *az, lop));
    }
    out.push_str(&pbris_err(utc, counters));
    debug!(
        sigma_nm = fix.sigma_nm().value(),
        sight_count = fix.sight_count,
        "emitted full PBRIS set for fix",
    );
    out
}

fn format_hms(utc: DateTime<Utc>) -> String {
    let h = utc.hour();
    let m = utc.minute();
    let s = f64::from(utc.second()) + f64::from(utc.nanosecond()) * 1e-9;
    format!("{h:02}{m:02}{s:05.2}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_core::{Latitude, Longitude, Sigma};
    use chrono::TimeZone;

    fn sample_utc() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 6, 15, 12, 34, 56).unwrap()
    }

    fn sample_fix() -> Fix {
        Fix {
            lat: Latitude::from_degrees(47.6).unwrap(),
            lon: Longitude::from_degrees(-122.3).unwrap(),
            covariance_nm2: [[0.25, 0.0], [0.0, 0.25]],
            sigma_major_nm: 0.5,
            sigma_minor_nm: 0.5,
            orientation_rad: 0.0,
            sight_count: 3,
        }
    }

    #[test]
    fn ver_includes_schema() {
        let s = pbris_ver();
        assert!(s.starts_with(&format!("$PBRIS,VER,{PBRIS_SCHEMA_VERSION}")));
        assert!(s.ends_with("\r\n"));
    }

    #[test]
    fn time_handles_no_sync() {
        let diag = TimeDiagnostic {
            seconds_since_sync: None,
            drift_ppm: None,
            step_detected: false,
        };
        let s = pbris_time(sample_utc(), &diag);
        // Two empty fields between the time and the step indicator.
        assert!(s.contains(",,,0*"), "got: {s}");
    }

    #[test]
    fn time_with_drift_and_step() {
        let diag = TimeDiagnostic {
            seconds_since_sync: Some(3600),
            drift_ppm: Some(12.345),
            step_detected: true,
        };
        let s = pbris_time(sample_utc(), &diag);
        assert!(s.contains(",3600,"));
        assert!(s.contains(",12.345,"));
        assert!(s.contains(",1*"));
    }

    #[test]
    fn dominant_source_picks_largest() {
        let budget = UncertaintyBudget {
            centroid_nm: 0.1,
            horizon_nm: 0.5,
            calibration_nm: 0.2,
            stitching_nm: 0.0,
            refraction_nm: 0.05,
            dip_nm: 0.05,
            timing_nm: 0.0,
        };
        assert_eq!(budget.dominant_source(), "horizon");
    }

    #[test]
    fn unc_emits_dominant_source() {
        let budget = UncertaintyBudget {
            centroid_nm: 0.1,
            horizon_nm: 1.5,
            calibration_nm: 0.2,
            stitching_nm: 0.0,
            refraction_nm: 0.05,
            dip_nm: 0.05,
            timing_nm: 0.0,
        };
        let s = pbris_unc(sample_utc(), &budget);
        assert!(s.contains(",horizon*"), "got: {s}");
    }

    #[test]
    fn sight_emits_index_and_body() {
        let lop = LineOfPosition {
            assumed_lat: Latitude::from_degrees(47.6).unwrap(),
            assumed_lon: Longitude::from_degrees(-122.3).unwrap(),
            azimuth_rad: 0.0,
            intercept_nm: 1.234,
            intercept_sigma_nm: Sigma::new(0.567).unwrap(),
        };
        let s = pbris_sight(
            2,
            "Sirius",
            30.0_f64.to_radians(),
            90.0_f64.to_radians(),
            &lop,
        );
        assert!(s.contains(",SIGHT,2,Sirius,"));
        assert!(s.contains(",1.234,"));
        assert!(s.contains(",0.567*"));
    }

    #[test]
    fn err_emits_counters() {
        let c = ErrorCounters {
            frames_dropped: 5,
            horizon_failures: 1,
            centroid_failures: 2,
            sights_rejected: 0,
        };
        let s = pbris_err(sample_utc(), &c);
        assert!(s.contains(",5,1,2,0*"));
    }

    #[test]
    fn full_set_includes_all_subtypes_in_order() {
        let fix = sample_fix();
        let lop = LineOfPosition {
            assumed_lat: fix.lat,
            assumed_lon: fix.lon,
            azimuth_rad: 0.0,
            intercept_nm: 0.5,
            intercept_sigma_nm: Sigma::new(0.1).unwrap(),
        };
        let sights = vec![("Sun".to_string(), 0.5, 0.0, lop)];
        let s = pbris_full(
            sample_utc(),
            &fix,
            &TimeDiagnostic {
                seconds_since_sync: Some(60),
                drift_ppm: None,
                step_detected: false,
            },
            &UncertaintyBudget {
                centroid_nm: 0.1,
                horizon_nm: 0.05,
                calibration_nm: 0.05,
                stitching_nm: 0.0,
                refraction_nm: 0.05,
                dip_nm: 0.05,
                timing_nm: 0.0,
            },
            &sights,
            &ErrorCounters::default(),
            true,
        );
        // Find each subtype in order.
        let ver = s.find("$PBRIS,VER").unwrap();
        let time = s.find("$PBRIS,TIME").unwrap();
        let unc = s.find("$PBRIS,UNC").unwrap();
        let sight = s.find("$PBRIS,SIGHT").unwrap();
        let err = s.find("$PBRIS,ERR").unwrap();
        assert!(
            ver < time && time < unc && unc < sight && sight < err,
            "subtypes out of order in: {s}"
        );
    }

    #[test]
    fn all_subtypes_under_82_char_nmea_limit() {
        let utc = sample_utc();
        let diag = TimeDiagnostic {
            seconds_since_sync: Some(1_000_000),
            drift_ppm: Some(99.999),
            step_detected: true,
        };
        let budget = UncertaintyBudget {
            centroid_nm: 9.999,
            horizon_nm: 9.999,
            calibration_nm: 9.999,
            stitching_nm: 9.999,
            refraction_nm: 9.999,
            dip_nm: 9.999,
            timing_nm: 9.999,
        };
        let lop = LineOfPosition {
            assumed_lat: Latitude::from_degrees(0.0).unwrap(),
            assumed_lon: Longitude::from_degrees(0.0).unwrap(),
            azimuth_rad: 0.0,
            intercept_nm: 9.999,
            intercept_sigma_nm: Sigma::new(9.999).unwrap(),
        };
        let counters = ErrorCounters {
            frames_dropped: 99_999,
            horizon_failures: 99_999,
            centroid_failures: 99_999,
            sights_rejected: 99_999,
        };
        for s in [
            pbris_ver(),
            pbris_time(utc, &diag),
            pbris_unc(utc, &budget),
            pbris_sight(255, "VeryLongStarName", 1.0, 1.0, &lop),
            pbris_err(utc, &counters),
            pbris_fix(
                utc,
                &FixSummary {
                    n_sights: 99,
                    azimuth_spread_rad: std::f64::consts::TAU,
                    oldest_sight_age_s: 99_999,
                    dominant_source: "calibration",
                },
            ),
        ] {
            // Strip the \r\n; NMEA's 82-char limit excludes line ending.
            let body_len = s.trim_end_matches("\r\n").len();
            assert!(
                body_len <= 82,
                "sentence exceeds 82 chars ({body_len}): {s}"
            );
        }
    }

    #[test]
    fn pbris_fix_emits_summary_fields() {
        let summary = FixSummary {
            n_sights: 4,
            azimuth_spread_rad: 0.5_f64,
            oldest_sight_age_s: 300,
            dominant_source: "horizon",
        };
        let s = pbris_fix(sample_utc(), &summary);
        assert!(s.starts_with("$PBRIS,FIX,"));
        assert!(s.ends_with("\r\n"));
        assert!(s.contains(",4,"), "expected n_sights=4 in {s}");
        // 0.5 rad ≈ 28.65°
        assert!(s.contains(",28.65,"), "expected ≈28.65° spread in {s}");
        assert!(s.contains(",300,"), "expected oldest_age=300 in {s}");
        assert!(s.contains(",horizon*"), "expected dominant=horizon in {s}");
    }
}
