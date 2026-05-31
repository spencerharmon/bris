package io.github.spencerharmon.bris.engine

import android.content.Context
import android.os.Build
import io.github.spencerharmon.bris.BuildConfig
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.util.UUID

/**
 * On-device session: the operator's UUIDv4 grouping under which
 * captures land. Mirrors `bris_bundle::SessionManifest`
 * (`schema_version = 1`).
 *
 * On disk:
 *
 *   <external-files>/sessions/<UUID>/session.json
 *   <external-files>/sessions/<UUID>/captures/<capture-id>/
 *       sights/<capture-id>/   (operator-captured fix entry)
 *       (debug-bundle output also lands under captures/<capture-id>/)
 *
 * JSON shape is identical to `SessionManifest` so the same file
 * loads under `bris-cli session show` / `bris replay --session`.
 *
 * Kotlin uses `org.json` (matching the rest of the app); the
 * Rust crate uses serde. The two stay in lock-step by virtue of
 * one writer per schema version.
 */
data class Session(
    val sessionId: UUID,
    val title: String,
    val createdUnixMs: Long,
    val device: DeviceInfo,
    val notes: String? = null,
    val apSeed: ApSeed? = null,
    val profile: Profile = Profile.Custom,
    val kinematics: Kinematics = Kinematics.Stationary,
    val sightRetentionSeconds: Long = DEFAULT_RETENTION_SECONDS,
    val sightRetentionCapacity: Int = DEFAULT_RETENTION_CAPACITY,
    val expectedToFail: Boolean = false,
    val orderedCaptureIds: List<String> = emptyList(),
) {
    data class DeviceInfo(
        val model: String,
        val os: String? = null,
        val appVersion: String? = null,
    )

    data class ApSeed(
        val latDeg: Double,
        val lonDeg: Double,
        val eyeHeightM: Double = 2.0,
    )

    enum class Profile { Custom, Marine, Aeronautical, LandBased, Urban }

    sealed interface Kinematics {
        data object Stationary : Kinematics
        data class MaxSpeedKn(val kn: Double) : Kinematics
    }

    fun toJson(): JSONObject {
        val deviceJson = JSONObject()
            .put("model", device.model)
        device.os?.let { deviceJson.put("os", it) }
        device.appVersion?.let { deviceJson.put("app_version", it) }

        val root = JSONObject()
            .put("schema_version", SCHEMA_VERSION)
            .put("session_id", sessionId.toString())
            .put("title", title)
            .put("created_unix_ms", createdUnixMs)
            .put("device", deviceJson)
            .put("profile", profile.toJsonString())
            .put("kinematics", kinematicsJson(kinematics))
            .put("sight_retention_seconds", sightRetentionSeconds)
            .put("sight_retention_capacity", sightRetentionCapacity)
            .put("expected_to_fail", expectedToFail)
            .put("ordered_capture_ids", JSONArray(orderedCaptureIds))

        notes?.let { root.put("notes", it) }
        apSeed?.let {
            root.put(
                "ap_seed",
                JSONObject()
                    .put("lat", it.latDeg)
                    .put("lon", it.lonDeg)
                    .put("eye_height_m", it.eyeHeightM)
                    .put("provenance", "operator_entered"),
            )
        }
        return root
    }

    companion object {
        const val SCHEMA_VERSION: Int = 1

        // Matches `bris_bundle::SessionManifest::DEFAULT_RETENTION_*`.
        const val DEFAULT_RETENTION_SECONDS: Long = 7200L
        const val DEFAULT_RETENTION_CAPACITY: Int = 50

        fun new(title: String): Session = Session(
            sessionId = UUID.randomUUID(),
            title = title,
            createdUnixMs = System.currentTimeMillis(),
            device = DeviceInfo(
                model = "${Build.MANUFACTURER} ${Build.MODEL}".trim(),
                os = "Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})",
                appVersion = BuildConfig.BRIS_APP_VERSION,
            ),
        )

        fun fromJson(json: JSONObject): Session {
            val schema = json.optInt("schema_version", 0)
            require(schema == SCHEMA_VERSION) {
                "unsupported session schema: $schema (expected $SCHEMA_VERSION)"
            }
            val device = json.getJSONObject("device").let { d ->
                DeviceInfo(
                    model = d.optString("model", "unknown"),
                    os = d.takeIf { it.has("os") }?.optString("os"),
                    appVersion = d.takeIf { it.has("app_version") }?.optString("app_version"),
                )
            }
            val kin = json.optJSONObject("kinematics")?.let { kinematicsFromJson(it) }
                ?: Kinematics.Stationary
            val profile = profileFromJsonString(json.optString("profile", "custom"))
            val notes = if (json.has("notes")) json.optString("notes") else null
            val apSeed = json.optJSONObject("ap_seed")?.let {
                ApSeed(
                    latDeg = it.getDouble("lat"),
                    lonDeg = it.getDouble("lon"),
                    eyeHeightM = it.optDouble("eye_height_m", 2.0),
                )
            }
            val captures = json.optJSONArray("ordered_capture_ids")?.let { arr ->
                List(arr.length()) { idx -> arr.getString(idx) }
            } ?: emptyList()
            return Session(
                sessionId = UUID.fromString(json.getString("session_id")),
                title = json.getString("title"),
                createdUnixMs = json.getLong("created_unix_ms"),
                device = device,
                notes = notes,
                apSeed = apSeed,
                profile = profile,
                kinematics = kin,
                sightRetentionSeconds = json.optLong(
                    "sight_retention_seconds",
                    DEFAULT_RETENTION_SECONDS,
                ),
                sightRetentionCapacity = json.optInt(
                    "sight_retention_capacity",
                    DEFAULT_RETENTION_CAPACITY,
                ),
                expectedToFail = json.optBoolean("expected_to_fail", false),
                orderedCaptureIds = captures,
            )
        }

        private fun kinematicsJson(k: Kinematics): JSONObject = when (k) {
            Kinematics.Stationary -> JSONObject().put("kind", "stationary")
            is Kinematics.MaxSpeedKn -> JSONObject()
                .put("kind", "max_speed_kn")
                .put("kn", k.kn)
        }

        private fun kinematicsFromJson(json: JSONObject): Kinematics =
            when (val kind = json.optString("kind", "stationary")) {
                "stationary" -> Kinematics.Stationary
                "max_speed_kn" -> Kinematics.MaxSpeedKn(json.getDouble("kn"))
                else -> throw IllegalArgumentException("unknown kinematics kind: $kind")
            }

        private fun Profile.toJsonString(): String = when (this) {
            Profile.Custom -> "custom"
            Profile.Marine -> "marine"
            Profile.Aeronautical -> "aeronautical"
            Profile.LandBased -> "land_based"
            Profile.Urban -> "urban"
        }

        private fun profileFromJsonString(s: String): Profile = when (s) {
            "marine" -> Profile.Marine
            "aeronautical" -> Profile.Aeronautical
            "land_based" -> Profile.LandBased
            "urban" -> Profile.Urban
            else -> Profile.Custom
        }
    }
}

/**
 * Filesystem store for [`Session`]s: list, load, save, delete.
 *
 * Lives at `<external-files>/sessions/`. Each session is one
 * `<UUID>/` subdirectory containing `session.json` + a
 * `captures/` subtree.
 */
class SessionStore(private val rootDir: File) {

    init {
        rootDir.mkdirs()
    }

    /** Directory backing a given session id. May not exist yet. */
    fun dirFor(sessionId: UUID): File = File(rootDir, sessionId.toString())

    /** Directory under which captures for [sessionId] land. */
    fun capturesDir(sessionId: UUID): File =
        File(dirFor(sessionId), "captures").apply { mkdirs() }

    /** All sessions on disk, newest first. Skips corrupt entries. */
    fun list(): List<Session> {
        val entries = rootDir.listFiles { f -> f.isDirectory } ?: return emptyList()
        return entries.mapNotNull { dir ->
            runCatching { load(File(dir, "session.json")) }.getOrNull()
        }.sortedByDescending { it.createdUnixMs }
    }

    /** Load a session by id, or `null` if not present / corrupt. */
    fun loadOrNull(sessionId: UUID): Session? = runCatching {
        load(File(dirFor(sessionId), "session.json"))
    }.getOrNull()

    /**
     * Persist the session. Rewrites `session.json` atomically
     * via a temp-file + rename.
     */
    fun save(session: Session) {
        val dir = dirFor(session.sessionId).apply { mkdirs() }
        val target = File(dir, "session.json")
        val tmp = File(dir, "session.json.tmp")
        tmp.writeText(session.toJson().toString(2))
        if (!tmp.renameTo(target)) {
            // renameTo can fail across some filesystems; fall
            // back to copy + delete.
            target.writeText(tmp.readText())
            tmp.delete()
        }
    }

    /** Append a capture id to [sessionId]'s ordered list, persist. */
    fun appendCapture(sessionId: UUID, captureId: String) {
        val s = loadOrNull(sessionId) ?: return
        if (s.orderedCaptureIds.contains(captureId)) return
        save(s.copy(orderedCaptureIds = s.orderedCaptureIds + captureId))
    }

    /** Delete the entire session directory (captures included). */
    fun delete(sessionId: UUID) {
        dirFor(sessionId).deleteRecursively()
    }

    private fun load(file: File): Session {
        val raw = file.readText()
        return Session.fromJson(JSONObject(raw))
    }

    companion object {
        /** Mount the session store under `<external-files>/sessions/`. */
        fun forApp(context: Context): SessionStore {
            val root = context.getExternalFilesDir(null) ?: context.filesDir
            return SessionStore(File(root, "sessions"))
        }
    }
}
