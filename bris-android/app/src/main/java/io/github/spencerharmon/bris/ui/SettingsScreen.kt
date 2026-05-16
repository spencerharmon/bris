package io.github.spencerharmon.bris.ui

import android.Manifest
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
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.engine.LensCatalog
import kotlinx.coroutines.launch

/**
 * Operator settings.
 *
 * Three controls:
 *
 *  - Debug mode toggle — the load-bearing switch.
 *  - Debug capture toggle — visible only when debug mode is on;
 *    starts/stops the rolling on-device frame buffer.
 *  - Collector base URL — required for any "send" action to
 *    succeed; visible only when debug mode is on.
 *
 * No analytics or telemetry. No remote config. No per-field
 * privacy toggles inside a debug-mode-gated submission — that
 * would be redundant with the consent that debug-mode-on
 * already constitutes.
 */
@Composable
fun SettingsScreen(prefs: Prefs, onBack: () -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val debugMode by prefs.debugModeFlow.collectAsState(initial = false)
    val debugCapture by prefs.debugCaptureFlow.collectAsState(initial = false)
    val collectorBase by prefs.collectorBaseFlow.collectAsState(initial = "")
    val selectedLensId by prefs.selectedLensIdFlow.collectAsState(initial = null)
    var collectorBaseDraft by remember { mutableStateOf(collectorBase) }

    // Lens enumeration is cheap (a single Camera2 metadata
    // walk) but we still cache it per-context: the set of
    // physical cameras doesn't change at runtime.
    val lenses = remember(context) { LensCatalog.enumerate(context) }
    val effectiveLensId = selectedLensId
        ?: LensCatalog.pickDefault(lenses)?.id
        ?: LensCatalog.FALLBACK_LENS_ID

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text("Settings", style = androidx.compose.material3.MaterialTheme.typography.titleLarge)

        // ---- Lens (Camera) ----
        // Multi-camera Android devices expose ultrawide / wide
        // / telephoto as separate physical cameras under one
        // logical id. Bris's accuracy is dominated by focal
        // length (longer = more arcsec/px at the body
        // centroid = tighter altitude σ), so the operator
        // explicitly picks here. Calibration is keyed by
        // (lens, resolution); switching lens forces a fresh
        // calibration run.
        Text(
            "Camera lens",
            style = androidx.compose.material3.MaterialTheme.typography.titleMedium,
        )
        Text(
            "Bris's altitude precision is set by your camera's focal length. " +
                "Pick the longest lens with acceptable low-light performance — " +
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
                "(default selection — auto-picked the longest non-ultrawide lens)",
                style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
            )
        }

        HorizontalDivider()

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

        if (debugMode) {
            // Coarse-location permission request, gated on debug
            // mode. The permission lets diagnostic submissions
            // optionally include a coarse fix; without it, the
            // submission proceeds with `gps: null`. The runtime
            // request is scoped here (not at app launch) so that
            // operators who never enable debug mode are never
            // prompted.
            val locationLauncher = rememberLauncherForActivityResult(
                ActivityResultContracts.RequestPermission(),
            ) { /* Result captured by Settings's next observation. */ }
            Button(onClick = {
                locationLauncher.launch(Manifest.permission.ACCESS_COARSE_LOCATION)
            }) {
                Text("Allow coarse location for diagnostic submissions")
            }

            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("Debug capture (retain all processed frames)")
                Switch(
                    checked = debugCapture,
                    onCheckedChange = { v -> scope.launch { prefs.setDebugCapture(v) } },
                )
            }

            OutlinedTextField(
                value = collectorBaseDraft,
                onValueChange = { collectorBaseDraft = it },
                label = { Text("Collector base URL") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Button(onClick = { scope.launch { prefs.setCollectorBase(collectorBaseDraft) } }) {
                Text("Save collector URL")
            }
        }

        Button(onClick = onBack) { Text("Back") }
    }
}
