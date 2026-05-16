package co.anomaly.bris.engine

import android.content.Context
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.util.SizeF
import androidx.camera.camera2.interop.Camera2CameraInfo
import androidx.camera.core.CameraSelector
import androidx.camera.lifecycle.ProcessCameraProvider

/**
 * Enumerates physical back cameras and presents them as
 * operator-selectable lenses.
 *
 * **Why this exists.** Modern Android devices expose multiple
 * physical back cameras (ultrawide, wide, telephoto) under a
 * single logical camera id. Bris's accuracy story
 * (`readme.org` §"What the operator can do" item 2) is
 * dominated by focal length: a longer lens gives more
 * arcsec-per-pixel at the body centroid, which directly
 * tightens altitude σ. The default `CameraSelector
 * .DEFAULT_BACK_CAMERA` resolves to the *wide* lens on most
 * phones — exactly the wrong choice for celestial fixes. The
 * operator needs an explicit way to pin Bris to the
 * telephoto.
 *
 * **What a "lens id" is.** A stable, per-device string keyed
 * to the underlying physical camera id reported by
 * `CameraCharacteristics`. Calibration intrinsics, captured
 * frames, and the current `CameraSelector` filter are all
 * keyed by this id. The id is opaque from the operator's
 * perspective; the UI shows a human label
 * (`"Telephoto · 4.7× · 24mm-eq"`) derived from focal length
 * + sensor geometry.
 *
 * **Calibration is per-(lens, resolution).** A calibration
 * solved against the wide lens silently produces wrong
 * altitudes if applied to the telephoto. The
 * [`CalibrationStore`] keys storage by `(lensId, width,
 * height)`; this catalog is the source of truth for the
 * `lensId` half of that key.
 *
 * **First-launch heuristic.** Pick the lens with the longest
 * focal length that is *not* an ultrawide (focal length ≥ the
 * wide reference). If only one lens is available, pick it.
 * Operators retune via Settings.
 */
object LensCatalog {

    /** One physical back camera the operator can choose. */
    data class Lens(
        /** Stable id used in Prefs / CalibrationStore / CameraSelector filters. */
        val id: String,
        /** Operator-visible label (focal length + zoom factor). */
        val label: String,
        /** Focal length in mm (rear-element value, not 35mm-equivalent). */
        val focalLengthMm: Float,
        /** 35mm-equivalent focal length, when sensor size is known; null otherwise. */
        val equivalentFocalLengthMm: Float?,
        /** Approximate zoom factor relative to the catalog's wide reference. */
        val zoomFactor: Float,
        /** True if this lens is shorter than the wide reference (ultrawide). */
        val isUltrawide: Boolean,
    )

    /**
     * Enumerate physical back cameras visible to CameraX +
     * Camera2. Falls back to a single synthetic "Default back
     * camera" entry if Camera2 can't enumerate (e.g.
     * permission missing, exotic hardware) so the UI is never
     * empty.
     *
     * Pure CameraX has no way to list physical cameras inside
     * a logical camera id; we drop down to the Camera2 API
     * (`CameraManager.cameraIdList`) and filter to back-facing.
     * The result is sorted by focal length ascending so the
     * UI presents ultrawide → wide → telephoto in order.
     */
    fun enumerate(context: Context): List<Lens> {
        val manager = context.getSystemService(Context.CAMERA_SERVICE) as? CameraManager
            ?: return listOf(fallbackLens())
        val results = mutableListOf<Lens>()
        try {
            for (camId in manager.cameraIdList) {
                val chars = manager.getCameraCharacteristics(camId)
                val facing = chars.get(CameraCharacteristics.LENS_FACING) ?: continue
                if (facing != CameraCharacteristics.LENS_FACING_BACK) continue
                val focals = chars.get(CameraCharacteristics.LENS_INFO_AVAILABLE_FOCAL_LENGTHS)
                val focal = focals?.maxOrNull() ?: continue
                val sensorSize = chars.get(CameraCharacteristics.SENSOR_INFO_PHYSICAL_SIZE)
                val equiv = sensorSize?.let { equivalentFocalLength(focal, it) }
                results += Lens(
                    id = camId,
                    label = "", // filled in below once we know the wide reference
                    focalLengthMm = focal,
                    equivalentFocalLengthMm = equiv,
                    zoomFactor = 1.0f,
                    isUltrawide = false,
                )
            }
        } catch (_: Exception) {
            return listOf(fallbackLens())
        }
        if (results.isEmpty()) return listOf(fallbackLens())

        // The wide reference is the median focal length, which
        // on a typical phone is the primary wide camera. Using
        // the minimum would label everything telephoto;
        // using the maximum would label everything ultrawide.
        val sorted = results.sortedBy { it.focalLengthMm }
        val wideFocal = sorted[sorted.size / 2].focalLengthMm

        return sorted.map { lens ->
            val zoom = lens.focalLengthMm / wideFocal
            val isUltra = zoom < 0.85f
            lens.copy(
                label = labelFor(lens.focalLengthMm, lens.equivalentFocalLengthMm, zoom, isUltra),
                zoomFactor = zoom,
                isUltrawide = isUltra,
            )
        }
    }

    /**
     * Pick a sensible default lens when the operator has not
     * yet chosen one. Prefers the longest non-ultrawide lens;
     * falls back to whatever is available if the device only
     * has ultrawides (rare).
     *
     * The accuracy section of `readme.org` argues for the
     * longest focal length the operator can reasonably use;
     * an ultrawide is the worst possible default for altitude
     * σ. Picking the longest non-ultrawide gets the operator
     * meaningfully closer to that goal without requiring a
     * settings visit on first launch.
     */
    fun pickDefault(lenses: List<Lens>): Lens? {
        if (lenses.isEmpty()) return null
        val nonUltra = lenses.filterNot { it.isUltrawide }
        val pool = if (nonUltra.isNotEmpty()) nonUltra else lenses
        return pool.maxByOrNull { it.focalLengthMm }
    }

    /**
     * Build a [`CameraSelector`] that pins CameraX to the
     * physical camera with the given id. If the id is unknown
     * or null, returns [`CameraSelector.DEFAULT_BACK_CAMERA`]
     * (the same behavior the app had before lens selection
     * existed).
     *
     * The filter checks each [`androidx.camera.core.CameraInfo`]
     * by dropping into Camera2 interop and reading the
     * physical camera id from the underlying characteristics.
     * On older devices that expose only one physical back
     * camera, the filter still matches that single entry.
     */
    fun selectorFor(lensId: String?): CameraSelector {
        if (lensId == null) return CameraSelector.DEFAULT_BACK_CAMERA
        return CameraSelector.Builder()
            .requireLensFacing(CameraSelector.LENS_FACING_BACK)
            .addCameraFilter { infos ->
                val matches = infos.filter { info ->
                    runCatching { Camera2CameraInfo.from(info).cameraId }
                        .getOrNull() == lensId
                }
                matches.ifEmpty { infos }
            }
            .build()
    }

    /**
     * Resolve the underlying physical camera id of CameraX's
     * default back camera. Used when no operator selection
     * exists yet and we need a stable id to record alongside
     * a calibration session.
     */
    fun defaultBackCameraId(context: Context): String? {
        return try {
            val provider = ProcessCameraProvider.getInstance(context).get()
            val info = provider.availableCameraInfos.firstOrNull { info ->
                runCatching {
                    CameraSelector.DEFAULT_BACK_CAMERA.filter(listOf(info)).isNotEmpty()
                }.getOrDefault(false)
            } ?: return null
            Camera2CameraInfo.from(info).cameraId
        } catch (_: Exception) {
            null
        }
    }

    // --- internal ----------------------------------------------------------

    private fun labelFor(
        focalMm: Float,
        equivMm: Float?,
        zoom: Float,
        isUltrawide: Boolean,
    ): String {
        val role = when {
            isUltrawide -> "Ultrawide"
            zoom > 1.4f -> "Telephoto"
            else -> "Wide"
        }
        val zoomStr = "%.1f×".format(zoom)
        val equivStr = equivMm?.let { " · ${it.toInt()}mm-eq" } ?: ""
        val rawStr = " · ${"%.1f".format(focalMm)}mm"
        return "$role · $zoomStr$rawStr$equivStr"
    }

    /**
     * 35mm-equivalent focal length given the lens's actual
     * focal length and the sensor's physical diagonal.
     * Standard photographic convention; 35mm full-frame has a
     * 43.27 mm diagonal.
     */
    private fun equivalentFocalLength(focalMm: Float, sensorMm: SizeF): Float {
        val sensorDiag = kotlin.math.sqrt(
            sensorMm.width * sensorMm.width + sensorMm.height * sensorMm.height,
        )
        if (sensorDiag <= 0f) return focalMm
        return focalMm * (FULL_FRAME_DIAGONAL_MM / sensorDiag)
    }

    private fun fallbackLens(): Lens = Lens(
        id = FALLBACK_LENS_ID,
        label = "Default back camera",
        focalLengthMm = Float.NaN,
        equivalentFocalLengthMm = null,
        zoomFactor = 1.0f,
        isUltrawide = false,
    )

    /**
     * Sentinel id used when Camera2 enumeration is
     * unavailable; tells [`selectorFor`] to use
     * [`CameraSelector.DEFAULT_BACK_CAMERA`]. Treated as a
     * normal id for storage keys so legacy data continues to
     * resolve.
     */
    const val FALLBACK_LENS_ID: String = "default-back"

    private const val FULL_FRAME_DIAGONAL_MM = 43.27f
}
