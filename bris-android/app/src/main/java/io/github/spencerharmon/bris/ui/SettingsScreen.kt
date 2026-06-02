package io.github.spencerharmon.bris.ui

import android.Manifest
import android.content.ContentResolver
import android.content.Context
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Snackbar
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.documentfile.provider.DocumentFile
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.engine.LensCatalog
import io.github.spencerharmon.bris.engine.ShareSessionsAction
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Operator settings.
 *
 *  - Lens selection.
 *  - Coarse-hemisphere hint (cold-start CoP).
 *  - Debug mode toggle \u2014 gates per-frame disk writes for
 *    non-fix frames and GPS-truth attachment.
 *  - Share sessions \u2014 SAF picker, zips the entire
 *    `<external-files>/sessions/` tree for off-device transfer.
 */
@Composable
fun SettingsScreen(prefs: Prefs, onBack: () -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val debugMode by prefs.debugModeFlow.collectAsState(initial = false)
    val selectedLensId by prefs.selectedLensIdFlow.collectAsState(initial = null)
    val snackbarHost = remember { SnackbarHostState() }

    val lenses = remember(context) { LensCatalog.enumerate(context) }
    val effectiveLensId = selectedLensId
        ?: LensCatalog.pickDefault(lenses)?.id
        ?: LensCatalog.FALLBACK_LENS_ID

    // SAF picker for Share sessions. Persisted permission so
    // subsequent shares can reuse the destination without
    // re-prompting.
    val sharePicker = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocumentTree(),
    ) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        scope.launch {
            takePersistableTreePermission(context.contentResolver, uri)
            try {
                val dest = withContext(Dispatchers.IO) {
                    ShareSessionsAction.shareTo(context, uri)
                }
                snackbarHost.showSnackbar("Wrote $dest")
            } catch (t: Throwable) {
                snackbarHost.showSnackbar(
                    "Share failed: ${t.javaClass.simpleName}: ${t.message ?: "?"}",
                )
            }
        }
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text("Settings", style = androidx.compose.material3.MaterialTheme.typography.titleLarge)

        // ---- Lens ----
        Text(
            "Camera lens",
            style = androidx.compose.material3.MaterialTheme.typography.titleMedium,
        )
        Text(
            "Bris's altitude precision is set by your camera's focal length. " +
                "Pick the longest lens with acceptable low-light performance \u2014 " +
                "usually the telephoto.",
            style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
        )
        if (lenses.isEmpty()) {
            Text("No back cameras detected.")
        } else {
            for (lens in lenses) {
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(vertical = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    RadioButton(
                        selected = lens.id == effectiveLensId,
                        onClick = { scope.launch { prefs.setSelectedLensId(lens.id) } },
                    )
                    Text(lens.label, modifier = Modifier.padding(start = 8.dp))
                }
            }
        }
        if (selectedLensId == null && lenses.isNotEmpty()) {
            Text(
                "(default selection \u2014 auto-picked the longest non-ultrawide lens)",
                style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
            )
        }

        HorizontalDivider()

        // ---- Coarse hemisphere hint ----
        Text(
            "Coarse hemisphere hint",
            style = androidx.compose.material3.MaterialTheme.typography.titleMedium,
        )
        Text(
            "Disambiguates the cold-start CoP solver between the two " +
                "latitude solutions on the first fix. Applied to the next " +
                "engine startup.",
            style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
        )
        val coarseHemi by prefs.coarseHemisphereFlow.collectAsState(initial = null)
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            for ((label, value) in listOf("Unset" to null, "North" to "N", "South" to "S")) {
                RadioButton(
                    selected = coarseHemi == value,
                    onClick = { scope.launch { prefs.setCoarseHemisphere(value) } },
                )
                Text(label, modifier = Modifier.padding(start = 4.dp, end = 12.dp))
            }
        }

        HorizontalDivider()

        // ---- Debug mode ----
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Debug mode")
            Switch(
                checked = debugMode,
                onCheckedChange = { v -> scope.launch { prefs.setDebugMode(v) } },
            )
        }
        Text(
            "When ON, every analyzer frame during Start\u2192Stop is written " +
                "to the capture directory and GPS-truth is attached to the " +
                "replay bundle. Fix-frame pixels persist regardless of this " +
                "toggle. Per-capture cost: ~4 MB \u00d7 fps \u00d7 duration.",
            style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
        )

        if (debugMode) {
            val locationLauncher = rememberLauncherForActivityResult(
                ActivityResultContracts.RequestPermission(),
            ) { /* permission result consumed at next read */ }
            Button(onClick = {
                locationLauncher.launch(Manifest.permission.ACCESS_COARSE_LOCATION)
            }) {
                Text("Allow coarse location for bundle gps_truth")
            }
        }

        HorizontalDivider()

        // ---- Share sessions ----
        Text(
            "Share sessions",
            style = androidx.compose.material3.MaterialTheme.typography.titleMedium,
        )
        Text(
            "Zip the entire on-device sessions/ tree to a folder you " +
                "pick. Recipient unpacks with `unzip -n` directly into a " +
                "corpus root.",
            style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
        )
        Button(onClick = { sharePicker.launch(null) }) {
            Text("Share sessions\u2026")
        }

        Button(onClick = onBack) { Text("Back") }

        SnackbarHost(hostState = snackbarHost) { data -> Snackbar(snackbarData = data) }
    }
}

private fun takePersistableTreePermission(resolver: ContentResolver, uri: Uri) {
    val flags = android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION or
        android.content.Intent.FLAG_GRANT_WRITE_URI_PERMISSION
    runCatching { resolver.takePersistableUriPermission(uri, flags) }
}
