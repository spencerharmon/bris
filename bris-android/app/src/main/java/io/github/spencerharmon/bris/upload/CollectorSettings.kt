package io.github.spencerharmon.bris.upload

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map

/**
 * Runtime collector configuration (ROI collection-path §1: the
 * bearer token and collector base URL are RUNTIME config, never
 * compiled into the APK). There is deliberately no default URL
 * or token: an unconfigured app cannot upload.
 *
 * - [baseUrl]: operator-entered collector base URL.
 * - [enteredToken]: the token the operator typed (an
 *   enrollment/admin token, or an already-issued per-device
 *   token).
 * - [deviceToken]: the per-device token issued by
 *   `POST /v1/devices/register`, once registration succeeded.
 *   When present it supersedes [enteredToken] for submissions
 *   and is sent together with the `X-Bris-Device-Uuid` header.
 */
data class CollectorSettings(
    val baseUrl: String = "",
    val enteredToken: String = "",
    val deviceToken: String? = null,
    val lastError: String? = null,
    val automaticUpload: Boolean = false,
    val debugMode: Boolean = false,
    val configGeneration: String = "0",
    val manualEpoch: String = "0",
    val automaticEpoch: String = "0",
    /**
     * Automatic captures authorized (ON) before the first collector configuration are
     * held under the preserved [automaticEpoch] until the device enrolls, so the
     * operator-entered enrollment/admin token never submits them. Cleared by a
     * successful enrollment (or a NotEnrollmentToken verdict, meaning the entered
     * token is itself a device token) and by any revocation.
     */
    val automaticAwaitingEnrollment: Boolean = false,
) {
    val manualScope: String? get() = if (debugMode && configured) "manual:$configGeneration:$manualEpoch" else null
    // The opt-in alone authorizes local queueing, even before first configuration.
    val automaticScope: String? get() = if (automaticUpload) "automatic:$automaticEpoch" else null
    val registered: Boolean get() = !deviceToken.isNullOrBlank()

    val configured: Boolean
        get() = baseUrl.isNotBlank() && (registered || enteredToken.isNotBlank())

    /** Endpoint to flush against, or `null` when not configured. */
    fun endpoint(deviceUuid: String): CollectorEndpointConfig? {
        if (!configured) return null
        return if (registered) {
            CollectorEndpointConfig(baseUrl, deviceToken!!, deviceUuid)
        } else {
            CollectorEndpointConfig(baseUrl, enteredToken, deviceUuid)
        }
    }

    /** Never let a token reach logs via data-class toString. */
    override fun toString(): String =
        "CollectorSettings(baseUrl=$baseUrl, enteredToken=${maskToken(enteredToken)}, " +
            "deviceToken=${maskToken(deviceToken)}, lastError=$lastError)"
}

/** Resolved endpoint passed to the FFI `Submitter.flush`. */
data class CollectorEndpointConfig(
    val baseUrl: String,
    val bearerToken: String,
    val deviceUuid: String?,
) {
    override fun toString(): String =
        "CollectorEndpointConfig(baseUrl=$baseUrl, bearerToken=${maskToken(bearerToken)}, deviceUuid=$deviceUuid)"
}

/** Mask a secret for display: only the last 4 chars survive. */
fun maskToken(token: String?): String = when {
    token.isNullOrEmpty() -> "(unset)"
    token.length <= 4 -> "\u2022".repeat(token.length)
    else -> "\u2022".repeat(8) + token.takeLast(4)
}

/**
 * Validate + normalize an operator-entered base URL: must be
 * http(s) with a host; trailing slashes are stripped. Returns
 * `null` for an invalid URL.
 */
fun normalizeBaseUrl(raw: String): String? {
    val trimmed = raw.trim().trimEnd('/')
    val uri = runCatching { java.net.URI(trimmed) }.getOrNull() ?: return null
    val scheme = uri.scheme?.lowercase() ?: return null
    if (scheme != "http" && scheme != "https") return null
    if (uri.host.isNullOrBlank()) return null
    if (uri.rawUserInfo != null || uri.rawQuery != null || uri.rawFragment != null) return null
    return trimmed
}

/**
 * Persistence for [CollectorSettings], backed by a dedicated
 * app-private Preferences DataStore (`files/datastore/`, not
 * backed up: the manifest sets `allowBackup=false`). Tokens are
 * never logged.
 */
class CollectorSettingsStore(private val store: DataStore<Preferences>) {

    val flow: Flow<CollectorSettings> = store.data.map(::decode)

    private fun decode(p: Preferences) = CollectorSettings(
            baseUrl = p[KEY_URL] ?: "",
            enteredToken = p[KEY_TOKEN] ?: "",
            deviceToken = p[KEY_DEVICE_TOKEN],
            lastError = p[KEY_LAST_ERROR],
            automaticUpload = p[KEY_AUTOMATIC] ?: false,
            debugMode = p[KEY_DEBUG] ?: false,
            configGeneration = p[KEY_GENERATION] ?: "0",
            manualEpoch = p[KEY_MANUAL_EPOCH] ?: "0",
            automaticEpoch = p[KEY_AUTOMATIC_EPOCH] ?: "0",
            automaticAwaitingEnrollment = p[KEY_AUTOMATIC_AWAITING_ENROLLMENT] ?: false,
        )

    suspend fun current(): CollectorSettings = flow.first()

    // Only the coordinator writes authorization. Its mutation lock also covers
    // native configureUploads, so a stale drain cannot restore revoked scopes.
    internal suspend fun update(
        beforePersist: (CollectorSettings) -> Unit = {},
        transform: (CollectorSettings) -> CollectorSettings,
    ): CollectorSettings {
        lateinit var result: CollectorSettings
        store.edit { p ->
            result = transform(decode(p))
            beforePersist(result)
            p[KEY_URL] = result.baseUrl
            p[KEY_TOKEN] = result.enteredToken
            result.deviceToken?.let { p[KEY_DEVICE_TOKEN] = it } ?: p.remove(KEY_DEVICE_TOKEN)
            result.lastError?.let { p[KEY_LAST_ERROR] = it } ?: p.remove(KEY_LAST_ERROR)
            p[KEY_AUTOMATIC] = result.automaticUpload
            p[KEY_DEBUG] = result.debugMode
            p[KEY_GENERATION] = result.configGeneration
            p[KEY_MANUAL_EPOCH] = result.manualEpoch
            p[KEY_AUTOMATIC_EPOCH] = result.automaticEpoch
            p[KEY_AUTOMATIC_AWAITING_ENROLLMENT] = result.automaticAwaitingEnrollment
        }
        return result
    }

    private companion object {
        val KEY_URL = stringPreferencesKey("collector_base_url")
        val KEY_TOKEN = stringPreferencesKey("collector_entered_token")
        val KEY_DEVICE_TOKEN = stringPreferencesKey("collector_device_token")
        val KEY_LAST_ERROR = stringPreferencesKey("collector_last_error")
        val KEY_AUTOMATIC = booleanPreferencesKey("collector_automatic_upload")
        val KEY_DEBUG = booleanPreferencesKey("collector_manual_debug")
        val KEY_GENERATION = stringPreferencesKey("collector_config_generation")
        val KEY_MANUAL_EPOCH = stringPreferencesKey("collector_manual_epoch")
        val KEY_AUTOMATIC_EPOCH = stringPreferencesKey("collector_automatic_epoch")
        val KEY_AUTOMATIC_AWAITING_ENROLLMENT = booleanPreferencesKey("collector_automatic_awaiting_enrollment")
    }
}
