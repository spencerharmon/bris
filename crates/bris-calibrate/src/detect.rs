//! Chessboard corner detection from on-disk frames or in-memory buffers.
//!
//! Wraps the [`chess_corners`] (`ChESS` detector) and
//! `calib-targets` (chessboard grid extraction) crates into
//! both:
//!
//! - [`detect_corners_in_jpeg`] / [`detect_corners_in_image`]:
//!   single-frame primitives that report a [`FrameOutcome`]
//!   discriminating *why* a frame failed (no board, wrong
//!   grid size, decode error). Used by the Android shell to
//!   give the operator immediate per-capture feedback so they
//!   stop wasting captures on frames the solver will reject.
//! - [`detect_corners_in_directory`]: convenience wrapper that
//!   loops the per-frame primitive over a directory and
//!   collects successful views + per-frame outcomes for the
//!   eventual solve. Used by the CLI and by Android's
//!   "solve all captured frames" path.
//!
//! # Detection failure handling
//!
//! Real calibration captures often have frames where the
//! board is partially occluded, motion-blurred, or out of
//! focus. We don't bail on the whole job for one bad frame:
//! the directory walker collects per-frame outcomes alongside
//! the successful views so the caller can show the operator
//! exactly which frames failed and why.

use std::path::{Path, PathBuf};

use calib_targets::chessboard::{Detection as ChessboardDetection, DetectorParams};
use calib_targets::detect::detect_chessboard;
use image::GrayImage;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::sharpness::laplacian_variance;
use crate::target::CheckerboardTarget;

/// One frame's worth of detected chessboard corners, ready
/// to feed the calibration solve.
///
/// Each corner has both its **pixel position** (in the
/// captured frame) and its **board position** (which
/// corner in the board's grid, in board-fixed meters). The
/// solve uses pixel ↔ board correspondences to fit the
/// camera model.
#[derive(Debug, Clone)]
pub struct DetectedView {
    /// Source frame file path, retained for diagnostics
    /// (per-view residual reports name the offending file).
    pub source: PathBuf,
    /// Image width in pixels. Same for every view in a
    /// single calibration session.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// `(pixel_x, pixel_y, board_x_m, board_y_m)` tuples for
    /// every successfully-labelled corner in this view.
    pub correspondences: Vec<Correspondence>,
}

/// One pixel-to-board correspondence.
#[derive(Debug, Clone, Copy)]
pub struct Correspondence {
    /// Detected sub-pixel column.
    pub pixel_x: f64,
    /// Detected sub-pixel row.
    pub pixel_y: f64,
    /// Board X coordinate in meters (column index × square size).
    pub board_x_m: f64,
    /// Board Y coordinate in meters (row index × square size).
    pub board_y_m: f64,
}

/// Outcome of attempting chessboard detection on a single
/// frame.
///
/// Distinguishes the failure modes the per-frame loop
/// already discriminates internally so the caller (CLI
/// progress display, Android per-capture chip, FFI consumer)
/// can render an actionable message instead of "skipped 27
/// frames, good luck guessing why".
#[derive(Debug, Clone)]
pub enum FrameOutcome {
    /// Chessboard found and its grid matches the configured
    /// target. The carried [`DetectedView`] is ready to feed
    /// the solve; [`Self::n_corners`] is its labelled-corner
    /// count.
    Detected {
        /// Number of inner corners labelled in this frame.
        /// Should equal `target.rows * target.cols` on a
        /// fully-visible board; partial occlusion lowers it.
        n_corners: u32,
        /// Axis-aligned bounding box of the labelled
        /// corners in pixel coordinates `(min_x, min_y,
        /// max_x, max_y)`. Useful for overlay rendering.
        bbox_px: BoundingBox,
        /// Laplacian variance over the board's bounding box
        /// — a sharpness proxy. Higher is sharper. < ~50 on
        /// 8-bit luma typically indicates motion blur or
        /// defocus; threshold is scene-dependent.
        sharpness: f64,
        /// The view itself, ready to drop into a
        /// `Vec<DetectedView>` for the solve. Path is
        /// `PathBuf::new()` for in-memory inputs; the
        /// directory walker fills it in.
        view: DetectedView,
    },
    /// Detector ran but found nothing chessboard-shaped.
    /// Most common cause is motion blur, severe defocus, or
    /// the board outside the FOV.
    NoBoardFound,
    /// Detector found a chessboard but its grid dimensions
    /// don't match the configured target. Either the board
    /// is partially occluded (so fewer corners labelled
    /// than expected) or the operator selected the wrong
    /// `rows`/`cols`.
    WrongGridSize {
        /// Grid rows the detector recovered.
        found_rows: u32,
        /// Grid cols the detector recovered.
        found_cols: u32,
        /// Rows the operator configured.
        expected_rows: u32,
        /// Cols the operator configured.
        expected_cols: u32,
    },
    /// Image decode failed.
    DecodeFailed {
        /// Underlying decoder error message.
        reason: String,
    },
}

impl FrameOutcome {
    /// `true` if this outcome carries a usable detection.
    #[must_use]
    pub fn is_detected(&self) -> bool {
        matches!(self, FrameOutcome::Detected { .. })
    }

    /// Stable short identifier suitable for log fields and
    /// machine-readable output.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            FrameOutcome::Detected { .. } => "detected",
            FrameOutcome::NoBoardFound => "no_board",
            FrameOutcome::WrongGridSize { .. } => "wrong_grid",
            FrameOutcome::DecodeFailed { .. } => "decode_failed",
        }
    }
}

/// Pixel-space axis-aligned bounding box of a detected
/// board's labelled corners.
#[derive(Debug, Clone, Copy)]
pub struct BoundingBox {
    /// Smallest X (column) of any labelled corner.
    pub min_x: f64,
    /// Smallest Y (row) of any labelled corner.
    pub min_y: f64,
    /// Largest X (column) of any labelled corner.
    pub max_x: f64,
    /// Largest Y (row) of any labelled corner.
    pub max_y: f64,
}

impl BoundingBox {
    /// Compute the bbox of a set of `(x, y)` points. Returns
    /// `None` if the slice is empty.
    #[must_use]
    pub fn from_points(points: &[(f64, f64)]) -> Option<Self> {
        if points.is_empty() {
            return None;
        }
        let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
        let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for &(x, y) in points {
            if x < min_x {
                min_x = x;
            }
            if y < min_y {
                min_y = y;
            }
            if x > max_x {
                max_x = x;
            }
            if y > max_y {
                max_y = y;
            }
        }
        Some(Self {
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }

    /// Clamp bbox corners to `[0, width)` × `[0, height)`
    /// and return integer pixel bounds suitable for
    /// indexing an image buffer. Returns `None` if the
    /// clamped bbox is empty.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[must_use]
    pub fn clamp_to_image(&self, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
        if width == 0 || height == 0 {
            return None;
        }
        let x0 = self.min_x.floor().max(0.0) as u32;
        let y0 = self.min_y.floor().max(0.0) as u32;
        let x1 = (self.max_x.ceil() as i64).clamp(0, i64::from(width - 1)) as u32;
        let y1 = (self.max_y.ceil() as i64).clamp(0, i64::from(height - 1)) as u32;
        if x1 < x0 || y1 < y0 {
            return None;
        }
        Some((x0, y0, x1, y1))
    }
}

/// Errors during corner detection across a directory.
#[derive(Debug, Error)]
pub enum DetectError {
    /// Failed to enumerate the directory's contents.
    #[error("read directory {path}: {source}")]
    ReadDir {
        /// The directory that couldn't be read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Directory contained no usable image files.
    #[error("no image files found in {0}")]
    NoImages(PathBuf),
    /// Frames have inconsistent dimensions; calibration
    /// requires a single (width, height) per session.
    #[error(
        "frames have inconsistent dimensions: {first_path} is {first_w}×{first_h}, \
         {other_path} is {other_w}×{other_h}"
    )]
    DimensionMismatch {
        /// First frame's path.
        first_path: PathBuf,
        /// First frame's width.
        first_w: u32,
        /// First frame's height.
        first_h: u32,
        /// Mismatching frame's path.
        other_path: PathBuf,
        /// Mismatching width.
        other_w: u32,
        /// Mismatching height.
        other_h: u32,
    },
    /// Corner detection succeeded on too few frames to run
    /// the calibration solve (Zhang's planar method needs
    /// ≥ 3 views).
    #[error(
        "too few successful detections: got {detected} views (out of {tried} tried), need ≥ 3"
    )]
    TooFewViews {
        /// Frames where detection produced a usable view.
        detected: usize,
        /// Total frames attempted.
        tried: usize,
    },
}

/// Detect chessboard corners in an in-memory grayscale image.
///
/// The single-frame primitive shared by
/// [`detect_corners_in_jpeg`] and the directory walker. The
/// returned outcome's [`DetectedView`] (when present) has
/// `source` set to `PathBuf::new()` — the caller fills it in
/// when reading from disk.
///
/// # Errors
///
/// This function does not currently return errors; failures
/// are reported via the [`FrameOutcome`] variants.
#[must_use]
pub fn detect_corners_in_image(image: &GrayImage, target: CheckerboardTarget) -> FrameOutcome {
    let width = image.width();
    let height = image.height();
    let detector_params = DetectorParams::default();
    let Some(detection) = detect_chessboard(image, &detector_params) else {
        return FrameOutcome::NoBoardFound;
    };
    match view_from_detection(Path::new(""), width, height, &detection, target) {
        ViewExtraction::Ok(view) => {
            let points: Vec<(f64, f64)> = view
                .correspondences
                .iter()
                .map(|c| (c.pixel_x, c.pixel_y))
                .collect();
            let bbox = BoundingBox::from_points(&points).unwrap_or(BoundingBox {
                min_x: 0.0,
                min_y: 0.0,
                max_x: f64::from(width.saturating_sub(1)),
                max_y: f64::from(height.saturating_sub(1)),
            });
            let sharpness = bbox
                .clamp_to_image(width, height)
                .map_or(f64::NAN, |(x0, y0, x1, y1)| {
                    laplacian_variance(image, x0, y0, x1, y1)
                });
            #[allow(clippy::cast_possible_truncation)]
            let n_corners = view.correspondences.len() as u32;
            FrameOutcome::Detected {
                n_corners,
                bbox_px: bbox,
                sharpness,
                view,
            }
        }
        ViewExtraction::WrongGridSize {
            found_rows,
            found_cols,
        } => FrameOutcome::WrongGridSize {
            found_rows,
            found_cols,
            expected_rows: target.rows,
            expected_cols: target.cols,
        },
        ViewExtraction::Empty => FrameOutcome::NoBoardFound,
    }
}

/// Detect chessboard corners in a JPEG (or any
/// image-rs-supported format) byte buffer.
///
/// Convenience wrapper around [`detect_corners_in_image`]
/// that decodes the buffer first. Returns
/// [`FrameOutcome::DecodeFailed`] if the bytes don't decode
/// as an image.
///
/// # Errors
///
/// This function does not return errors; decode failures
/// are reported via [`FrameOutcome::DecodeFailed`].
#[must_use]
pub fn detect_corners_in_jpeg(bytes: &[u8], target: CheckerboardTarget) -> FrameOutcome {
    let cursor = std::io::Cursor::new(bytes);
    let reader = match image::ImageReader::new(cursor).with_guessed_format() {
        Ok(r) => r,
        Err(e) => {
            return FrameOutcome::DecodeFailed {
                reason: format!("guess format: {e}"),
            }
        }
    };
    let img = match reader.decode() {
        Ok(d) => d.to_luma8(),
        Err(e) => {
            return FrameOutcome::DecodeFailed {
                reason: format!("decode: {e}"),
            }
        }
    };
    detect_corners_in_image(&img, target)
}

/// Detect chessboard corners in every supported image file
/// in `directory`, in lexicographic filename order.
///
/// `target` describes the expected board geometry; detection
/// is filtered to keep only views whose recovered grid
/// matches the expected dimensions (after accounting for
/// the detector's choice of which axis is "rows").
///
/// Convenience wrapper around
/// [`detect_corners_in_directory_with_progress`] with a
/// no-op progress callback. Library callers and tests use
/// this; the CLI uses the `_with_progress` variant to drive
/// a progress bar.
///
/// # Errors
///
/// See [`DetectError`].
pub fn detect_corners_in_directory(
    directory: &Path,
    target: CheckerboardTarget,
) -> Result<DirectoryDetection, DetectError> {
    detect_corners_in_directory_with_progress(directory, target, &mut |_, _, _| {})
}

/// Detect chessboard corners in every supported image file
/// in `directory`, in lexicographic filename order, calling
/// `on_progress` once per frame with that frame's outcome.
///
/// `target` describes the expected board geometry; detection
/// is filtered to keep only views whose recovered grid
/// matches the expected dimensions (after accounting for
/// the detector's choice of which axis is "rows").
///
/// `on_progress(current, total, &outcome)` is called
/// immediately after processing the `current`-th frame
/// (0-indexed) of `total`. The CLI uses this to render
/// per-frame status alongside its progress bar; library
/// callers can pass a no-op via [`detect_corners_in_directory`].
///
/// Returns a [`DirectoryDetection`] carrying the successful
/// views, the per-frame outcomes (in path order), and an
/// aggregate [`DetectionStats`] summary.
///
/// # Errors
///
/// See [`DetectError`].
#[allow(
    clippy::too_many_lines,
    // r/d/w/h/fw/fh are local to the per-frame loop and the
    // visual pairing of width/height twins is clearer than
    // longer names would be.
    clippy::many_single_char_names,
)]
pub fn detect_corners_in_directory_with_progress(
    directory: &Path,
    target: CheckerboardTarget,
    on_progress: &mut dyn FnMut(usize, usize, &FrameDetection),
) -> Result<DirectoryDetection, DetectError> {
    let entries = std::fs::read_dir(directory).map_err(|e| DetectError::ReadDir {
        path: directory.to_path_buf(),
        source: e,
    })?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()).is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "ppm" | "pgm"
                )
            })
        })
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(DetectError::NoImages(directory.to_path_buf()));
    }
    info!(
        directory = %directory.display(),
        candidate_frames = paths.len(),
        rows = target.rows,
        cols = target.cols,
        "bris-calibrate: scanning frames"
    );

    let mut views: Vec<DetectedView> = Vec::with_capacity(paths.len());
    let mut per_frame: Vec<FrameDetection> = Vec::with_capacity(paths.len());
    let mut stats = DetectionStats {
        tried: paths.len(),
        skipped_no_board: 0,
        skipped_wrong_size: 0,
        skipped_io: 0,
    };
    let mut canonical_dims: Option<(PathBuf, u32, u32)> = None;
    let total = paths.len();

    for (i, path) in paths.iter().enumerate() {
        let outcome = match image::ImageReader::open(path) {
            Ok(r) => match r.decode() {
                Ok(d) => {
                    let img = d.to_luma8();
                    let w = img.width();
                    let h = img.height();
                    match &canonical_dims {
                        None => canonical_dims = Some((path.clone(), w, h)),
                        Some((first_path, fw, fh)) if (*fw, *fh) != (w, h) => {
                            return Err(DetectError::DimensionMismatch {
                                first_path: first_path.clone(),
                                first_w: *fw,
                                first_h: *fh,
                                other_path: path.clone(),
                                other_w: w,
                                other_h: h,
                            });
                        }
                        _ => {}
                    }
                    let mut o = detect_corners_in_image(&img, target);
                    // Stamp the source path into the view so per-view
                    // diagnostics name the file.
                    if let FrameOutcome::Detected { view, .. } = &mut o {
                        view.source.clone_from(path);
                    }
                    o
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "decode failed");
                    FrameOutcome::DecodeFailed {
                        reason: format!("decode: {e}"),
                    }
                }
            },
            Err(e) => {
                warn!(path = %path.display(), error = %e, "open failed");
                FrameOutcome::DecodeFailed {
                    reason: format!("open: {e}"),
                }
            }
        };
        match &outcome {
            FrameOutcome::Detected { view, .. } => {
                views.push(view.clone());
            }
            FrameOutcome::NoBoardFound => {
                debug!(path = %path.display(), "no chessboard detected");
                stats.skipped_no_board += 1;
            }
            FrameOutcome::WrongGridSize { .. } => {
                debug!(
                    path = %path.display(),
                    "detection grid didn't match expected {}×{}",
                    target.rows,
                    target.cols,
                );
                stats.skipped_wrong_size += 1;
            }
            FrameOutcome::DecodeFailed { .. } => {
                stats.skipped_io += 1;
            }
        }
        let detection = FrameDetection {
            path: path.clone(),
            outcome,
        };
        on_progress(i, total, &detection);
        per_frame.push(detection);
    }

    if views.len() < 3 {
        return Err(DetectError::TooFewViews {
            detected: views.len(),
            tried: paths.len(),
        });
    }
    info!(
        successful_views = views.len(),
        skipped_no_board = stats.skipped_no_board,
        skipped_wrong_size = stats.skipped_wrong_size,
        skipped_io = stats.skipped_io,
        "bris-calibrate: detection complete"
    );
    Ok(DirectoryDetection {
        views,
        per_frame,
        stats,
    })
}

/// One frame's path-tagged detection outcome from a
/// directory walk.
#[derive(Debug, Clone)]
pub struct FrameDetection {
    /// Source frame path.
    pub path: PathBuf,
    /// Outcome of detection on that frame.
    pub outcome: FrameOutcome,
}

/// Result of walking a directory of calibration frames.
#[derive(Debug, Clone)]
pub struct DirectoryDetection {
    /// Successful views, in path order. Feed straight into
    /// [`crate::calibrate`].
    pub views: Vec<DetectedView>,
    /// Per-frame outcomes, in path order. Same length as
    /// the candidate-frame count; the i-th entry's
    /// [`FrameDetection::outcome`] matches the i-th frame.
    pub per_frame: Vec<FrameDetection>,
    /// Aggregate counters.
    pub stats: DetectionStats,
}

/// Per-directory detection statistics. Useful for the CLI
/// to surface "23/30 frames detected; consider re-shooting
/// if you wanted higher coverage" advice.
#[derive(Debug, Clone, Copy)]
pub struct DetectionStats {
    /// Total frames considered.
    pub tried: usize,
    /// Frames where the detector found nothing
    /// chessboard-shaped.
    pub skipped_no_board: usize,
    /// Frames where the detector found a chessboard but
    /// the grid dimensions didn't match the configured
    /// target.
    pub skipped_wrong_size: usize,
    /// Frames that couldn't be opened or decoded.
    pub skipped_io: usize,
}

/// Internal result of [`view_from_detection`].
enum ViewExtraction {
    /// Successfully extracted a view matching the target.
    Ok(DetectedView),
    /// A grid was found but its dimensions didn't match.
    WrongGridSize { found_rows: u32, found_cols: u32 },
    /// The detection had no labelled corners (degenerate;
    /// reported as "no board found" to the caller).
    Empty,
}

/// Extract a [`DetectedView`] from a `chessboard::Detection`
/// if and only if its labelled grid matches the expected
/// `(rows, cols)` (in either orientation).
///
/// The detector reports the bounding box of the *labelled*
/// corners; we infer the grid dimensions from the maximum
/// (i, j) indices observed and accept the view when those
/// match the target either as (rows, cols) or as
/// (cols, rows). The latter handles boards captured rotated
/// 90° from the operator's mental image.
#[allow(clippy::cast_lossless)]
fn view_from_detection(
    source: &Path,
    width: u32,
    height: u32,
    detection: &ChessboardDetection,
    target: CheckerboardTarget,
) -> ViewExtraction {
    let mut max_i: i32 = i32::MIN;
    let mut max_j: i32 = i32::MIN;
    let mut min_i: i32 = i32::MAX;
    let mut min_j: i32 = i32::MAX;
    let mut points: Vec<(i32, i32, f64, f64)> = Vec::new();
    for c in &detection.target.corners {
        let Some(g) = c.grid else {
            continue;
        };
        max_i = max_i.max(g.i);
        max_j = max_j.max(g.j);
        min_i = min_i.min(g.i);
        min_j = min_j.min(g.j);
        points.push((g.i, g.j, c.position.x as f64, c.position.y as f64));
    }
    if points.is_empty() {
        return ViewExtraction::Empty;
    }
    // Cast: max_i ≥ min_i is guaranteed by the bounds-tracking
    // loop above (we only enter this code path with at least
    // one corner). max_i - min_i + 1 is therefore positive,
    // so the i32 → u32 cast doesn't wrap.
    #[allow(clippy::cast_sign_loss)]
    let span_i = (max_i - min_i + 1) as u32;
    #[allow(clippy::cast_sign_loss)]
    let span_j = (max_j - min_j + 1) as u32;
    // Accept either orientation: (span_i, span_j) ==
    // (rows, cols) or (cols, rows). The board-frame
    // axis convention (which dimension is "X") doesn't
    // matter for the calibration solve as long as the
    // 3D grid points are consistent within a view.
    let direct = span_i == target.rows && span_j == target.cols;
    let swapped = span_i == target.cols && span_j == target.rows;
    if !direct && !swapped {
        return ViewExtraction::WrongGridSize {
            found_rows: span_i,
            found_cols: span_j,
        };
    }

    // Build correspondences. Shift indices to be 0-based.
    let correspondences: Vec<Correspondence> = points
        .into_iter()
        .map(|(i, j, px, py)| {
            let board_i = (i - min_i) as f64;
            let board_j = (j - min_j) as f64;
            Correspondence {
                pixel_x: px,
                pixel_y: py,
                board_x_m: board_j * target.square_size_m,
                board_y_m: board_i * target.square_size_m,
            }
        })
        .collect();
    ViewExtraction::Ok(DetectedView {
        source: source.to_path_buf(),
        width,
        height,
        correspondences,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_directory_errors_with_no_images() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            detect_corners_in_directory(dir.path(), CheckerboardTarget::default()).unwrap_err();
        assert!(matches!(err, DetectError::NoImages(_)));
    }

    #[test]
    fn nonexistent_directory_errors_with_read_dir() {
        let err = detect_corners_in_directory(
            std::path::Path::new("/definitely/does/not/exist/bris-test"),
            CheckerboardTarget::default(),
        )
        .unwrap_err();
        assert!(matches!(err, DetectError::ReadDir { .. }));
    }

    #[test]
    fn no_board_in_blank_image_returns_no_board() {
        let img = GrayImage::from_pixel(640, 480, image::Luma([128]));
        let outcome = detect_corners_in_image(&img, CheckerboardTarget::default());
        assert!(
            matches!(outcome, FrameOutcome::NoBoardFound),
            "expected NoBoardFound on blank image, got {outcome:?}"
        );
    }

    #[test]
    fn malformed_jpeg_returns_decode_failed() {
        let bytes = b"this is not a JPEG, just some text bytes";
        let outcome = detect_corners_in_jpeg(bytes, CheckerboardTarget::default());
        match outcome {
            FrameOutcome::DecodeFailed { reason } => {
                assert!(
                    !reason.is_empty(),
                    "DecodeFailed reason should be non-empty"
                );
            }
            other => panic!("expected DecodeFailed, got {other:?}"),
        }
    }

    #[test]
    fn frame_outcome_code_labels_are_stable() {
        assert_eq!(FrameOutcome::NoBoardFound.code(), "no_board");
        assert_eq!(
            FrameOutcome::WrongGridSize {
                found_rows: 5,
                found_cols: 4,
                expected_rows: 7,
                expected_cols: 11,
            }
            .code(),
            "wrong_grid"
        );
        assert_eq!(
            FrameOutcome::DecodeFailed { reason: "x".into() }.code(),
            "decode_failed"
        );
    }

    #[test]
    fn bbox_from_points_handles_empty() {
        assert!(BoundingBox::from_points(&[]).is_none());
    }

    #[test]
    fn bbox_from_points_finds_extents() {
        let bbox =
            BoundingBox::from_points(&[(10.0, 20.0), (5.0, 50.0), (100.0, 30.0), (40.0, 15.0)])
                .unwrap();
        assert!((bbox.min_x - 5.0).abs() < 1e-12);
        assert!((bbox.min_y - 15.0).abs() < 1e-12);
        assert!((bbox.max_x - 100.0).abs() < 1e-12);
        assert!((bbox.max_y - 50.0).abs() < 1e-12);
    }

    #[test]
    fn bbox_clamp_caps_to_image_bounds() {
        let bbox = BoundingBox {
            min_x: -5.0,
            min_y: -5.0,
            max_x: 1000.0,
            max_y: 1000.0,
        };
        let (x0, y0, x1, y1) = bbox.clamp_to_image(640, 480).unwrap();
        assert_eq!((x0, y0), (0, 0));
        assert_eq!((x1, y1), (639, 479));
    }
}
