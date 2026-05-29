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
            val ffiFrame = image.image?.toFfiFrame(
                intrinsicsProvider(),
                sensorGainProvider(),
            ) ?: return
            engine.pushFrame(ffiFrame)
            frameCount.incrementAndGet()
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
): FfiFrame {
    val yPlane = planes[0]
    val rowStride = yPlane.rowStride
    val pixelStride = yPlane.pixelStride
    val w = width
    val h = height
    val src = yPlane.buffer
    val dst = ByteArray(w * h)

    if (pixelStride == 1 && rowStride == w) {
        // Tight packing. The buffer's remaining() should equal
        // w * h exactly in this case but cap defensively.
        val available = src.remaining()
        val toRead = minOf(dst.size, available)
        src.get(dst, 0, toRead)
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
                System.arraycopy(rowBuf, 0, dst, row * w, copy)
            } else {
                val maxCol = minOf(w, (toRead + pixelStride - 1) / pixelStride)
                for (col in 0 until maxCol) {
                    dst[row * w + col] = rowBuf[col * pixelStride]
                }
            }
        }
    }
    return FfiFrame(
        width = w.toUInt(),
        height = h.toUInt(),
        format = FfiPixelFormat.GRAY8,
        pixels = dst,
        capturedUnixMs = System.currentTimeMillis(),
        exposureUs = 0u,
        intrinsics = intrinsics,
        gainEPerAdu = gainEPerAdu,
    )
}
