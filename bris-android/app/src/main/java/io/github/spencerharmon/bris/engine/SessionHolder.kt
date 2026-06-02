package io.github.spencerharmon.bris.engine

import android.content.Context
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import uniffi.bris_ffi.FfiEngineConfig
import java.io.File
import java.util.UUID

/**
 * Process-lifetime owner of the active [`EngineWrapper`].
 *
 * **Keyed on active-session UUID.** When the operator switches
 * active sessions, the held engine is dropped and a new one
 * built against the new session's `engine-store/` path. This
 * gives per-session isolation of the on-disk
 * `bris_streaming::SightStore`: sights from session A never
 * hydrate into session B's pool on restart, because the
 * append-log files are separate.
 *
 * The Rust crate stays session-blind; session-awareness lives
 * purely in which `store_data_root` we pass via
 * [`FfiEngineConfig`].
 *
 * No-active-session fallback (orphan): the engine opens
 * against `<external-files>/sessions/orphan/engine-store/`.
 */
object SessionHolder {
    private var engine: EngineWrapper? = null
    private var currentSessionId: UUID? = null
    private var currentIsOrphan: Boolean = false
    private val scope = CoroutineScope(SupervisorJob())

    /**
     * Return the engine for [activeSessionId], constructing
     * (or rebuilding) it if the held instance is for a
     * different session. Pass `null` for the orphan path.
     *
     * [configFactory] is invoked at construction time with the
     * resolved `store_data_root` already applied; the factory
     * sees the augmented config and may layer further changes
     * on top (cold-start hemisphere, session overlays, etc.).
     */
    @Synchronized
    fun acquire(
        context: Context,
        activeSessionId: UUID?,
        configFactory: (storeDataRoot: String) -> FfiEngineConfig,
        pbrisSink: ((String) -> Unit)? = null,
    ): EngineWrapper {
        val existing = engine
        val wantsOrphan = activeSessionId == null
        val matches = existing != null &&
            currentIsOrphan == wantsOrphan &&
            currentSessionId == activeSessionId
        if (matches) return existing!!

        // Tear down the previous engine. EngineWrapper holds an
        // Arc<Engine>; dropping our reference releases it once
        // any in-flight FFI calls complete.
        engine = null

        val storeRoot = engineStoreDirFor(context, activeSessionId).also { it.mkdirs() }
        val cfg = configFactory(storeRoot.absolutePath)
        val fresh = EngineWrapper.create(config = cfg, scope = scope, pbrisSink = pbrisSink)
        engine = fresh
        currentSessionId = activeSessionId
        currentIsOrphan = wantsOrphan
        return fresh
    }

    /** Currently-acquired engine, or null before the first acquire. */
    fun peek(): EngineWrapper? = engine

    /**
     * Compute the engine-store directory for [activeSessionId].
     * `null` -> orphan path. Does not create the directory;
     * caller is responsible.
     */
    fun engineStoreDirFor(context: Context, activeSessionId: UUID?): File {
        val externalRoot = context.getExternalFilesDir(null) ?: context.filesDir
        val sessionDir = if (activeSessionId != null) {
            File(File(externalRoot, "sessions"), activeSessionId.toString())
        } else {
            File(File(externalRoot, "sessions"), "orphan")
        }
        return File(sessionDir, "engine-store")
    }
}
