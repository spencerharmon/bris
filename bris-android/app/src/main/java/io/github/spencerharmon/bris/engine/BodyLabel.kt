package io.github.spencerharmon.bris.engine

import uniffi.bris_ffi.FfiSight

/**
 * Resolve a short human-readable label for an [`FfiSight`]'s body.
 *
 * Mirrors the encoding in `bris_streaming::store` (`solar_to_u32` /
 * `planet_to_u32`):
 *
 *  - `body_kind == 0` (SolarSystem):
 *      0 → "Sun"; 1 → "Moon"; 100 + n → planet n.
 *  - `body_kind == 1` (Star): payload is the Yale BSC HR number;
 *      label is "HR <n>" (operator-recognizable, language-neutral).
 *      A future commit can join against the embedded catalog to
 *      surface conventional names like "Sirius"; right now the
 *      catalog isn't reachable across the FFI without a new
 *      exported function.
 */
object BodyLabel {
    private val PLANETS = arrayOf(
        "Mercury", "Venus", "EMB", "Mars", "Jupiter", "Saturn", "Uranus", "Neptune",
    )

    fun forSight(sight: FfiSight): String = when (sight.bodyKind.toInt()) {
        0 -> when (val p = sight.bodyPayload.toInt()) {
            0 -> "Sun"
            1 -> "Moon"
            else -> if (p in 100..(100 + PLANETS.size - 1)) PLANETS[p - 100] else "Body($p)"
        }
        1 -> "HR ${sight.bodyPayload}"
        else -> "Body?"
    }
}
