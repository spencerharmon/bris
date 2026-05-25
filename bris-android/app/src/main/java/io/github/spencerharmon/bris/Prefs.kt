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
 * The single load-bearing key is `debug_mode`: when off, no
 * diagnostic-collection UI is visible anywhere in the app. The
 * three contextual actions (debug capture, send fix, send
 * calibration) all gate on `debug_mode = true`.
 *
 * `device_uuid` is a stable per-install identifier generated on
 * first access. It is included in every diagnostic submission's
 * manifest. The collector logs it hashed-truncated; the raw
 * value never leaves the device except in submission manifests.
 */
class Prefs(private val context: Context) {

    val debugModeFlow: Flow<Boolean> = context.dataStore.data.map { it[KEY_DEBUG_MODE] ?: false }
    val debugCaptureFlow: Flow<Boolean> = context.dataStore.data.map { it[KEY_DEBUG_CAPTURE] ?: false }
    val collectorBaseFlow: Flow<String> = context.dataStore.data.map { it[KEY_COLLECTOR_BASE] ?: "" }

    /**
     * Operator-chosen Storage Access Framework tree URI for
     * "Save buffer" exports, or `null` if not yet picked. Stored
     * as the URI string; the caller resolves it via
     * `Uri.parse(...)`. First save without this prompts the
     * system tree picker; subsequent saves go directly to the
     * stored location.
     */
    val debugSaveLocationFlow: Flow<String?> = context.dataStore.data.map { it[KEY_DEBUG_SAVE_URI] }

    /**
     * Operator-selected physical-camera lens id, or `null` if
     * the operator has not chosen one yet (in which case
     * callers fall back to a heuristic default).
     *
     * The id is the underlying Camera2 `cameraId` string
     * surfaced by [`io.github.spencerharmon.bris.engine.LensCatalog`]; it is
     * stable across app launches on a given device but is
     * device-specific. Calibration intrinsics are keyed by
     * this id so a wide-lens calibration never silently
     * applies to the telephoto.
     */
    val selectedLensIdFlow: Flow<String?> = context.dataStore.data.map { it[KEY_SELECTED_LENS_ID] }

    /**
     * Operator-supplied coarse hemisphere hint ("N", "S", or
     * `null` = unset) for the cold-start CoP solver. Maps to
     * `EngineConfig::cold_start.coarse_hemisphere` once the
     * FFI surfaces that field; currently scaffolded so the UI
     * has somewhere to write to.
     *
     * TODO(cold-start): wire to FFI once the cold-start
     * fallback PR adds the field.
     */
    val coarseHemisphereFlow: Flow<String?> = context.dataStore.data.map { it[KEY_COARSE_HEMISPHERE] }

    suspend fun setDebugMode(enabled: Boolean) {
        context.dataStore.edit { it[KEY_DEBUG_MODE] = enabled }
    }

    suspend fun setDebugCapture(enabled: Boolean) {
        context.dataStore.edit { it[KEY_DEBUG_CAPTURE] = enabled }
    }

    suspend fun setCollectorBase(url: String) {
        context.dataStore.edit { it[KEY_COLLECTOR_BASE] = url }
    }

    /** Persist (or clear, with `null`) the SAF tree URI used
     *  by "Save buffer" for debug exports. */
    suspend fun setDebugSaveLocation(uri: String?) {
        context.dataStore.edit {
            if (uri == null) it.remove(KEY_DEBUG_SAVE_URI) else it[KEY_DEBUG_SAVE_URI] = uri
        }
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
        private val KEY_DEBUG_CAPTURE = booleanPreferencesKey("debug_capture")
        private val KEY_COLLECTOR_BASE = stringPreferencesKey("collector_base")
        private val KEY_DEVICE_UUID = stringPreferencesKey("device_uuid")
        private val KEY_SELECTED_LENS_ID = stringPreferencesKey("selected_lens_id")
        private val KEY_DEBUG_SAVE_URI = stringPreferencesKey("debug_save_uri")
        private val KEY_COARSE_HEMISPHERE = stringPreferencesKey("coarse_hemisphere")
    }
}
