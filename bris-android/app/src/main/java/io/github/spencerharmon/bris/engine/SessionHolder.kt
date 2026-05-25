package io.github.spencerharmon.bris.engine

import android.content.Context
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import uniffi.bris_ffi.FfiEngineConfig

/**
 * Process-lifetime singleton owning the [`EngineWrapper`].
 *
 * Hoisting the engine out of [`io.github.spencerharmon.bris.ui.LiveScreen`]
 * is necessary so the [`io.github.spencerharmon.bris.ui.SightLogScreen`]
 * can call `recent_sights()` / `last_persisted_fix()` without
 * constructing a second engine instance (which would race on
 * the on-disk store).
 *
 * The engine is lazy-initialised on first [`acquire`] with the
 * supplied config factory; subsequent calls return the same
 * instance regardless of config. A future commit could expose
 * a `reconfigure` that drops and rebuilds the engine when the
 * operator changes observer settings; the spike doesn't yet.
 */
object SessionHolder {
    private var engine: EngineWrapper? = null
    private val scope = CoroutineScope(SupervisorJob())

    @Synchronized
    fun acquire(
        @Suppress("UNUSED_PARAMETER") context: Context,
        configFactory: () -> FfiEngineConfig,
        pbrisSink: ((String) -> Unit)? = null,
    ): EngineWrapper {
        val existing = engine
        if (existing != null) return existing
        val fresh = EngineWrapper.create(
            config = configFactory(),
            scope = scope,
            pbrisSink = pbrisSink,
        )
        engine = fresh
        return fresh
    }

    /** Currently-acquired engine, or null before the first acquire. */
    fun peek(): EngineWrapper? = engine
}
