package io.github.spencerharmon.bris.engine

/**
 * Camera intrinsics rotated to match a frame-pixel rotation.
 *
 * Plain data so JVM unit tests don't need to load the
 * UniFFI-generated `FfiIntrinsics` (and, transitively, the
 * native library) just to assert on the math.
 *
 * `width` / `height` are the **output** dimensions \u2014 i.e.
 * `(w, h)` swap for 90/270 degree rotations.
 */
data class RotatedIntrinsics(
    val fx: Double,
    val fy: Double,
    val cx: Double,
    val cy: Double,
    val width: Int,
    val height: Int,
)

/**
 * Rotate a sensor-native pinhole calibration by `rotationDeg`
 * (CW) so it lines up with a pixel buffer that has been
 * rotated by the same amount.
 *
 * `w` and `h` are the sensor-native (pre-rotation) frame
 * dimensions; the returned `RotatedIntrinsics` carries the
 * post-rotation `(width, height)` so callers can stamp the
 * rotated dimensions into `bundle.json` without needing to
 * re-derive the swap rule.
 *
 * Distortion coefficients (k1/k2/k3/p1/p2) are radially
 * symmetric around the principal point and so do not change
 * under a frame rotation. p1/p2 are tangential and **do**
 * rotate, but for the small values bris's calibration
 * produces (<1e-3 typical) the rotation is a second-order
 * correction we defer until the calibration path itself
 * becomes rotation-aware (Phase-2 follow-up; same caveat
 * applies to the inline rotation logic in
 * `FrameAnalyzer.toFfiFrame`).
 *
 * Mirrors the inline math in `FrameAnalyzer.toFfiFrame`;
 * pulled into a pure helper so `DebugBundleWriter` can apply
 * the same transform without duplicating it.
 *
 * Rotations are normalised to one of 0/90/180/270 by
 * (`(deg % 360) + 360) % 360`. Anything outside the set
 * collapses to 0 (in practice CameraX never emits anything
 * else).
 */
fun rotateIntrinsicsForFrameRotation(
    fx: Double,
    fy: Double,
    cx: Double,
    cy: Double,
    w: Int,
    h: Int,
    rotationDeg: Int,
): RotatedIntrinsics {
    val rot = ((rotationDeg % 360) + 360) % 360
    return when (rot) {
        0 -> RotatedIntrinsics(fx, fy, cx, cy, w, h)
        180 -> RotatedIntrinsics(
            fx = fx, fy = fy,
            cx = (w - 1).toDouble() - cx,
            cy = (h - 1).toDouble() - cy,
            width = w, height = h,
        )
        90 -> RotatedIntrinsics(
            // 90° CW: (x,y) -> (h-1-y, x). Swap fx/fy, output (h, w).
            fx = fy, fy = fx,
            cx = (h - 1).toDouble() - cy,
            cy = cx,
            width = h, height = w,
        )
        270 -> RotatedIntrinsics(
            // 270° CW: (x,y) -> (y, w-1-x). Swap fx/fy, output (h, w).
            fx = fy, fy = fx,
            cx = cy,
            cy = (w - 1).toDouble() - cx,
            width = h, height = w,
        )
        else -> RotatedIntrinsics(fx, fy, cx, cy, w, h)
    }
}
