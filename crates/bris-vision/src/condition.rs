//! Day / night / twilight condition classifier.
//!
//! The pipeline runs different algorithms depending on conditions:
//! day uses Sun/Moon centroiding; night uses peak detection plus
//! plate solving; twilight is a hybrid. This module provides the
//! function that decides which regime we're in.
//!
//! # Two evidence sources
//!
//! 1. **Image evidence.** Mean luminance over a horizontal band in
//!    the middle of the frame, plus the saturated-pixel fraction.
//!    The middle band avoids the bias from a deck (typically darker
//!    than the rest) or a bright sky-only top crop.
//! 2. **Astronomical prior** (optional). Sun altitude in degrees,
//!    computed by the caller from the almanac if they have an
//!    observer position and time. Maps directly to the standard
//!    twilight bands (civil / nautical / astronomical).
//!
//! When both sources are available we combine them. When they
//! agree, confidence is high. When they disagree, confidence drops
//! and the [`Classification::disagreement`] flag is set so the
//! caller knows not to trust the result.
//!
//! # No method-set selection here
//!
//! The classifier reports what conditions it sees. The decision of
//! which detectors to run is the engine's job (or the regression
//! harness's, when running offline cases). Keeping the
//! responsibilities separate makes both pieces simpler to test.
//!
//! # Why not ML?
//!
//! Day/night/twilight is a regime where simple physics
//! (image is bright ⇒ daylight; sun is below −18° ⇒ night) does
//! the job well. An ML classifier would add a 1-5 MB model and a
//! second source of failure for a problem that doesn't need it.
//! This is consistent with the project's "classical CV everywhere
//! we can" principle.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use crate::frame::Frame;

/// Lighting conditions in the captured scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// Daylight. Sun-altitude prior says ≥ 0°, or image is bright
    /// enough that no other regime fits.
    Day,
    /// Civil, nautical, or astronomical twilight. Sun between −18°
    /// and 0°, or image is in the dim-but-not-dark band.
    Twilight,
    /// Astronomical night. Sun ≤ −18°, or image is uniformly dark.
    Night,
    /// Image and astronomical evidence both fail to fit any of the
    /// above. Uniformly mid-gray, or saturated everywhere — a
    /// scene Bris cannot extract a sight from. Caller should
    /// surface this to the operator rather than guess.
    Unusable,
}

/// Image-derived evidence used by the classifier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageEvidence {
    /// Mean luminance over the middle horizontal third of the frame,
    /// normalized to [0, 1] over the u16 pixel range.
    pub mean_luma: f64,
    /// Fraction of pixels at or above 95% of `u16::MAX`. High → bright
    /// sun (or moon at very short exposure) in frame.
    pub saturated_fraction: f64,
}

/// Sun-altitude-derived evidence, when the caller supplies it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AstronomicalEvidence {
    /// Sun altitude relative to the geometric horizon, degrees.
    /// Pre-refraction, pre-dip — the standard reference used by the
    /// twilight definitions.
    pub sun_altitude_deg: f64,
    /// Which named band the altitude falls in.
    pub band: TwilightBand,
}

/// Standard twilight bands by sun altitude (degrees, geocentric
/// pre-refraction). The conventional cutoffs (Bowditch §22) put
/// the named boundary in the *lower* band: -6° is Nautical,
/// -12° is Astronomical, -18° is Night.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwilightBand {
    /// Sun altitude ≥ 0°: above the horizon. Full daylight.
    AboveHorizon,
    /// −6° < sun altitude < 0°. Sun below horizon but bright
    /// enough that artificial light isn't yet needed; horizon
    /// still clearly visible.
    Civil,
    /// −12° < sun altitude ≤ −6°. Sea horizon still visible against
    /// a faint sky glow; this is when navigators traditionally take
    /// star sights ("the morning / evening star fix").
    Nautical,
    /// −18° < sun altitude ≤ −12°. Most stars visible; horizon
    /// fading.
    Astronomical,
    /// Sun altitude ≤ −18°. True astronomical night.
    Night,
}

impl TwilightBand {
    /// Classify a sun altitude (degrees) into a named band. The
    /// boundary altitudes (-6°, -12°, -18°) are placed in the lower
    /// (darker) band to match the navigation-convention reading of
    /// "civil/nautical/astronomical twilight ends at altitude X."
    #[must_use]
    pub fn from_sun_altitude_deg(alt_deg: f64) -> Self {
        if alt_deg >= 0.0 {
            Self::AboveHorizon
        } else if alt_deg > -6.0 {
            Self::Civil
        } else if alt_deg > -12.0 {
            Self::Nautical
        } else if alt_deg > -18.0 {
            Self::Astronomical
        } else {
            Self::Night
        }
    }

    /// Map a band to the [`Condition`] it implies on its own. Some
    /// bands map to twilight regardless of image evidence; the
    /// classifier may still override based on disagreement.
    #[must_use]
    pub fn implied_condition(self) -> Condition {
        match self {
            Self::AboveHorizon => Condition::Day,
            Self::Civil | Self::Nautical | Self::Astronomical => Condition::Twilight,
            Self::Night => Condition::Night,
        }
    }
}

/// Output of [`classify`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Classification {
    /// The classifier's best guess.
    pub condition: Condition,
    /// Confidence in the guess, in [0, 1]. Roughly: 0.9+ means
    /// "image and almanac agree firmly," 0.6-0.9 means "agreement
    /// from one strong source," and ≤ 0.5 means "evidence is mixed
    /// or marginal."
    pub confidence: f64,
    /// Image-derived evidence used (always present).
    pub image_evidence: ImageEvidence,
    /// Astronomical evidence, if the caller supplied a sun altitude.
    pub astronomical_evidence: Option<AstronomicalEvidence>,
    /// True iff the image and astronomical evidence imply different
    /// conditions. The caller should treat the classification as
    /// unreliable; downstream pipelines should either run both
    /// method sets or surface the disagreement to the operator.
    pub disagreement: bool,
}

/// Classifier configuration. All knobs have defaults sized to the
/// standard twilight definitions and to typical 8-bit-widened-to-
/// u16 imagery.
#[derive(Debug, Clone, Copy)]
pub struct ConditionConfig {
    /// Below this normalized mean luma the image is "night".
    /// Default 0.05.
    pub night_max_luma: f64,
    /// Above this normalized mean luma the image is "day".
    /// Default 0.30.
    pub day_min_luma: f64,
    /// Above this saturated-pixel fraction the image must be
    /// daytime regardless of mean luma — a saturated bright body
    /// in frame implies the sun (or, much more rarely, the moon
    /// at short exposure). Default 0.005 (0.5%).
    pub saturation_force_day: f64,
    /// Vertical extent of the middle band used for mean-luma
    /// computation, as a fraction of frame height. Default 0.34
    /// (the middle third).
    pub middle_band_fraction: f64,
}

impl Default for ConditionConfig {
    fn default() -> Self {
        Self {
            night_max_luma: 0.05,
            day_min_luma: 0.30,
            saturation_force_day: 0.005,
            middle_band_fraction: 0.34,
        }
    }
}

/// Classify a frame's lighting conditions.
///
/// `sun_altitude_deg` is optional. When the caller has an observer
/// position and a capture time, computing it from
/// [`bris_almanac`] before calling lets the classifier consult the
/// astronomical prior and detect disagreement between the image
/// and the almanac. Without it, the classifier falls back to image
/// evidence alone with somewhat lower confidence.
///
/// This function never errors: it always returns *some*
/// classification, possibly [`Condition::Unusable`] with low
/// confidence when neither evidence source produces a clear answer.
/// The caller decides what to do about that.
#[must_use]
pub fn classify(
    frame: &Frame,
    sun_altitude_deg: Option<f64>,
    cfg: ConditionConfig,
) -> Classification {
    let image_evidence = compute_image_evidence(frame, cfg);
    let image_condition = classify_from_image(image_evidence, cfg);
    let astronomical_evidence = sun_altitude_deg.map(|alt| AstronomicalEvidence {
        sun_altitude_deg: alt,
        band: TwilightBand::from_sun_altitude_deg(alt),
    });

    match astronomical_evidence {
        None => Classification {
            condition: image_condition,
            // Image-only confidence: how cleanly the mean luma
            // separates from the nearest threshold. Bounded so
            // image-only classifications never claim almanac-grade
            // certainty.
            confidence: image_only_confidence(image_evidence, cfg),
            image_evidence,
            astronomical_evidence: None,
            disagreement: false,
        },
        Some(astro) => combine(image_evidence, image_condition, astro, cfg),
    }
}

fn combine(
    image_evidence: ImageEvidence,
    image_condition: Condition,
    astro: AstronomicalEvidence,
    cfg: ConditionConfig,
) -> Classification {
    let astro_condition = astro.band.implied_condition();
    let agree = image_condition == astro_condition;

    if agree {
        // Both sources point the same way. Confidence is the higher
        // of the two underlying confidences — this is the regime
        // where the classifier is most trustworthy.
        let img_conf = image_only_confidence(image_evidence, cfg);
        let astro_conf = astronomical_confidence(astro);
        Classification {
            condition: image_condition,
            confidence: img_conf.max(astro_conf),
            image_evidence,
            astronomical_evidence: Some(astro),
            disagreement: false,
        }
    } else {
        // Sources disagree. Take the more conservative (less-bright)
        // classification: if the clock says night and the image is
        // bright, "twilight" is safer than "day"; if the clock says
        // day and the image is dark, "twilight" is safer than
        // "night". `Unusable` if either source indicated it.
        let conservative = pick_conservative(image_condition, astro_condition);
        Classification {
            condition: conservative,
            confidence: 0.4_f64.min(image_only_confidence(image_evidence, cfg)),
            image_evidence,
            astronomical_evidence: Some(astro),
            disagreement: true,
        }
    }
}

/// Order: `Unusable` < `Day` < `Twilight` < `Night` (in the sense
/// "more conservative when the bright/dark evidence disagrees").
/// Returns the more conservative of two conditions.
fn pick_conservative(a: Condition, b: Condition) -> Condition {
    if a == Condition::Unusable || b == Condition::Unusable {
        return Condition::Unusable;
    }
    // When one says Day and the other Night, twilight is between.
    match (a, b) {
        (Condition::Day, Condition::Night) | (Condition::Night, Condition::Day) => {
            Condition::Twilight
        }
        // Day vs. Twilight, or Twilight vs. Night → Twilight is the
        // less-bright / less-dark of the pair, picked here.
        (Condition::Twilight, _) | (_, Condition::Twilight) => Condition::Twilight,
        // Same value (already handled by `agree`) — defensive.
        _ => a,
    }
}

fn classify_from_image(ev: ImageEvidence, cfg: ConditionConfig) -> Condition {
    if ev.saturated_fraction >= cfg.saturation_force_day {
        return Condition::Day;
    }
    if ev.mean_luma >= cfg.day_min_luma {
        Condition::Day
    } else if ev.mean_luma >= cfg.night_max_luma {
        Condition::Twilight
    } else {
        Condition::Night
    }
}

/// Image-only confidence: how far the mean luma is from the nearest
/// threshold, scaled and clamped to [0.4, 0.85]. We never report
/// 0.95+ confidence from image evidence alone, because uniformly
/// dim-mid-gray frames (haze, overcast twilight) read as "night"
/// or "twilight" with high mean-luma certainty but low actual
/// information about which regime we're in.
fn image_only_confidence(ev: ImageEvidence, cfg: ConditionConfig) -> f64 {
    let dist_to_night = (ev.mean_luma - cfg.night_max_luma).abs();
    let dist_to_day = (ev.mean_luma - cfg.day_min_luma).abs();
    let nearest = dist_to_night.min(dist_to_day);
    // Scale: a mean luma 0.10 away from the nearest threshold maps
    // to ≈ 0.85 confidence; very close to a threshold ⇒ ≈ 0.4.
    let scaled = 0.4 + 4.5 * nearest;
    scaled.clamp(0.4, 0.85)
}

/// Astronomical confidence: high when the sun altitude is well
/// inside a band, lower at boundaries. Capped below 1.0 because
/// even the almanac doesn't tell us about local clouds, eclipses,
/// or a flashlight aimed at the camera.
fn astronomical_confidence(astro: AstronomicalEvidence) -> f64 {
    // Distance from the nearest band boundary, in degrees.
    let alt = astro.sun_altitude_deg;
    let nearest_boundary = [0.0, -6.0, -12.0, -18.0]
        .iter()
        .map(|b| (alt - b).abs())
        .fold(f64::INFINITY, f64::min);
    // 6° from any boundary ⇒ very confident; 0° from boundary ⇒ 0.5.
    (0.5 + 0.075 * nearest_boundary).clamp(0.5, 0.95)
}

/// Compute mean luma over the middle horizontal band, plus the
/// saturated-pixel fraction.
fn compute_image_evidence(frame: &Frame, cfg: ConditionConfig) -> ImageEvidence {
    const SAT_THRESHOLD: u16 = (u16::MAX as u32 * 95 / 100) as u16;

    let h = frame.height();
    let band_height = ((f64::from(h) * cfg.middle_band_fraction).round() as u32).max(1);
    let y_start = h.saturating_sub(band_height) / 2;
    let y_end = (y_start + band_height).min(h);

    let w = frame.width();
    let pixels = frame.pixels();
    let row_stride = w as usize;

    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    let mut saturated: u64 = 0;
    let mut total: u64 = 0;

    // Mean luma is computed over the middle band only — top/bottom
    // bands are typically biased by sky / deck respectively.
    for y in y_start..y_end {
        let row = &pixels[(y as usize) * row_stride..(y as usize + 1) * row_stride];
        for &p in row {
            sum += u64::from(p);
            count += 1;
        }
    }

    // Saturation, on the other hand, is computed over the full
    // frame: a saturated body high in the sky shouldn't be missed
    // because the band cropped it out.
    for &p in pixels {
        total += 1;
        if p >= SAT_THRESHOLD {
            saturated += 1;
        }
    }

    let mean_luma = if count == 0 {
        0.0
    } else {
        (sum as f64) / (count as f64) / f64::from(u16::MAX)
    };
    let saturated_fraction = if total == 0 {
        0.0
    } else {
        (saturated as f64) / (total as f64)
    };

    ImageEvidence {
        mean_luma,
        saturated_fraction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, Intrinsics};
    use bris_core::time::{Tt, JD_J2000};

    fn frame_uniform(value: u16, w: u32, h: u32) -> Frame {
        let pixels = vec![value; (w * h) as usize];
        Frame::new(
            w,
            h,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(w, h),
        )
        .unwrap()
    }

    #[test]
    fn twilight_band_thresholds() {
        assert_eq!(
            TwilightBand::from_sun_altitude_deg(10.0),
            TwilightBand::AboveHorizon
        );
        assert_eq!(
            TwilightBand::from_sun_altitude_deg(0.0),
            TwilightBand::AboveHorizon
        );
        assert_eq!(
            TwilightBand::from_sun_altitude_deg(-3.0),
            TwilightBand::Civil
        );
        assert_eq!(
            TwilightBand::from_sun_altitude_deg(-6.0),
            TwilightBand::Nautical
        );
        assert_eq!(
            TwilightBand::from_sun_altitude_deg(-9.0),
            TwilightBand::Nautical
        );
        assert_eq!(
            TwilightBand::from_sun_altitude_deg(-12.0),
            TwilightBand::Astronomical
        );
        assert_eq!(
            TwilightBand::from_sun_altitude_deg(-18.0),
            TwilightBand::Night
        );
        assert_eq!(
            TwilightBand::from_sun_altitude_deg(-30.0),
            TwilightBand::Night
        );
    }

    #[test]
    fn image_only_uniform_bright_classifies_as_day() {
        // Mean luma at u16::MAX/2 = 0.5, well above day_min_luma.
        let frame = frame_uniform(u16::MAX / 2, 64, 64);
        let c = classify(&frame, None, ConditionConfig::default());
        assert_eq!(c.condition, Condition::Day);
        assert!(!c.disagreement);
        assert!(c.astronomical_evidence.is_none());
        assert!(c.confidence >= 0.4);
    }

    #[test]
    fn image_only_uniform_dark_classifies_as_night() {
        let frame = frame_uniform(100, 64, 64); // ≈ 0.0015 of u16::MAX
        let c = classify(&frame, None, ConditionConfig::default());
        assert_eq!(c.condition, Condition::Night);
        assert!(!c.disagreement);
    }

    #[test]
    fn image_only_uniform_dim_classifies_as_twilight() {
        // Mean luma in the [0.05, 0.30) band → twilight.
        // 0.15 × u16::MAX ≈ 9830.
        let frame = frame_uniform(9830, 64, 64);
        let c = classify(&frame, None, ConditionConfig::default());
        assert_eq!(c.condition, Condition::Twilight);
    }

    #[test]
    fn saturated_pixels_force_day_even_in_dim_frame() {
        // Mostly-dark frame with a small bright saturated patch in
        // the top-left: dim mean luma but saturation_fraction
        // exceeds the force-day threshold.
        let w: u32 = 100;
        let h: u32 = 100;
        let mut pixels = vec![0u16; (w * h) as usize];
        // 200 pixels saturated = 2% > 0.5% threshold.
        for p in pixels.iter_mut().take(200) {
            *p = u16::MAX;
        }
        let frame = Frame::new(
            w,
            h,
            pixels,
            Tt::from_julian_date(JD_J2000),
            0,
            Intrinsics::placeholder(w, h),
        )
        .unwrap();
        let c = classify(&frame, None, ConditionConfig::default());
        assert_eq!(c.condition, Condition::Day);
    }

    #[test]
    fn agreement_between_image_and_almanac_raises_confidence() {
        // Bright frame + sun above horizon = clear day.
        let frame = frame_uniform(u16::MAX / 2, 64, 64);
        let c = classify(&frame, Some(45.0), ConditionConfig::default());
        assert_eq!(c.condition, Condition::Day);
        assert!(!c.disagreement);
        // Should be at least as confident as the image-only path.
        let img_only = classify(&frame, None, ConditionConfig::default());
        assert!(c.confidence >= img_only.confidence);
    }

    #[test]
    fn disagreement_dark_image_with_high_sun_flags_disagreement() {
        // Sun says day, image says night — operator should be told
        // the result is unreliable.
        let frame = frame_uniform(100, 64, 64);
        let c = classify(&frame, Some(45.0), ConditionConfig::default());
        assert!(c.disagreement, "expected disagreement flag");
        assert!(c.confidence <= 0.4 + 1e-9);
        // Conservative pick between Day and Night is Twilight.
        assert_eq!(c.condition, Condition::Twilight);
    }

    #[test]
    fn disagreement_bright_image_with_low_sun_flags_disagreement() {
        // E.g. floodlight on the sea at night.
        let frame = frame_uniform(u16::MAX / 2, 64, 64);
        let c = classify(&frame, Some(-30.0), ConditionConfig::default());
        assert!(c.disagreement);
        assert_eq!(c.condition, Condition::Twilight);
    }

    #[test]
    fn nautical_twilight_with_dim_image_agrees() {
        // Sun at -10° (nautical twilight), image dim → both agree
        // on twilight, no disagreement.
        let frame = frame_uniform(9830, 64, 64);
        let c = classify(&frame, Some(-10.0), ConditionConfig::default());
        assert_eq!(c.condition, Condition::Twilight);
        assert!(!c.disagreement);
    }

    #[test]
    fn astronomical_confidence_drops_at_band_boundary() {
        // Right at the −6° civil/nautical boundary, confidence
        // should be at the floor (0.5) rather than near the cap.
        let evidence = AstronomicalEvidence {
            sun_altitude_deg: -6.0,
            band: TwilightBand::from_sun_altitude_deg(-6.0),
        };
        let conf = astronomical_confidence(evidence);
        assert!(
            (conf - 0.5).abs() < 1e-9,
            "expected ~0.5 at boundary, got {conf}"
        );
        // Far from any boundary, confidence should be near the cap.
        let evidence = AstronomicalEvidence {
            sun_altitude_deg: 30.0,
            band: TwilightBand::from_sun_altitude_deg(30.0),
        };
        let conf = astronomical_confidence(evidence);
        assert!(
            conf > 0.85,
            "expected high confidence away from boundaries, got {conf}"
        );
    }

    #[test]
    fn pick_conservative_day_night_is_twilight() {
        assert_eq!(
            pick_conservative(Condition::Day, Condition::Night),
            Condition::Twilight
        );
        assert_eq!(
            pick_conservative(Condition::Night, Condition::Day),
            Condition::Twilight
        );
    }

    #[test]
    fn pick_conservative_unusable_dominates() {
        assert_eq!(
            pick_conservative(Condition::Unusable, Condition::Day),
            Condition::Unusable
        );
        assert_eq!(
            pick_conservative(Condition::Night, Condition::Unusable),
            Condition::Unusable
        );
    }
}
