package io.github.spencerharmon.bris.ui

import android.content.Context
import android.net.Uri
import android.text.format.Formatter
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.engine.DebugBufferActions
import io.github.spencerharmon.bris.engine.DebugBundleWriter
import io.github.spencerharmon.bris.engine.DebugCaptureBuffer
import io.github.spencerharmon.bris.engine.SaveResult
import kotlinx.coroutines.launch

/**
 * Build a single click-handler that runs the canonical
 * "Save the entire debug buffer" flow:
 *
 *  1. If no SAF tree URI is stored yet, launch the
 *     `OpenDocumentTree` picker. On success, persist the URI
 *     (only after `takePersistableUriPermission` succeeds —
 *     a failed grant must not poison DataStore with a URI we
 *     can't reuse next session), then proceed to save with the
 *     freshly-granted (possibly ephemeral) URI.
 *  2. If a URI is already stored, save against it directly.
 *  3. Surface the outcome in the supplied [SnackbarHostState]
 *     using identical wording from every call site (LiveScreen,
 *     SettingsScreen, PreUploadReviewScreen).
 *
 * The helper is a Composable because the SAF launcher must be
 * registered during composition. The returned lambda is safe
 * to call from any UI handler.
 */
@Composable
fun rememberDebugSaveAction(
    buffer: DebugCaptureBuffer,
    prefs: Prefs,
    snackbarHost: SnackbarHostState,
    bundleInputsProvider: (() -> DebugBundleWriter.Inputs?)? = null,
): () -> Unit {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val saveLocation by prefs.debugSaveLocationFlow.collectAsState(initial = null)
    // Pending == true means the picker was launched expressly
    // to enable a save; on a successful pick we should run the
    // save automatically.
    var pendingSave by remember { mutableStateOf(false) }

    val picker = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocumentTree(),
    ) { uri ->
        val wasPending = pendingSave
        pendingSave = false
        if (uri == null) return@rememberLauncherForActivityResult
        val flags = android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION or
            android.content.Intent.FLAG_GRANT_WRITE_URI_PERMISSION
        var persisted = false
        try {
            context.contentResolver.takePersistableUriPermission(uri, flags)
            persisted = true
        } catch (_: SecurityException) {
            // Some pickers don't grant persistable rights.
            // The ephemeral grant is still good for this save;
            // we just must not poison DataStore with a URI we
            // can't reuse next session.
        }
        scope.launch {
            if (persisted) {
                prefs.setDebugSaveLocation(uri.toString())
            } else {
                snackbarHost.showSnackbar(
                    "Save location wasn't persisted; this save will still proceed.",
                )
            }
            if (wasPending) {
                runDebugSave(context, buffer, uri, snackbarHost, bundleInputsProvider)
            }
        }
    }

    return {
        val saved = saveLocation?.let { Uri.parse(it) }
        if (saved == null) {
            pendingSave = true
            picker.launch(null)
        } else {
            scope.launch { runDebugSave(context, buffer, saved, snackbarHost, bundleInputsProvider) }
        }
    }
}

/**
 * Drive [DebugBufferActions.saveAll] and surface a Snackbar
 * with identical wording from every call site.
 */
suspend fun runDebugSave(
    context: Context,
    buffer: DebugCaptureBuffer,
    uri: Uri,
    snackbarHost: SnackbarHostState,
    bundleInputsProvider: (() -> DebugBundleWriter.Inputs?)? = null,
) {
    val prepare: ((java.io.File, String) -> Unit)? = bundleInputsProvider?.let { provider ->
        { dir, id ->
            val inputs = provider()
            val snapshot = buffer.bundleSnapshot()
            // A missing snapshot or missing inputs (no
            // calibration resolved yet, etc.) is not fatal:
            // we save the legacy frame layout without a
            // `bundle.json` so the operator still gets their
            // bytes off the device. `bris-cli replay` falls
            // back to its `--frames`-only path in that case.
            if (inputs != null && snapshot != null) {
                DebugBundleWriter.write(dir, id, snapshot, inputs)
            }
        }
    }
    val msg = when (val r = DebugBufferActions.saveAll(context, buffer, uri, prepare)) {
        is SaveResult.Ok ->
            "Saved ${r.frameCount} frames " +
                "(${Formatter.formatShortFileSize(context, r.bytes)}) " +
                "to ${r.destinationDisplay}"
        is SaveResult.Failed -> "Save failed: ${r.message}"
        SaveResult.NeedLocation -> "Pick a save location to continue."
    }
    snackbarHost.showSnackbar(msg)
}
