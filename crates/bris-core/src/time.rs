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
//! IERS in Bulletin C. We embed the table as a `const` and refuse to
//! convert times beyond a documented expiration; see [`LEAP_TABLE_EXPIRES`].
//!
//! As of the most recent commit, no leap second has been added since
//! 2017-01-01 (TAI−UTC = 37 s). The IERS has announced an intent to
//! deprecate leap seconds by 2035, but this code does not assume that.
//! The table will be regenerated periodically; stale-table detection
//! lives in `plan.org` Phase 1.5 and inflates time uncertainty rather
//! than refusing to compute fixes.
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

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc};

/// One entry in the historical leap second table.
#[derive(Debug, Clone, Copy)]
struct LeapEntry {
    /// First UTC instant at which `tai_minus_utc` becomes effective.
    /// All leap seconds historically have occurred at midnight UTC
    /// at the end of June or December.
    effective_utc_y: i32,
    effective_utc_m: u32,
    effective_utc_d: u32,
    /// Integer seconds: `TAI = UTC + tai_minus_utc` from this instant on.
    tai_minus_utc: i32,
}

/// The embedded leap second table. Sourced from IERS Bulletin C history.
///
/// Order is chronological; the last entry is the currently-effective offset
/// from the most recent leap second to [`LEAP_TABLE_EXPIRES`].
const LEAP_TABLE: &[LeapEntry] = &[
    // First IERS leap second (after the 1972 introduction of UTC with
    // a 10-second TAI offset baseline). This table starts at the modern
    // leap-second era; pre-1972 atomic time conventions are not supported.
    LeapEntry {
        effective_utc_y: 1972,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 10,
    },
    LeapEntry {
        effective_utc_y: 1972,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 11,
    },
    LeapEntry {
        effective_utc_y: 1973,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 12,
    },
    LeapEntry {
        effective_utc_y: 1974,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 13,
    },
    LeapEntry {
        effective_utc_y: 1975,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 14,
    },
    LeapEntry {
        effective_utc_y: 1976,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 15,
    },
    LeapEntry {
        effective_utc_y: 1977,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 16,
    },
    LeapEntry {
        effective_utc_y: 1978,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 17,
    },
    LeapEntry {
        effective_utc_y: 1979,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 18,
    },
    LeapEntry {
        effective_utc_y: 1980,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 19,
    },
    LeapEntry {
        effective_utc_y: 1981,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 20,
    },
    LeapEntry {
        effective_utc_y: 1982,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 21,
    },
    LeapEntry {
        effective_utc_y: 1983,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 22,
    },
    LeapEntry {
        effective_utc_y: 1985,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 23,
    },
    LeapEntry {
        effective_utc_y: 1988,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 24,
    },
    LeapEntry {
        effective_utc_y: 1990,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 25,
    },
    LeapEntry {
        effective_utc_y: 1991,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 26,
    },
    LeapEntry {
        effective_utc_y: 1992,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 27,
    },
    LeapEntry {
        effective_utc_y: 1993,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 28,
    },
    LeapEntry {
        effective_utc_y: 1994,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 29,
    },
    LeapEntry {
        effective_utc_y: 1996,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 30,
    },
    LeapEntry {
        effective_utc_y: 1997,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 31,
    },
    LeapEntry {
        effective_utc_y: 1999,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 32,
    },
    LeapEntry {
        effective_utc_y: 2006,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 33,
    },
    LeapEntry {
        effective_utc_y: 2009,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 34,
    },
    LeapEntry {
        effective_utc_y: 2012,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 35,
    },
    LeapEntry {
        effective_utc_y: 2015,
        effective_utc_m: 7,
        effective_utc_d: 1,
        tai_minus_utc: 36,
    },
    LeapEntry {
        effective_utc_y: 2017,
        effective_utc_m: 1,
        effective_utc_d: 1,
        tai_minus_utc: 37,
    },
];

/// Date through which the embedded leap-second table is authoritative.
///
/// This is the announced validity end of the most recent IERS Bulletin C
/// known to the build. After this date, the table is considered stale
/// and time uncertainty must be inflated; see `plan.org` Phase 1.5.
///
/// Stored as `(year, month, day)` of the first day for which the table
/// is no longer authoritative.
pub const LEAP_TABLE_EXPIRES: (i32, u32, u32) = (2027, 7, 1);

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
    let (y, m, d) = LEAP_TABLE_EXPIRES;
    let expires = NaiveDate::from_ymd_opt(y, m, d)
        .and_then(|nd| nd.and_hms_opt(0, 0, 0))
        .and_then(|ndt| Utc.from_local_datetime(&ndt).single())
        .expect("LEAP_TABLE_EXPIRES is a valid date");
    utc < expires
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
    let first = LEAP_TABLE.first().expect("leap table is non-empty");
    let first_effective = build_utc(
        first.effective_utc_y,
        first.effective_utc_m,
        first.effective_utc_d,
    )?;
    if utc < first_effective {
        return Err(TimeError::BeforeLeapTable);
    }
    // Linear scan — table is small (< 30 entries) and called infrequently
    // enough that a binary search would be premature optimization.
    let mut current = first.tai_minus_utc;
    for entry in LEAP_TABLE.iter().skip(1) {
        let effective = build_utc(
            entry.effective_utc_y,
            entry.effective_utc_m,
            entry.effective_utc_d,
        )?;
        if utc < effective {
            break;
        }
        current = entry.tai_minus_utc;
    }
    Ok(current)
}

/// Construct a UTC `DateTime` at midnight on the given date.
fn build_utc(y: i32, m: u32, d: u32) -> Result<DateTime<Utc>, TimeError> {
    NaiveDate::from_ymd_opt(y, m, d)
        .and_then(|nd| nd.and_hms_opt(0, 0, 0))
        .and_then(|ndt| Utc.from_local_datetime(&ndt).single())
        .ok_or(TimeError::InvalidDate)
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
}
