package io.github.spencerharmon.bris.ui

import androidx.compose.ui.test.junit4.createComposeRule
import org.junit.Rule
import org.junit.Test
import uniffi.bris_ffi.FfiPublishedFix

/**
 * Compose smoke tests for the confidence-ellipse HUD.
 *
 * No semantic assertions — the geometry is already covered by
 * the pure-JVM [`EllipseGeometryTest`]. These tests exist only
 * to confirm that the Compose `Canvas` drawing path does not
 * crash under representative fix inputs (small σ, cold-start
 * sized σ, pathologically elongated σ).
 */
class ConfidenceEllipseOverlayTest {

    @get:Rule
    val composeTestRule = createComposeRule()

    @Test
    fun ellipse_renders_without_crash_small_sigma() {
        composeTestRule.setContent {
            ConfidenceEllipseOverlay(
                fix = cannedFix(sigmaMajorNm = 0.3, sigmaMinorNm = 0.2),
                sights = emptyList(),
            )
        }
        composeTestRule.waitForIdle()
    }

    @Test
    fun ellipse_renders_without_crash_large_sigma_cold_start() {
        composeTestRule.setContent {
            ConfidenceEllipseOverlay(
                fix = cannedFix(sigmaMajorNm = 80.0, sigmaMinorNm = 60.0),
                sights = emptyList(),
                recovered = true,
            )
        }
        composeTestRule.waitForIdle()
    }

    @Test
    fun ellipse_renders_without_crash_very_elongated() {
        composeTestRule.setContent {
            ConfidenceEllipseOverlay(
                fix = cannedFix(sigmaMajorNm = 5.0, sigmaMinorNm = 0.01),
                sights = emptyList(),
            )
        }
        composeTestRule.waitForIdle()
    }
}

private fun cannedFix(sigmaMajorNm: Double, sigmaMinorNm: Double): FfiPublishedFix =
    FfiPublishedFix(
        latitudeDeg = 37.5,
        longitudeDeg = -122.3,
        sigmaMajorNm = sigmaMajorNm,
        sigmaMinorNm = sigmaMinorNm,
        orientationRad = 0.5,
        nSights = 3u,
        azimuthSpreadRad = 1.2,
        oldestSightAgeSeconds = 45.0,
        dominantSource = "centroid",
        timestampTtJd = 2_460_700.5,
        contributingFrameIds = emptyList(),
        provenance = "saint_hilaire",
    )
