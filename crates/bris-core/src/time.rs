//! Astronomical time scales and Julian Date conversions.
//!
//! Bris consumes time from the system wall clock (UTC) and converts it
//! through the chain UTC → TAI → TT, since almanac formulas (VSOP87,
//! ELP2000, IAU precession/nutation models) are expressed in Terrestrial
//! Time. UT1, used for sidereal-time calculations, differs from UTC by
//! ΔUT1 ≤ 0.9 s; we default ΔUT1 to zero and allow a user override.
//!
//! # The leap second table
//!
//! UTC is offset from TAI by an integer number of seconds that grows
//! whenever a leap second is inserted. The schedule is published by the
//! IERS in Bulletin C, and a machine-readable form is mirrored by IANA
//! at <https://data.iana.org/time-zones/data/leap-seconds.list>.
//!
//! We vendor that file at `crates/bris-core/data/leap-seconds.list` and
//! the build script (`crates/bris-core/build.rs`) parses it at compile
//! time, emitting [`LEAP_TABLE`] and [`LEAP_TABLE_EXPIRES_UNIX`].
//! Refreshing the table is a single-file `cp`; no Rust code changes
//! are required.
//!
//! As of the most recent vendored update, no leap second has been added
//! since 2017-01-01 (TAI−UTC = 37 s). The IERS has announced an intent
//! to deprecate leap seconds by 2035, but this code does not assume that.
//!
//! # Floating-point precision
//!
//! Julian Dates around the modern era are ~2.46 million; representing
//! them as `f64` gives ~1e-9-day (~100 µs) precision after arithmetic.
//! That is two orders of magnitude better than our overall time budget
//! and four orders better than our position budget, so a single-`f64`
//! representation is sufficient for Bris's accuracy targets. If
//! sub-microsecond precision is ever needed (it isn't for celestial
//! navigation), a split (`jd_int`, `jd_frac`) representation would be the
//! correct upgrade.

use chrono::{DateTime, Datelike, Timelike, Utc};

// Generated at build time from data/leap-seconds.list.
#[allow(clippy::unreadable_literal)]
mod leap_table {
    include!(concat!(env!("OUT_DIR"), "/leap_table.rs"));
}
use leap_table::{LEAP_TABLE, LEAP_TABLE_EXPIRES_UNIX, LEAP_TABLE_UPDATED_UNIX};

/// Unix timestamp at which the embedded `leap-seconds.list` was last
/// updated. Phase 1.5 (time integrity) uses this to drive the
/// stale-table-detection rule: if the table is older than a configured
/// threshold, time uncertainty is inflated.
pub const LEAP_TABLE_UPDATED_AT_UNIX: i64 = LEAP_TABLE_UPDATED_UNIX;

/// Date through which the embedded leap-second table is authoritative,
/// as `(year, month, day)` of the first day for which the table is no
/// longer authoritative.
///
/// Computed at compile time from the vendored IANA `leap-seconds.list`.
pub const LEAP_TABLE_EXPIRES: (i32, u32, u32) = compute_expires_ymd();

const fn compute_expires_ymd() -> (i32, u32, u32) {
    // We unconditionally trust the generated `LEAP_TABLE_EXPIRES_UNIX`
    // here; computing it back to (Y, M, D) at const-eval time without
    // the chrono crate is a small chore. Chrono's `from_timestamp` is
    // not const, so we approximate with the standard civil-from-days
    // algorithm. Reference: Howard Hinnant's "Date Algorithms".
    let days = LEAP_TABLE_EXPIRES_UNIX / 86_400;
    civil_from_days(days)
}

/// Converts days since the Unix epoch (1970-01-01) to a proleptic
/// Gregorian (year, month, day). const-fn version of Howard Hinnant's
/// algorithm; published explicitly into the public domain by the
/// author. The integer-narrowing casts are intentional: at any plausible
/// input range (Unix-epoch days fit in i32 for any year < 5.8M AD) they
/// don't lose information.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_possible_wrap
)]
const fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// TT − TAI is a defined constant: 32.184 seconds.
const TT_MINUS_TAI: f64 = 32.184;

/// Julian Date of the J2000.0 epoch (2000-01-01T12:00:00 TT).
pub const JD_J2000: f64 = 2_451_545.0;

/// Seconds in one Julian day.
const SECS_PER_DAY: f64 = 86_400.0;

/// A continuous time scale that astronomical algorithms consume.
///
/// Stored as a Julian Date in the named scale. Construct via the
/// conversion functions in this module.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Tt(f64);

impl Tt {
    /// Julian Date in TT.
    pub const fn julian_date(self) -> f64 {
        self.0
    }

    /// Julian centuries since J2000.0 (TT). Standard input for IAU
    /// precession, nutation, and many other formulas.
    pub fn julian_centuries_j2000(self) -> f64 {
        (self.0 - JD_J2000) / 36525.0
    }

    /// Construct directly from a Julian Date in TT.
    ///
    /// Most callers should use [`utc_to_tt`] to go through the leap
    /// table from a wall-clock instant. This constructor exists for
    /// almanac internals and tests that work in TT directly.
    #[must_use]
    pub const fn from_julian_date(jd: f64) -> Self {
        Self(jd)
    }
}

/// International Atomic Time as a Julian Date.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Tai(f64);

impl Tai {
    /// Julian Date in TAI.
    pub const fn julian_date(self) -> f64 {
        self.0
    }

    /// Convert TAI to TT.
    #[must_use]
    pub fn to_tt(self) -> Tt {
        Tt(self.0 + TT_MINUS_TAI / SECS_PER_DAY)
    }
}

/// Universal Time 1: a solar-derived time scale.
///
/// Differs from UTC by ΔUT1, |ΔUT1| ≤ 0.9 s. Default ΔUT1 = 0 s.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Ut1(f64);

impl Ut1 {
    /// Julian Date in UT1.
    pub const fn julian_date(self) -> f64 {
        self.0
    }
}

/// Convert a UTC instant to TAI Julian Date using the embedded leap
/// second table.
///
/// # Errors
///
/// Returns [`TimeError::BeforeLeapTable`] for instants before the start
/// of the embedded table (1972-01-01 UTC).
pub fn utc_to_tai(utc: DateTime<Utc>) -> Result<Tai, TimeError> {
    let offset_secs = leap_offset_secs(utc)?;
    let jd_utc = utc_to_julian_date(utc);
    Ok(Tai(jd_utc + f64::from(offset_secs) / SECS_PER_DAY))
}

/// Convert a UTC instant to TT Julian Date.
///
/// Equivalent to `utc_to_tai(utc)?.to_tt()` and provided as a convenience
/// because TT is the time scale almanac formulas actually consume.
///
/// # Errors
///
/// As [`utc_to_tai`].
pub fn utc_to_tt(utc: DateTime<Utc>) -> Result<Tt, TimeError> {
    Ok(utc_to_tai(utc)?.to_tt())
}

/// Convert a UTC instant to UT1 Julian Date, given an explicit ΔUT1
/// (in seconds, |ΔUT1| ≤ 0.9 expected but not enforced).
///
/// Pass `delta_ut1_secs = 0.0` when no ΔUT1 source is available; this
/// is the documented default for offshore operation. The contribution to
/// position error from a 0.9 s ΔUT1 is up to ~0.225 nm of longitude,
/// which is folded into the time-uncertainty contribution to the fix
/// covariance in `bris-nav`.
pub fn utc_to_ut1(utc: DateTime<Utc>, delta_ut1_secs: f64) -> Ut1 {
    let jd_utc = utc_to_julian_date(utc);
    Ut1(jd_utc + delta_ut1_secs / SECS_PER_DAY)
}

/// Whether the embedded leap-second table is still authoritative for the
/// given UTC instant.
///
/// Returns `false` when `utc >= LEAP_TABLE_EXPIRES`, in which case
/// [`utc_to_tai`] still computes a value (using the most recent known
/// offset) but callers should treat the result with elevated time
/// uncertainty per `plan.org` Phase 1.5.
pub fn leap_table_valid_for(utc: DateTime<Utc>) -> bool {
    utc.timestamp() < LEAP_TABLE_EXPIRES_UNIX
}

/// Convert a UTC `DateTime` to its Julian Date.
///
/// Uses the standard astronomical convention: JD 2451545.0 = 2000-01-01
/// 12:00:00 UTC. Sub-second precision preserved from the input.
fn utc_to_julian_date(utc: DateTime<Utc>) -> f64 {
    // Algorithm from Meeus, *Astronomical Algorithms*, ch. 7.
    let mut y = utc.year();
    let mut m = i32::try_from(utc.month()).expect("month fits in i32");
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    let a = y.div_euclid(100);
    let b = 2 - a + a.div_euclid(4);
    let day_fraction = (f64::from(utc.hour()) * 3600.0
        + f64::from(utc.minute()) * 60.0
        + f64::from(utc.second())
        + f64::from(utc.nanosecond()) * 1e-9)
        / SECS_PER_DAY;
    let jd_int = (365.25 * f64::from(y + 4716)).floor()
        + (30.6001 * f64::from(m + 1)).floor()
        + f64::from(utc.day())
        + f64::from(b)
        - 1524.5;
    jd_int + day_fraction
}

/// Look up the TAI−UTC integer-second offset effective at the given UTC
/// instant.
fn leap_offset_secs(utc: DateTime<Utc>) -> Result<i32, TimeError> {
    let unix = utc.timestamp();
    let first = LEAP_TABLE.first().expect("leap table is non-empty");
    if unix < first.0 {
        return Err(TimeError::BeforeLeapTable);
    }
    // Binary search: find the entry with the largest effective_unix ≤ unix.
    let idx = LEAP_TABLE
        .binary_search_by_key(&unix, |&(u, _)| u)
        .unwrap_or_else(|insert_pos| insert_pos.saturating_sub(1));
    Ok(LEAP_TABLE[idx].1)
}

/// Errors converting between time scales.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TimeError {
    /// The instant predates the embedded leap-second table (before
    /// 1972-01-01 UTC).
    #[error("UTC instant predates the leap second table (before 1972)")]
    BeforeLeapTable,
    /// A date in the leap-second table or in user input is not valid
    /// in the proleptic Gregorian calendar. Indicates a bug in the
    /// embedded table; user inputs are validated by `chrono`.
    #[error("invalid Gregorian date")]
    InvalidDate,
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).single().unwrap()
    }

    #[test]
    fn jd_j2000_matches_constant() {
        let j2000 = utc(2000, 1, 1, 12, 0, 0);
        let jd = utc_to_julian_date(j2000);
        // J2000.0 is defined as 2000-01-01T12:00:00 TT, but the JD of
        // 2000-01-01T12:00:00 *UTC* is exactly 2451545.0 by construction
        // of the Gregorian-to-JD algorithm. The TT-vs-UTC distinction
        // (~64.184 s in 2000) is a separate concern handled by utc_to_tt.
        assert_relative_eq!(jd, JD_J2000, epsilon = 1e-9);
    }

    #[test]
    fn leap_offset_known_dates() {
        // Just before the 2017-01-01 leap second: TAI−UTC = 36.
        let before = utc(2016, 12, 31, 23, 59, 59);
        assert_eq!(leap_offset_secs(before).unwrap(), 36);
        // At and after 2017-01-01: TAI−UTC = 37.
        let after = utc(2017, 1, 1, 0, 0, 0);
        assert_eq!(leap_offset_secs(after).unwrap(), 37);
        // Modern instant: still 37.
        let modern = utc(2024, 6, 15, 12, 0, 0);
        assert_eq!(leap_offset_secs(modern).unwrap(), 37);
    }

    #[test]
    fn leap_offset_table_start() {
        // 1972-01-01 is the first entry; offset is 10.
        let start = utc(1972, 1, 1, 0, 0, 0);
        assert_eq!(leap_offset_secs(start).unwrap(), 10);
        // Before the table → error.
        let before = utc(1971, 12, 31, 23, 59, 59);
        assert_eq!(leap_offset_secs(before), Err(TimeError::BeforeLeapTable));
    }

    #[test]
    fn utc_to_tt_modern() {
        // 2020-01-01T00:00:00 UTC: TAI−UTC = 37, TT−TAI = 32.184,
        // so TT−UTC = 69.184 s.
        let now = utc(2020, 1, 1, 0, 0, 0);
        let tt = utc_to_tt(now).unwrap();
        let tai = utc_to_tai(now).unwrap();
        // Floating-point addition of (~32 s / 86400) to a JD around
        // 2.46 million loses ~1e-9 day (~140 µs) of precision; the
        // tolerance must be wider than f64 epsilon at this magnitude.
        assert_relative_eq!(
            tt.julian_date() - tai.julian_date(),
            32.184 / SECS_PER_DAY,
            epsilon = 1e-8
        );
        let jd_utc = utc_to_julian_date(now);
        assert_relative_eq!(
            tt.julian_date() - jd_utc,
            69.184 / SECS_PER_DAY,
            epsilon = 1e-8
        );
    }

    #[test]
    fn julian_centuries_j2000_at_epoch() {
        // J2000.0: T = 0 by definition.
        let tt = Tt(JD_J2000);
        assert_relative_eq!(tt.julian_centuries_j2000(), 0.0);
    }

    #[test]
    fn ut1_default_equals_utc_jd() {
        let now = utc(2024, 6, 15, 12, 0, 0);
        let ut1 = utc_to_ut1(now, 0.0);
        assert_relative_eq!(ut1.julian_date(), utc_to_julian_date(now));
    }

    #[test]
    fn ut1_offset_applied() {
        let now = utc(2024, 6, 15, 12, 0, 0);
        let ut1 = utc_to_ut1(now, 0.5);
        let expected = utc_to_julian_date(now) + 0.5 / SECS_PER_DAY;
        assert_relative_eq!(ut1.julian_date(), expected, epsilon = 1e-15);
    }

    #[test]
    fn leap_table_valid_today_and_invalid_far_future() {
        // The table claims validity through LEAP_TABLE_EXPIRES.
        let (y, m, d) = LEAP_TABLE_EXPIRES;
        let just_before = utc(y, m, d, 0, 0, 0) - chrono::Duration::seconds(1);
        let just_after = utc(y, m, d, 0, 0, 0);
        assert!(leap_table_valid_for(just_before));
        assert!(!leap_table_valid_for(just_after));
    }

    #[test]
    fn jd_round_trips_for_modern_dates() {
        // Compare to a hand-computed value from Meeus example 7.a:
        // 1957-10-04.81 UT → JD 2436116.31
        let utc = Utc
            .with_ymd_and_hms(1957, 10, 4, 19, 26, 24)
            .single()
            .unwrap();
        let jd = utc_to_julian_date(utc);
        assert_relative_eq!(jd, 2_436_116.31, epsilon = 1e-5);
    }

    #[test]
    fn tt_from_julian_date_round_trips() {
        let jd = 2_460_000.5;
        let tt = Tt::from_julian_date(jd);
        assert_relative_eq!(tt.julian_date(), jd);
    }

    #[test]
    fn tai_to_tt_offset_is_constant() {
        // Build a TAI instant directly via utc_to_tai and verify the
        // TT − TAI offset matches the defined 32.184 s constant.
        let now = utc(2024, 1, 1, 0, 0, 0);
        let tai = utc_to_tai(now).unwrap();
        let tt = tai.to_tt();
        let delta_days = tt.julian_date() - tai.julian_date();
        assert_relative_eq!(delta_days, 32.184 / SECS_PER_DAY, epsilon = 1e-8);
    }

    #[test]
    fn ut1_julian_date_accessor_returns_stored_jd() {
        let now = utc(2024, 6, 15, 12, 0, 0);
        let ut1 = utc_to_ut1(now, 0.0);
        assert_relative_eq!(ut1.julian_date(), utc_to_julian_date(now));
    }

    #[test]
    fn julian_centuries_grows_linearly() {
        // One Julian century after J2000 → exactly T = 1.0.
        let tt = Tt::from_julian_date(JD_J2000 + 36_525.0);
        assert_relative_eq!(tt.julian_centuries_j2000(), 1.0, epsilon = 1e-12);
        // One Julian year after J2000 → T ≈ 0.01.
        let tt = Tt::from_julian_date(JD_J2000 + 365.25);
        assert_relative_eq!(tt.julian_centuries_j2000(), 0.01, epsilon = 1e-12);
    }

    #[test]
    fn leap_table_metadata_consistent() {
        // The "expires at" timestamp must be at or after the "updated at"
        // timestamp; both must be plausible Unix timestamps (post-2000).
        let updated = LEAP_TABLE_UPDATED_AT_UNIX;
        let expires = LEAP_TABLE_EXPIRES_UNIX;
        assert!(updated >= 946_684_800, "updated unix {updated} pre-2000");
        assert!(expires >= updated, "expires {expires} < updated {updated}");
    }

    #[test]
    fn leap_table_valid_for_past_instant() {
        // Any sensible past instant (after 1972, before vendored expiry)
        // should be considered valid.
        let past = utc(2000, 1, 1, 0, 0, 0);
        assert!(leap_table_valid_for(past));
    }

    #[test]
    fn utc_to_tai_rejects_pre_table_instants() {
        let before = utc(1971, 6, 1, 0, 0, 0);
        assert_eq!(utc_to_tai(before), Err(TimeError::BeforeLeapTable));
        // utc_to_tt should propagate the same error.
        assert_eq!(utc_to_tt(before), Err(TimeError::BeforeLeapTable));
    }
}
