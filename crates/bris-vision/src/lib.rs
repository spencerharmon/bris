//! Vision pipeline: capture-agnostic image processing, lens calibration,
//! horizon detection, body centroiding, and multi-frame stitching.
//!
//! All algorithms are classical (no ML inference runtime). See `plan.org`
//! Phase 2 for design.

pub mod centroid;
pub mod frame;
pub mod fusion;
pub mod horizon;
pub mod io;
pub mod lens;
pub mod measure;
pub mod panorama;
pub mod peak;
pub mod track;

pub use centroid::{centroid_brightest_body, Centroid, CentroidConfig, CentroidError};
pub use frame::{Frame, FrameError, Intrinsics};
pub use fusion::{fuse_altitudes, FrameMeasurement, FusionConfig, FusionError};
pub use horizon::{
    detect_horizon, detect_horizon_via_sky_region, HorizonConfig, HorizonError, HorizonLine,
};
pub use io::{load_frame_from_path, save_frame_as_png, LoadError};
pub use lens::{distort_normalized, pixel_ray_direction, project_pinhole, undistort_pixel};
pub use measure::{measure_altitude, MeasurementError};
pub use panorama::{panorama_altitude, panorama_altitude_with_detector, FrameRoles, PanoramaError};
pub use peak::{detect_peaks, Peak, PeakConfig};
pub use track::{
    detect_corners, track, track_peaks, Corner, RigidTransform, TrackConfig, TrackError,
};
