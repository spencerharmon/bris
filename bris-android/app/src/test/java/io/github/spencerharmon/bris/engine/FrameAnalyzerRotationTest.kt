package io.github.spencerharmon.bris.engine

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Pure-JVM tests for the Y-plane rotation helpers in
 * [FrameAnalyzer]. CameraX hands the analyzer the raw sensor
 * orientation; these helpers turn that into a gravity-up
 * buffer before the engine sees it.
 *
 * Image layout: row-major, byte-per-pixel. A 3x2 source
 *   1 2 3
 *   4 5 6
 * rotated 90\u00b0 CW becomes (2x3)
 *   4 1
 *   5 2
 *   6 3
 * rotated 180\u00b0 becomes (3x2)
 *   6 5 4
 *   3 2 1
 * rotated 270\u00b0 CW becomes (2x3)
 *   3 6
 *   2 5
 *   1 4
 */
class FrameAnalyzerRotationTest {

    @Test
    fun rotate90_3x2() {
        val src = byteArrayOf(1, 2, 3, 4, 5, 6)
        val dst = rotate90(src, w = 3, h = 2)
        assertEquals(6, dst.size)
        assertArrayEquals(byteArrayOf(4, 1, 5, 2, 6, 3), dst)
    }

    @Test
    fun rotate180_3x2() {
        val src = byteArrayOf(1, 2, 3, 4, 5, 6)
        val dst = rotate180(src, w = 3, h = 2)
        assertArrayEquals(byteArrayOf(6, 5, 4, 3, 2, 1), dst)
    }

    @Test
    fun rotate270_3x2() {
        val src = byteArrayOf(1, 2, 3, 4, 5, 6)
        val dst = rotate270(src, w = 3, h = 2)
        assertArrayEquals(byteArrayOf(3, 6, 2, 5, 1, 4), dst)
    }

    @Test
    fun rotate90_then_270_is_identity() {
        val src = ByteArray(12) { it.toByte() }
        val r90 = rotate90(src, w = 4, h = 3) // 3x4
        val back = rotate270(r90, w = 3, h = 4) // 4x3
        assertArrayEquals(src, back)
    }

    @Test
    fun rotate180_is_self_inverse() {
        val src = ByteArray(15) { (it + 7).toByte() }
        val once = rotate180(src, w = 5, h = 3)
        val twice = rotate180(once, w = 5, h = 3)
        assertArrayEquals(src, twice)
    }
}
