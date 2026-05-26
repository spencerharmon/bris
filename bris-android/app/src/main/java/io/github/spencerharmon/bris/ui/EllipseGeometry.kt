package io.github.spencerharmon.bris.ui

import kotlin.math.ceil
import kotlin.math.cos
import kotlin.math.log10
import kotlin.math.max
import kotlin.math.pow
import kotlin.math.sin

/**
 * Pure-math helpers for the confidence-ellipse HUD overlay.
 *
 * Kept separate from the Compose Canvas drawing so the maths can
 * be unit-tested under JVM Robolectric-free.
 *
 * Coordinate model:
 *  - Output coordinates are pixel offsets from the centre of a
 *    square canvas, with `+x = east`, `+y = north`. The caller
 *    flips `y` when handing to Compose's screen-space draw API
 *    (which has `+y = down`).
 *  - The covariance `orientation_rad` (per `FfiPublishedFix`) is
 *    radians clockwise from north of the semi-major axis. Adopt
 *    that directly when rotating.
 *  - Scale: 1 nm in the world maps to `pxPerNm` pixels.
 */
object EllipseGeometry {

    /**
     * Pick the displayed scale (in nautical miles) for the ellipse
     * frame. Tiers 1 / 10 / 100 / 1000 nm: the smallest power of
     * ten ≥ `sigma_major_nm * 1.2` (with the 20 % slack chosen so
     * the ellipse outline doesn't kiss the frame at the picked
     * tier). Capped at 1000 nm — anything larger is suppressed by
     * [`isDrawable`] anyway.
     */
    fun pickScaleNm(sigmaMajorNm: Double): Double {
        if (!sigmaMajorNm.isFinite() || sigmaMajorNm <= 0.0) return 1.0
        val target = sigmaMajorNm * SCALE_SLACK
        val exp = ceil(log10(target)).toInt().coerceAtLeast(0)
        val picked = 10.0.pow(exp)
        return picked.coerceAtMost(MAX_SCALE_NM)
    }

    /**
     * Pixels-per-nm given a square canvas of [canvasPx] pixels on a
     * side. The fix is at the centre and we reserve `MARGIN_FRAC`
     * for the scale label, so the ellipse fits if
     * `sigma_major_nm < scale_nm`.
     */
    fun pixelsPerNm(canvasPx: Float, scaleNm: Double): Float =
        (canvasPx * (1f - 2f * MARGIN_FRAC) / 2f) / scaleNm.toFloat()

    /**
     * Sample N points on the boundary of the 1σ ellipse, in
     * `(eastPx, northPx)` offsets from the centre. Caller maps
     * north → screen-up by negating the y component.
     */
    fun ellipsePoints(
        sigmaMajorNm: Double,
        sigmaMinorNm: Double,
        orientationRad: Double,
        pxPerNm: Float,
        nSamples: Int = 64,
    ): List<Pair<Float, Float>> {
        val a = sigmaMajorNm.toFloat() * pxPerNm
        val b = sigmaMinorNm.toFloat() * pxPerNm
        // orientation_rad = angle of the semi-major axis clockwise
        // from north. East coord = a*sin(θ)*sinφ + b*cos(θ)*cosφ
        // is fiddly to read; build it from a parametric ellipse in
        // (major, minor) frame and rotate.
        val cosO = cos(orientationRad).toFloat()
        val sinO = sin(orientationRad).toFloat()
        val pts = ArrayList<Pair<Float, Float>>(nSamples)
        for (i in 0 until nSamples) {
            val t = (2.0 * Math.PI * i / nSamples).toFloat()
            val xMajor = a * cos(t)
            val yMinor = b * sin(t)
            // Major-axis frame: +major is along bearing
            // orientation_rad clockwise-from-north. Rotate
            // (xMajor along major, yMinor perpendicular) into
            // (east, north).
            val east = xMajor * sinO + yMinor * cosO
            val north = xMajor * cosO - yMinor * sinO
            pts.add(east to north)
        }
        return pts
    }

    /**
     * Project a sight's bearing-from-north (radians clockwise from
     * north) into the two endpoints of a line-of-position segment
     * crossing the canvas through the centre.
     *
     * Returns endpoints in (eastPx, northPx) offsets from centre.
     * The line is drawn long enough to span the diagonal of the
     * square canvas; the caller clips with the canvas bounds.
     */
    fun lopEndpoints(
        azimuthRad: Double,
        canvasPx: Float,
    ): Pair<Pair<Float, Float>, Pair<Float, Float>> {
        // LOP is perpendicular to the body's azimuth (great-circle
        // tangent in the small neighbourhood of the fix). Bearing
        // perpendicular to azimuth is azimuth + 90°.
        val perp = azimuthRad + Math.PI / 2.0
        val east = sin(perp).toFloat()
        val north = cos(perp).toFloat()
        val r = canvasPx // generous; caller clips
        return (-east * r to -north * r) to (east * r to north * r)
    }

    private const val MARGIN_FRAC = 0.12f
    private const val SCALE_SLACK = 1.2
    private const val MAX_SCALE_NM = 1000.0

    /**
     * If `sigma_minor > sigma_major`, swap them and add π/2 to
     * the orientation so the semi-major axis remains the longer
     * one. The covariance interpretation is preserved.
     */
    fun canonicalize(
        sigmaMajorNm: Double,
        sigmaMinorNm: Double,
        orientationRad: Double,
    ): Triple<Double, Double, Double> =
        if (sigmaMinorNm > sigmaMajorNm) {
            Triple(sigmaMinorNm, sigmaMajorNm, orientationRad + Math.PI / 2.0)
        } else {
            Triple(sigmaMajorNm, sigmaMinorNm, orientationRad)
        }

    /** True when the covariance is non-pathological enough to draw. */
    fun isDrawable(sigmaMajorNm: Double, sigmaMinorNm: Double): Boolean =
        sigmaMajorNm.isFinite() && sigmaMinorNm.isFinite() &&
            sigmaMajorNm >= 0.0 && sigmaMinorNm >= 0.0 &&
            // Pure-zero ellipses degenerate to a point; still
            // drawable as the central dot, so keep the gate
            // pure-finite + nonnegative.
            max(sigmaMajorNm, sigmaMinorNm) < MAX_DRAWABLE_NM

    /**
     * Cap at 1000 nm; beyond this the fix is meaningless and the
     * overlay should show nothing rather than a giant frame.
     */
    private const val MAX_DRAWABLE_NM = 1000.0
}
