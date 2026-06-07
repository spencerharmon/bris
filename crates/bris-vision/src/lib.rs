//! Vision pipeline: capture-agnostic image processing, lens calibration,
//! horizon detection, body centroiding, and multi-frame stitching.
//!
//! All algorithms are classical (no ML inference runtime). See `plan.org`
//! Phase 2 for design.

pub mod bright_blob;
pub mod centroid;
pub mod centroid_refine;
pub mod condition;
pub mod debug_render;
pub mod frame;
pub mod fusion;
pub mod horizon;
pub mod horizon_providers;
pub mod io;
pub mod lens;
pub mod measure;
pub mod night_horizon;
pub mod panorama;
pub mod peak;
pub mod pyramid;
pub mod ray;
#[cfg(feature = "segmentation")]
pub mod segment;
pub mod track;

pub use bright_blob::{compute_bright_blob_mask, BrightBlobConfig};
pub use centroid::{
    centroid_brightest_body, centroid_brightest_body_in_mask, centroid_saturated_body_in_mask,
    extract_multi_saturated_centroids, Centroid, CentroidConfig, CentroidError,
    SaturatedBodyConfig,
};
pub use centroid_refine::{
    extract_halo_pixels, refine_centroid_subpixel, HaloPixel, RefinedCentroid,
    DEFAULT_GAIN_E_PER_ADU,
};
pub use condition::{
    classify, classify_with_masks, AstronomicalEvidence, Classification, Condition,
    ConditionConfig, ImageEvidence, TwilightBand,
};
pub use debug_render::{
    render_base_image, render_debug_overlay, CentroidOverlay, HorizonOverlay, OverlayData,
    RenderMetadata, StageEOutcomeView, RENDER_MAX_SIDE_PX,
};
pub use frame::{rotate_pixels, Frame, FrameError, Intrinsics, IntrinsicsScaleError, Rotation};
pub use fusion::{fuse_altitudes, FrameMeasurement, FusionConfig, FusionError};
pub use horizon::{
    body_column_mask, detect_horizon, detect_horizon_via_sky_region,
    detect_horizon_via_sky_region_with_column_mask, detect_horizon_with_column_mask, HorizonConfig,
    HorizonError, HorizonLine,
};
pub use horizon_providers::{
    fuse_horizon_hypotheses, BodyCandidate, DirectSight, FusionMode, FusionOutcome,
    HorizonFusionConfig, HorizonHypothesis, HorizonProvenance, HorizonProvider,
    HorizonProviderContext, OpticalKind, PositionPrior, ReflectionPairConfig,
    ReflectionPairProvider, ReflectionPairStats, TemporalScope, VanishingPointConfig,
    VanishingPointProvider, VanishingPointStats, VerticalLineConfig, VerticalLineProvider,
    VerticalLineStats,
};
#[cfg(feature = "ml-gravity")]
pub use horizon_providers::{MlGravityConfig, MlGravityProvider, MlGravityStats};
pub use io::{
    load_frame_from_path, load_frame_from_path_with_rotation, save_frame_as_png, LoadError,
};
pub use lens::{distort_normalized, pixel_ray_direction, project_pinhole, undistort_pixel};
pub use measure::{measure_altitude, measure_altitude_from_ray, MeasurementError};
pub use night_horizon::{
    body_box_mask, detect_horizon_night, detect_horizon_night_excluding_body,
    detect_horizon_night_multi_pass, detect_horizon_night_textured,
    detect_horizon_night_textured_excluding_body, detect_horizon_night_textured_with_pixel_mask,
    detect_horizon_night_with_column_mask, NightHorizonConfig, TexturedHorizonConfig,
};
pub use panorama::{
    panorama_altitude, panorama_altitude_for_pair, panorama_altitude_via_rotation,
    panorama_altitude_via_rotation_with_detector, panorama_altitude_with_detector, FrameRoles,
    PanoramaError,
};
pub use peak::{detect_peaks, detect_peaks_above_horizon, Peak, PeakConfig};
pub use pyramid::{FramePyramid, PyramidError, PyramidLevel};
pub use ray::{
    altitude_from_rays, bisector_normal, horizon_line_from_normal, AltitudeMeasurement, BodyRay,
    CameraRay, HorizonRay,
};
#[cfg(feature = "segmentation")]
pub use segment::{
    detect_horizon_via_segmentation, detect_horizon_via_segmentation_with_column_mask,
    detect_horizon_via_segmentation_with_mask, load_model, segment, segment_with_rotation,
    SegmentError, SegmentationMask,
};
pub use track::{
    detect_corners, track, track_peaks, track_rotation, Corner, RigidTransform,
    RotationBetweenFrames, TrackConfig, TrackError,
};
