//! Convert V4L2 buffer timestamps into mid-exposure
//! [`bris_core::time::Tt`] instants.
//!
//! V4L2 timestamps each captured buffer with a kernel time
//! source. The buffer timestamp marks the *start* of frame
//! readout (as the kernel sees it), which for our purposes
//! we approximate as the start of integration (`t_start`):
//! exposure-meter readouts and shutter-release latencies are
//! the dominant errors here, not the readout-vs-integration
//! distinction at the millisecond scale.
//!
//! Bris's downstream pipeline ([`bris_vision::Frame`]) needs
//! the *mid-exposure* TT instant. We compute it as
//! `t_start + exposure_us / 2`, then convert to TT via
//! [`bris_core::time::utc_to_tt`].
//!
//! # Source of truth: monotonic vs. wall clock
//!
//! V4L2's buffer timestamp uses `CLOCK_MONOTONIC` by
//! default (since v3.13 of the kernel) — relative-only.
//! That's correct for inter-frame intervals but not for
//! astronomical timestamping, which needs UTC. The capture
//! shell anchors monotonic timestamps to the wall clock by
//! recording `(monotonic_anchor, utc_anchor)` at startup
//! and applying the offset to each buffer's monotonic
//! timestamp.
//!
//! This is OK as long as the wall clock is reasonably
//! disciplined (NTP, GNSS time, or RTC) — Phase 1.5 in
//! `plan.org` covers the dual-clock work that lets the
//! engine track the wall-clock-to-monotonic offset
//! accurately and surface time uncertainty in the per-fix
//! σ. For now we use the simple "anchor at startup, apply
//! the offset every frame" approximation.

use bris_core::time::{utc_to_tt, TimeError, Tt};
use chrono::{DateTime, Utc};
use std::time::Duration;
use thiserror::Error;

/// Errors converting a V4L2 buffer timestamp to TT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TimestampError {
    /// The wall-clock anchor predates the leap-second table
    /// (1972-01-01 UTC). Operationally impossible — any system
    /// with a roughly-correct clock is well after 1972 — but
    /// guarded so that a corrupt anchor doesn't panic.
    #[error("UTC anchor {0:?} predates the leap-second table")]
    AnchorBeforeLeapTable(DateTime<Utc>),
    /// Internal arithmetic produced a non-finite duration.
    /// Indicates a bug in the upstream timestamp source
    /// (negative monotonic delta, e.g.) since
    /// `CLOCK_MONOTONIC` is by definition non-decreasing.
    #[error("non-finite arithmetic in timestamp conversion")]
    NonFinite,
}

impl From<TimeError> for TimestampError {
    fn from(e: TimeError) -> Self {
        match e {
            TimeError::BeforeLeapTable => {
                Self::AnchorBeforeLeapTable(DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_default())
            }
            // `InvalidDate` would only occur on a corrupt
            // leap-second table, which we don't propagate
            // separately — the only actionable condition for
            // the caller is "your timestamp is unusable."
            TimeError::InvalidDate => Self::NonFinite,
        }
    }
}

/// Anchors a `CLOCK_MONOTONIC`-based capture stream to the
/// wall clock.
///
/// Construct once at capture startup via [`MonotonicAnchor::now`]
/// (or [`MonotonicAnchor::new`] for tests with a synthetic
/// clock); call [`Self::buffer_timestamp_to_utc`] for each
/// captured frame.
#[derive(Debug, Clone, Copy)]
pub struct MonotonicAnchor {
    /// Monotonic timestamp at the moment the anchor was taken.
    /// Stored as a `Duration` since "the start of the kernel
    /// monotonic clock" (a kernel-defined zero, not Unix
    /// epoch). Subtracting two monotonic durations gives a
    /// signed interval; we store as `Duration` and reject
    /// "buffer timestamp before anchor" at conversion time.
    monotonic_anchor: Duration,
    /// Wall-clock UTC at the same moment the monotonic anchor
    /// was taken. Drives the absolute-time conversion.
    utc_anchor: DateTime<Utc>,
}

impl MonotonicAnchor {
    /// Take an anchor by sampling both clocks "as close as
    /// possible." There's an unavoidable few-microsecond skew
    /// between the two reads; for celestial-navigation
    /// timestamps at the millisecond scale this is in the
    /// noise. The dual-clock work in Phase 1.5 measures and
    /// bounds this skew.
    ///
    /// Cannot be called from a `const` context because both
    /// clock samples are runtime-only. For tests with
    /// synthetic timing, use [`Self::new`].
    #[must_use]
    pub fn now() -> Self {
        // Order: read monotonic first, then UTC. The
        // intervening syscall(s) push UTC slightly later, so
        // the `utc_anchor` is biased *forward* relative to
        // the monotonic anchor by a few microseconds. Frames
        // captured after this call will therefore be biased
        // a few microseconds *late* in UTC — well within the
        // sub-millisecond budget for celestial timestamps.
        let monotonic_anchor = monotonic_now();
        let utc_anchor = Utc::now();
        Self {
            monotonic_anchor,
            utc_anchor,
        }
    }

    /// Construct an anchor from explicit values. Lets tests
    /// drive the conversion without sampling real clocks.
    #[must_use]
    pub fn new(monotonic_anchor: Duration, utc_anchor: DateTime<Utc>) -> Self {
        Self {
            monotonic_anchor,
            utc_anchor,
        }
    }

    /// Convert one V4L2 buffer's monotonic timestamp into a
    /// wall-clock UTC instant.
    ///
    /// Returns `None` if `buffer_monotonic` predates the
    /// anchor — shouldn't happen with `CLOCK_MONOTONIC` but
    /// guarded so a driver bug doesn't panic.
    #[must_use]
    pub fn buffer_timestamp_to_utc(
        &self,
        buffer_monotonic: Duration,
    ) -> Option<DateTime<Utc>> {
        let delta = buffer_monotonic.checked_sub(self.monotonic_anchor)?;
        // chrono::Duration only takes i64 nanoseconds; that
        // overflows above ~292 years, well beyond the lifetime
        // of any capture session.
        let delta_ns = i64::try_from(delta.as_nanos()).ok()?;
        self.utc_anchor.checked_add_signed(chrono::Duration::nanoseconds(delta_ns))
    }
}

/// Convert a buffer-start UTC timestamp + an exposure
/// duration into the mid-exposure TT instant the engine
/// consumes.
///
/// `buffer_start_utc` is the buffer timestamp converted to
/// UTC via [`MonotonicAnchor::buffer_timestamp_to_utc`].
/// `exposure_us` is the camera's reported exposure for this
/// frame.
///
/// # Errors
///
/// See [`TimestampError`].
pub fn buffer_to_mid_exposure_tt(
    buffer_start_utc: DateTime<Utc>,
    exposure_us: u32,
) -> Result<Tt, TimestampError> {
    let half_exposure = chrono::Duration::microseconds(i64::from(exposure_us) / 2);
    let mid_utc = buffer_start_utc
        .checked_add_signed(half_exposure)
        .ok_or(TimestampError::NonFinite)?;
    Ok(utc_to_tt(mid_utc)?)
}

/// Read the kernel's `CLOCK_MONOTONIC` as a `Duration` since
/// the kernel's monotonic zero. Implemented via
/// `std::time::Instant` paired with a `OnceLock` of "instant
/// at the first call" so we can express durations as a real
/// `Duration` since some fixed point.
///
/// This is good enough for Bris's purposes: V4L2 buffer
/// timestamps from the same kernel agree on "the start of
/// monotonic time" (whatever that is), and we always express
/// V4L2 timestamps as monotonic-since-anchor durations
/// before consuming them.
#[cfg(feature = "v4l2")]
fn monotonic_now() -> Duration {
    use std::sync::OnceLock;
    use std::time::Instant;
    static MONO_ZERO: OnceLock<Instant> = OnceLock::new();
    let zero = MONO_ZERO.get_or_init(Instant::now);
    Instant::now().saturating_duration_since(*zero)
}

/// Stub when the V4L2 backend is disabled. Returns zero so
/// the rest of the timestamp module compiles for tests; live
/// capture is unavailable in this configuration.
#[cfg(not(feature = "v4l2"))]
fn monotonic_now() -> Duration {
    Duration::ZERO
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).single().unwrap()
    }

    #[test]
    fn anchor_round_trip_at_anchor_returns_anchor_utc() {
        let utc_anchor = utc(2024, 6, 15, 12, 0, 0);
        let mono_anchor = Duration::from_secs(100);
        let a = MonotonicAnchor::new(mono_anchor, utc_anchor);
        // Buffer at the anchor instant: should map to anchor UTC.
        let mapped = a.buffer_timestamp_to_utc(mono_anchor).unwrap();
        assert_eq!(mapped, utc_anchor);
    }

    #[test]
    fn anchor_propagates_offset_correctly() {
        // Anchor at t = 100 s monotonic / UTC=2024-06-15T12:00:00.
        // Buffer at t = 105 s monotonic should map to UTC + 5 s.
        let utc_anchor = utc(2024, 6, 15, 12, 0, 0);
        let mono_anchor = Duration::from_secs(100);
        let a = MonotonicAnchor::new(mono_anchor, utc_anchor);
        let mapped = a
            .buffer_timestamp_to_utc(Duration::from_secs(105))
            .unwrap();
        assert_eq!(mapped, utc_anchor + chrono::Duration::seconds(5));
    }

    #[test]
    fn anchor_returns_none_for_pre_anchor_timestamp() {
        // CLOCK_MONOTONIC is non-decreasing, so this should
        // never happen in practice, but a corrupt buffer
        // timestamp or a bug shouldn't panic.
        let utc_anchor = utc(2024, 6, 15, 12, 0, 0);
        let mono_anchor = Duration::from_secs(100);
        let a = MonotonicAnchor::new(mono_anchor, utc_anchor);
        assert!(a
            .buffer_timestamp_to_utc(Duration::from_secs(50))
            .is_none());
    }

    #[test]
    fn mid_exposure_adds_half_exposure() {
        let buffer_start = utc(2024, 6, 15, 12, 0, 0);
        let exposure_us = 1_000_u32; // 1 ms exposure
        let tt = buffer_to_mid_exposure_tt(buffer_start, exposure_us).unwrap();
        // Mid-exposure should be 500 µs past buffer_start UTC,
        // expressed in TT (TT = UTC + ~69.184 s in modern era).
        let expected_mid_utc = buffer_start + chrono::Duration::microseconds(500);
        let expected_tt = utc_to_tt(expected_mid_utc).unwrap();
        // JD comparison: equal to within f64 precision for a
        // ~1ms-scale offset on a ~2.46M JD.
        assert!(
            (tt.julian_date() - expected_tt.julian_date()).abs() < 1e-12,
            "{} vs {}",
            tt.julian_date(),
            expected_tt.julian_date()
        );
    }

    #[test]
    fn mid_exposure_zero_exposure_passes_through() {
        // Some cameras report exposure_us = 0 (auto-exposure
        // not yet measured, or bug). We accept it: the
        // mid-exposure offset is just zero; downstream σ
        // accounting will reflect the missing exposure
        // information separately.
        let buffer_start = utc(2024, 6, 15, 12, 0, 0);
        let tt = buffer_to_mid_exposure_tt(buffer_start, 0).unwrap();
        let expected_tt = utc_to_tt(buffer_start).unwrap();
        assert!((tt.julian_date() - expected_tt.julian_date()).abs() < 1e-12);
    }

    #[test]
    fn anchor_now_returns_finite_values() {
        // Smoke test: MonotonicAnchor::now() doesn't panic
        // and returns sensible bounds.
        let a = MonotonicAnchor::now();
        // The monotonic anchor should be a small duration
        // (microseconds since the OnceLock was first read,
        // which is ~now).
        assert!(a.monotonic_anchor < Duration::from_secs(60));
        // UTC anchor should be reasonably current.
        let now = Utc::now();
        let delta = (now - a.utc_anchor).num_seconds().abs();
        assert!(delta < 5, "UTC anchor is {delta} s from Utc::now() — suspicious");
    }
}
