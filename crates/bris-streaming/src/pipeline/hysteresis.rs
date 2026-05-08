//! Day/night classifier hysteresis.
//!
//! The per-frame classifier ([`bris_vision::classify`]) returns
//! a fresh verdict for every captured frame based on local
//! evidence. That's correct for the classifier's job
//! (per-frame condition reporting) but wrong for the engine's
//! method-set selection: a dim cloud transit can flip the
//! image evidence from Day to Twilight on a single frame, and
//! back to Day on the next. Switching detector pipelines on
//! every such transient would chatter wildly between day and
//! night code paths.
//!
//! [`ClassifierHysteresis`] sits between the raw classifier
//! and the engine's dispatch. On each new classification:
//!
//! - If it agrees with the currently-trusted (smoothed)
//!   condition, the candidate-transition counter resets.
//! - If it disagrees, it becomes (or extends) a *candidate*
//!   transition. Only after
//!   [`crate::EngineConfig::classifier_hysteresis_frames`]
//!   *consecutive* frames agree on the candidate does the
//!   smoothed condition switch.
//!
//! # First-frame behaviour
//!
//! The first classification ever seen is trusted immediately
//! (no warm-up). Operators don't expect the engine to wait
//! three seconds before reporting any condition; the chattering
//! it guards against requires a *prior* condition to chatter
//! against.
//!
//! # State-machine semantics
//!
//! With four conditions ([`bris_vision::Condition::Day`],
//! [`bris_vision::Condition::Twilight`],
//! [`bris_vision::Condition::Night`],
//! [`bris_vision::Condition::Unusable`]), there are 12
//! possible transitions; this hysteresis applies the same
//! settle time to all of them. A more sophisticated
//! implementation could hold per-transition settle times (e.g.
//! "Day → Twilight is fine immediately because dusk is
//! gradual; Day → Night requires longer because it shouldn't
//! happen physically"), but the simple uniform-settle approach
//! is the design-doc recommendation and the right starting
//! point.

use bris_vision::Condition;

/// Per-engine classifier hysteresis state.
#[derive(Debug, Default)]
pub(crate) struct ClassifierHysteresis {
    /// The condition the engine is currently treating as
    /// authoritative. `None` until the first classification.
    smoothed: Option<Condition>,
    /// A pending alternative condition that has been observed
    /// for the last `pending_count` consecutive frames.
    /// `None` when the most recent frame agreed with
    /// `smoothed`.
    pending: Option<Condition>,
    /// Number of consecutive frames the `pending` condition
    /// has been observed. Reset to 0 whenever an agreeing
    /// observation lands; reset to 1 when a *new* disagreeing
    /// observation displaces the previous pending candidate.
    pending_count: u32,
}

impl ClassifierHysteresis {
    /// Update the hysteresis with one new classification and
    /// return the smoothed condition the engine should
    /// dispatch on.
    ///
    /// `settle_frames` is the number of consecutive frames of
    /// agreement required to honour a transition (matches
    /// [`crate::EngineConfig::classifier_hysteresis_frames`]).
    /// A value of 1 disables hysteresis (every observation
    /// becomes the smoothed verdict immediately); 0 is treated
    /// the same as 1 (an immediate-transition policy is the
    /// only sensible interpretation of "wait for zero
    /// frames").
    pub(crate) fn update(&mut self, raw: Condition, settle_frames: u32) -> Condition {
        let settle = settle_frames.max(1);
        match self.smoothed {
            None => {
                // First-ever classification: trust immediately.
                self.smoothed = Some(raw);
                self.pending = None;
                self.pending_count = 0;
                raw
            }
            Some(current) if raw == current => {
                // Observation agrees with the trusted
                // condition; clear any pending transition.
                self.pending = None;
                self.pending_count = 0;
                current
            }
            Some(current) => {
                // Disagreement.
                if Some(raw) == self.pending {
                    // Extend the existing pending streak.
                    self.pending_count = self.pending_count.saturating_add(1);
                } else {
                    // New candidate displaces the previous
                    // pending one; restart the streak at 1.
                    self.pending = Some(raw);
                    self.pending_count = 1;
                }
                if self.pending_count >= settle {
                    // Promote the pending candidate.
                    self.smoothed = Some(raw);
                    self.pending = None;
                    self.pending_count = 0;
                    raw
                } else {
                    current
                }
            }
        }
    }

    /// The current smoothed condition, if any. `None` until
    /// the first [`Self::update`] call.
    #[allow(dead_code)] // diagnostic accessor; useful for tests and debugging.
    pub(crate) fn current(&self) -> Option<Condition> {
        self.smoothed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_observation_is_trusted_immediately() {
        let mut h = ClassifierHysteresis::default();
        assert_eq!(h.update(Condition::Day, 90), Condition::Day);
        assert_eq!(h.current(), Some(Condition::Day));
    }

    #[test]
    fn agreeing_observations_keep_smoothed_unchanged() {
        let mut h = ClassifierHysteresis::default();
        let _ = h.update(Condition::Day, 90);
        for _ in 0..100 {
            assert_eq!(h.update(Condition::Day, 90), Condition::Day);
        }
    }

    #[test]
    fn single_disagreeing_frame_does_not_switch() {
        let mut h = ClassifierHysteresis::default();
        let _ = h.update(Condition::Day, 90);
        assert_eq!(
            h.update(Condition::Twilight, 90),
            Condition::Day,
            "single transient frame must not flip the smoothed verdict"
        );
    }

    #[test]
    fn n_consecutive_frames_promotes_the_candidate() {
        let mut h = ClassifierHysteresis::default();
        let _ = h.update(Condition::Day, 5);
        // 4 consecutive Twilight observations: still Day.
        for _ in 0..4 {
            assert_eq!(h.update(Condition::Twilight, 5), Condition::Day);
        }
        // 5th: promotion fires.
        assert_eq!(h.update(Condition::Twilight, 5), Condition::Twilight);
    }

    #[test]
    fn agreeing_frame_resets_pending_streak() {
        let mut h = ClassifierHysteresis::default();
        let _ = h.update(Condition::Day, 5);
        // 3 Twilights, then Day, then 4 Twilights again: still Day.
        for _ in 0..3 {
            assert_eq!(h.update(Condition::Twilight, 5), Condition::Day);
        }
        assert_eq!(h.update(Condition::Day, 5), Condition::Day);
        for _ in 0..4 {
            assert_eq!(h.update(Condition::Twilight, 5), Condition::Day);
        }
        // 5th consecutive Twilight after the reset: promote.
        assert_eq!(h.update(Condition::Twilight, 5), Condition::Twilight);
    }

    #[test]
    fn different_disagreeing_observation_restarts_streak() {
        let mut h = ClassifierHysteresis::default();
        let _ = h.update(Condition::Day, 5);
        // Twilight pending for 3 frames.
        for _ in 0..3 {
            let _ = h.update(Condition::Twilight, 5);
        }
        // Then Night: pending switches to Night, count resets to 1.
        assert_eq!(h.update(Condition::Night, 5), Condition::Day);
        // 4 more Nights: total 5 → promotion.
        for _ in 0..4 {
            let _ = h.update(Condition::Night, 5);
        }
        // After 5 Nights total, smoothed should be Night.
        assert_eq!(h.current(), Some(Condition::Night));
    }

    #[test]
    fn settle_frames_zero_or_one_disables_hysteresis() {
        let mut h = ClassifierHysteresis::default();
        let _ = h.update(Condition::Day, 0);
        // settle=0 is treated as 1: any new observation
        // becomes smoothed immediately.
        assert_eq!(h.update(Condition::Twilight, 0), Condition::Twilight);
        let mut h = ClassifierHysteresis::default();
        let _ = h.update(Condition::Day, 1);
        assert_eq!(h.update(Condition::Twilight, 1), Condition::Twilight);
    }
}
