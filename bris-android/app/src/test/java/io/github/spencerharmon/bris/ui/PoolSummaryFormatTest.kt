package io.github.spencerharmon.bris.ui

import org.junit.Assert.assertEquals
import org.junit.Test
import uniffi.bris_ffi.FfiSight

class PoolSummaryFormatTest {

    private fun star(hr: UInt, az: Double = 0.0): FfiSight = FfiSight(
        bodyKind = 1u,
        bodyPayload = hr,
        azimuthRad = az,
        altitudeSigmaRad = 1e-5,
        interceptNm = 0.0,
        interceptSigmaNm = 0.1,
        anchorTtJd = 2_460_000.0,
        sourceFrameId = 0u,
    )

    private fun moon(): FfiSight = star(0u).copy(bodyKind = 0u, bodyPayload = 1u)

    @Test
    fun empty_pool_summary() {
        assertEquals("Pool: empty", formatPoolSummary(emptyList()))
    }

    @Test
    fun groups_by_body_and_sorts_by_count() {
        val sights = listOf(
            moon(), moon(), moon(),
            star(2491u), star(2491u),
            star(7001u), star(7001u),
        )
        // 7 sights, Moon: 3, HR 2491: 2, HR 7001: 2.
        val out = formatPoolSummary(sights)
        assertEquals("Pool: 7 sights (Moon: 3, HR 2491: 2, HR 7001: 2)", out)
    }

    @Test
    fun caps_displayed_bodies_with_more_suffix() {
        val sights = (1..6).flatMap { listOf(star(it.toUInt())) }
        val out = formatPoolSummary(sights)
        // 4 bodies shown, +2 more.
        assertEquals(true, out.contains("+2 more"))
    }
}
