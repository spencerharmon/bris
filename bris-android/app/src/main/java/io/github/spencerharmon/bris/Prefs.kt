package io.github.spencerharmon.bris

import android.content.Context
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import java.util.UUID

private val Context.dataStore by preferencesDataStore(name = "bris_prefs")

/**
 * Operator-facing preferences.
 *
 * `debug_mode` gates per-frame disk writes for non-fix frames
 * during a capture, plus GPS-truth attachment to `bundle.json`.
 * See `docs/design/diagnostic_collection.md` for the full
 * contract.
 *
 * `device_uuid` is a stable per-install identifier generated on
 * first access. Included in capture manifests.
 */
class Prefs(private val context: Context) {

    val debugModeFlow: Flow<Boolean> = context.dataStore.data.map { it[KEY_DEBUG_MODE] ?: false }

    /**
     * Operator-selected physical-camera lens id, or `null` if
     * the operator has not chosen one yet (in which case
     * callers fall back to a heuristic default).
     */
    val selectedLensIdFlow: Flow<String?> = context.dataStore.data.map { it[KEY_SELECTED_LENS_ID] }

    /**
     * Coarse hemisphere hint ("N", "S", or `null` = unset) for
     * the cold-start CoP solver. Wires through to
     * `FfiEngineConfig.cold_start_coarse_hemisphere`. Applied
     * at next engine construction.
     */
    val coarseHemisphereFlow: Flow<String?> = context.dataStore.data.map { it[KEY_COARSE_HEMISPHERE] }

    /**
     * Operator-selected active session id (UUIDv4 string), or
     * `null` if no session is currently selected. New captures
     * land under `<external-files>/sessions/<uuid>/captures/`
     * when set; the LiveScreen auto-creates an `Untitled <date>`
     * session at first-capture press otherwise.
     */
    val activeSessionIdFlow: Flow<String?> = context.dataStore.data.map { it[KEY_ACTIVE_SESSION_ID] }

    suspend fun setDebugMode(enabled: Boolean) {
        context.dataStore.edit { it[KEY_DEBUG_MODE] = enabled }
    }

    /** Persist the operator's lens selection. */
    suspend fun setSelectedLensId(lensId: String) {
        context.dataStore.edit { it[KEY_SELECTED_LENS_ID] = lensId }
    }

    /** Persist the coarse-hemisphere hint, or clear with `null`. */
    suspend fun setCoarseHemisphere(value: String?) {
        context.dataStore.edit {
            if (value == null) it.remove(KEY_COARSE_HEMISPHERE)
            else it[KEY_COARSE_HEMISPHERE] = value
        }
    }

    /** Persist (or clear) the active session id. */
    suspend fun setActiveSessionId(sessionId: String?) {
        context.dataStore.edit {
            if (sessionId == null) it.remove(KEY_ACTIVE_SESSION_ID)
            else it[KEY_ACTIVE_SESSION_ID] = sessionId
        }
    }

    /** Get or lazily generate the per-install device UUID. */
    suspend fun deviceUuid(): String {
        val current = context.dataStore.data.map { it[KEY_DEVICE_UUID] }.first()
        if (current != null) return current
        val fresh = UUID.randomUUID().toString()
        context.dataStore.edit { it[KEY_DEVICE_UUID] = fresh }
        return fresh
    }

    companion object {
        private val KEY_DEBUG_MODE = booleanPreferencesKey("debug_mode")
        private val KEY_DEVICE_UUID = stringPreferencesKey("device_uuid")
        private val KEY_SELECTED_LENS_ID = stringPreferencesKey("selected_lens_id")
        private val KEY_COARSE_HEMISPHERE = stringPreferencesKey("coarse_hemisphere")
        private val KEY_ACTIVE_SESSION_ID = stringPreferencesKey("active_session_id")
    }
}
