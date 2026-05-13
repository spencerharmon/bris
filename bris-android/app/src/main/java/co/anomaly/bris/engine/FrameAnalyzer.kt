package co.anomaly.bris.engine

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
) : ImageAnalysis.Analyzer {

    private val frameCount = AtomicLong(0)

    override fun analyze(image: ImageProxy) {
        try {
            val ffiFrame = image.image?.toFfiFrame(intrinsicsProvider()) ?: return
            engine.pushFrame(ffiFrame)
            frameCount.incrementAndGet()
            if (debugCaptureProvider() && debugBuffer != null) {
                val snap = engine.snapshot.value
                debugBuffer.appendFrame(ffiFrame, snap)
            }
        } finally {
            image.close()
        }
    }

    /** How many frames have been forwarded since construction. */
    fun framesForwarded(): Long = frameCount.get()
}

/**
 * Copy the Y plane of an Android [`Image`] (YUV_420_888) into
 * an [`FfiFrame`].
 *
 * The Y plane may be padded with row-stride bytes greater than
 * width; we copy row-by-row to produce a tightly-packed buffer.
 * The captured timestamp is converted from CameraX's nanosecond
 * domain (uptime) to wall-clock milliseconds; the engine's
 * dual-clock handling (Phase 1.5) is the authoritative model
 * for time integrity, but at the FFI boundary the caller-side
 * monotonic-vs-wall conversion is a small approximation.
 */
private fun Image.toFfiFrame(intrinsics: FfiIntrinsics): FfiFrame {
    val yPlane = planes[0]
    val rowStride = yPlane.rowStride
    val pixelStride = yPlane.pixelStride
    val w = width
    val h = height
    val src = yPlane.buffer
    val dst = ByteArray(w * h)
    if (pixelStride == 1 && rowStride == w) {
        src.get(dst)
    } else {
        // Slow path: stride normalization.
        val rowBuf = ByteArray(rowStride)
        for (row in 0 until h) {
            src.position(row * rowStride)
            src.get(rowBuf, 0, rowStride)
            if (pixelStride == 1) {
                System.arraycopy(rowBuf, 0, dst, row * w, w)
            } else {
                for (col in 0 until w) {
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
    )
}
