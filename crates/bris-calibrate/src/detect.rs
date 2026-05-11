//! Chessboard corner detection from on-disk frames.
//!
//! Wraps the [`chess_corners`] (`ChESS` detector) and
//! `calib-targets` (chessboard grid extraction) crates into
//! a single "directory of PNGs → list of detected views"
//! function suitable for the calibration workflow.
//!
//! # Detection failure handling
//!
//! Real calibration captures often have frames where the
//! board is partially occluded, motion-blurred, or out of
//! focus. We don't bail on the whole job for one bad frame:
//! [`detect_corners_in_directory`] silently skips a frame
//! when no detection succeeds, returning the successful
//! views and a count of skipped frames. The CLI surfaces
//! the skip count as a warning so the operator can re-shoot
//! if too many were skipped.

use std::path::{Path, PathBuf};

use calib_targets::chessboard::{Detection as ChessboardDetection, DetectorParams};
use calib_targets::detect::detect_chessboard;
use thiserror::Error;
use tracing::{debug, info, warn};

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

/// Detect chessboard corners in every supported image file
/// in `directory`, in lexicographic filename order.
///
/// `target` describes the expected board geometry; detection
/// is filtered to keep only views whose recovered grid
/// matches the expected dimensions (after accounting for
/// the detector's choice of which axis is "rows").
///
/// Returns the successful views. The caller observes
/// per-frame failures via the `skipped` field of the
/// returned `(views, skipped)` pair; if more than ~30% of
/// frames are skipped the operator should consider re-shooting.
///
/// # Errors
///
/// See [`DetectError`].
/// Detect chessboard corners in every supported image file
/// in `directory`, in lexicographic filename order.
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
) -> Result<(Vec<DetectedView>, DetectionStats), DetectError> {
    detect_corners_in_directory_with_progress(directory, target, &mut |_, _| {})
}

/// Detect chessboard corners in every supported image file
/// in `directory`, in lexicographic filename order, calling
/// `on_progress` once before each frame.
///
/// `target` describes the expected board geometry; detection
/// is filtered to keep only views whose recovered grid
/// matches the expected dimensions (after accounting for
/// the detector's choice of which axis is "rows").
///
/// `on_progress(current, total)` is called immediately
/// before processing the `current`-th frame (0-indexed) of
/// `total`. The CLI passes a closure that ticks an
/// [`indicatif::ProgressBar`]; library callers pass a
/// no-op via [`detect_corners_in_directory`].
///
/// Returns the successful views and a [`DetectionStats`]
/// summary. If more than ~30% of frames are skipped the
/// operator should consider re-shooting.
///
/// # Errors
///
/// See [`DetectError`].
pub fn detect_corners_in_directory_with_progress(
    directory: &Path,
    target: CheckerboardTarget,
    on_progress: &mut dyn FnMut(usize, usize),
) -> Result<(Vec<DetectedView>, DetectionStats), DetectError> {
    let entries = std::fs::read_dir(directory).map_err(|e| DetectError::ReadDir {
        path: directory.to_path_buf(),
        source: e,
    })?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "ppm" | "pgm"))
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

    let detector_params = DetectorParams::default();
    let mut views: Vec<DetectedView> = Vec::with_capacity(paths.len());
    let mut stats = DetectionStats {
        tried: paths.len(),
        skipped_no_board: 0,
        skipped_wrong_size: 0,
        skipped_io: 0,
    };
    let mut canonical_dims: Option<(PathBuf, u32, u32)> = None;
    let total = paths.len();

    for (i, path) in paths.iter().enumerate() {
        on_progress(i, total);
        let img = match image::ImageReader::open(path) {
            Ok(r) => match r.decode() {
                Ok(d) => d.to_luma8(),
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "decode failed");
                    stats.skipped_io += 1;
                    continue;
                }
            },
            Err(e) => {
                warn!(path = %path.display(), error = %e, "open failed");
                stats.skipped_io += 1;
                continue;
            }
        };
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

        let Some(detection) = detect_chessboard(&img, &detector_params) else {
            debug!(path = %path.display(), "no chessboard detected");
            stats.skipped_no_board += 1;
            continue;
        };

        // Filter by expected grid size. The detector
        // returns whatever it finds; we want only boards
        // that match the operator's stated target so we
        // don't accidentally calibrate against background
        // texture that happened to look chessboard-ish.
        let Some(view) = view_from_detection(path, w, h, &detection, target) else {
            debug!(
                path = %path.display(),
                "detection grid didn't match expected {}×{} (or vice versa)",
                target.rows,
                target.cols,
            );
            stats.skipped_wrong_size += 1;
            continue;
        };
        views.push(view);
    }
    // Final tick at total so the bar reaches 100%.
    on_progress(total, total);

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
    Ok((views, stats))
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
) -> Option<DetectedView> {
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
        return None;
    }
    // The detector returns whatever it finds; we want only
    // boards that match the operator's stated target so we
    // don't accidentally calibrate against background
    // texture that happened to look chessboard-ish.
    //
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
        return None;
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
    Some(DetectedView {
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
        let err = detect_corners_in_directory(dir.path(), CheckerboardTarget::default()).unwrap_err();
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

    // End-to-end detection tests would require either
    // synthetic frame generation (substantial) or a
    // real-camera capture corpus. Both are deferred to
    // bring-up testing on actual hardware.
}
