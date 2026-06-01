package io.github.spencerharmon.bris.engine

import android.view.Surface
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class DeviceOrientationSourceTest {

    @Test
    fun face_up_returns_null() {
        // gz dominant, +9.81 ≈ face-up; gx/gy small.
        val r = DeviceOrientationSource.orientationFromGravity(0.1f, 0.1f, 9.81f)
        assertNull(r)
    }

    @Test
    fun face_down_returns_null() {
        val r = DeviceOrientationSource.orientationFromGravity(-0.1f, 0.1f, -9.81f)
        assertNull(r)
    }

    @Test
    fun portrait_top_up_yields_rotation_0() {
        // Portrait held with top of phone up: gravity points
        // down the -Y device axis.
        val r = DeviceOrientationSource.orientationFromGravity(0f, -9.81f, 0f)
        assertEquals(Surface.ROTATION_0, r)
    }

    @Test
    fun landscape_usbc_right_yields_rotation_90() {
        // Landscape with USB-C port on the right: gravity
        // points along -X.
        val r = DeviceOrientationSource.orientationFromGravity(-9.81f, 0f, 0f)
        assertEquals(Surface.ROTATION_90, r)
    }

    @Test
    fun portrait_upside_down_yields_rotation_180() {
        val r = DeviceOrientationSource.orientationFromGravity(0f, 9.81f, 0f)
        assertEquals(Surface.ROTATION_180, r)
    }

    @Test
    fun landscape_usbc_left_yields_rotation_270() {
        val r = DeviceOrientationSource.orientationFromGravity(9.81f, 0f, 0f)
        assertEquals(Surface.ROTATION_270, r)
    }

    @Test
    fun mild_tilt_does_not_change_classification() {
        // Slightly tilted portrait: should still be ROTATION_0.
        val r = DeviceOrientationSource.orientationFromGravity(2f, -9.5f, 0.5f)
        assertEquals(Surface.ROTATION_0, r)
    }

    @Test
    fun hysteresis_proposed_equals_current_is_no_op() {
        var pending = Surface.ROTATION_0
        var since = 0L
        val r = DeviceOrientationSource.hysteresis(
            current = Surface.ROTATION_0,
            proposed = Surface.ROTATION_0,
            pending = Surface.ROTATION_0,
            pendingSinceMs = 0L,
            nowMs = 1_000L,
            debounceMs = 250L,
        ) { p, t -> pending = p; since = t }
        assertEquals(Surface.ROTATION_0, r)
    }

    @Test
    fun hysteresis_new_proposal_starts_dwell() {
        var pending = -1
        var since = -1L
        val r = DeviceOrientationSource.hysteresis(
            current = Surface.ROTATION_0,
            proposed = Surface.ROTATION_90,
            pending = Surface.ROTATION_0, // was unset
            pendingSinceMs = 0L,
            nowMs = 1_000L,
            debounceMs = 250L,
        ) { p, t -> pending = p; since = t }
        assertEquals(Surface.ROTATION_0, r) // not yet committed
        assertEquals(Surface.ROTATION_90, pending)
        assertEquals(1_000L, since)
    }

    @Test
    fun hysteresis_commit_after_dwell() {
        var pending = -1
        var since = -1L
        val r = DeviceOrientationSource.hysteresis(
            current = Surface.ROTATION_0,
            proposed = Surface.ROTATION_90,
            pending = Surface.ROTATION_90,
            pendingSinceMs = 1_000L,
            nowMs = 1_300L,
            debounceMs = 250L,
        ) { p, t -> pending = p; since = t }
        assertEquals(Surface.ROTATION_90, r)
    }

    @Test
    fun hysteresis_short_dwell_doesnt_commit() {
        val r = DeviceOrientationSource.hysteresis(
            current = Surface.ROTATION_0,
            proposed = Surface.ROTATION_90,
            pending = Surface.ROTATION_90,
            pendingSinceMs = 1_000L,
            nowMs = 1_100L,
            debounceMs = 250L,
        ) { _, _ -> }
        assertEquals(Surface.ROTATION_0, r)
    }

    @Test
    fun hysteresis_different_proposal_resets_timer() {
        var pending = -1
        var since = -1L
        val r = DeviceOrientationSource.hysteresis(
            current = Surface.ROTATION_0,
            proposed = Surface.ROTATION_180,
            pending = Surface.ROTATION_90,
            pendingSinceMs = 1_000L,
            nowMs = 1_100L,
            debounceMs = 250L,
        ) { p, t -> pending = p; since = t }
        assertEquals(Surface.ROTATION_0, r)
        assertEquals(Surface.ROTATION_180, pending)
        assertEquals(1_100L, since)
    }
}
