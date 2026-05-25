package io.github.spencerharmon.bris.ui

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.drawText
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.geometry.Offset
import io.github.spencerharmon.bris.engine.BodyLabel
import uniffi.bris_ffi.FfiPublishedFix
import uniffi.bris_ffi.FfiSight

/**
 * Top-right confidence-ellipse overlay.
 *
 * Renders, in compass-rose-aligned coordinates (north = up,
 * east = right):
 *
 *  - A square frame with a corner tick mark and a "1 nm" or
 *    "10 nm" scale label across the bottom.
 *  - The fix's 1σ covariance ellipse, semi-axes scaled by
 *    `sigma_major_nm` / `sigma_minor_nm` and rotated by
 *    `orientation_rad`.
 *  - Faint lines-of-position through the centre for each
 *    contributing sight (gives the cocked-hat impression).
 *  - A filled dot at the centre marking the fix point.
 *
 * Sized for a HUD chip (default 120 dp square). When [fix] is
 * null the overlay collapses to nothing.
 *
 * Lives outside the main HUD column so it can sit
 * top-aligned-right without disturbing the rest of the layout.
 */
@Composable
fun ConfidenceEllipseOverlay(
    fix: FfiPublishedFix?,
    sights: List<FfiSight>,
    modifier: Modifier = Modifier,
) {
    if (fix == null) return
    if (!EllipseGeometry.isDrawable(fix.sigmaMajorNm, fix.sigmaMinorNm)) return

    val scaleNm = EllipseGeometry.pickScaleNm(fix.sigmaMajorNm)
    val measurer = rememberTextMeasurer()

    Box(
        modifier = modifier
            .size(ELLIPSE_OVERLAY_DP.dp)
            .background(Color(0x80000000)),
    ) {
        Canvas(modifier = Modifier.size(ELLIPSE_OVERLAY_DP.dp).padding(4.dp)) {
            val side = kotlin.math.min(size.width, size.height)
            val cx = size.width / 2f
            val cy = size.height / 2f
            val pxPerNm = EllipseGeometry.pixelsPerNm(side, scaleNm)

            // Frame box.
            drawRect(
                color = Color(0xFF606060),
                topLeft = Offset(0f, 0f),
                size = androidx.compose.ui.geometry.Size(size.width, size.height),
                style = Stroke(width = 1f),
            )

            // North arrow at the top.
            val nLabel = measurer.measure(
                "N",
                style = TextStyle(color = Color.White, fontSize = 9.sp),
            )
            drawText(nLabel, topLeft = Offset(cx - nLabel.size.width / 2f, 2f))

            // Lines-of-position for contributing sights. Drawn
            // *before* the ellipse so the ellipse outline stays
            // on top.
            for (sight in sights) {
                val (a, b) = EllipseGeometry.lopEndpoints(sight.azimuthRad, side)
                drawLine(
                    color = Color(0x6633A5FF),
                    start = Offset(cx + a.first, cy - a.second),
                    end = Offset(cx + b.first, cy - b.second),
                    strokeWidth = 1f,
                )
            }

            // Ellipse polyline.
            val pts = EllipseGeometry.ellipsePoints(
                fix.sigmaMajorNm,
                fix.sigmaMinorNm,
                fix.orientationRad,
                pxPerNm,
            )
            for (i in pts.indices) {
                val (e0, n0) = pts[i]
                val (e1, n1) = pts[(i + 1) % pts.size]
                drawLine(
                    color = Color(0xFF35D673),
                    start = Offset(cx + e0, cy - n0),
                    end = Offset(cx + e1, cy - n1),
                    strokeWidth = 2f,
                )
            }

            // Fix point.
            drawCircle(
                color = Color.White,
                radius = 3f,
                center = Offset(cx, cy),
            )

            // Scale label bottom-centre, e.g. "1 nm".
            val scaleText = if (scaleNm >= 10.0) "10 nm" else "1 nm"
            val sl = measurer.measure(
                scaleText,
                style = TextStyle(color = Color.White, fontSize = 9.sp),
            )
            drawText(
                sl,
                topLeft = Offset(
                    cx - sl.size.width / 2f,
                    size.height - sl.size.height - 2f,
                ),
            )
        }
    }
}

private const val ELLIPSE_OVERLAY_DP = 120

/**
 * One-line pool summary chip: `"Pool: 7 sights (Moon: 3, HR 2491: 2, …)"`.
 *
 * Shows the top few bodies by sight count; remainder collapsed
 * to "+N more". Empty pool reads `"Pool: empty"`.
 */
@Composable
fun PoolSummaryChip(sights: List<FfiSight>, modifier: Modifier = Modifier) {
    val text = formatPoolSummary(sights)
    androidx.compose.material3.Text(
        text = text,
        color = Color.White,
        modifier = modifier,
    )
}

internal fun formatPoolSummary(sights: List<FfiSight>): String {
    if (sights.isEmpty()) return "Pool: empty"
    val byBody = sights.groupBy { BodyLabel.forSight(it) }
        .mapValues { it.value.size }
        .entries
        .sortedByDescending { it.value }
    val shown = byBody.take(POOL_SUMMARY_MAX_BODIES)
        .joinToString(", ") { "${it.key}: ${it.value}" }
    val extra = byBody.size - POOL_SUMMARY_MAX_BODIES
    val tail = if (extra > 0) ", +$extra more" else ""
    return "Pool: ${sights.size} sights ($shown$tail)"
}

private const val POOL_SUMMARY_MAX_BODIES = 4

/**
 * Transient "recovered from previous session" banner. The host
 * controls visibility via a `visible` flag (typically driven by
 * a `LaunchedEffect` that flips it false after ~10 s).
 */
@Composable
fun RecoveredFixBanner(visible: Boolean, fix: FfiPublishedFix?) {
    if (!visible || fix == null) return
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .background(Color(0xCC1565C0))
            .padding(8.dp),
    ) {
        Column {
            androidx.compose.material3.Text(
                "Recovered fix from previous session",
                color = Color.White,
            )
            androidx.compose.material3.Text(
                "lat=${"%.4f".format(fix.latitudeDeg)}°  " +
                    "lon=${"%.4f".format(fix.longitudeDeg)}°  " +
                    "σ=${"%.2f".format(fix.sigmaMajorNm)} nm",
                color = Color.White,
                fontSize = 12.sp,
            )
        }
    }
}

/**
 * Provenance badge for a fix (Saint-Hilaire vs cold-start). The
 * provenance field is not yet on the FFI [`FfiPublishedFix`];
 * this function exists so the call site is already wired and a
 * future commit only has to fill in the string lookup.
 *
 * TODO(provenance): once the cold-start fallback PR exposes a
 * `provenance` string on `FfiPublishedFix`, switch on it here.
 */
@Composable
fun ProvenanceBadge(fix: FfiPublishedFix?) {
    // No-op placeholder. Once the cold-start fallback PR adds a
    // `provenance` discriminator on FfiPublishedFix, this is
    // where we render the "Saint-Hilaire" / "Cold start" /
    // "Cold start (ambiguous)" badge.
    @Suppress("UNUSED_PARAMETER") fix
}
