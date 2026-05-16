package io.github.spencerharmon.bris.engine

import android.util.Size

/**
 * Resolution + aspect ratio shared by every camera-using screen
 * (LiveScreen + CalibrationScreen).
 *
 * The streaming engine consumes whatever pixel grid the
 * analyzer hands it; the calibration solver consumes whatever
 * pixel grid the still capture produces. **Those two grids
 * must be identical**, otherwise the intrinsics solved during
 * calibration silently apply to the wrong pixel grid during
 * fix capture and produce wrong altitudes.
 *
 * Concretely: this constant is the resolution we ask CameraX
 * to deliver via `ResolutionStrategy`, and it is the aspect
 * ratio we pin into the `ViewPort` so the visible
 * `PreviewView` shows exactly the same crop region the
 * analyzer / image-capture sees. No surprise off-screen pixels.
 */
object CameraConstants {
    /** Width of the analyzed / captured frame, in pixels. */
    const val WIDTH = 1280

    /** Height of the analyzed / captured frame, in pixels. */
    const val HEIGHT = 720

    /** [`android.util.Size`] convenience for CameraX builders. */
    val SIZE: Size get() = Size(WIDTH, HEIGHT)
}
