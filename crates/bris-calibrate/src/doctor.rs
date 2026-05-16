//! Calibration quality diagnostic.
//!
//! Inspects a [`crate::CalibrationResult`] for common
//! failure modes and reports them with operator-actionable
//! advice. Run after [`crate::calibrate`] before trusting
//! the result.
//!
//! # Checks performed
//!
//! - **RMS reprojection error** > 1.0 px: warn; > 2.0 px:
//!   error. Sub-pixel is the target.
//! - **View count** < 10: warn; < 5: error. Zhang's solve
//!   technically converges with 3 views but is brittle
//!   (degenerate when all views share an axis); ≥ 15
//!   well-distributed views is the comfortable regime.
//! - **Focal length** non-positive or wildly different
//!   between fx/fy (>10% asymmetry): error. Real cameras
//!   have fx ≈ fy unless the sensor has rectangular pixels
//!   (extremely rare).
//! - **Principal point** more than 20% of the image away
//!   from the center: warn (could indicate a mounting
//!   problem or a calibration that fitted noise).
//! - **Distortion magnitude**: |k1| > 0.5 is unusual for
//!   anything but fisheye lenses; warn if outside that
//!   range. |p1|, |p2| > 0.01 is unusual; warn.
//!
//! # Output
//!
//! [`Diagnosis`] carries a list of [`DiagnosisIssue`]s and
//! an overall [`DiagnosisLevel`] (the worst issue). The CLI
//! prints them in order with a one-line remediation hint
//! per issue.

use crate::solve::CalibrationResult;

/// Severity level of a single diagnostic finding (or the
/// overall diagnosis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosisLevel {
    /// Calibration looks healthy; no concerns flagged.
    Ok,
    /// Calibration is usable but a quality concern is
    /// noted. Operators may want to re-shoot for better
    /// results but the fix isn't blocked.
    Warn,
    /// Calibration is unlikely to produce trustworthy
    /// fixes. Operators should re-shoot before relying on
    /// it.
    Error,
}

impl DiagnosisLevel {
    /// Stable string label for human-readable rendering.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// One issue surfaced by the diagnostic.
#[derive(Debug, Clone)]
pub struct DiagnosisIssue {
    /// Severity of this issue.
    pub level: DiagnosisLevel,
    /// Short identifier (suitable for log fields and
    /// machine-readable output).
    pub code: &'static str,
    /// Human-readable description of what was found.
    pub message: String,
    /// Operator-actionable remediation advice.
    pub remediation: &'static str,
}

/// Aggregate diagnostic over a calibration result.
#[derive(Debug, Clone)]
pub struct Diagnosis {
    /// Worst severity across all `issues`. `Ok` if the
    /// vector is empty.
    pub overall: DiagnosisLevel,
    /// Per-finding details, in detection order.
    pub issues: Vec<DiagnosisIssue>,
}

/// Run all checks against a calibration result.
///
/// Pure function; doesn't read files or touch hardware.
#[must_use]
pub fn diagnose(result: &CalibrationResult) -> Diagnosis {
    let mut issues: Vec<DiagnosisIssue> = Vec::new();
    check_view_count(result, &mut issues);
    check_reproj_error(result, &mut issues);
    check_focal_lengths(result, &mut issues);
    check_principal_point(result, &mut issues);
    check_distortion(result, &mut issues);
    let overall = issues
        .iter()
        .map(|i| i.level)
        .max()
        .unwrap_or(DiagnosisLevel::Ok);
    Diagnosis { overall, issues }
}

fn check_view_count(result: &CalibrationResult, issues: &mut Vec<DiagnosisIssue>) {
    if result.view_count < 5 {
        issues.push(DiagnosisIssue {
            level: DiagnosisLevel::Error,
            code: "view_count_too_low",
            message: format!(
                "calibrated against only {} views; Zhang's method needs ≥ 5 well-distributed \
                 views for a reliable solve",
                result.view_count
            ),
            remediation: "capture more frames covering varied tilts, distances, and \
                          board positions in the FOV",
        });
    } else if result.view_count < 10 {
        issues.push(DiagnosisIssue {
            level: DiagnosisLevel::Warn,
            code: "view_count_low",
            message: format!(
                "calibrated against only {} views; ≥ 15 is the comfortable regime",
                result.view_count
            ),
            remediation: "capture more frames if you're chasing tight residuals",
        });
    }
}

fn check_reproj_error(result: &CalibrationResult, issues: &mut Vec<DiagnosisIssue>) {
    let rms = result.mean_reproj_error_px;
    if rms > 2.0 {
        issues.push(DiagnosisIssue {
            level: DiagnosisLevel::Error,
            code: "reproj_error_high",
            message: format!(
                "mean reprojection error {rms:.2} px is well above the sub-pixel target",
            ),
            remediation: "re-shoot with sharper focus, less motion blur, and the board \
                          held steadier; verify the printed board is flat (mounted on \
                          rigid backing)",
        });
    } else if rms > 1.0 {
        issues.push(DiagnosisIssue {
            level: DiagnosisLevel::Warn,
            code: "reproj_error_elevated",
            message: format!(
                "mean reprojection error {rms:.2} px is above sub-pixel; calibration is \
                 usable but could be tighter",
            ),
            remediation: "if accuracy matters, re-shoot a subset of frames and run again",
        });
    }
}

fn check_focal_lengths(result: &CalibrationResult, issues: &mut Vec<DiagnosisIssue>) {
    let fx = result.intrinsics.fx;
    let fy = result.intrinsics.fy;
    if !fx.is_finite() || fx <= 0.0 || !fy.is_finite() || fy <= 0.0 {
        issues.push(DiagnosisIssue {
            level: DiagnosisLevel::Error,
            code: "focal_invalid",
            message: format!("focal lengths are non-positive or non-finite: fx={fx}, fy={fy}"),
            remediation: "the solve diverged; re-shoot calibration frames",
        });
        return;
    }
    let asymmetry = (fx - fy).abs() / fx.max(fy);
    if asymmetry > 0.10 {
        issues.push(DiagnosisIssue {
            level: DiagnosisLevel::Warn,
            code: "focal_asymmetric",
            message: format!(
                "fx and fy differ by {:.1}% (fx={:.1}, fy={:.1}); real cameras typically \
                 have fx ≈ fy",
                asymmetry * 100.0,
                fx,
                fy
            ),
            remediation: "could indicate a board that wasn't flat, sensor cropping \
                          that's not square, or insufficient view diversity",
        });
    }
}

// dx_frac and dy_frac are domain-paired x/y twins; the
// similar_names lint flags them but renaming would just
// add visual noise.
#[allow(clippy::similar_names)]
fn check_principal_point(result: &CalibrationResult, issues: &mut Vec<DiagnosisIssue>) {
    let cx = result.intrinsics.cx;
    let cy = result.intrinsics.cy;
    let w = f64::from(result.image_width);
    let h = f64::from(result.image_height);
    let dx_frac = (cx - w / 2.0).abs() / w;
    let dy_frac = (cy - h / 2.0).abs() / h;
    if dx_frac > 0.20 || dy_frac > 0.20 {
        issues.push(DiagnosisIssue {
            level: DiagnosisLevel::Warn,
            code: "principal_point_off_center",
            message: format!(
                "principal point ({cx:.0}, {cy:.0}) is {dx_frac:.0}%/{dy_frac:.0}% off the \
                 image center ({:.0}, {:.0}); typical lenses are within 5%",
                w / 2.0,
                h / 2.0
            ),
            remediation: "could indicate the calibration solve fitted noise; re-shoot with \
                          more views distributed across the full FOV",
        });
    }
}

fn check_distortion(result: &CalibrationResult, issues: &mut Vec<DiagnosisIssue>) {
    let k1 = result.intrinsics.k1;
    let p1 = result.intrinsics.p1;
    let p2 = result.intrinsics.p2;
    if k1.abs() > 0.5 {
        issues.push(DiagnosisIssue {
            level: DiagnosisLevel::Warn,
            code: "k1_unusual",
            message: format!(
                "|k1| = {:.3} is unusually large for non-fisheye lenses; check the lens type",
                k1.abs()
            ),
            remediation: "fisheye lenses (FOV > 120°) need a different distortion model; \
                          Bris uses Brown-Conrady which fits standard rectilinear lenses well",
        });
    }
    if p1.abs() > 0.01 || p2.abs() > 0.01 {
        issues.push(DiagnosisIssue {
            level: DiagnosisLevel::Warn,
            code: "tangential_unusual",
            message: format!(
                "|p1| = {:.4}, |p2| = {:.4}; tangential distortion is usually < 0.005 \
                 even on poorly-mounted sensors",
                p1.abs(),
                p2.abs()
            ),
            remediation: "could indicate a sensor mounting issue or insufficient view \
                          diversity (the solver folds noise into the tangential terms)",
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bris_vision::Intrinsics;

    fn good_result() -> CalibrationResult {
        CalibrationResult {
            intrinsics: Intrinsics {
                fx: 600.0,
                fy: 601.0,
                cx: 320.0,
                cy: 240.0,
                k1: -0.05,
                k2: 0.10,
                k3: 0.0,
                p1: 0.0001,
                p2: -0.0002,
            },
            image_width: 640,
            image_height: 480,
            mean_reproj_error_px: 0.32,
            view_count: 22,
            observation_count: 1700,
            per_view: Vec::new(),
        }
    }

    #[test]
    fn healthy_result_diagnoses_ok() {
        let d = diagnose(&good_result());
        assert_eq!(d.overall, DiagnosisLevel::Ok);
        assert!(d.issues.is_empty(), "issues: {:#?}", d.issues);
    }

    #[test]
    fn high_reproj_error_diagnoses_error() {
        let mut r = good_result();
        r.mean_reproj_error_px = 2.5;
        let d = diagnose(&r);
        assert_eq!(d.overall, DiagnosisLevel::Error);
        assert!(d
            .issues
            .iter()
            .any(|i| i.code == "reproj_error_high" && i.level == DiagnosisLevel::Error));
    }

    #[test]
    fn elevated_reproj_error_diagnoses_warn() {
        let mut r = good_result();
        r.mean_reproj_error_px = 1.3;
        let d = diagnose(&r);
        assert_eq!(d.overall, DiagnosisLevel::Warn);
    }

    #[test]
    fn too_few_views_diagnoses_error() {
        let mut r = good_result();
        r.view_count = 4;
        let d = diagnose(&r);
        assert_eq!(d.overall, DiagnosisLevel::Error);
        assert!(d.issues.iter().any(|i| i.code == "view_count_too_low"));
    }

    #[test]
    fn marginal_view_count_diagnoses_warn() {
        let mut r = good_result();
        r.view_count = 7;
        let d = diagnose(&r);
        assert_eq!(d.overall, DiagnosisLevel::Warn);
    }

    #[test]
    fn invalid_focal_length_diagnoses_error() {
        let mut r = good_result();
        r.intrinsics.fx = -1.0;
        let d = diagnose(&r);
        assert_eq!(d.overall, DiagnosisLevel::Error);
    }

    #[test]
    fn asymmetric_focal_diagnoses_warn() {
        let mut r = good_result();
        r.intrinsics.fx = 600.0;
        r.intrinsics.fy = 800.0; // 25% asymmetry
        let d = diagnose(&r);
        assert!(d.issues.iter().any(|i| i.code == "focal_asymmetric"));
    }

    #[test]
    fn off_center_principal_point_diagnoses_warn() {
        let mut r = good_result();
        r.intrinsics.cx = 100.0; // 640/2 = 320; 100 is 34% off
        let d = diagnose(&r);
        assert!(d
            .issues
            .iter()
            .any(|i| i.code == "principal_point_off_center"));
    }

    #[test]
    fn extreme_distortion_diagnoses_warn() {
        let mut r = good_result();
        r.intrinsics.k1 = -0.7; // fisheye-ish
        let d = diagnose(&r);
        assert!(d.issues.iter().any(|i| i.code == "k1_unusual"));
    }

    #[test]
    fn diagnosis_level_orders_correctly() {
        assert!(DiagnosisLevel::Ok < DiagnosisLevel::Warn);
        assert!(DiagnosisLevel::Warn < DiagnosisLevel::Error);
    }
}
