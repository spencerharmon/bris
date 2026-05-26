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
    fun pickScaleNm_tiers_1_10_100_1000() {
        assertEquals(1.0, EllipseGeometry.pickScaleNm(0.5), 0.0)
        assertEquals(10.0, EllipseGeometry.pickScaleNm(5.0), 0.0)
        assertEquals(100.0, EllipseGeometry.pickScaleNm(50.0), 0.0)
        assertEquals(1000.0, EllipseGeometry.pickScaleNm(500.0), 0.0)
        // Capped at 1000 nm even if the operator hands us a huge
        // value; isDrawable will suppress drawing above MAX_DRAWABLE_NM.
        assertEquals(1000.0, EllipseGeometry.pickScaleNm(5000.0), 0.0)
    }

    @Test
    fun canonicalize_swapsAxesAndRotates90() {
        val (major, minor, orient) =
            EllipseGeometry.canonicalize(0.5, 5.0, 0.25)
        assertEquals(5.0, major, 0.0)
        assertEquals(0.5, minor, 0.0)
        assertEquals(0.25 + Math.PI / 2.0, orient, 1e-12)
    }

    @Test
    fun canonicalize_leavesWellOrderedInputsAlone() {
        val (major, minor, orient) =
            EllipseGeometry.canonicalize(5.0, 0.5, 0.3)
        assertEquals(5.0, major, 0.0)
        assertEquals(0.5, minor, 0.0)
        assertEquals(0.3, orient, 0.0)
    }

    @Test
    fun canonicalize_swappedEllipseMatchesOriginal() {
        // Drawing the swapped (major, minor, orient + π/2) must
        // produce the same point set as drawing (minor, major,
        // orient) directly, modulo ordering.
        val direct = EllipseGeometry.ellipsePoints(
            sigmaMajorNm = 5.0,
            sigmaMinorNm = 0.5,
            orientationRad = 0.0,
            pxPerNm = 10f,
        )
        val (m, n, o) = EllipseGeometry.canonicalize(0.5, 5.0, 0.0)
        val swapped = EllipseGeometry.ellipsePoints(
            sigmaMajorNm = m,
            sigmaMinorNm = n,
            orientationRad = o,
            pxPerNm = 10f,
        )
        val directExtent = direct.maxOf { kotlin.math.hypot(it.first, it.second) }
        val swappedExtent = swapped.maxOf { kotlin.math.hypot(it.first, it.second) }
        assertEquals(directExtent.toDouble(), swappedExtent.toDouble(), 1e-3)
        val directMaxN = direct.maxOf { kotlin.math.abs(it.second) }
        val swappedMaxN = swapped.maxOf { kotlin.math.abs(it.second) }
        assertEquals(directMaxN.toDouble(), swappedMaxN.toDouble(), 1e-3)
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
