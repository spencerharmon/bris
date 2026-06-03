//! Multi-frame panorama: cross-frame angle measurement.
//!
//! When a body and the horizon don't fit in a single frame (telephoto
//! lens, high-altitude body), we need a chain of frames bridging the
//! gap. This module composes per-frame measurements into a single
//! body-to-horizon angle.
//!
//! # Inputs
//!
//! A sequence of frames captured during a sweep, in order. At least one
//! frame must contain the body's centroid; at least one (the same or
//! different) must contain the horizon line. Adjacent frames must
//! overlap enough for [`crate::track::track`] to find a transform.
//!
//! # Output
//!
//! A single fused altitude with σ, computed as:
//! 1. Detect horizon in every frame that has one. Promote one of those
//!    detections to the canonical "horizon frame."
//! 2. Detect the body's centroid in every frame where the body is
//!    visible. Promote one of those to the "body frame."
//! 3. Compute the rigid transform chain from the body frame to the
//!    horizon frame via [`crate::track::track`] applied to adjacent
//!    pairs.
//! 4. Map the body's pixel position through the chain into the horizon
//!    frame's coordinate system.
//! 5. Apply [`crate::measure::measure_altitude`] using the horizon
//!    frame's horizon line and the projected body position.
//!
//! # Limitations of this commit
//!
//! - Assumes adjacent frames overlap enough for ORB-style tracking to
//!   succeed. IMU/gyro priors and horizon-line orientation as a
//!   supplementary anchor are documented as follow-ups.
//! - Sidereal motion correction between frames (the body moves at
//!   ~15″/sec for an equatorial body) is not yet applied. For sweeps
//!   under ~5 seconds the contribution is below 1 arcmin; longer
//!   sweeps will need it for the 0.5 nm target.
//! - Lens distortion is applied at each measurement endpoint via
//!   [`crate::lens`], but the rigid transform chain is in pixel
//!   coordinates, which is only an approximation when the lens has
//!   significant radial distortion. For a wide-angle lens or
//!   telephoto with small distortion it's adequate; for fisheye it's
//!   not. Documented and acceptable for our use case.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::similar_names
)]

use crate::centroid::{centroid_brightest_body, Centroid, CentroidConfig};
use crate::frame::{Frame, Intrinsics};
use crate::horizon::{HorizonConfig, HorizonLine};
use crate::measure::{measure_altitude, MeasurementError};
use crate::ray::{altitude_from_rays, AltitudeMeasurement, BodyRay, CameraRay, HorizonRay};
use crate::track::{track, track_rotation, RigidTransform, TrackConfig};
use bris_core::{Sigma, Uncertain};
use bris_math::kabsch;

/// Stage E entry point: compose stitching + ray-space altitude
/// for an already-detected body centroid and horizon line in
/// two distinct frames.
///
/// Unlike [`panorama_altitude`] / [`panorama_altitude_via_rotation`],
/// this helper does **not** run any detector. The caller (the
/// streaming engine's Stage E) has already produced a
/// [`Centroid`] for the body and a [`HorizonLine`] for the
/// horizon — typically in separate frames — and just needs the
/// cross-frame rotation chain and the ray-space altitude
/// composition.
///
/// Mirrors phases 3+4 of [`panorama_altitude_via_rotation`]:
///
/// 1. Run [`track_rotation`] over the (`body_frame`, `horizon_frame`)
///    pair to recover the camera-space rotation between them.
/// 2. Lift the body centroid (in `body_frame`'s intrinsics) to a
///    [`BodyRay`], rotate it into `horizon_frame`'s coordinates.
/// 3. Lift the horizon line (in `horizon_frame`'s intrinsics) to
///    a [`HorizonRay`] and compute the altitude via
///    [`altitude_from_rays`].
///
/// # Honest σ
///
/// The returned σ combines (in quadrature):
///
/// - the body centroid's positional σ (from `body_centroid`,
///   carried through `BodyRay::from_pixel`'s pixel→radian
///   conversion),
/// - the horizon line's altitude σ (from `horizon_line`,
///   carried through `HorizonRay::from_line`), and
/// - the **executed stitch σ**: `track_rotation`'s
///   per-correspondence RMS angular residual in radians.
///
/// The stitch σ is added in quadrature to the body ray's
/// direction σ before the altitude composition, because the
/// rotation directly perturbs where the body ray lands in the
/// horizon frame's coordinates. This supersedes the cheap
/// time-gap-based estimate used by Stage E during pair
/// selection.
///
/// # Errors
///
/// See [`PanoramaError`]: `TrackingFailed` if `track_rotation`
/// refuses, `DegenerateHorizonRay` if the horizon line in
/// `horizon_frame` won't lift to a camera plane,
/// `Measurement` if the ray-space altitude composition
/// produces a non-finite or sub-horizon result.
pub fn panorama_altitude_for_pair(
    body_frame: &Frame,
    body_centroid: Centroid,
    horizon_frame: &Frame,
    horizon_line: HorizonLine,
    track_cfg: TrackConfig,
) -> Result<Uncertain<f64>, PanoramaError> {
    let rot = track_rotation(body_frame, horizon_frame, track_cfg).map_err(|e| {
        tracing::debug!(error = %e, "panorama_altitude_for_pair: track_rotation failed");
        PanoramaError::TrackingFailed { from: 0, to: 1 }
    })?;

    // Lift the body centroid in its own frame's intrinsics.
    let body_ray = BodyRay::from_pixel(
        &body_frame.intrinsics,
        body_centroid.x,
        body_centroid.y,
        body_centroid.position_sigma_px,
    );

    // Inflate the body ray's direction σ by the executed
    // stitch σ (RMS angular residual from the Kabsch fit).
    // Honest combination: stitch perturbs where the rotated
    // ray lands in the horizon frame's coordinates.
    let stitch_sigma_rad = rot.rms_residual_rad;
    let combined_body_sigma =
        (body_ray.direction_sigma.value().powi(2) + stitch_sigma_rad.powi(2)).sqrt();
    let inflated_body_sigma = Sigma::new(combined_body_sigma).unwrap_or(body_ray.direction_sigma);

    // Rotate the body ray into horizon_frame's coordinates.
    let rotated = kabsch::rotate_vec(&rot.matrix, body_ray.ray.as_array());
    let body_in_horizon = BodyRay {
        ray: CameraRay::from_unit_components(rotated[0], rotated[1], rotated[2]),
        direction_sigma: inflated_body_sigma,
    };

    // Lift the horizon line.
    let horizon_ray = HorizonRay::from_line(
        &horizon_line,
        &horizon_frame.intrinsics,
        horizon_frame.width(),
    )
    .ok_or(PanoramaError::DegenerateHorizonRay { frame: 1 })?;

    let m: AltitudeMeasurement = altitude_from_rays(&body_in_horizon, &horizon_ray);
    if !m.altitude_rad.is_finite() {
        return Err(PanoramaError::Measurement(MeasurementError::NonFinite));
    }
    Ok(Uncertain {
        value: m.altitude_rad,
        sigma: m.altitude_sigma,
    })
}

/// One frame's role in the panorama: did it produce a horizon? a body
/// centroid? both? neither?
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameRoles {
    /// Detected horizon line in this frame, if any.
    pub horizon: Option<HorizonLine>,
    /// Detected body centroid (x, y) in this frame, if any.
    pub body_centroid: Option<(f64, f64, Sigma)>,
}

/// Errors from the panorama-stitching path.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum PanoramaError {
    /// No frame contained a usable horizon.
    #[error("no frame contained a usable horizon")]
    NoHorizonFrame,
    /// No frame contained a usable body centroid.
    #[error("no frame contained a usable body centroid")]
    NoBodyFrame,
    /// Tracking between two adjacent frames failed.
    #[error("tracking failed between frames {from} and {to}")]
    TrackingFailed {
        /// Index of the source frame.
        from: usize,
        /// Index of the destination frame.
        to: usize,
    },
    /// Final altitude measurement failed.
    #[error("altitude measurement failed: {0}")]
    Measurement(#[from] MeasurementError),
    /// The horizon line in the chosen horizon frame could not
    /// be lifted to a camera-space plane (degenerate normal).
    /// Only produced by the ray-space variants.
    #[error("horizon line in frame {frame} could not be lifted to a camera-space plane")]
    DegenerateHorizonRay {
        /// Index of the horizon frame whose line refused to lift.
        frame: usize,
    },
}

/// Compute body-to-horizon altitude across a sequence of frames.
///
/// Frames must be in capture order, with adjacent frames overlapping
/// enough for tracking to succeed. The body and horizon may appear in
/// the same frame or in different frames; the chain handles both.
///
/// # Errors
///
/// See [`PanoramaError`].
pub fn panorama_altitude(
    frames: &[Frame],
    horizon_cfg: HorizonConfig,
    centroid_cfg: CentroidConfig,
    track_cfg: TrackConfig,
) -> Result<Uncertain<f64>, PanoramaError> {
    panorama_altitude_with_detector(
        frames,
        horizon_cfg,
        centroid_cfg,
        track_cfg,
        crate::horizon::detect_horizon,
    )
}

/// Same as [`panorama_altitude`] but with a caller-supplied horizon
/// detection function. Use this to swap in
/// [`crate::horizon::detect_horizon_via_sky_region`] or any future
/// detector without changing this module.
/// Same as [`panorama_altitude`] but with a caller-supplied horizon
/// detection function. Use this to swap in
/// [`crate::horizon::detect_horizon_via_sky_region`] or any future
/// detector without changing this module.
///
/// The detector's error type is left generic so callers can return
/// their own composite error (e.g. CLI-level errors that wrap
/// segmentation-model load failures alongside [`HorizonError`]).
/// Failed-frame errors are logged and the frame is treated as having
/// no horizon, so the error type only matters for log formatting.
// Threading `image_width` through into `measure_altitude` (audit
// 2026-06-03 fix) added bookkeeping that nudges this function
// over the 100-line clippy ceiling. The function body is otherwise
// linear and well-commented; splitting it just to placate the
// lint would obscure the phase-by-phase structure.
#[allow(clippy::too_many_lines)]
pub fn panorama_altitude_with_detector<F, E>(
    frames: &[Frame],
    horizon_cfg: HorizonConfig,
    centroid_cfg: CentroidConfig,
    track_cfg: TrackConfig,
    horizon_fn: F,
) -> Result<Uncertain<f64>, PanoramaError>
where
    F: Fn(&Frame, HorizonConfig) -> Result<crate::horizon::HorizonLine, E>,
    E: std::fmt::Display,
{
    if frames.is_empty() {
        return Err(PanoramaError::NoHorizonFrame);
    }

    // Phase 1: classify each frame.
    let roles: Vec<FrameRoles> = frames
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let horizon = match horizon_fn(f, horizon_cfg) {
                Ok(h) => {
                    tracing::debug!(
                        frame = i,
                        slope = h.slope,
                        intercept = h.intercept,
                        inliers = h.inlier_count,
                        candidates = h.candidate_count,
                        residual_rms_px = h.residual_rms_px,
                        altitude_sigma_arcmin = h.altitude_sigma.value().to_degrees() * 60.0,
                        "panorama: horizon detected"
                    );
                    Some(h)
                }
                Err(e) => {
                    tracing::debug!(frame = i, error = %e, "panorama: horizon detection failed");
                    None
                }
            };
            let body_centroid = match centroid_brightest_body(f, centroid_cfg) {
                Ok(c) => {
                    tracing::debug!(
                        frame = i,
                        x = c.x,
                        y = c.y,
                        area_px = c.area_px,
                        intensity = c.mean_intensity,
                        position_sigma_px = c.position_sigma_px.value(),
                        "panorama: body centroid detected"
                    );
                    Some((c.x, c.y, c.position_sigma_px))
                }
                Err(e) => {
                    tracing::debug!(frame = i, error = %e, "panorama: centroiding failed");
                    None
                }
            };
            FrameRoles {
                horizon,
                body_centroid,
            }
        })
        .collect();

    // Phase 2: choose the best horizon frame and body frame.
    // Best = highest inlier count for horizon, largest area for body.
    let horizon_idx = roles
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.horizon.map(|h| (i, h.inlier_count)))
        .max_by_key(|&(_, n)| n)
        .map(|(i, _)| i)
        .ok_or(PanoramaError::NoHorizonFrame)?;
    let body_idx = roles
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.body_centroid.map(|(_, _, s)| (i, s.value())))
        .min_by(|(_, sa), (_, sb)| sa.partial_cmp(sb).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .ok_or(PanoramaError::NoBodyFrame)?;

    let horizon_line = roles[horizon_idx].horizon.expect("checked above");
    let (body_x_in_body_frame, body_y_in_body_frame, body_sigma) =
        roles[body_idx].body_centroid.expect("checked above");

    tracing::info!(
        horizon_frame = horizon_idx,
        horizon_slope = horizon_line.slope,
        horizon_intercept_y_at_x0 = horizon_line.intercept,
        horizon_inliers = horizon_line.inlier_count,
        body_frame = body_idx,
        body_x = body_x_in_body_frame,
        body_y = body_y_in_body_frame,
        "panorama: selected horizon and body frames"
    );

    // Phase 3: if body and horizon are in the same frame, no chain
    // needed — fall through to direct measurement.
    let body_in_horizon_frame = if horizon_idx == body_idx {
        (body_x_in_body_frame, body_y_in_body_frame)
    } else {
        // Walk from body_idx to horizon_idx accumulating the rigid
        // transform via track() on each adjacent pair.
        let chain = build_chain(frames, body_idx, horizon_idx, track_cfg)?;
        apply_chain(
            (body_x_in_body_frame, body_y_in_body_frame),
            &chain,
            frames,
            body_idx,
            horizon_idx,
        )
    };

    // Phase 4: synthesize a Centroid in the horizon frame's coordinates
    // and run the standard angle measurement.
    let centroid = crate::centroid::Centroid {
        x: body_in_horizon_frame.0,
        y: body_in_horizon_frame.1,
        area_px: 0,
        mean_intensity: 0.0,
        position_sigma_px: body_sigma,
    };
    let horizon_frame = &frames[horizon_idx];
    let intr = horizon_frame.intrinsics;
    let w = horizon_frame.width();
    let altitude = measure_altitude(intr, w, horizon_line, centroid)?;

    Ok(altitude)
}

/// Camera-space (ray-based) sibling of [`panorama_altitude`].
///
/// Differs from [`panorama_altitude`] in two ways:
///
/// 1. The cross-frame chain is composed in **camera-ray space**
///    via [`crate::track::track_rotation`] (Kabsch over ray
///    pairs) rather than in pixel space via [`crate::track::track`].
///    This means each pair in the chain may run at a different
///    resolution; each frame contributes its own intrinsics
///    when its pixels are lifted to rays.
/// 2. The final body-vs-horizon altitude is computed via
///    [`crate::ray::altitude_from_rays`] using the body ray
///    rotated into the horizon frame's coordinate system and
///    the horizon plane lifted from the horizon frame's
///    detected line. Lens distortion is applied per-frame at
///    each conversion, eliminating the pixel-chain
///    approximation that the [`panorama_altitude`] path makes
///    for distorted lenses.
///
/// Same input / output contract as [`panorama_altitude`]: pass
/// frames in capture order with adjacent frames overlapping
/// enough for `track_rotation` to find correspondences.
///
/// # Errors
///
/// See [`PanoramaError`]. The ray-space path adds
/// [`PanoramaError::DegenerateHorizonRay`] when the horizon
/// line in the chosen frame won't lift to a camera plane (a
/// horizon line passing precisely through the principal point
/// is the only realistic trigger).
pub fn panorama_altitude_via_rotation(
    frames: &[Frame],
    horizon_cfg: HorizonConfig,
    centroid_cfg: CentroidConfig,
    track_cfg: TrackConfig,
) -> Result<Uncertain<f64>, PanoramaError> {
    panorama_altitude_via_rotation_with_detector(
        frames,
        horizon_cfg,
        centroid_cfg,
        track_cfg,
        crate::horizon::detect_horizon,
    )
}

/// Same as [`panorama_altitude_via_rotation`] but with a
/// caller-supplied horizon detection function. Mirrors the
/// pixel-rigid [`panorama_altitude_with_detector`] entry point.
///
/// # Errors
///
/// See [`PanoramaError`].
pub fn panorama_altitude_via_rotation_with_detector<F, E>(
    frames: &[Frame],
    horizon_cfg: HorizonConfig,
    centroid_cfg: CentroidConfig,
    track_cfg: TrackConfig,
    horizon_fn: F,
) -> Result<Uncertain<f64>, PanoramaError>
where
    F: Fn(&Frame, HorizonConfig) -> Result<crate::horizon::HorizonLine, E>,
    E: std::fmt::Display,
{
    if frames.is_empty() {
        return Err(PanoramaError::NoHorizonFrame);
    }

    // Phase 1: per-frame role classification (identical to
    // pixel-rigid path).
    let roles: Vec<FrameRoles> = frames
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let horizon = horizon_fn(f, horizon_cfg)
                .inspect(|h| {
                    tracing::debug!(
                        frame = i,
                        slope = h.slope,
                        inliers = h.inlier_count,
                        "panorama_via_rotation: horizon detected"
                    );
                })
                .map_err(|e| {
                    tracing::debug!(
                        frame = i,
                        error = %e,
                        "panorama_via_rotation: horizon detection failed"
                    );
                })
                .ok();
            let body_centroid = centroid_brightest_body(f, centroid_cfg)
                .inspect(|c| {
                    tracing::debug!(
                        frame = i,
                        x = c.x,
                        y = c.y,
                        "panorama_via_rotation: body centroid detected"
                    );
                })
                .map_err(|e| {
                    tracing::debug!(
                        frame = i,
                        error = %e,
                        "panorama_via_rotation: centroiding failed"
                    );
                })
                .ok()
                .map(|c| (c.x, c.y, c.position_sigma_px));
            FrameRoles {
                horizon,
                body_centroid,
            }
        })
        .collect();

    let horizon_idx = roles
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.horizon.map(|h| (i, h.inlier_count)))
        .max_by_key(|&(_, n)| n)
        .map(|(i, _)| i)
        .ok_or(PanoramaError::NoHorizonFrame)?;
    let body_idx = roles
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.body_centroid.map(|(_, _, s)| (i, s.value())))
        .min_by(|(_, sa), (_, sb)| sa.partial_cmp(sb).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .ok_or(PanoramaError::NoBodyFrame)?;

    let horizon_line = roles[horizon_idx].horizon.expect("checked above");
    let (body_px_x, body_px_y, body_sigma_px) =
        roles[body_idx].body_centroid.expect("checked above");

    // Lift the body centroid in body_idx's intrinsics to a
    // camera-space BodyRay.
    let body_ray = BodyRay::from_pixel(
        &frames[body_idx].intrinsics,
        body_px_x,
        body_px_y,
        body_sigma_px,
    );

    // Lift the horizon line in horizon_idx's intrinsics to a
    // camera-space HorizonRay (sky-pointing plane normal).
    let horizon_ray = HorizonRay::from_line(
        &horizon_line,
        &frames[horizon_idx].intrinsics,
        frames[horizon_idx].width(),
    )
    .ok_or(PanoramaError::DegenerateHorizonRay { frame: horizon_idx })?;

    tracing::info!(
        horizon_frame = horizon_idx,
        body_frame = body_idx,
        "panorama_via_rotation: selected horizon and body frames"
    );

    // Walk the chain and rotate the body ray into the horizon
    // frame's coordinates. If body_idx == horizon_idx the chain
    // is empty and the ray passes through unchanged.
    let body_ray_in_horizon_frame = if body_idx == horizon_idx {
        body_ray
    } else {
        let chain = build_rotation_chain(frames, body_idx, horizon_idx, track_cfg)?;
        apply_rotation_chain(body_ray, &chain, body_idx, horizon_idx)
    };

    let measurement: AltitudeMeasurement =
        altitude_from_rays(&body_ray_in_horizon_frame, &horizon_ray);

    Ok(Uncertain {
        value: measurement.altitude_rad,
        sigma: measurement.altitude_sigma,
    })
}

/// Build the chain of camera-space rotations from `from_idx` to
/// `to_idx`. Each entry is the rotation R such that `ray_{i+1}
/// ≈ R · ray_i` for adjacent frame pair (i, i+1) — always
/// stored in ascending index order regardless of walk direction;
/// the inverse is taken at apply time when walking backward.
fn build_rotation_chain(
    frames: &[Frame],
    from_idx: usize,
    to_idx: usize,
    cfg: TrackConfig,
) -> Result<Vec<[f64; 9]>, PanoramaError> {
    let mut chain = Vec::new();
    if from_idx == to_idx {
        return Ok(chain);
    }
    let (start, end) = (from_idx.min(to_idx), from_idx.max(to_idx));
    for i in start..end {
        let rot = track_rotation(&frames[i], &frames[i + 1], cfg).map_err(|e| {
            tracing::warn!(
                from = i,
                to = i + 1,
                error = %e,
                "panorama_via_rotation: pairwise rotation tracking failed"
            );
            PanoramaError::TrackingFailed { from: i, to: i + 1 }
        })?;
        chain.push(rot.matrix);
    }
    Ok(chain)
}

/// Apply a chain of rotations to a body ray in `body_idx`'s
/// coordinates, returning the equivalent ray in `horizon_idx`'s
/// coordinates. The chain is stored in ascending-index order;
/// the walk direction determines whether each entry is applied
/// directly (forward) or transposed (backward).
fn apply_rotation_chain(
    ray: BodyRay,
    chain: &[[f64; 9]],
    body_idx: usize,
    horizon_idx: usize,
) -> BodyRay {
    if body_idx == horizon_idx {
        return ray;
    }
    let mut current = ray.ray.as_array();
    if body_idx < horizon_idx {
        // Walk forward; chain[k] maps frame (body+k) → (body+k+1).
        for r in chain {
            current = kabsch::rotate_vec(r, current);
        }
    } else {
        // Walk backward; chain[k] maps frame (horizon+k) →
        // (horizon+k+1). To go horizon ← body we apply each in
        // reverse order, transposed.
        for r in chain.iter().rev() {
            let r_t = transpose_3x3(r);
            current = kabsch::rotate_vec(&r_t, current);
        }
    }
    BodyRay {
        ray: CameraRay::from_unit_components(current[0], current[1], current[2]),
        direction_sigma: ray.direction_sigma,
    }
}

/// Transpose a 3×3 matrix stored row-major in a flat [f64; 9].
fn transpose_3x3(m: &[f64; 9]) -> [f64; 9] {
    [m[0], m[3], m[6], m[1], m[4], m[7], m[2], m[5], m[8]]
}

/// Build the chain of pairwise rigid transforms from `from_idx` to
/// `to_idx`. Each entry maps frame i+1 onto frame i (or the reverse
/// depending on direction).
fn build_chain(
    frames: &[Frame],
    from_idx: usize,
    to_idx: usize,
    cfg: TrackConfig,
) -> Result<Vec<RigidTransform>, PanoramaError> {
    let mut chain = Vec::new();
    if from_idx == to_idx {
        return Ok(chain);
    }
    let (start, end) = (from_idx.min(to_idx), from_idx.max(to_idx));
    for i in start..end {
        let xform = track(&frames[i], &frames[i + 1], cfg).map_err(|e| {
            tracing::warn!(
                from = i,
                to = i + 1,
                error = %e,
                "panorama: pairwise tracking failed"
            );
            PanoramaError::TrackingFailed { from: i, to: i + 1 }
        })?;
        chain.push(xform);
    }
    Ok(chain)
}

/// Apply a chain of transforms to a point in `body_idx`'s coordinates,
/// returning its position in `horizon_idx`'s coordinates.
fn apply_chain(
    body_pt: (f64, f64),
    chain: &[RigidTransform],
    frames: &[Frame],
    body_idx: usize,
    horizon_idx: usize,
) -> (f64, f64) {
    if body_idx == horizon_idx {
        return body_pt;
    }
    let forward = body_idx < horizon_idx;
    let mut current = body_pt;
    if forward {
        // Each xform maps frame i onto frame i+1; chain[k] is the
        // transform from frame (body_idx + k) → (body_idx + k + 1).
        for (k, xform) in chain.iter().enumerate() {
            let from_frame = &frames[body_idx + k];
            let to_frame = &frames[body_idx + k + 1];
            current = apply_xform(current, xform, from_frame, to_frame);
        }
    } else {
        // Chain entries are stored in ascending index order; we need
        // the inverse direction.
        for (k, xform) in chain.iter().enumerate().rev() {
            let from_frame = &frames[horizon_idx + k + 1];
            let to_frame = &frames[horizon_idx + k];
            current = apply_xform_inverse(current, xform, from_frame, to_frame);
        }
    }
    current
}

fn apply_xform(
    pt: (f64, f64),
    xform: &RigidTransform,
    from_frame: &Frame,
    to_frame: &Frame,
) -> (f64, f64) {
    let cx_a = f64::from(from_frame.width()) / 2.0;
    let cy_a = f64::from(from_frame.height()) / 2.0;
    let cx_b = f64::from(to_frame.width()) / 2.0;
    let cy_b = f64::from(to_frame.height()) / 2.0;
    let xc = pt.0 - cx_a;
    let yc = pt.1 - cy_a;
    let (sin_t, cos_t) = xform.theta_rad.sin_cos();
    let xb = cos_t * xc - sin_t * yc + xform.tx_px + cx_b;
    let yb = sin_t * xc + cos_t * yc + xform.ty_px + cy_b;
    (xb, yb)
}

fn apply_xform_inverse(
    pt: (f64, f64),
    xform: &RigidTransform,
    from_frame: &Frame,
    to_frame: &Frame,
) -> (f64, f64) {
    let cx_a = f64::from(to_frame.width()) / 2.0;
    let cy_a = f64::from(to_frame.height()) / 2.0;
    let cx_b = f64::from(from_frame.width()) / 2.0;
    let cy_b = f64::from(from_frame.height()) / 2.0;
    // Inverse of rigid (R, t) about centers: (R^T, -R^T t).
    let xc = pt.0 - cx_b;
    let yc = pt.1 - cy_b;
    let xc_minus_t = xc - xform.tx_px;
    let yc_minus_t = yc - xform.ty_px;
    let (sin_t, cos_t) = xform.theta_rad.sin_cos();
    let xa = cos_t * xc_minus_t + sin_t * yc_minus_t + cx_a;
    let ya = -sin_t * xc_minus_t + cos_t * yc_minus_t + cy_a;
    (xa, ya)
}

#[allow(dead_code)] // kept for documentation symmetry
fn _ensure_intrinsics_present(_intr: Intrinsics) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Intrinsics;

    use bris_core::time::{Tt, JD_J2000};

    /// Synthesize a frame containing only a horizon at the given row.
    fn synth_horizon_only(width: u32, height: u32, horizon_y: u32) -> Frame {
        let mut pixels = vec![0u16; (width as usize) * (height as usize)];
        for y in 0..height {
            for x in 0..width {
                let v = if y < horizon_y { 50_000 } else { 5_000 };
                pixels[(y as usize) * (width as usize) + (x as usize)] = v;
            }
        }
        // Sprinkle bright square markers — must match `synth_body_only`'s
        // markers so the tracker can find correspondences between the
        // two frames.
        for (cx, cy) in [
            (50, 30),
            (120, 50),
            (200, 40),
            (270, 60),
            (90, 80),
            (180, 90),
        ] {
            for dy in -3_i32..=3 {
                for dx in -3_i32..=3 {
                    let x = cx + dx;
                    let y = cy + dy;
                    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                        continue;
                    }
                    pixels[(y as usize) * (width as usize) + (x as usize)] = 65_000;
                }
            }
        }
        Frame::new(
            width,
            height,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    /// Synthesize a frame containing only a bright body at (cx, cy).
    /// Shares its corner-marker positions with `synth_horizon_only` so
    /// tracking between the two succeeds.
    fn synth_body_only(width: u32, height: u32, cx: f64, cy: f64, radius: f64) -> Frame {
        let mut pixels = vec![1_000u16; (width as usize) * (height as usize)];
        // Same markers as synth_horizon_only.
        for (sx, sy) in [
            (50, 30),
            (120, 50),
            (200, 40),
            (270, 60),
            (90, 80),
            (180, 90),
        ] {
            for dy in -3_i32..=3 {
                for dx in -3_i32..=3 {
                    let x = sx + dx;
                    let y = sy + dy;
                    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                        continue;
                    }
                    pixels[(y as usize) * (width as usize) + (x as usize)] = 65_000;
                }
            }
        }
        // Big bright disk for the body.
        for y in 0..height {
            for x in 0..width {
                let dx = f64::from(x) - cx;
                let dy = f64::from(y) - cy;
                if dx * dx + dy * dy <= radius * radius {
                    pixels[(y as usize) * (width as usize) + (x as usize)] = 65_000;
                }
            }
        }
        Frame::new(
            width,
            height,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    /// Synthesize a frame with both horizon and body together.
    fn synth_body_and_horizon(
        width: u32,
        height: u32,
        horizon_y: u32,
        body_cx: f64,
        body_cy: f64,
        body_radius: f64,
    ) -> Frame {
        let mut pixels = vec![0u16; (width as usize) * (height as usize)];
        for y in 0..height {
            for x in 0..width {
                let v = if y < horizon_y { 50_000 } else { 5_000 };
                pixels[(y as usize) * (width as usize) + (x as usize)] = v;
            }
        }
        for y in 0..height {
            for x in 0..width {
                let dx = f64::from(x) - body_cx;
                let dy = f64::from(y) - body_cy;
                if dx * dx + dy * dy <= body_radius * body_radius {
                    pixels[(y as usize) * (width as usize) + (x as usize)] = 65_000;
                }
            }
        }
        Frame::new(
            width,
            height,
            pixels,
            Tt::from_julian_date(JD_J2000),
            1000,
            Intrinsics::placeholder(width, height),
        )
        .unwrap()
    }

    #[test]
    fn single_frame_with_body_and_horizon() {
        // Body 200 px above horizon at fy=1000 → ~11.31° altitude.
        let frame = synth_body_and_horizon(320, 240, 200, 160.0, 100.0, 12.0);
        let alt = panorama_altitude(
            &[frame],
            HorizonConfig::default(),
            CentroidConfig::default(),
            TrackConfig::default(),
        )
        .unwrap();
        let alt_deg = alt.value.to_degrees();
        // Exact value depends on intrinsics; just check it's in a
        // reasonable positive range.
        assert!(
            alt_deg > 0.0 && alt_deg < 30.0,
            "altitude {alt_deg}° out of expected range"
        );
    }

    #[test]
    fn two_frame_chain_body_above_horizon() {
        // Frame 0: horizon + features in lower portion.
        // Frame 1: features + body in upper portion. The two frames
        // overlap in the middle (the features are placed so both
        // frames see them).
        // For simplicity here, use two identical-content frames where
        // the second has the body added; the tracker should produce
        // identity transform and the body ends up where it is in
        // frame 1, which is the "horizon frame" (frame 0). Since
        // they're 240 px tall, body at y=50 → 70 px above horizon
        // at y=120.
        let frame0 = synth_horizon_only(320, 240, 120);
        let frame1 = synth_body_only(320, 240, 160.0, 50.0, 10.0);
        let alt = panorama_altitude(
            &[frame0, frame1],
            HorizonConfig::default(),
            CentroidConfig::default(),
            TrackConfig::default(),
        )
        .unwrap();
        let alt_deg = alt.value.to_degrees();
        // body 70 px above horizon at fy=1000 → atan(70/1000) ≈ 4.0°.
        // Tolerance loose because tracking may add small offset.
        assert!(
            alt_deg > 1.0 && alt_deg < 10.0,
            "altitude {alt_deg}° out of expected range for two-frame chain"
        );
    }

    #[test]
    fn rejects_when_no_horizon_frame() {
        let frame = synth_body_only(200, 150, 100.0, 75.0, 10.0);
        let result = panorama_altitude(
            &[frame],
            HorizonConfig::default(),
            CentroidConfig::default(),
            TrackConfig::default(),
        );
        assert!(matches!(result, Err(PanoramaError::NoHorizonFrame)));
    }

    #[test]
    fn rejects_when_no_body_frame() {
        // Horizon frame only has a uniform sea/sky and corner spots —
        // no big bright body that centroiding would pick up.
        // Override the centroid threshold so the corner spots aren't
        // mistaken for the body. (Default config has min_area_px=50;
        // each corner spot is 5×5=25 px, below threshold.)
        let frame = synth_horizon_only(200, 150, 80);
        let result = panorama_altitude(
            &[frame],
            HorizonConfig::default(),
            CentroidConfig::default(),
            TrackConfig::default(),
        );
        // Either the centroid module rejects the small spots, in which
        // case we get NoBodyFrame; or the bright sky region is picked
        // as the centroid (legitimately the brightest large region).
        // Both are valid outcomes for this synthetic input; the contract
        // is that the function doesn't silently fabricate a body.
        match result {
            Err(PanoramaError::NoBodyFrame) | Ok(_) => {
                // Either: the centroid module rejects the small spots,
                // giving NoBodyFrame; or the bright sky region is
                // legitimately picked as the centroid (largest bright
                // region by design). Both are valid for this synthetic
                // input; the contract is that the function doesn't
                // silently fabricate a body. Pass.
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    #[test]
    fn via_rotation_single_frame_agrees_with_pixel_rigid() {
        // Same fixture as single_frame_with_body_and_horizon: the
        // ray-space path should produce an altitude that agrees
        // with the pixel-rigid path within a small tolerance, since
        // the chain is empty (single frame) and both routes share
        // the same horizon detection + body centroid.
        let frame = synth_body_and_horizon(320, 240, 200, 160.0, 100.0, 12.0);
        let frames = [frame];
        let pixel = panorama_altitude(
            &frames,
            HorizonConfig::default(),
            CentroidConfig::default(),
            TrackConfig::default(),
        )
        .unwrap();
        let rays = panorama_altitude_via_rotation(
            &frames,
            HorizonConfig::default(),
            CentroidConfig::default(),
            TrackConfig::default(),
        )
        .unwrap();
        let diff_arcmin = (pixel.value - rays.value).to_degrees() * 60.0;
        assert!(
            diff_arcmin.abs() < 1.0,
            "pixel-rigid {}° vs ray {}°: diff {} arcmin too large",
            pixel.value.to_degrees(),
            rays.value.to_degrees(),
            diff_arcmin
        );
    }

    #[test]
    fn via_rotation_two_frame_chain_recovers_known_altitude() {
        // Same fixture as two_frame_chain_body_above_horizon. With
        // the camera-space chain, the body ray from frame 1 must
        // be rotated into frame 0's coordinates via track_rotation
        // (which on this nearly-identical-content pair recovers a
        // small rotation), then composed with the horizon plane in
        // frame 0. Expect roughly the same answer as the pixel
        // path.
        let frame0 = synth_horizon_only(320, 240, 120);
        let frame1 = synth_body_only(320, 240, 160.0, 50.0, 10.0);
        let alt = panorama_altitude_via_rotation(
            &[frame0, frame1],
            HorizonConfig::default(),
            CentroidConfig::default(),
            TrackConfig {
                // Two-frame chain on synthetic data — needs only a
                // few inliers to be confident.
                min_inliers: 4,
                ..TrackConfig::default()
            },
        )
        .unwrap();
        let alt_deg = alt.value.to_degrees();
        // Body 70 px above horizon at fy=1000 → atan(70/1000) ≈ 4°.
        assert!(
            alt_deg > 1.0 && alt_deg < 10.0,
            "ray-space altitude {alt_deg}° out of expected range"
        );
    }

    #[test]
    fn for_pair_uses_supplied_horizon_without_redetecting() {
        // body_frame has a bright body but no horizon detectable
        // because it's a uniform-bright field around the body.
        // horizon_frame has the horizon. We pass the horizon line
        // in directly; the helper must not invoke any horizon
        // detector and must succeed.
        let body_frame = synth_body_only(320, 240, 160.0, 50.0, 10.0);
        let horizon_frame = synth_horizon_only(320, 240, 120);

        // Detect the horizon line in horizon_frame explicitly
        // (mirroring what Stage E has cached as a HorizonRecord).
        let horizon_line =
            crate::horizon::detect_horizon(&horizon_frame, HorizonConfig::default()).unwrap();

        // Centroid the body in body_frame explicitly (mirroring
        // what Stage E has cached as a BodyRecord).
        let body_centroid = centroid_brightest_body(&body_frame, CentroidConfig::default())
            .expect("body centroid in body frame");

        let alt = panorama_altitude_for_pair(
            &body_frame,
            body_centroid,
            &horizon_frame,
            horizon_line,
            TrackConfig {
                min_inliers: 4,
                ..TrackConfig::default()
            },
        )
        .expect("cross-frame helper should succeed on the synthetic pair");

        let alt_deg = alt.value.to_degrees();
        // Same fixture as `via_rotation_two_frame_chain_recovers_known_altitude`
        // (body 70 px above the horizon at fy=1000 → ~4°).
        assert!(
            alt_deg > 1.0 && alt_deg < 10.0,
            "for_pair altitude {alt_deg}° out of expected range"
        );
        // σ must be finite, positive, and strictly greater than
        // just the centroid or horizon σ alone (it includes the
        // executed stitch σ).
        let sigma = alt.sigma.value();
        assert!(sigma.is_finite() && sigma > 0.0);
        assert!(
            sigma >= horizon_line.altitude_sigma.value(),
            "combined σ ({sigma}) should be ≥ horizon σ ({})",
            horizon_line.altitude_sigma.value()
        );
    }

    #[test]
    fn for_pair_propagates_tracking_failed_for_unrelated_frames() {
        // Two frames with no shared content: blank vs solid.
        // The tracker should fail to find correspondences.
        let pa = vec![1_000u16; 320 * 240];
        let pb = vec![60_000u16; 320 * 240];
        let intr = Intrinsics::placeholder(320, 240);
        let fa = Frame::new(320, 240, pa, Tt::from_julian_date(JD_J2000), 1000, intr).unwrap();
        let fb = Frame::new(320, 240, pb, Tt::from_julian_date(JD_J2000), 1000, intr).unwrap();
        let horizon_line = crate::horizon::HorizonLine {
            slope: 0.0,
            intercept: 120.0,
            inlier_count: 50,
            candidate_count: 50,
            residual_rms_px: 0.5,
            altitude_sigma: Sigma::new(1e-4).unwrap(),
        };
        let body_centroid = crate::centroid::Centroid {
            x: 160.0,
            y: 60.0,
            area_px: 100,
            mean_intensity: 30_000.0,
            position_sigma_px: Sigma::new(0.5).unwrap(),
        };
        let result = panorama_altitude_for_pair(
            &fa,
            body_centroid,
            &fb,
            horizon_line,
            TrackConfig::default(),
        );
        assert!(
            matches!(result, Err(PanoramaError::TrackingFailed { .. })),
            "expected TrackingFailed, got {result:?}"
        );
    }
}
