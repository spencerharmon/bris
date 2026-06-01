package io.github.spencerharmon.bris.engine

import android.media.Image
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import uniffi.bris_ffi.FfiFrame
import uniffi.bris_ffi.FfiIntrinsics
import uniffi.bris_ffi.FfiPixelFormat
import java.util.concurrent.atomic.AtomicLong

/**
 * CameraX [`ImageAnalysis.Analyzer`] that copies the Y plane of
 * each incoming frame into an [`FfiFrame`] and forwards it to
 * the engine.
 *
 * Backpressure: this analyzer is paired with
 * [`ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST`] in
 * [`LiveScreen`]. CameraX drops frames at *its* layer if the
 * analyzer is busy; the engine drops frames at *its* layer if
 * the input ring is full. The two layers do not fight because
 * the analyzer always copies the pixel buffer before returning,
 * so CameraX can release its [`ImageProxy`] immediately. See
 * `docs/design/diagnostic_collection.md` for the rationale.
 *
 * The Y plane comes in as 8-bit luminance; the engine widens
 * to its u16 internal representation on the Rust side. We pass
 * [`FfiPixelFormat.GRAY8`] to declare the format honestly.
 *
 * Debug-capture: when [`debugCaptureProvider`] returns true, the
 * frame is also persisted to the [`DebugCaptureBuffer`] *after*
 * the engine push, with the engine's just-taken
 * [`DiagnosticSnapshot`]. Snapshot is taken at frame-push time
 * (not capture time) because the operator-meaningful "what did
 * the engine think when it saw this frame" is the post-push
 * state, not the pre-push state.
 */
class FrameAnalyzer(
    private val engine: EngineWrapper,
    private val intrinsicsProvider: () -> FfiIntrinsics,
    private val debugCaptureProvider: () -> Boolean = { false },
    private val debugBuffer: DebugCaptureBuffer? = null,
    /**
     * Per-frame callback fired after the analyzer has
     * constructed the [`FfiFrame`] and pushed it to the
     * engine. `null` when no active capture is recording.
     * Used by `CaptureRecorder` to tap every analyzer frame
     * into the capture directory's `frames/` subtree so
     * Start→Stop recording captures *every* frame, not just
     * the contributing-frame bytes of a fix.
     */
    private val captureFrameTap: ((FfiFrame) -> Unit)? = null,
    /**
     * Returns the **current** sensor analog conversion gain
     * in electrons per ADU for the active camera, scaled to
     * the current ISO. The wiring is:
     *
     *   gain_e_per_adu = profile.gainEPerAduAtMinIso *
     *                    (currentIso / minIso)
     *
     * Where `currentIso` comes from the most recent
     * `CaptureResult.SENSOR_SENSITIVITY` and `minIso` from
     * `CameraCharacteristics.SENSOR_INFO_SENSITIVITY_RANGE`
     * lower bound. The relationship is approximately
     * linear inside the analog-gain range; once the camera
     * runs out of analog gain it falls back to digital,
     * which **should not** be folded in (the engine assumes
     * the value here reflects analog gain only). For the
     * placeholder profile in [`FactoryCalibration`] the
     * caller treats the entire ISO range as analog — OK
     * for the spike, refine when a per-unit measurement
     * lands.
     *
     * Returns `0.0` when no profile / ISO is yet available;
     * the FFI substitutes [`SensorGain::UNITY`].
     *
     * TODO: add a Robolectric test for the gain-scaling
     * behaviour when the Android test infrastructure grows.
     */
    private val sensorGainProvider: () -> Double = { 0.0 },
) : ImageAnalysis.Analyzer {

    private val frameCount = AtomicLong(0)

    override fun analyze(image: ImageProxy) {
        try {
            // CameraX sets `rotationDegrees` to the rotation
            // the analyzer must apply to the buffer to land it
            // at the use case's `targetRotation` (which we tie
            // to the device's display rotation; see
            // `CameraSurface` in `LiveScreen`). The Y bytes in
            // `image.image.planes[0]` are *not* pre-rotated by
            // CameraX for the YUV format; we have to rotate
            // them ourselves here. Doing it on Android keeps
            // the FfiFrame contract honest: the engine always
            // sees gravity-up pixels and `source_rotation_deg`
            // in the bundle stays truthfully 0.
            val rotationDeg = image.imageInfo.rotationDegrees
            val ffiFrame = image.image?.toFfiFrame(
                intrinsicsProvider(),
                sensorGainProvider(),
                rotationDeg,
            ) ?: return
            engine.pushFrame(ffiFrame)
            frameCount.incrementAndGet()
            captureFrameTap?.invoke(ffiFrame)
            if (debugCaptureProvider() && debugBuffer != null) {
                val snap = engine.snapshot.value
                debugBuffer.appendFrame(
                    ffiFrame,
                    snap,
                    exposureUs = ffiFrame.exposureUs,
                    sensorGainEPerAdu = ffiFrame.gainEPerAdu,
                )
            }
        } catch (t: Throwable) {
            // CameraX runs the analyzer on a background thread.
            // An uncaught exception here would propagate up
            // through the executor and crash the activity. The
            // streaming engine already tolerates dropped frames;
            // logging and dropping is strictly better than
            // taking down the app for a recoverable per-frame
            // hiccup.
            android.util.Log.w(TAG, "analyze: dropping frame due to ${t.javaClass.simpleName}: ${t.message}")
        } finally {
            image.close()
        }
    }

    /** How many frames have been forwarded since construction. */
    fun framesForwarded(): Long = frameCount.get()

    private companion object {
        private const val TAG = "FrameAnalyzer"
    }
}

/**
 * Copy the Y plane of an Android [`Image`] (YUV_420_888) into
 * an [`FfiFrame`].
 *
 * The Y plane may be padded with row-stride bytes greater than
 * width; we copy row-by-row to produce a tightly-packed buffer.
 *
 * Important: the Y plane buffer's `remaining()` is
 * `(height - 1) * rowStride + width`, **not** `height *
 * rowStride`. The last row contains only `width` bytes; the
 * trailing `rowStride - width` padding of that row is not
 * actually present in the buffer. Reading `rowStride` bytes
 * for every row including the last one therefore overruns the
 * buffer with a `BufferUnderflowException` (observed on
 * sdm660-class devices). The loop below special-cases the
 * final row and reads only `width` bytes for it. The
 * pixel-stride normalization is then identical to the
 * non-final rows since the final row has no per-pixel stride
 * padding to skip past width either.
 *
 * The captured timestamp is converted from CameraX's nanosecond
 * domain (uptime) to wall-clock milliseconds; the engine's
 * dual-clock handling (Phase 1.5) is the authoritative model
 * for time integrity, but at the FFI boundary the caller-side
 * monotonic-vs-wall conversion is a small approximation.
 */
private fun Image.toFfiFrame(
    intrinsics: FfiIntrinsics,
    gainEPerAdu: Double,
    rotationDeg: Int,
): FfiFrame {
    val yPlane = planes[0]
    val rowStride = yPlane.rowStride
    val pixelStride = yPlane.pixelStride
    val w = width
    val h = height
    val src = yPlane.buffer
    val sensor = ByteArray(w * h)

    if (pixelStride == 1 && rowStride == w) {
        // Tight packing. The buffer's remaining() should equal
        // w * h exactly in this case but cap defensively.
        val available = src.remaining()
        val toRead = minOf(sensor.size, available)
        src.get(sensor, 0, toRead)
    } else {
        // Stride normalization. Each row except the last is
        // `rowStride` bytes in the source buffer; the last row
        // is only `width * pixelStride` bytes (no trailing
        // row-padding for the last row).
        val fullRowBytes = rowStride
        val lastRowBytes = (w - 1) * pixelStride + 1
        val rowBuf = ByteArray(fullRowBytes)
        for (row in 0 until h) {
            src.position(row * rowStride)
            val bytesThisRow = if (row == h - 1) lastRowBytes else fullRowBytes
            // Defensive cap: never read more than the buffer's
            // remaining bytes. On any honest device this never
            // truncates; on a misbehaving HAL it converts a
            // crash into a partially-filled frame.
            val toRead = minOf(bytesThisRow, src.remaining())
            src.get(rowBuf, 0, toRead)
            if (pixelStride == 1) {
                val copy = minOf(w, toRead)
                System.arraycopy(rowBuf, 0, sensor, row * w, copy)
            } else {
                val maxCol = minOf(w, (toRead + pixelStride - 1) / pixelStride)
                for (col in 0 until maxCol) {
                    sensor[row * w + col] = rowBuf[col * pixelStride]
                }
            }
        }
    }

    // Rotate sensor-orientation pixels to gravity-up. Mod 360
    // and normalise; CameraX only ever emits 0/90/180/270.
    val rot = ((rotationDeg % 360) + 360) % 360
    val (outW, outH, dst) = when (rot) {
        0 -> Triple(w, h, sensor)
        90 -> Triple(h, w, rotate90(sensor, w, h))
        180 -> Triple(w, h, rotate180(sensor, w, h))
        270 -> Triple(h, w, rotate270(sensor, w, h))
        else -> Triple(w, h, sensor) // unreachable in practice
    }

    // Rotate intrinsics to match. cx/cy/fx/fy are defined in
    // sensor coords; when we rotate the pixel grid we must
    // rotate the principal point and swap (fx, fy) for 90/270.
    // Distortion coefficients k1/k2/k3/p1/p2 are radially
    // symmetric (around the principal point) so they don't
    // change under a frame-rotation; p1/p2 are tangential and
    // *do* rotate, but for the small values bris's calibration
    // produces (<1e-3 typical), the rotation is a second-order
    // correction we can defer until the calibration path itself
    // becomes rotation-aware (separate Phase-2 follow-up).
    val rotatedIntrinsics = when (rot) {
        0 -> intrinsics
        180 -> FfiIntrinsics(
            fx = intrinsics.fx, fy = intrinsics.fy,
            cx = (w - 1).toDouble() - intrinsics.cx,
            cy = (h - 1).toDouble() - intrinsics.cy,
            k1 = intrinsics.k1, k2 = intrinsics.k2, k3 = intrinsics.k3,
            p1 = intrinsics.p1, p2 = intrinsics.p2,
        )
        90 -> FfiIntrinsics(
            // 90° CW: (x,y) -> (h-1-y, x). Swap fx/fy, remap cx/cy.
            fx = intrinsics.fy, fy = intrinsics.fx,
            cx = (h - 1).toDouble() - intrinsics.cy,
            cy = intrinsics.cx,
            k1 = intrinsics.k1, k2 = intrinsics.k2, k3 = intrinsics.k3,
            p1 = intrinsics.p1, p2 = intrinsics.p2,
        )
        270 -> FfiIntrinsics(
            // 270° CW: (x,y) -> (y, w-1-x). Swap fx/fy.
            fx = intrinsics.fy, fy = intrinsics.fx,
            cx = intrinsics.cy,
            cy = (w - 1).toDouble() - intrinsics.cx,
            k1 = intrinsics.k1, k2 = intrinsics.k2, k3 = intrinsics.k3,
            p1 = intrinsics.p1, p2 = intrinsics.p2,
        )
        else -> intrinsics
    }

    return FfiFrame(
        width = outW.toUInt(),
        height = outH.toUInt(),
        format = FfiPixelFormat.GRAY8,
        pixels = dst,
        capturedUnixMs = System.currentTimeMillis(),
        exposureUs = 0u,
        intrinsics = rotatedIntrinsics,
        gainEPerAdu = gainEPerAdu,
    )
}

/**
 * 90° clockwise rotation: dst[col, h-1-row] <- src[row, col].
 * Output dimensions: (h, w).
 */
internal fun rotate90(src: ByteArray, w: Int, h: Int): ByteArray {
    val dst = ByteArray(w * h)
    for (row in 0 until h) {
        val srcBase = row * w
        val dstCol = h - 1 - row
        for (col in 0 until w) {
            // dst is (h, w) addressed as col * h + dstCol
            dst[col * h + dstCol] = src[srcBase + col]
        }
    }
    return dst
}

/** 180°: dst[w-1-col, h-1-row] <- src[col, row]. */
internal fun rotate180(src: ByteArray, w: Int, h: Int): ByteArray {
    val n = w * h
    val dst = ByteArray(n)
    for (i in 0 until n) dst[i] = src[n - 1 - i]
    return dst
}

/**
 * 270° clockwise (= 90° counter-clockwise):
 * dst[h-1-col, row] <- src[row, col]. Output dimensions: (h, w).
 */
internal fun rotate270(src: ByteArray, w: Int, h: Int): ByteArray {
    val dst = ByteArray(w * h)
    for (row in 0 until h) {
        val srcBase = row * w
        val dstRow0 = row
        for (col in 0 until w) {
            // dst is (h, w) addressed as (w-1-col) * h + row
            dst[(w - 1 - col) * h + dstRow0] = src[srcBase + col]
        }
    }
    return dst
}
