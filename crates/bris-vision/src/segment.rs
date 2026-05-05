//! Segmentation-based horizon detection.
//!
//! When the deck-occluded shipboard scene defeats both the gradient
//! and sky-region detectors (the sail and rigging produce strong
//! competing edges; the sky region's lower boundary follows the
//! sail's top edge rather than the sea), we need a method that
//! actually understands what's in the frame.
//!
//! This module loads a pretrained semantic-segmentation model
//! (SegFormer-B0 finetuned on ADE20K, ~14.5 MB ONNX, exported via
//! `scripts/export_segformer_ade.py`) and uses its per-pixel class
//! predictions to find horizon candidates.
//!
//! # ADE20K classes we care about
//!
//! ADE20K's 150-class label set distinguishes sky, sea, water, ship,
//! and boat as separate classes. The ones Bris uses:
//! - `2` sky
//! - `21` water (lakes, rivers)
//! - `26` sea (the open ocean)
//! - `76` boat (small vessels)
//! - `103` ship (large vessels)
//!
//! For Bris's purposes we treat sky as "above," sea/water as "below,"
//! and boat/ship as "occluded — skip this column." Other classes
//! (person, tree, etc.) are also treated as occluded.
//!
//! # Algorithm
//!
//! For each column of the segmentation mask:
//! 1. Walk top-to-bottom.
//! 2. Skip until the first pixel classified as sky.
//! 3. Continue until a pixel is *not* sky.
//! 4. If that pixel is sea or water, the row is a horizon candidate.
//! 5. If that pixel is anything else (boat, ship, person, ...), the
//!    column is occluded; skip it.
//!
//! Then RANSAC-fit a line through the candidates exactly like the
//! other horizon detectors. The shared finalize step in
//! [`crate::horizon`] handles the rest.
//!
//! # Limitations
//!
//! - **Daytime only.** The model was trained on daylight imagery.
//!   At night every class prediction is meaningless.
//! - **~14.5 MB model.** Inference is ~100 ms on `x86_64` with ort
//!   and the ONNX Runtime native library. Pi Zero 2W will be slower
//!   (~500 ms-1 s) — adequate for one-shot fixes; too slow for 30 fps
//!   streaming. The streaming engine should fall back to the
//!   gradient or sky-region detectors when this is too slow.
//! - **`ort` runtime dependency.** Adds ~50 MB to the binary size on
//!   Linux `x86_64` (downloaded ONNX Runtime native library). Acceptable
//!   for embedded Linux appliances; significant for mobile builds.
//! - **Not Bris-specific.** A future improvement would be to train a
//!   small marine-specific model (sky/sea/obstruction only, ~1-5 MB)
//!   on captured Bris frames. Tracked in `plan.org`.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]

use crate::frame::Frame;
use crate::horizon::{HorizonConfig, HorizonError, HorizonLine};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// ADE20K class index for sky.
pub const CLASS_SKY: u32 = 2;
/// ADE20K class index for water (lakes, rivers).
pub const CLASS_WATER: u32 = 21;
/// ADE20K class index for sea (open ocean).
pub const CLASS_SEA: u32 = 26;
/// ADE20K class index for boat (small vessels).
pub const CLASS_BOAT: u32 = 76;
/// ADE20K class index for ship (large vessels).
pub const CLASS_SHIP: u32 = 103;

/// Inference resolution. The SegFormer-B0 ADE20K model was trained at
/// 512×512; the export pipeline upsamples logits to this resolution.
pub const INFERENCE_SIZE: usize = 512;

/// Number of output classes in the ADE20K label set.
pub const NUM_CLASSES: usize = 150;

/// Lazily-loaded global model. The model file is ~14.5 MB so we want
/// to load it once per process. The `Session` is wrapped in a `Mutex`
/// because `Session::run` requires `&mut self`.
static MODEL: OnceLock<Result<Mutex<Session>, String>> = OnceLock::new();

/// Errors from segmentation-based horizon detection.
#[derive(Debug, thiserror::Error)]
pub enum SegmentError {
    /// The model file couldn't be loaded.
    #[error("failed to load segmentation model: {0}")]
    LoadFailed(String),
    /// Model inference failed.
    #[error("inference failed: {0}")]
    InferenceFailed(String),
    /// Per-pixel class output had the wrong shape.
    #[error("model output shape unexpected: {0:?}")]
    UnexpectedOutputShape(Vec<usize>),
    /// Internal Mutex poisoned (only occurs on prior panic in inference).
    #[error("model session lock poisoned")]
    Poisoned,
    /// The downstream horizon-line fit failed.
    #[error("horizon fit: {0}")]
    Horizon(#[from] HorizonError),
}

/// Class-label map produced by the segmentation model. Values are
/// ADE20K class indices (0-149).
#[derive(Debug, Clone)]
pub struct SegmentationMask {
    /// Mask width in pixels (= [`INFERENCE_SIZE`]).
    pub width: u32,
    /// Mask height in pixels (= [`INFERENCE_SIZE`]).
    pub height: u32,
    /// Row-major class indices.
    pub labels: Vec<u32>,
}

/// Load the segmentation model from the given path.
///
/// Cached after the first successful load; subsequent calls with a
/// different path are silently ignored (the cached model wins).
///
/// # Errors
///
/// Returns [`SegmentError::LoadFailed`] if the file is missing or
/// the runtime rejects it.
pub fn load_model(path: &Path) -> Result<(), SegmentError> {
    let result = MODEL.get_or_init(|| build_session(path));
    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(SegmentError::LoadFailed(e.clone())),
    }
}

fn build_session(path: &Path) -> Result<Mutex<Session>, String> {
    let session = Session::builder()
        .map_err(|e| e.to_string())?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| e.to_string())?
        .commit_from_file(path)
        .map_err(|e| e.to_string())?;
    Ok(Mutex::new(session))
}

/// Run the segmentation model on an image file and return the
/// per-pixel class mask at [`INFERENCE_SIZE`] × [`INFERENCE_SIZE`].
///
/// The model expects RGB input. Bris's `Frame` is grayscale u16 by
/// deliberate pipeline-wide design (the rest of the pipeline only
/// needs luminance). For segmentation we load the original color
/// image from disk separately so the pretrained model sees the data
/// it was trained on. Replicating grayscale into three channels gives
/// dramatically wrong results — verified empirically on real
/// shipboard frames.
///
/// `load_model` must have been called successfully at least once
/// before; this function does not load the model itself, by design,
/// so the caller controls when the (one-time, ~100 ms) load happens.
///
/// # Errors
///
/// See [`SegmentError`].
pub fn segment(image_path: &Path) -> Result<SegmentationMask, SegmentError> {
    let session_mutex = MODEL
        .get()
        .ok_or_else(|| SegmentError::LoadFailed("model not loaded; call load_model first".into()))?
        .as_ref()
        .map_err(|e| SegmentError::LoadFailed(e.clone()))?;
    let mut session = session_mutex.lock().map_err(|_| SegmentError::Poisoned)?;

    let arr = image_path_to_input_array(image_path)?;
    let input =
        Tensor::from_array(arr).map_err(|e| SegmentError::InferenceFailed(e.to_string()))?;
    let outputs = session
        .run(ort::inputs![input])
        .map_err(|e| SegmentError::InferenceFailed(e.to_string()))?;

    let view = outputs[0]
        .try_extract_array::<f32>()
        .map_err(|e| SegmentError::InferenceFailed(e.to_string()))?;
    let shape: Vec<usize> = view.shape().to_vec();
    if shape.len() != 4
        || shape[0] != 1
        || shape[1] != NUM_CLASSES
        || shape[2] != INFERENCE_SIZE
        || shape[3] != INFERENCE_SIZE
    {
        return Err(SegmentError::UnexpectedOutputShape(shape));
    }
    let logits = view
        .into_dimensionality::<ndarray::Ix4>()
        .map_err(|e| SegmentError::InferenceFailed(e.to_string()))?;

    let mut labels = vec![0u32; INFERENCE_SIZE * INFERENCE_SIZE];
    for y in 0..INFERENCE_SIZE {
        for x in 0..INFERENCE_SIZE {
            let mut best_class: u32 = 0;
            let mut best_logit = f32::NEG_INFINITY;
            for c in 0..NUM_CLASSES {
                let logit = logits[[0, c, y, x]];
                if logit > best_logit {
                    best_logit = logit;
                    best_class = c as u32;
                }
            }
            labels[y * INFERENCE_SIZE + x] = best_class;
        }
    }

    Ok(SegmentationMask {
        width: INFERENCE_SIZE as u32,
        height: INFERENCE_SIZE as u32,
        labels,
    })
}

/// Detect the sea horizon using a per-column sky→sea boundary search.
///
/// `frame.source_path` must be `Some(_)` because the segmentation
/// model needs the original color image (Bris's grayscale `Frame` is
/// inadequate for inference — see [`segment`]).
///
/// `load_model` must have been called successfully at least once
/// before this function.
///
/// # Errors
///
/// See [`SegmentError`]. Returns [`SegmentError::LoadFailed`] with
/// a "frame has no `source_path`" message if the frame wasn't loaded
/// with [`crate::Frame::with_source_path`].
pub fn detect_horizon_via_segmentation(
    frame: &Frame,
    cfg: HorizonConfig,
) -> Result<HorizonLine, SegmentError> {
    let path = frame.source_path.as_ref().ok_or_else(|| {
        SegmentError::LoadFailed(
            "segmentation requires Frame::with_source_path; \
             grayscale frame data alone is insufficient for the \
             pretrained color-trained model"
                .into(),
        )
    })?;
    let mask = segment(path)?;
    let candidates = sky_to_sea_transitions(&mask);
    tracing::debug!(
        candidate_columns = candidates.len(),
        "segmentation: sky→sea transition columns"
    );

    let scale_x = f64::from(frame.width()) / f64::from(mask.width);
    let scale_y = f64::from(frame.height()) / f64::from(mask.height);
    let candidates_in_frame: Vec<(f64, f64)> = candidates
        .into_iter()
        .map(|(x, y)| (x * scale_x, y * scale_y))
        .collect();

    Ok(crate::horizon::finalize_horizon(
        frame,
        &candidates_in_frame,
        1.0,
        &cfg,
    )?)
}

/// Build the (1, 3, `INFERENCE_SIZE`, `INFERENCE_SIZE`) RGB f32 array
/// the model expects, by loading the original color image from disk.
fn image_path_to_input_array(path: &Path) -> Result<ndarray::Array4<f32>, SegmentError> {
    const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const STD: [f32; 3] = [0.229, 0.224, 0.225];

    let img = image::open(path)
        .map_err(|e| SegmentError::InferenceFailed(format!("load color image: {e}")))?
        .to_rgb8();
    let resized = image::imageops::resize(
        &img,
        INFERENCE_SIZE as u32,
        INFERENCE_SIZE as u32,
        image::imageops::FilterType::Triangle,
    );

    let mut data = vec![0.0_f32; 3 * INFERENCE_SIZE * INFERENCE_SIZE];
    for c in 0..3 {
        for y in 0..INFERENCE_SIZE {
            for x in 0..INFERENCE_SIZE {
                let p = resized.get_pixel(x as u32, y as u32);
                let v = (f32::from(p[c]) / 255.0 - MEAN[c]) / STD[c];
                data[c * INFERENCE_SIZE * INFERENCE_SIZE + y * INFERENCE_SIZE + x] = v;
            }
        }
    }

    ndarray::Array4::from_shape_vec((1, 3, INFERENCE_SIZE, INFERENCE_SIZE), data)
        .map_err(|e| SegmentError::InferenceFailed(format!("array shape: {e}")))
}

/// Find sky→sea transitions per column in the segmentation mask.
///
/// For each column, walk top-to-bottom:
/// 1. Skip until the first sky pixel.
/// 2. Continue until a non-sky pixel.
/// 3. If that pixel is sea or water, this column has a horizon
///    candidate at that row.
/// 4. Otherwise (boat, ship, anything else), the column is occluded;
///    skip it.
fn sky_to_sea_transitions(mask: &SegmentationMask) -> Vec<(f64, f64)> {
    let w = mask.width as usize;
    let h = mask.height as usize;
    let mut points = Vec::new();
    // Counters for diagnostic logging.
    let mut col_no_sky = 0usize;
    let mut col_sky_to_sea = 0usize;
    let mut col_sky_to_other = 0usize;
    let mut col_all_sky = 0usize;
    for x in 0..w {
        let mut saw_sky = false;
        let mut handled = false;
        for y in 0..h {
            let class = mask.labels[y * w + x];
            if class == CLASS_SKY {
                saw_sky = true;
                continue;
            }
            if !saw_sky {
                col_no_sky += 1;
                handled = true;
                break;
            }
            if class == CLASS_SEA || class == CLASS_WATER {
                points.push((x as f64, y as f64));
                col_sky_to_sea += 1;
            } else {
                col_sky_to_other += 1;
            }
            handled = true;
            break;
        }
        if !handled {
            col_all_sky += 1;
        }
    }
    tracing::debug!(
        col_no_sky,
        col_sky_to_sea,
        col_sky_to_other,
        col_all_sky,
        "segmentation: per-column transition counts"
    );
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join("segmentation.onnx")
    }

    #[test]
    fn model_loads_if_present() {
        let path = model_path();
        if !path.exists() {
            eprintln!("skipping: model file not present at {}", path.display());
            return;
        }
        load_model(&path).expect("model should load");
    }

    #[test]
    fn segment_returns_mask_of_expected_shape() {
        let path = model_path();
        if !path.exists() {
            return;
        }
        load_model(&path).unwrap();
        // Need a real image file for inference; write a tiny test PNG.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let tmp_path = tmp.path().with_extension("png");
        let buf = image::ImageBuffer::<image::Rgb<u8>, _>::from_pixel(
            32,
            24,
            image::Rgb([100u8, 100, 100]),
        );
        buf.save_with_format(&tmp_path, image::ImageFormat::Png)
            .unwrap();
        let mask = segment(&tmp_path).unwrap();
        assert_eq!(mask.width, INFERENCE_SIZE as u32);
        assert_eq!(mask.height, INFERENCE_SIZE as u32);
        assert_eq!(mask.labels.len(), INFERENCE_SIZE * INFERENCE_SIZE);
        assert!(mask.labels.iter().all(|&c| (c as usize) < NUM_CLASSES));
    }

    #[test]
    fn sky_to_sea_transitions_finds_known_pattern() {
        // Synthesize a tiny mask: top half sky, bottom half sea.
        // Each column should produce a transition at row 4.
        let w = 8usize;
        let h = 8usize;
        let mut labels = vec![CLASS_SEA; w * h];
        for y in 0..4 {
            for x in 0..w {
                labels[y * w + x] = CLASS_SKY;
            }
        }
        let mask = SegmentationMask {
            width: w as u32,
            height: h as u32,
            labels,
        };
        let points = sky_to_sea_transitions(&mask);
        assert_eq!(points.len(), w);
        for &(_x, y) in &points {
            assert!((y - 4.0).abs() < 1e-9);
        }
    }

    #[test]
    fn sky_to_sea_transitions_skips_occluded_columns() {
        // Column 0: sky → sea (good)
        // Column 1: sky → boat (occluded)
        // Column 2: ship from top (no sky)
        // Column 3: sky all the way down (no transition)
        let w = 4usize;
        let h = 4usize;
        let mut labels = vec![CLASS_SKY; w * h];
        labels[2 * w] = CLASS_SEA;
        labels[3 * w] = CLASS_SEA;
        labels[2 * w + 1] = CLASS_BOAT;
        labels[3 * w + 1] = CLASS_BOAT;
        labels[2] = CLASS_SHIP;
        labels[w + 2] = CLASS_SHIP;
        labels[2 * w + 2] = CLASS_SHIP;
        labels[3 * w + 2] = CLASS_SHIP;
        let mask = SegmentationMask {
            width: w as u32,
            height: h as u32,
            labels,
        };
        let points = sky_to_sea_transitions(&mask);
        assert_eq!(points.len(), 1, "only column 0 should produce a candidate");
        assert!((points[0].0 - 0.0).abs() < 1e-9);
        assert!((points[0].1 - 2.0).abs() < 1e-9);
    }
}
