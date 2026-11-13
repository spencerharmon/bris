package io.github.spencerharmon.bris.upload

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
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
) {
    val registered: Boolean get() = !deviceToken.isNullOrBlank()

    val configured: Boolean
        get() = baseUrl.isNotBlank() && (registered || enteredToken.isNotBlank())

    /** Endpoint to flush against, or `null` when not configured. */
    fun endpoint(deviceUuid: String): CollectorEndpointConfig? {
        if (!configured) return null
        return if (registered) {
            CollectorEndpointConfig(baseUrl, deviceToken!!, deviceUuid)
        } else {
            CollectorEndpointConfig(baseUrl, enteredToken, null)
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
    return trimmed
}

/**
 * Persistence for [CollectorSettings], backed by a dedicated
 * app-private Preferences DataStore (`files/datastore/`, not
 * backed up: the manifest sets `allowBackup=false`). Tokens are
 * never logged.
 */
class CollectorSettingsStore(private val store: DataStore<Preferences>) {

    val flow: Flow<CollectorSettings> = store.data.map { p ->
        CollectorSettings(
            baseUrl = p[KEY_URL] ?: "",
            enteredToken = p[KEY_TOKEN] ?: "",
            deviceToken = p[KEY_DEVICE_TOKEN],
            lastError = p[KEY_LAST_ERROR],
        )
    }

    suspend fun current(): CollectorSettings = flow.first()

    /**
     * Save URL + entered token. Changing either invalidates any
     * previously-issued per-device token (it belongs to the old
     * collector / enrollment).
     *
     * @throws IllegalArgumentException on an invalid URL.
     */
    suspend fun save(baseUrl: String, token: String) {
        val url = requireNotNull(normalizeBaseUrl(baseUrl)) { "invalid collector URL: $baseUrl" }
        val tok = token.trim()
        store.edit { p ->
            val changed = p[KEY_URL] != url || p[KEY_TOKEN] != tok
            p[KEY_URL] = url
            p[KEY_TOKEN] = tok
            if (changed) p.remove(KEY_DEVICE_TOKEN)
        }
    }

    suspend fun setDeviceToken(token: String) {
        store.edit { it[KEY_DEVICE_TOKEN] = token }
    }

    suspend fun setLastError(error: String?) {
        store.edit { if (error == null) it.remove(KEY_LAST_ERROR) else it[KEY_LAST_ERROR] = error }
    }

    suspend fun clear() {
        store.edit { it.clear() }
    }

    private companion object {
        val KEY_URL = stringPreferencesKey("collector_base_url")
        val KEY_TOKEN = stringPreferencesKey("collector_entered_token")
        val KEY_DEVICE_TOKEN = stringPreferencesKey("collector_device_token")
        val KEY_LAST_ERROR = stringPreferencesKey("collector_last_error")
    }
}
