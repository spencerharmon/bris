package io.github.spencerharmon.bris.engine

import android.content.Context
import org.json.JSONObject
import java.io.File
import java.util.UUID

/**
 * Read-only catalog of all on-device captures, grouped by
 * session. Used by `SightLogScreen` to surface captures that
 * land under the canonical `sessions/<UUID>/captures/<cap-id>/`
 * layout (which `SightLog.list` of the legacy orphan
 * `<files>/sights/` root doesn't see).
 *
 * Pure-JVM. No coroutines, no Compose. The filesystem walk is
 * small and synchronous; the caller invokes it inside a
 * `LaunchedEffect` if it cares about staying off the main
 * thread.
 */
class CaptureCatalog(private val externalFilesRoot: File) {

    /**
     * Sessions plus the captures attached to each, newest
     * session first. Orphan captures from the legacy
     * `<files>/sights/` root surface as one synthetic
     * [`SessionGroup`] with `sessionId = null`.
     */
    fun listGroups(): List<SessionGroup> {
        val groups = mutableListOf<SessionGroup>()

        // Session-attached captures.
        val sessionsRoot = File(externalFilesRoot, "sessions")
        if (sessionsRoot.isDirectory) {
            sessionsRoot.listFiles { f -> f.isDirectory }
                ?.mapNotNull { sessionDir ->
                    val sessionJson = File(sessionDir, "session.json")
                    if (!sessionJson.isFile) return@mapNotNull null
                    val parsed = runCatching {
                        Session.fromJson(JSONObject(sessionJson.readText()))
                    }.getOrNull() ?: return@mapNotNull null
                    val capturesDir = File(sessionDir, "captures")
                    val captures = if (capturesDir.isDirectory) {
                        capturesDir.listFiles { f -> f.isDirectory }
                            ?.map { CaptureEntry(it.name, it, it.kind()) }
                            ?.sortedByDescending { it.dir.lastModified() }
                            .orEmpty()
                    } else emptyList()
                    SessionGroup(
                        sessionId = parsed.sessionId,
                        title = parsed.title,
                        createdUnixMs = parsed.createdUnixMs,
                        captures = captures,
                    )
                }
                ?.sortedByDescending { it.createdUnixMs }
                ?.let(groups::addAll)
        }

        // Orphan captures (legacy <files>/sights/).
        val sightsRoot = File(externalFilesRoot, "sights")
        if (sightsRoot.isDirectory) {
            val orphans = sightsRoot.listFiles { f -> f.isDirectory }
                ?.map { CaptureEntry(it.name, it, it.kind()) }
                ?.sortedByDescending { it.dir.lastModified() }
                .orEmpty()
            if (orphans.isNotEmpty()) {
                groups.add(
                    SessionGroup(
                        sessionId = null,
                        title = "(orphan captures)",
                        createdUnixMs = 0L,
                        captures = orphans,
                    ),
                )
            }
        }

        return groups
    }

    /** A session and its captures, for display. */
    data class SessionGroup(
        val sessionId: UUID?,
        val title: String,
        val createdUnixMs: Long,
        val captures: List<CaptureEntry>,
    )

    /** One capture directory and its kind. */
    data class CaptureEntry(
        val id: String,
        val dir: File,
        val kind: CaptureKind,
    )

    /** How the capture was written. */
    enum class CaptureKind {
        /** Has `bundle.json` (full capture from `CaptureRecorder`). */
        Bundle,

        /** Has `manifest.json` (legacy sight-log entry only). */
        SightLog,

        /** Neither manifest is present \u2014 dir exists, contents unknown. */
        Unknown,
    }

    companion object {
        fun forApp(context: Context): CaptureCatalog {
            val root = context.getExternalFilesDir(null) ?: context.filesDir
            return CaptureCatalog(root)
        }

        private fun File.kind(): CaptureKind = when {
            File(this, "bundle.json").isFile -> CaptureKind.Bundle
            File(this, "manifest.json").isFile -> CaptureKind.SightLog
            else -> CaptureKind.Unknown
        }
    }
}
