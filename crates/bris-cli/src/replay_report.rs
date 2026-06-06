//! Replay report data types and writers.
//!
//! `bris replay --render-frames` populates one
//! `ReplayCaptureReport` per capture by querying
//! [`bris_streaming::EngineDiagnostics`] after each
//! `push_frame`. Per-session, the reports for every capture in
//! a session are bundled into a [`ReplaySessionReport`] and
//! written as `bris-replay-report.json` at the session root.
//! Per-corpus, a lightweight `index.json` enumerates every
//! session whose report exists.
//!
//! Schema is documented in `docs/design/replay_report.md`.

#![allow(clippy::module_name_repetitions)]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// On-disk filename for a per-session replay report.
pub(crate) const SESSION_REPORT_FILENAME: &str = "bris-replay-report.json";

/// On-disk filename for the corpus-root index.
pub(crate) const CORPUS_INDEX_FILENAME: &str = "index.json";

/// Schema version for both [`ReplaySessionReport`] and
/// [`CorpusIndex`]. Additive within a version; breaking
/// changes bump the integer.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// Build metadata stamped into the report so consumers know
/// what produced it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct EngineBuild {
    /// Git short-sha at build time. Empty when unavailable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub git_sha: String,
    /// `git describe --always --dirty`. Empty when unavailable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub git_describe: String,
    /// `CARGO_PKG_VERSION` of `bris-cli`.
    pub crate_version: String,
}

impl EngineBuild {
    /// Resolve the build metadata at runtime. Reads
    /// `CARGO_PKG_VERSION` (always available) and shells out
    /// to `git` for sha / describe; missing git is silently
    /// reported as empty strings (the field's
    /// `skip_serializing_if` then omits them).
    #[must_use]
    pub(crate) fn current() -> Self {
        Self {
            git_sha: git_output(&["rev-parse", "--short=12", "HEAD"]),
            git_describe: git_output(&["describe", "--always", "--dirty"]),
            crate_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

fn git_output(args: &[&str]) -> String {
    std::process::Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Horizon record in the per-frame report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HorizonReport {
    /// Short provider label (e.g. `"gradient"`, `"vertical-line"`,
    /// `"ml-gravity"`).
    pub provider: String,
    /// Source-frame pixel intercept.
    pub intercept_px: f64,
    /// Slope (dy/dx in pixel units).
    pub slope: f64,
    /// Altitude-σ attributed to the horizon fit (radians).
    pub sigma_rad: f64,
    /// When `provider == "ml-gravity"`, the 12-char model id
    /// of the loaded ONNX file (BLAKE3-truncated). Absent on
    /// other providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

/// Body centroid record in the per-frame report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BodyCentroidReport {
    /// X coordinate in source-frame pixels.
    pub x: f64,
    /// Y coordinate in source-frame pixels.
    pub y: f64,
    /// 1σ positional uncertainty in source-frame pixels.
    pub sigma_px: f64,
    /// Connected-component area (source-frame pixels) for the
    /// day path, or contributing peak count for night/star
    /// paths.
    pub area_px: u32,
    /// Number of additional bodies above the area threshold.
    pub secondaries: u32,
}

/// One Stage E reduction attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "PascalCase")]
pub(crate) enum StageEAttemptReport {
    /// Reduction succeeded.
    Ok {
        /// Observed altitude (radians).
        altitude_rad: f64,
        /// Altitude 1σ (radians).
        sigma_rad: f64,
    },
    /// Reduction failed; `error` is a short variant name.
    Err {
        /// Short, stable error-variant identifier.
        error: String,
    },
}

/// Per-frame entry in the per-capture frame array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FrameReport {
    /// Zero-based frame index within the capture.
    pub seq: u32,
    /// Capture wall-clock (Unix milliseconds).
    pub captured_unix_ms: i64,
    /// Path to the annotated render PNG, relative to the
    /// corpus root. Absent when `--render-frames` did not
    /// produce one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_path: Option<String>,
    /// Path to the source PGM relative to the corpus root.
    pub pgm_path: String,
    /// Render geometry: lets the corpus explorer overlay
    /// horizon / centroid SVG client-side onto the cached
    /// base PNG without re-rendering. Absent on reports
    /// generated before this field shipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_geometry: Option<RenderGeometry>,
    /// Classification label
    /// (`"Day"`, `"Twilight"`, `"Night"`, `"Unusable"`).
    pub classification: String,
    /// Horizon outcome, `None` when the frame produced no
    /// horizon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon: Option<HorizonReport>,
    /// Body centroid, `None` when the frame produced no
    /// body candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_centroid: Option<BodyCentroidReport>,
    /// Stage E reduction attempts for this frame.
    pub stage_e_outcomes: Vec<StageEAttemptReport>,
    /// True iff at least one Stage E attempt succeeded on this
    /// frame.
    pub sight_emitted: bool,
}

/// Per-frame render geometry mirroring
/// [`bris_vision::RenderMetadata`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct RenderGeometry {
    /// Source frame width in pixels.
    pub source_width: u32,
    /// Source frame height in pixels.
    pub source_height: u32,
    /// Base-image canvas width in pixels (the PNG written
    /// to `render_path`).
    pub canvas_width: u32,
    /// Base-image canvas height in pixels.
    pub canvas_height: u32,
    /// Source-to-canvas scale: `canvas_x = source_x * scale`.
    pub scale: f64,
}

/// Per-capture report block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CaptureReport {
    /// Capture id (ULID / opaque string).
    pub capture_id: String,
    /// Bundle directory, relative to the session root or the
    /// corpus root.
    pub bundle_dir: String,
    /// `bundle.json.device.app_version`, if recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    /// Number of frames enumerated for this capture.
    pub frame_count: u32,
    /// Number of frames the engine successfully accepted.
    pub frames_pushed: u64,
    /// Number of fixes published over the capture.
    pub fixes_published: u64,
    /// Cumulative count of sights inserted into the active
    /// window.
    pub sights_inserted_total: u64,
    /// Histogram of Stage E reduction-error variants over the
    /// capture (e.g. `{"BelowHorizon": 28, "NonFinite": 0}`).
    pub stage_e_rejection_counts: std::collections::BTreeMap<String, u64>,
    /// Per-frame records, in capture order.
    pub frames: Vec<FrameReport>,
}

/// Per-session report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReplaySessionReport {
    /// Schema version ([`SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Session UUID as a string.
    pub session_id: String,
    /// Operator-entered session title.
    pub session_title: String,
    /// Generation timestamp (Unix milliseconds).
    pub generated_unix_ms: i64,
    /// Build metadata for the replay binary.
    pub engine_build: EngineBuild,
    /// One entry per capture in `ordered_capture_ids`.
    pub captures: Vec<CaptureReport>,
}

/// One entry in the corpus-root `index.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CorpusIndexEntry {
    /// Session UUID as a string.
    pub session_id: String,
    /// Operator-entered session title.
    pub session_title: String,
    /// Path to the per-session report, relative to the corpus
    /// root.
    pub report_path: String,
    /// Number of captures included in the session report.
    pub capture_count: u32,
}

/// Corpus-root index of available replay reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CorpusIndex {
    /// Schema version ([`SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Generation timestamp (Unix milliseconds).
    pub generated_unix_ms: i64,
    /// One entry per session found at write time.
    pub sessions: Vec<CorpusIndexEntry>,
}

/// Write a per-session report to `<session_dir>/bris-replay-report.json`.
///
/// # Errors
///
/// Returns `Err` on I/O or serialisation failure.
pub(crate) fn write_session_report(
    session_dir: &Path,
    report: &ReplaySessionReport,
) -> std::io::Result<PathBuf> {
    let path = session_dir.join(SESSION_REPORT_FILENAME);
    let bytes = serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?;
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Write a corpus index to `<corpus_root>/index.json`.
///
/// # Errors
///
/// Returns `Err` on I/O or serialisation failure.
pub(crate) fn write_corpus_index(
    corpus_root: &Path,
    index: &CorpusIndex,
) -> std::io::Result<PathBuf> {
    let path = corpus_root.join(CORPUS_INDEX_FILENAME);
    let bytes = serde_json::to_vec_pretty(index).map_err(std::io::Error::other)?;
    std::fs::write(&path, bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_report_round_trips_through_json() {
        let mut rejections = std::collections::BTreeMap::new();
        rejections.insert("BelowHorizon".to_string(), 6_u64);
        let report = ReplaySessionReport {
            schema_version: SCHEMA_VERSION,
            session_id: "508197ac-0000-0000-0000-000000000000".into(),
            session_title: "test session".into(),
            generated_unix_ms: 1_700_000_000_000,
            engine_build: EngineBuild {
                git_sha: "abcdef123456".into(),
                git_describe: "v0.1-rc1-3-gabcdef".into(),
                crate_version: "0.0.1".into(),
            },
            captures: vec![CaptureReport {
                capture_id: "0019abc".into(),
                bundle_dir: "sessions/UUID/captures/0019abc/".into(),
                app_version: Some("e8a7211".into()),
                frame_count: 7,
                frames_pushed: 7,
                fixes_published: 0,
                sights_inserted_total: 0,
                stage_e_rejection_counts: rejections,
                frames: vec![FrameReport {
                    seq: 0,
                    captured_unix_ms: 1_700_000_000_000,
                    render_path: Some(
                        "sessions/UUID/captures/0019abc/frames/00000000-render.png".into(),
                    ),
                    pgm_path: "sessions/UUID/captures/0019abc/frames/00000000.pgm".into(),
                    render_geometry: Some(RenderGeometry {
                        source_width: 3024,
                        source_height: 4032,
                        canvas_width: 900,
                        canvas_height: 1200,
                        scale: 0.297_619_047_6,
                    }),
                    classification: "Twilight".into(),
                    horizon: Some(HorizonReport {
                        provider: "vertical-line".into(),
                        intercept_px: 583.6,
                        slope: 0.0058,
                        sigma_rad: 0.001,
                        model_id: None,
                    }),
                    body_centroid: Some(BodyCentroidReport {
                        x: 1743.2,
                        y: 2979.9,
                        sigma_px: 0.5,
                        area_px: 1779,
                        secondaries: 0,
                    }),
                    stage_e_outcomes: vec![StageEAttemptReport::Err {
                        error: "BelowHorizon".into(),
                    }],
                    sight_emitted: false,
                }],
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: ReplaySessionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert_eq!(back.captures.len(), 1);
        assert_eq!(back.captures[0].frames.len(), 1);
        // Stage E rejection histogram round-trips.
        assert_eq!(
            back.captures[0]
                .stage_e_rejection_counts
                .get("BelowHorizon"),
            Some(&6),
        );
        match &back.captures[0].frames[0].stage_e_outcomes[0] {
            StageEAttemptReport::Err { error } => assert_eq!(error, "BelowHorizon"),
            other @ StageEAttemptReport::Ok { .. } => {
                panic!("expected Err variant, got {other:?}")
            }
        }
    }
}
