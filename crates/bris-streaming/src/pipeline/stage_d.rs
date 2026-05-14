//! Stage D: plate solving for night/twilight body records.
//!
//! After Stage B produces a [`BodyDetection::Night(peaks)`]
//! record and Stage C produces a horizon for the same frame,
//! Stage D attempts to identify those peaks against the
//! geometric-hash star database
//! ([`bris_platesolve::plate_solve`]). On success, the body
//! detection's variant is promoted from
//! [`BodyDetection::Night`] to
//! [`BodyDetection::IdentifiedStars`], carrying the camera
//! attitude and the per-peak HR identifications. Stage E later
//! expands one such record into one sight per identified star.
//!
//! # When Stage D runs
//!
//! Per the design doc, Stage D runs only when:
//!
//! - Stage B produced a `Night` payload (peaks; no day-path
//!   centroid).
//! - The classifier verdict is Night or Twilight (commit 6
//!   gates on the Stage A verdict, not on the payload alone,
//!   to avoid spurious solver invocations on day-path frames
//!   that happen to have bright peaks).
//!
//! Plate solving costs ~10-50 ms per frame once the database
//! is built, plus a one-shot ~10-30 s build cost (deferred
//! per [`crate::PlateSolverInit`]). Failed plate solves leave
//! the [`BodyDetection::Night`] payload untouched so Stage E
//! can ignore it; subsequent frames may succeed where this
//! one didn't (different pointing → different star pattern).
//!
//! # When Stage D doesn't run
//!
//! - Day or Twilight-day-success body records (the variant is
//!   already [`BodyDetection::Day`]; no peaks to solve).
//! - When [`crate::PlateSolverInit`] is `Lazy` and the database
//!   hasn't been built yet — Stage D returns `Skipped` so
//!   the engine can build the database on the *next* call
//!   (asynchronously in a future commit; synchronously in
//!   commit 6).

use crate::pipeline::BodyDetection;
use bris_platesolve::{plate_solve, PlateSolveConfig, StarHashDb};
use bris_vision::Frame;
use tracing::{debug, trace};

/// Outcome of one Stage D run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageDOutcome {
    /// Stage D ran and successfully identified ≥
    /// `min_verifications` stars; the body record's payload
    /// has been promoted to
    /// [`BodyDetection::IdentifiedStars`].
    Identified,
    /// Stage D ran but the plate solve failed (insufficient
    /// peaks, no candidate matched, or refinement-residual
    /// gate tripped). The body record is left as
    /// [`BodyDetection::Night`].
    NoMatch,
    /// Stage D was skipped because the body record isn't a
    /// candidate (Day, IdentifiedStars-already, or None) or
    /// because the database hasn't been built yet
    /// (`PlateSolverInit::Lazy`). No state change.
    Skipped,
}

/// Run Stage D on a body record.
///
/// `db` is the (already-built) plate-solve database; pass
/// `None` to indicate "DB not available; skip Stage D this
/// frame." `frame` provides the camera intrinsics needed by
/// [`plate_solve`].
///
/// Mutates the body detection in place: a successful solve
/// replaces a `Night(peaks)` payload with
/// `IdentifiedStars(result)`. Day, None, and already-identified
/// records are left untouched.
pub(crate) fn run(
    body: &mut BodyDetection,
    frame: &Frame,
    db: Option<&StarHashDb>,
    cfg: PlateSolveConfig,
) -> StageDOutcome {
    let Some(db) = db else {
        // Lazy DB not built yet; engine will build before the
        // next call.
        if matches!(body, BodyDetection::Night(_)) {
            debug!("Stage D skipped: hash database not yet built");
        }
        return StageDOutcome::Skipped;
    };
    // Take the peaks vector out of the BodyDetection so we can
    // pass &[Peak] to plate_solve and reassemble afterwards.
    let peaks = match body {
        BodyDetection::Night(peaks) => std::mem::take(peaks),
        BodyDetection::Day(_) | BodyDetection::IdentifiedStars(_) | BodyDetection::None => {
            return StageDOutcome::Skipped
        }
    };
    trace!(peak_count = peaks.len(), "Stage D: attempting plate solve");
    match plate_solve(&peaks, &frame.intrinsics, db, cfg) {
        Ok(result) => {
            trace!(
                identified = result.identified.len(),
                "Stage D: plate solve succeeded"
            );
            *body = BodyDetection::IdentifiedStars(result);
            StageDOutcome::Identified
        }
        Err(e) => {
            trace!(error = %e, "Stage D: plate solve declined");
            // Restore the peaks so they can still be consumed
            // (or discarded) downstream.
            *body = BodyDetection::Night(peaks);
            StageDOutcome::NoMatch
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_core::time::{Tt, JD_J2000};
    use bris_platesolve::StarHashDbConfig;
    use bris_vision::{Frame, Intrinsics, Peak};

    fn dummy_frame() -> Frame {
        Frame::new(
            8,
            8,
            vec![0u16; 64],
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(8, 8),
        )
        .unwrap()
    }

    #[test]
    fn skipped_when_db_is_none() {
        let mut body = BodyDetection::Night(vec![Peak {
            x: 0.0,
            y: 0.0,
            intensity: 1000.0,
        }]);
        let outcome = run(&mut body, &dummy_frame(), None, PlateSolveConfig::default());
        assert_eq!(outcome, StageDOutcome::Skipped);
        assert!(matches!(body, BodyDetection::Night(_)));
    }

    #[test]
    fn skipped_for_day_payload() {
        // Day records have no peaks; Stage D must leave them
        // alone whether the DB is present or not.
        let cfg = StarHashDbConfig {
            mag_cutoff: 1.5, // small DB for fast test
            ..StarHashDbConfig::default()
        };
        let db = StarHashDb::build(cfg);
        let mut body = BodyDetection::Day(bris_vision::Centroid {
            x: 0.0,
            y: 0.0,
            area_px: 100,
            mean_intensity: 50_000.0,
            position_sigma_px: bris_core::Sigma::new(0.5).unwrap(),
        });
        let outcome = run(
            &mut body,
            &dummy_frame(),
            Some(&db),
            PlateSolveConfig::default(),
        );
        assert_eq!(outcome, StageDOutcome::Skipped);
        assert!(matches!(body, BodyDetection::Day(_)));
    }

    #[test]
    fn no_match_for_random_peaks() {
        // Scattered peaks with no underlying star pattern
        // should fail the refinement-residual gate. The body
        // record is left as `Night(peaks)` so subsequent
        // logic can handle it.
        let cfg = StarHashDbConfig {
            mag_cutoff: 1.5,
            ..StarHashDbConfig::default()
        };
        let db = StarHashDb::build(cfg);
        let peaks: Vec<Peak> = (0..8)
            .map(|i| Peak {
                x: 50.0 + 30.0 * f64::from(i),
                y: 100.0 + 20.0 * f64::from(i % 4),
                intensity: 50_000.0 - 1000.0 * f64::from(i),
            })
            .collect();
        let mut body = BodyDetection::Night(peaks.clone());
        let outcome = run(
            &mut body,
            &dummy_frame(),
            Some(&db),
            PlateSolveConfig::default(),
        );
        assert_eq!(outcome, StageDOutcome::NoMatch);
        match body {
            BodyDetection::Night(restored) => {
                assert_eq!(
                    restored.len(),
                    peaks.len(),
                    "peaks must be restored on no-match"
                );
            }
            other => panic!("expected Night to be restored, got {other:?}"),
        }
    }
}
