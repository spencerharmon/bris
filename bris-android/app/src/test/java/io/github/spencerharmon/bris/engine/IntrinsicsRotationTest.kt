package io.github.spencerharmon.bris.engine

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Pure-JVM tests for [rotateIntrinsicsForFrameRotation], the
 * shared helper used by both [FrameAnalyzer.toFfiFrame] and
 * [DebugBundleWriter] to keep camera intrinsics consistent
 * with whatever rotation was applied to the pixel buffer.
 *
 * Mirrors the rotation math already encoded inline in
 * `FrameAnalyzer.toFfiFrame`. Tests live here so the math is
 * exercised in isolation: a regression in the helper that
 * doesn't change `FfiFrame`-construction observable behavior
 * still trips a unit test.
 *
 * The (fx, fy, cx, cy, w, h) input is the sensor-native
 * calibration; the rotation matches CameraX's
 * `image.imageInfo.rotationDegrees`.
 */
class IntrinsicsRotationTest {

    private val W = 4032
    private val H = 3024
    private val FX = 3100.0
    private val FY = 3090.0
    private val CX = 2014.0
    private val CY = 1491.0

    @Test
    fun rotation_0_is_identity() {
        val r = rotateIntrinsicsForFrameRotation(FX, FY, CX, CY, W, H, 0)
        assertEquals(W, r.width)
        assertEquals(H, r.height)
        assertEquals(FX, r.fx, 0.0)
        assertEquals(FY, r.fy, 0.0)
        assertEquals(CX, r.cx, 0.0)
        assertEquals(CY, r.cy, 0.0)
    }

    @Test
    fun rotation_180_flips_principal_point() {
        val r = rotateIntrinsicsForFrameRotation(FX, FY, CX, CY, W, H, 180)
        assertEquals(W, r.width)
        assertEquals(H, r.height)
        assertEquals(FX, r.fx, 0.0)
        assertEquals(FY, r.fy, 0.0)
        assertEquals((W - 1).toDouble() - CX, r.cx, 0.0)
        assertEquals((H - 1).toDouble() - CY, r.cy, 0.0)
    }

    @Test
    fun rotation_90_swaps_axes_and_remaps_principal_point() {
        // 90° CW: (x,y) -> (h-1-y, x). Swap fx/fy, output is (h, w).
        val r = rotateIntrinsicsForFrameRotation(FX, FY, CX, CY, W, H, 90)
        assertEquals(H, r.width)
        assertEquals(W, r.height)
        assertEquals(FY, r.fx, 0.0)
        assertEquals(FX, r.fy, 0.0)
        assertEquals((H - 1).toDouble() - CY, r.cx, 0.0)
        assertEquals(CX, r.cy, 0.0)
    }

    @Test
    fun rotation_270_swaps_axes_and_remaps_principal_point() {
        // 270° CW: (x,y) -> (y, w-1-x). Swap fx/fy, output is (h, w).
        val r = rotateIntrinsicsForFrameRotation(FX, FY, CX, CY, W, H, 270)
        assertEquals(H, r.width)
        assertEquals(W, r.height)
        assertEquals(FY, r.fx, 0.0)
        assertEquals(FX, r.fy, 0.0)
        assertEquals(CY, r.cx, 0.0)
        assertEquals((W - 1).toDouble() - CX, r.cy, 0.0)
    }

    @Test
    fun rotation_normalized_modulo_360() {
        // -90 == 270, 450 == 90, 360 == 0.
        val a = rotateIntrinsicsForFrameRotation(FX, FY, CX, CY, W, H, -90)
        val b = rotateIntrinsicsForFrameRotation(FX, FY, CX, CY, W, H, 270)
        assertEquals(b.width, a.width)
        assertEquals(b.cx, a.cx, 0.0)
        assertEquals(b.cy, a.cy, 0.0)
        val z = rotateIntrinsicsForFrameRotation(FX, FY, CX, CY, W, H, 360)
        assertEquals(W, z.width)
        assertEquals(CX, z.cx, 0.0)
    }
}
