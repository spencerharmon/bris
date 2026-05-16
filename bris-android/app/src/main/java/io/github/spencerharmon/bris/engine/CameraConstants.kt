package io.github.spencerharmon.bris.engine

import android.content.Context
import android.graphics.ImageFormat
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.hardware.camera2.params.StreamConfigurationMap
import android.util.Rational
import android.util.Size

/**
 * Per-lens capture-resolution lookup.
 *
 * **Why this is a runtime lookup, not a constant.** Bris's
 * accuracy is dominated by focal length and pixel pitch
 * (`readme.org` §"What the operator can do" #2): more pixels
 * on the body centroid means more arcsec / pixel, means
 * tighter altitude σ. The per-stage resolution architecture
 * (`plan.org` Phase 2) was built exactly so the operator can
 * capture at the sensor's *native maximum* and have each
 * pipeline stage downsample to its own preferred resolution
 * (`bris-vision::FramePyramid` + `Intrinsics::scaled_to`). The
 * earlier hardcoded 1280×720 capture request defeated that
 * architecture — every downstream stage saw at most 720p
 * regardless of sensor capability.
 *
 * This object queries the chosen lens's
 * [`StreamConfigurationMap`] and picks the largest output size
 * the device advertises for YUV_420_888 (the format CameraX
 * delivers to `ImageAnalysis`). Capture and calibration both
 * use the same per-lens maximum so the intrinsics solved
 * during calibration apply unaltered at fix time.
 *
 * **Throughput trade-off.** Asking for a 12 MP frame on a Pi
 * Zero 2W ingesting at sensor rate produces frames faster
 * than the single-threaded streaming engine can process them;
 * CameraX's `STRATEGY_KEEP_ONLY_LATEST` (set in `LiveScreen`)
 * drops the excess. The engine publishes fixes at whatever
 * cadence the slowest stage allows — *lower fix cadence is
 * the preferred trade*, per the operator: a 5-second-per-fix
 * cadence at full sensor resolution beats a 1-second cadence
 * at 720p because the σ on each fix is what matters for the
 * 0.5 nm target.
 */
object CameraConstants {

    /**
     * Largest output size the given physical camera advertises
     * for the analyzer-delivered pixel format. Returns null if
     * the camera id is unknown or has no advertised output
     * sizes.
     *
     * `ImageFormat.YUV_420_888` is the format CameraX uses for
     * `ImageAnalysis`; for `ImageCapture` we follow the same
     * choice so calibration stills and live frames share the
     * exact pixel grid.
     */
    fun maxOutputSizeFor(context: Context, cameraId: String): Size? {
        val manager = context.getSystemService(Context.CAMERA_SERVICE) as? CameraManager
            ?: return null
        return runCatching {
            val chars = manager.getCameraCharacteristics(cameraId)
            val map: StreamConfigurationMap = chars.get(
                CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP,
            ) ?: return@runCatching null
            // YUV_420_888 is what CameraX's ImageAnalysis hands
            // to the analyzer; sticking to it keeps the capture
            // and analysis grids identical. JPEG sizes are
            // separately advertised and often larger, but using
            // a different size for ImageCapture would defeat
            // the calibration-equals-capture invariant.
            val sizes = map.getOutputSizes(ImageFormat.YUV_420_888) ?: return@runCatching null
            sizes.maxByOrNull { it.width.toLong() * it.height.toLong() }
        }.getOrNull()
    }

    /**
     * The aspect ratio of the chosen capture size, expressed
     * as an `android.util.Rational` suitable for a CameraX
     * `ViewPort`. Returned alongside the size so the
     * `ViewPort` and `ResolutionStrategy` agree exactly — a
     * mismatch silently causes CameraX to crop and the
     * operator's visible preview to misalign with what the
     * analyzer sees.
     */
    fun aspectRatioOf(size: Size): Rational = Rational(size.width, size.height)
}
