package io.github.spencerharmon.bris.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.math.hypot

/**
 * Pure-JVM tests for [EllipseGeometry]. The Compose Canvas
 * drawing path is not exercised here — verifying the maths is
 * sufficient because the drawing path is a straight-line
 * mapping from these outputs into [androidx.compose.foundation.Canvas]
 * calls, with no per-frame branching.
 */
class EllipseGeometryTest {

    @Test
    fun pickScaleNm_smallSigma_uses1nm() {
        assertEquals(1.0, EllipseGeometry.pickScaleNm(0.1), 0.0)
        assertEquals(1.0, EllipseGeometry.pickScaleNm(0.9), 0.0)
    }

    @Test
    fun pickScaleNm_largeSigma_uses10nm() {
        assertEquals(10.0, EllipseGeometry.pickScaleNm(5.0), 0.0)
        assertEquals(10.0, EllipseGeometry.pickScaleNm(100.0), 0.0)
    }

    @Test
    fun ellipsePoints_circularCovariance_isCircle() {
        val pts = EllipseGeometry.ellipsePoints(
            sigmaMajorNm = 0.5,
            sigmaMinorNm = 0.5,
            orientationRad = 0.0,
            pxPerNm = 100f,
            nSamples = 32,
        )
        val expectedR = 50f
        for ((e, n) in pts) {
            assertEquals(expectedR.toDouble(), hypot(e, n).toDouble(), 1e-3)
        }
    }

    @Test
    fun ellipsePoints_elongatedAlignedNorth_hasMajorAxisOnY() {
        // orientation = 0 → semi-major along north (+y).
        val pts = EllipseGeometry.ellipsePoints(
            sigmaMajorNm = 5.0,
            sigmaMinorNm = 0.5,
            orientationRad = 0.0,
            pxPerNm = 10f,
        )
        // Max |north| ≈ 50, max |east| ≈ 5.
        val maxN = pts.maxOf { kotlin.math.abs(it.second) }
        val maxE = pts.maxOf { kotlin.math.abs(it.first) }
        assertEquals(50.0, maxN.toDouble(), 0.5)
        assertEquals(5.0, maxE.toDouble(), 0.5)
    }

    @Test
    fun ellipsePoints_rotated90deg_swapsAxes() {
        val pts = EllipseGeometry.ellipsePoints(
            sigmaMajorNm = 5.0,
            sigmaMinorNm = 0.5,
            orientationRad = Math.PI / 2.0,
            pxPerNm = 10f,
        )
        // Now semi-major along east; max |east| ≈ 50, max |north| ≈ 5.
        val maxE = pts.maxOf { kotlin.math.abs(it.first) }
        val maxN = pts.maxOf { kotlin.math.abs(it.second) }
        assertEquals(50.0, maxE.toDouble(), 0.5)
        assertEquals(5.0, maxN.toDouble(), 0.5)
    }

    @Test
    fun isDrawable_rejectsNaNAndHugeValues() {
        assertFalse(EllipseGeometry.isDrawable(Double.NaN, 1.0))
        assertFalse(EllipseGeometry.isDrawable(1.0, Double.POSITIVE_INFINITY))
        assertFalse(EllipseGeometry.isDrawable(-1.0, 1.0))
        assertFalse(EllipseGeometry.isDrawable(10_000.0, 1.0))
        assertTrue(EllipseGeometry.isDrawable(0.0, 0.0))
        assertTrue(EllipseGeometry.isDrawable(0.5, 0.3))
    }

    @Test
    fun lopEndpoints_returnsLineCenteredOnOrigin() {
        val (a, b) = EllipseGeometry.lopEndpoints(0.0, 100f)
        // azimuth = 0 (body to north) → perpendicular line is
        // east-west; endpoints have north ≈ 0.
        assertEquals(0.0, a.second.toDouble(), 1e-3)
        assertEquals(0.0, b.second.toDouble(), 1e-3)
        // And symmetric about origin.
        assertEquals(-a.first, b.first, 1e-3f)
        assertNotNull(a)
    }
}
