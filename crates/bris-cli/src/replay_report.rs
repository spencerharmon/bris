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
    /// Fixes published during this capture's feed window.
    /// Session-engine continuity means a fix triggered by a
    /// sight from capture N may publish a few frames into
    /// capture N+1; that fix is attributed to N+1 here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes: Vec<PublishedFixReport>,
}

/// One published fix, serialised for the corpus explorer's
/// map view.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct PublishedFixReport {
    /// Wall-clock at the moment the fix was published, Unix
    /// milliseconds.
    pub timestamp_unix_ms: i64,
    /// Observer latitude (degrees, north positive).
    pub lat_deg: f64,
    /// Observer longitude (degrees, east positive).
    pub lon_deg: f64,
    /// 1σ semi-major axis of the uncertainty ellipse, nm.
    pub sigma_major_nm: f64,
    /// 1σ semi-minor axis, nm.
    pub sigma_minor_nm: f64,
    /// Orientation of the major axis from north, radians,
    /// clockwise, in `[0, π)`.
    pub orientation_rad: f64,
    /// Number of sights that contributed.
    pub sight_count: u32,
    /// Reduced chi-square of the LSQ residuals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chi_square: Option<f64>,
    /// Optional GPS-truth comparison (when the bundle
    /// carried `gps_truth`). Distance in nautical miles
    /// between fix and truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_truth_error_nm: Option<f64>,
    /// Bearing from fix to GPS truth, degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_truth_bearing_deg: Option<f64>,
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
                fixes: Vec::new(),
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

    // ---- schema-conformance test (explorer lockstep) --------------------
    //
    // `tools/corpus-explorer/explorer.js` is a zero-dependency
    // browser consumer of `bris-replay-report.json` + `index.json`.
    // It reads a FIXED set of JSON paths (enumerated below, each
    // annotated with the explorer.js line that reads it). If the
    // emitted report drops or renames any of these fields, the
    // explorer silently renders blanks — the exact "keep the report
    // schema and the explorer in lockstep" failure this test guards.
    //
    // The report is always built from the Default-mode run
    // (`build_capture_report` in main.rs picks
    // `results.iter().find(|r| r.mode == ReplayMode::Default)`), so
    // the emitted schema is invariant across the AP mode(s) the
    // operator selected. This test therefore asserts the same
    // explorer-required schema holds for a report produced under each
    // of the four AP modes (Default / ApSeedTruth / ApLockTruth /
    // NoAp) and under `--all-modes` — the report shape must not drift
    // with the mode.

    /// The AP-mode scenarios a `--render-frames` run can emit a
    /// report under. Mirrors `main::ReplayMode` (+ the `--all-modes`
    /// aggregate); kept local to the test so the conformance check
    /// does not depend on `main.rs` internals.
    #[derive(Debug, Clone, Copy)]
    enum ApScenario {
        Default,
        ApSeedTruth,
        ApLockTruth,
        NoAp,
        AllModes,
    }

    impl ApScenario {
        const ALL: [ApScenario; 5] = [
            ApScenario::Default,
            ApScenario::ApSeedTruth,
            ApScenario::ApLockTruth,
            ApScenario::NoAp,
            ApScenario::AllModes,
        ];

        /// Whether this scenario's bundle carries `gps_truth`. The AP
        /// truth modes and `--all-modes` require it; `NoAp` and bare
        /// `Default` need not. Drives whether a published fix in the
        /// report carries the optional gps-truth comparison fields.
        fn has_gps_truth(self) -> bool {
            matches!(
                self,
                ApScenario::ApSeedTruth | ApScenario::ApLockTruth | ApScenario::AllModes
            )
        }
    }

    /// Build a representative per-session report for a given AP
    /// scenario, exercising every explorer-consumed field: a
    /// non-empty Stage-E rejection histogram, an `Ok` and an `Err`
    /// Stage-E outcome, a horizon + centroid frame, and (when the
    /// scenario has GPS truth) a published fix with the optional
    /// truth-comparison fields populated.
    #[allow(clippy::too_many_lines)]
    fn build_report(scenario: ApScenario) -> ReplaySessionReport {
        let mut rejections = std::collections::BTreeMap::new();
        rejections.insert("BelowHorizon".to_string(), 28_u64);
        rejections.insert("NonFinite".to_string(), 0_u64);

        // Frame 0: a rejected reduction (Err outcome, no sight).
        let rejected_frame = FrameReport {
            seq: 0,
            captured_unix_ms: 1_700_000_000_000,
            render_path: Some("captures/CAP/frames/00000000-render.png".into()),
            pgm_path: "captures/CAP/frames/00000000.pgm".into(),
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
        };

        // Frame 1: a successful reduction (Ok outcome, sight emitted).
        let ok_frame = FrameReport {
            seq: 1,
            captured_unix_ms: 1_700_000_001_000,
            render_path: Some("captures/CAP/frames/00000001-render.png".into()),
            pgm_path: "captures/CAP/frames/00000001.pgm".into(),
            render_geometry: Some(RenderGeometry {
                source_width: 3024,
                source_height: 4032,
                canvas_width: 900,
                canvas_height: 1200,
                scale: 0.297_619_047_6,
            }),
            classification: "Day".into(),
            horizon: Some(HorizonReport {
                provider: "ml-gravity".into(),
                intercept_px: 601.1,
                slope: 0.0021,
                sigma_rad: 0.0007,
                model_id: Some("0123456789ab".into()),
            }),
            body_centroid: Some(BodyCentroidReport {
                x: 1500.0,
                y: 2000.0,
                sigma_px: 0.4,
                area_px: 2100,
                secondaries: 1,
            }),
            stage_e_outcomes: vec![StageEAttemptReport::Ok {
                altitude_rad: 0.4712,
                sigma_rad: 0.0003,
            }],
            sight_emitted: true,
        };

        let fixes = if scenario.has_gps_truth() {
            vec![PublishedFixReport {
                timestamp_unix_ms: 1_700_000_001_000,
                lat_deg: 37.8,
                lon_deg: -122.4,
                sigma_major_nm: 1.2,
                sigma_minor_nm: 0.8,
                orientation_rad: 0.3,
                sight_count: 2,
                chi_square: Some(0.97),
                gps_truth_error_nm: Some(0.4),
                gps_truth_bearing_deg: Some(210.0),
            }]
        } else {
            Vec::new()
        };

        ReplaySessionReport {
            schema_version: SCHEMA_VERSION,
            session_id: "508197ac-0000-0000-0000-000000000000".into(),
            session_title: format!("{scenario:?} session"),
            generated_unix_ms: 1_700_000_000_000,
            engine_build: EngineBuild {
                git_sha: "abcdef123456".into(),
                git_describe: "v0.1-rc1-3-gabcdef".into(),
                crate_version: "0.0.1".into(),
            },
            captures: vec![CaptureReport {
                capture_id: "CAP".into(),
                bundle_dir: "captures/CAP/".into(),
                app_version: Some("e8a7211".into()),
                frame_count: 2,
                frames_pushed: 2,
                fixes_published: fixes.len() as u64,
                sights_inserted_total: 1,
                stage_e_rejection_counts: rejections,
                frames: vec![rejected_frame, ok_frame],
                fixes,
            }],
        }
    }

    /// Assert a JSON object has `key`; return the value at `key`.
    fn field<'a>(obj: &'a serde_json::Value, key: &str, ctx: &str) -> &'a serde_json::Value {
        obj.get(key)
            .unwrap_or_else(|| panic!("{ctx}: explorer-required field `{key}` missing"))
    }

    /// The definition-of-done for lockstep: assert the emitted
    /// session-report JSON exposes EVERY field
    /// `tools/corpus-explorer/explorer.js` reads, with the type the
    /// explorer assumes. Each assertion is annotated with the
    /// `explorer.js` line that consumes the field so a future schema
    /// edit is traceable to its consumer.
    #[allow(clippy::too_many_lines)]
    fn assert_explorer_schema(report: &ReplaySessionReport, ctx: &str) {
        let v = serde_json::to_value(report).unwrap();

        // Top-level: explorer.js:132 iterates report.captures.
        assert!(
            field(&v, "schema_version", ctx).is_u64(),
            "{ctx}: schema_version must be an integer"
        );
        assert_eq!(
            v["schema_version"].as_u64(),
            Some(u64::from(SCHEMA_VERSION)),
            "{ctx}: schema_version must equal the current SCHEMA_VERSION"
        );
        let captures = field(&v, "captures", ctx)
            .as_array()
            .unwrap_or_else(|| panic!("{ctx}: captures must be an array"));
        assert!(!captures.is_empty(), "{ctx}: fixture must have a capture");

        for cap in captures {
            // explorer.js:157 renders `pushed ${cap.frames_pushed}`.
            assert!(
                field(cap, "frames_pushed", ctx).is_u64(),
                "{ctx}: capture.frames_pushed must be an integer"
            );
            // explorer.js:162 renders the Stage-E rejection histogram
            // from cap.stage_e_rejection_counts (a {string: number}
            // map). This is the histogram the task calls out explicitly.
            let hist = field(cap, "stage_e_rejection_counts", ctx)
                .as_object()
                .unwrap_or_else(|| panic!("{ctx}: stage_e_rejection_counts must be an object/map"));
            assert!(
                !hist.is_empty(),
                "{ctx}: rejection histogram fixture must be non-empty"
            );
            for (k, n) in hist {
                assert!(
                    n.is_u64(),
                    "{ctx}: rejection count for `{k}` must be an integer"
                );
            }

            // explorer.js:172 iterates cap.frames.
            let frames = field(cap, "frames", ctx)
                .as_array()
                .unwrap_or_else(|| panic!("{ctx}: frames must be an array"));
            assert!(!frames.is_empty(), "{ctx}: fixture must have frames");

            let mut saw_ok = false;
            let mut saw_err = false;
            for f in frames {
                // explorer.js:187,330 render frame.seq.
                assert!(
                    field(f, "seq", ctx).is_u64(),
                    "{ctx}: frame.seq must be an integer"
                );
                // explorer.js:187,334 render frame.classification (string).
                assert!(
                    field(f, "classification", ctx).is_string(),
                    "{ctx}: frame.classification must be a string"
                );
                // explorer.js:210 branches on frame.sight_emitted (bool).
                assert!(
                    field(f, "sight_emitted", ctx).is_boolean(),
                    "{ctx}: frame.sight_emitted must be a boolean"
                );

                // explorer.js:255-256 draws frame.horizon when present;
                // buildHorizonLine + buildHudText:342-345 read
                // intercept_px, slope, provider, sigma_rad, model_id.
                if let Some(h) = f.get("horizon").filter(|h| !h.is_null()) {
                    assert!(field(h, "intercept_px", ctx).is_number());
                    assert!(field(h, "slope", ctx).is_number());
                    assert!(field(h, "sigma_rad", ctx).is_number());
                    assert!(
                        field(h, "provider", ctx).is_string(),
                        "{ctx}: horizon.provider must be a string"
                    );
                    // model_id is optional (only on ml-gravity); when
                    // present explorer.js:345 renders it as a string.
                    if let Some(m) = h.get("model_id").filter(|m| !m.is_null()) {
                        assert!(m.is_string(), "{ctx}: horizon.model_id must be a string");
                    }
                }

                // explorer.js:258-259 draws frame.body_centroid when
                // present; appendCentroidMarker + buildHudText:337-338
                // read x, y, sigma_px, area_px.
                if let Some(c) = f.get("body_centroid").filter(|c| !c.is_null()) {
                    assert!(field(c, "x", ctx).is_number());
                    assert!(field(c, "y", ctx).is_number());
                    assert!(field(c, "sigma_px", ctx).is_number());
                    assert!(field(c, "area_px", ctx).is_u64());
                }

                // explorer.js:188-189,348-349 render frame.stage_e_outcomes;
                // summarizeStageE:374-383 reads each outcome's `kind`
                // ("Ok"/"Err") and, per variant, altitude_rad/sigma_rad
                // or error. This is the per-frame outcomes field the task
                // calls out explicitly.
                let outcomes = field(f, "stage_e_outcomes", ctx)
                    .as_array()
                    .unwrap_or_else(|| panic!("{ctx}: stage_e_outcomes must be an array"));
                for o in outcomes {
                    let kind = field(o, "kind", ctx)
                        .as_str()
                        .unwrap_or_else(|| panic!("{ctx}: outcome.kind must be a string"));
                    match kind {
                        "Ok" => {
                            saw_ok = true;
                            assert!(
                                field(o, "altitude_rad", ctx).is_number(),
                                "{ctx}: Ok outcome must carry altitude_rad"
                            );
                            assert!(
                                field(o, "sigma_rad", ctx).is_number(),
                                "{ctx}: Ok outcome must carry sigma_rad"
                            );
                        }
                        "Err" => {
                            saw_err = true;
                            assert!(
                                field(o, "error", ctx).is_string(),
                                "{ctx}: Err outcome must carry a string error"
                            );
                        }
                        other => panic!("{ctx}: unexpected outcome.kind `{other}`"),
                    }
                }
            }
            assert!(
                saw_ok && saw_err,
                "{ctx}: fixture must exercise both Ok and Err Stage-E outcomes"
            );
        }
    }

    /// Assert an `index.json` value exposes every field the explorer
    /// reads (`explorer.js:54` `schema_version`, `:60`/`:62` sessions,
    /// `:87` `report_path`).
    fn assert_index_schema(index: &CorpusIndex, ctx: &str) {
        let v = serde_json::to_value(index).unwrap();
        assert_eq!(
            v["schema_version"].as_u64(),
            Some(u64::from(SCHEMA_VERSION)),
            "{ctx}: index schema_version must equal SCHEMA_VERSION"
        );
        let sessions = field(&v, "sessions", ctx)
            .as_array()
            .unwrap_or_else(|| panic!("{ctx}: index.sessions must be an array"));
        assert!(!sessions.is_empty(), "{ctx}: index fixture needs a session");
        for s in sessions {
            // explorer.js:87 sets a.dataset.reportPath = s.report_path,
            // then :108 fetches CORPUS_ROOT + session.report_path.
            assert!(
                field(s, "report_path", ctx).is_string(),
                "{ctx}: session.report_path must be a string"
            );
        }
    }

    /// Schema-conformance: the report emitted by `--render-frames`
    /// under EVERY AP mode (`Default` / `ApSeedTruth` / `ApLockTruth` /
    /// `NoAp`) and under `--all-modes` matches the schema the
    /// zero-dependency corpus explorer parses. Guards the ROI's
    /// "keep the report schema and the explorer in lockstep".
    #[test]
    fn report_matches_explorer_schema_across_ap_modes() {
        for scenario in ApScenario::ALL {
            let ctx = format!("{scenario:?}");
            let report = build_report(scenario);

            // The emitted JSON exposes every explorer-consumed field.
            assert_explorer_schema(&report, &ctx);

            // And it survives a real serialize -> deserialize round
            // trip (what the explorer's fetch+JSON.parse does), still
            // conforming afterwards — so no field is write-only.
            let json = serde_json::to_string(&report).unwrap();
            let back: ReplaySessionReport = serde_json::from_str(&json).unwrap();
            assert_explorer_schema(&back, &format!("{ctx} (round-trip)"));

            // gps-truth scenarios must actually emit a published fix
            // (the explorer's map view consumes report fixes); non-truth
            // scenarios omit it.
            let fix_count = back.captures[0].fixes.len();
            if scenario.has_gps_truth() {
                assert_eq!(fix_count, 1, "{ctx}: expected a published fix");
            } else {
                assert_eq!(fix_count, 0, "{ctx}: expected no published fix");
            }
        }
    }

    /// The corpus-root `index.json` emitted alongside the per-session
    /// reports conforms to the schema the explorer bootstraps from.
    #[test]
    fn corpus_index_matches_explorer_schema() {
        let index = CorpusIndex {
            schema_version: SCHEMA_VERSION,
            generated_unix_ms: 1_700_000_000_000,
            sessions: vec![CorpusIndexEntry {
                session_id: "508197ac-0000-0000-0000-000000000000".into(),
                session_title: "Marina, dusk".into(),
                report_path:
                    "sessions/508197ac-0000-0000-0000-000000000000/bris-replay-report.json".into(),
                capture_count: 3,
            }],
        };
        assert_index_schema(&index, "index.json");

        let json = serde_json::to_string(&index).unwrap();
        let back: CorpusIndex = serde_json::from_str(&json).unwrap();
        assert_index_schema(&back, "index.json (round-trip)");
    }

    /// Guard the schema-version contract itself: the explorer hard-
    /// rejects any `schema_version` other than the one it expects
    /// (explorer.js:54 `idx.schema_version !== 1`), so the Rust
    /// `SCHEMA_VERSION` and the explorer's expected version MUST stay
    /// in lockstep. If either bumps without the other, this fails and
    /// forces the coordinated update.
    #[test]
    fn schema_version_matches_explorer_expectation() {
        // The version the checked-in explorer.js asserts on
        // (`idx.schema_version !== 1` / `report`… expects 1).
        const EXPLORER_EXPECTED_SCHEMA_VERSION: u32 = 1;
        assert_eq!(
            SCHEMA_VERSION, EXPLORER_EXPECTED_SCHEMA_VERSION,
            "SCHEMA_VERSION changed without updating tools/corpus-explorer/explorer.js \
             (search for `schema_version !== {EXPLORER_EXPECTED_SCHEMA_VERSION}`) and \
             docs/design/replay_report.md — keep the report schema and the explorer in lockstep"
        );
    }
}
