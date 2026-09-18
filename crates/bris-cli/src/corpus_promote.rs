//! Corpus-promotion tooling: field capture → regression `case.toml`.
//!
//! Closes the loop described in `docs/design/diagnostic_collection.md`:
//! a reviewed field capture (a stored bris-bundle v1 submission, or
//! any bundle-shaped capture directory — `bundle.json` + a
//! `media/`/`frames/` frame layout, per `bris_bundle::enumerate_frames`)
//! can be promoted into a `tests/regression/<case>/case.toml` skeleton
//! consumable by the `bris-vision` regression harness
//! (`crates/bris-vision/tests/regression_test.rs`), instead of an
//! operator hand-writing the TOML and eyeballing pixel coordinates.
//!
//! The generated `case.toml` is a **skeleton**, not a locked
//! baseline: every expectation is derived by actually *running* the
//! vision pipeline (classifier, centroid, both horizon detectors)
//! against the capture's frame 0, so it's immediately runnable, but
//! an operator must review it — confirm the derived numbers reflect
//! ground truth for the scene, add a `reference_observer` table if
//! astronomical cross-checks are wanted, and commit it into the real
//! corpus directory (`crates/bris-vision/tests/regression/`) — before
//! it's promoted to a real regression baseline.
//!
//! Errors from the vision pipeline (e.g. `detect_horizon` fails to
//! find a horizon) are captured too: the skeleton records `outcome =
//! "err"` with the actual error text, exactly as a hand-written
//! `expected_failure` case would, rather than silently omitting the
//! table.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bris_bundle::{enumerate_frames, BundleManifest};
use bris_core::time::{Tt, JD_J2000};
use bris_vision::{
    centroid_brightest_body, classify, detect_horizon, detect_horizon_via_sky_region,
    load_frame_from_path_with_rotation, CentroidConfig, Condition, ConditionConfig, HorizonConfig,
    HorizonError, HorizonLine, Intrinsics, Rotation,
};

/// Arguments to [`promote`].
#[derive(Debug)]
pub(crate) struct PromoteArgs {
    /// Directory holding the stored capture: `bundle.json` plus a
    /// `media/` or `frames/` frame layout (see
    /// `bris_bundle::enumerate_frames`).
    pub capture_dir: PathBuf,
    /// Directory the generated case is written into (created if
    /// missing). Conventionally
    /// `crates/bris-vision/tests/regression/<name>/`.
    pub case_dir: PathBuf,
    /// Case name; must match `case_dir`'s final component per the
    /// harness's convention, but this tool doesn't enforce that —
    /// the harness's own tests do.
    pub case_name: String,
    /// Free-text description copied into `case.toml`.
    pub description: String,
}

/// Promote a stored capture into a regression-corpus case skeleton.
///
/// Reads `bundle.json`, locates frame 0 via
/// `bris_bundle::enumerate_frames`, decodes and re-encodes it as
/// `frame.png` under `case_dir`, runs the classifier/centroid/
/// horizon detectors against it, and writes `case_dir/case.toml`
/// with the derived values as expectation stubs.
///
/// # Errors
///
/// Returns an error if the bundle manifest is missing/unparseable,
/// the capture has zero frames, the frame fails to decode, or the
/// output directory can't be created/written.
pub(crate) fn promote(args: &PromoteArgs) -> Result<PathBuf> {
    let bundle_path = args.capture_dir.join("bundle.json");
    let bundle_bytes =
        std::fs::read(&bundle_path).with_context(|| format!("read {}", bundle_path.display()))?;
    let manifest: BundleManifest = serde_json::from_slice(&bundle_bytes)
        .with_context(|| format!("parse {}", bundle_path.display()))?;

    let frames = enumerate_frames(&args.capture_dir).with_context(|| {
        format!(
            "enumerate frames under {}",
            args.capture_dir.display()
        )
    })?;
    let first = frames.first().with_context(|| {
        format!(
            "capture {} has zero frames",
            args.capture_dir.display()
        )
    })?;

    std::fs::create_dir_all(&args.case_dir)
        .with_context(|| format!("create {}", args.case_dir.display()))?;

    // Decode frame 0 and re-encode as PNG under the case dir — the
    // corpus convention is PNG, not the on-device raw PGM.
    let src_img = image::open(&first.pgm)
        .with_context(|| format!("decode {}", first.pgm.display()))?;
    let (src_w, src_h) = (
        image::GenericImageView::width(&src_img),
        image::GenericImageView::height(&src_img),
    );
    let frame_png = args.case_dir.join("frame.png");
    src_img
        .save(&frame_png)
        .with_context(|| format!("write {}", frame_png.display()))?;

    let rotation_deg = manifest.capture.source_rotation_deg;
    let rotation = Rotation::from_degrees(rotation_deg).map_err(|d| {
        anyhow::anyhow!("bundle declares unsupported source_rotation_deg {d}")
    })?;
    let (post_w, post_h) = match rotation {
        Rotation::Deg0 | Rotation::Deg180 => (src_w, src_h),
        Rotation::Deg90 | Rotation::Deg270 => (src_h, src_w),
    };
    let intrinsics = Intrinsics::placeholder(post_w, post_h);
    let frame = load_frame_from_path_with_rotation(
        &frame_png,
        Tt::from_julian_date(JD_J2000),
        0,
        intrinsics,
        rotation,
    )
    .with_context(|| format!("load {}", frame_png.display()))?;

    let classification = classify(&frame, None, ConditionConfig::default());
    let centroid = centroid_brightest_body(&frame, CentroidConfig::default());
    let gradient = detect_horizon(&frame, HorizonConfig::default());
    let sky_region = detect_horizon_via_sky_region(&frame, HorizonConfig::default());

    let toml_text = render_case_toml(
        &args.case_name,
        &args.description,
        post_w,
        post_h,
        rotation_deg,
        &classification,
        centroid.as_ref().ok(),
        gradient.as_ref(),
        sky_region.as_ref(),
    );
    let case_toml_path = args.case_dir.join("case.toml");
    std::fs::write(&case_toml_path, toml_text)
        .with_context(|| format!("write {}", case_toml_path.display()))?;

    Ok(case_toml_path)
}

fn condition_str(c: Condition) -> &'static str {
    match c {
        Condition::Day => "day",
        Condition::Twilight => "twilight",
        Condition::Night => "night",
        Condition::Unusable => "unusable",
    }
}

#[allow(clippy::too_many_arguments)]
fn render_case_toml(
    case_name: &str,
    description: &str,
    frame_width: u32,
    frame_height: u32,
    source_rotation_deg: u16,
    classification: &bris_vision::Classification,
    centroid: Option<&bris_vision::Centroid>,
    gradient: Result<&HorizonLine, &HorizonError>,
    sky_region: Result<&HorizonLine, &HorizonError>,
) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "# Regression case skeleton promoted from a field capture by\n\
         # `bris corpus promote`. GENERATED — review before treating\n\
         # this as a locked baseline: the values below are what the\n\
         # pipeline *actually produced* on this frame, not verified\n\
         # ground truth. Confirm they're correct for the scene (and\n\
         # add a [reference_observer] table for astronomical\n\
         # cross-checks) before committing to the real corpus.\n\n\
         [case]\n\
         name         = \"{case_name}\"\n\
         description  = \"{description}\"\n\
         kind         = \"working\"\n\
         frame_count  = 1\n\
         frame_width  = {frame_width}\n\
         frame_height = {frame_height}\n\
         source_rotation_deg = {source_rotation_deg}\n\n"
    );

    let _ = write!(
        out,
        "[expected_classifier]\n\
         condition = \"{}\"\n\
         # confidence observed: {:.3} — set min_confidence explicitly\n\
         # once reviewed; omitted here since a single sample doesn't\n\
         # establish a safe floor.\n\n",
        condition_str(classification.condition),
        classification.confidence,
    );

    if let Some(c) = centroid {
        let _ = write!(
            out,
            "[expected_centroid_frame0]\n\
             x_px         = {:.2}\n\
             y_px         = {:.2}\n\
             tolerance_px = 5.0\n\n",
            c.x, c.y,
        );
    } else {
        out.push_str(
            "# No [expected_centroid_frame0]: centroid_brightest_body found no\n\
             # bright body candidate on this frame (no sun/moon disc, or the\n\
             # scene is night). Add the table by hand if a body is expected.\n\n",
        );
    }

    out.push_str("[horizon.gradient]\n");
    out.push_str(&render_horizon_expectation(gradient));
    out.push('\n');

    out.push_str("[horizon.sky_region]\n");
    out.push_str(&render_horizon_expectation(sky_region));
    out.push('\n');

    out
}

fn render_horizon_expectation(result: Result<&HorizonLine, &HorizonError>) -> String {
    match result {
        Ok(line) => format!(
            "outcome             = \"ok\"\n\
             slope               = {:.4}\n\
             intercept           = {:.2}\n\
             slope_tolerance     = 0.05\n\
             intercept_tolerance = 15.0\n\
             inlier_count_min    = {}\n",
            line.slope, line.intercept, line.inlier_count,
        ),
        Err(e) => format!(
            "outcome       = \"err\"\n\
             error_variant = \"{e}\"\n"
        ),
    }
}

/// Resolve the case name from `case_dir`'s final path component, so
/// callers that only pass `--case-dir` still get a sensible default
/// `name` in the generated TOML.
#[must_use]
pub(crate) fn default_case_name(case_dir: &Path) -> String {
    case_dir
        .file_name()
        .map_or_else(|| "unnamed_case".to_string(), |s| s.to_string_lossy().into_owned())
}
