package io.github.spencerharmon.bris.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.engine.Session
import io.github.spencerharmon.bris.engine.SessionStore
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import java.util.UUID

/**
 * Editor for a single [`Session`]. Loads from
 * [`SessionStore`], commits on Save, optional Delete.
 *
 * Mirrors the CLI's `bris session new` flag set: title, notes,
 * AP seed (lat/lon/eye-height), kinematics, retention,
 * use-case profile, expected-to-fail flag.
 */
@Composable
fun SessionEditScreen(
    prefs: Prefs,
    sessionId: UUID,
    onBack: () -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val store = remember(context) { SessionStore.forApp(context) }

    var loaded by remember { mutableStateOf<Session?>(null) }
    var notFound by remember { mutableStateOf(false) }

    LaunchedEffect(sessionId) {
        val s = store.loadOrNull(sessionId)
        if (s == null) notFound = true else loaded = s
    }

    if (notFound) {
        Column(modifier = Modifier.fillMaxSize().padding(16.dp)) {
            OutlinedButton(onClick = onBack) { Text("Back") }
            Spacer(Modifier.height(12.dp))
            Text("Session not found.")
        }
        return
    }

    val session = loaded ?: run {
        Text("Loading\u2026", modifier = Modifier.padding(16.dp))
        return
    }

    // Local edit state.
    var title by remember(session.sessionId) { mutableStateOf(session.title) }
    var notes by remember(session.sessionId) { mutableStateOf(session.notes ?: "") }
    var apLat by remember(session.sessionId) {
        mutableStateOf(session.apSeed?.latDeg?.toString().orEmpty())
    }
    var apLon by remember(session.sessionId) {
        mutableStateOf(session.apSeed?.lonDeg?.toString().orEmpty())
    }
    var apEye by remember(session.sessionId) {
        mutableStateOf((session.apSeed?.eyeHeightM ?: 2.0).toString())
    }
    var kindStationary by remember(session.sessionId) {
        mutableStateOf(session.kinematics is Session.Kinematics.Stationary)
    }
    var kindMaxKn by remember(session.sessionId) {
        mutableStateOf(
            (session.kinematics as? Session.Kinematics.MaxSpeedKn)?.kn?.toString() ?: "8.0",
        )
    }
    var retSec by remember(session.sessionId) {
        mutableStateOf(session.sightRetentionSeconds.toString())
    }
    var retCap by remember(session.sessionId) {
        mutableStateOf(session.sightRetentionCapacity.toString())
    }
    var profile by remember(session.sessionId) { mutableStateOf(session.profile) }
    var expectFail by remember(session.sessionId) { mutableStateOf(session.expectedToFail) }
    var error by remember { mutableStateOf<String?>(null) }
    var showDelete by remember { mutableStateOf(false) }

    Column(
        modifier = Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            OutlinedButton(onClick = onBack) { Text("Back") }
            Spacer(Modifier.height(8.dp))
            Text("  Edit session", fontWeight = FontWeight.Bold)
        }

        Text("id ${session.sessionId}")

        OutlinedTextField(
            value = title,
            onValueChange = { title = it },
            label = { Text("Title") },
            modifier = Modifier.fillMaxWidth(),
            singleLine = true,
        )
        OutlinedTextField(
            value = notes,
            onValueChange = { notes = it },
            label = { Text("Notes") },
            modifier = Modifier.fillMaxWidth(),
            minLines = 2,
        )

        HorizontalDivider()
        Text("Assumed position (optional)", fontWeight = FontWeight.Bold)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(
                value = apLat,
                onValueChange = { apLat = it },
                label = { Text("Lat \u00b0") },
                modifier = Modifier.weight(1f),
                singleLine = true,
            )
            OutlinedTextField(
                value = apLon,
                onValueChange = { apLon = it },
                label = { Text("Lon \u00b0") },
                modifier = Modifier.weight(1f),
                singleLine = true,
            )
            OutlinedTextField(
                value = apEye,
                onValueChange = { apEye = it },
                label = { Text("Eye m") },
                modifier = Modifier.weight(1f),
                singleLine = true,
            )
        }

        HorizontalDivider()
        Text("Kinematics", fontWeight = FontWeight.Bold)
        Row(verticalAlignment = Alignment.CenterVertically) {
            RadioButton(selected = kindStationary, onClick = { kindStationary = true })
            Text("Stationary")
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            RadioButton(selected = !kindStationary, onClick = { kindStationary = false })
            Text("Max speed (kn): ")
            OutlinedTextField(
                value = kindMaxKn,
                onValueChange = { kindMaxKn = it },
                modifier = Modifier.weight(1f),
                singleLine = true,
                enabled = !kindStationary,
            )
        }

        HorizontalDivider()
        Text("Sight retention", fontWeight = FontWeight.Bold)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(
                value = retSec,
                onValueChange = { retSec = it },
                label = { Text("Seconds") },
                modifier = Modifier.weight(1f),
                singleLine = true,
            )
            OutlinedTextField(
                value = retCap,
                onValueChange = { retCap = it },
                label = { Text("Capacity") },
                modifier = Modifier.weight(1f),
                singleLine = true,
            )
        }

        HorizontalDivider()
        Text("Use-case profile", fontWeight = FontWeight.Bold)
        Session.Profile.entries.forEach { p ->
            Row(verticalAlignment = Alignment.CenterVertically) {
                RadioButton(selected = profile == p, onClick = { profile = p })
                Text(p.name)
            }
        }

        HorizontalDivider()
        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(checked = expectFail, onCheckedChange = { expectFail = it })
            Text("Expected to fail (adversarial corpus)")
        }

        error?.let {
            Text(it, color = androidx.compose.ui.graphics.Color(0xFFE57373))
        }

        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = {
                val parsed = parseEdits(
                    title = title,
                    notes = notes,
                    apLat = apLat,
                    apLon = apLon,
                    apEye = apEye,
                    kindStationary = kindStationary,
                    kindMaxKn = kindMaxKn,
                    retSec = retSec,
                    retCap = retCap,
                )
                parsed.onSuccess { (apSeed, kin, retSecL, retCapI) ->
                    val updated = session.copy(
                        title = title.trim().ifEmpty { session.title },
                        notes = notes.takeIf { it.isNotBlank() },
                        apSeed = apSeed,
                        kinematics = kin,
                        sightRetentionSeconds = retSecL,
                        sightRetentionCapacity = retCapI,
                        profile = profile,
                        expectedToFail = expectFail,
                    )
                    store.save(updated)
                    error = null
                    onBack()
                }
                parsed.onFailure { error = it.message }
            }) { Text("Save") }
            OutlinedButton(onClick = {
                scope.launch { prefs.setActiveSessionId(session.sessionId.toString()) }
            }) { Text("Set active") }
            TextButton(onClick = { showDelete = true }) { Text("Delete") }
        }
    }

    if (showDelete) {
        AlertDialog(
            onDismissRequest = { showDelete = false },
            confirmButton = {
                TextButton(onClick = {
                    showDelete = false
                    store.delete(session.sessionId)
                    scope.launch {
                        val current = prefs.activeSessionIdFlow.first()
                        if (current == session.sessionId.toString()) {
                            prefs.setActiveSessionId(null)
                        }
                    }
                    onBack()
                }) { Text("Delete", color = androidx.compose.ui.graphics.Color.Red) }
            },
            dismissButton = {
                TextButton(onClick = { showDelete = false }) { Text("Cancel") }
            },
            title = { Text("Delete session?") },
            text = {
                Text(
                    "Removes session.json and all captures under it. " +
                        "Cannot be undone.",
                )
            },
        )
    }
}

private data class ParsedEdits(
    val apSeed: Session.ApSeed?,
    val kin: Session.Kinematics,
    val retSec: Long,
    val retCap: Int,
)

private fun parseEdits(
    title: String,
    notes: String,
    apLat: String,
    apLon: String,
    apEye: String,
    kindStationary: Boolean,
    kindMaxKn: String,
    retSec: String,
    retCap: String,
): Result<ParsedEdits> = runCatching {
    require(title.isNotBlank()) { "Title is required" }
    val apSeed: Session.ApSeed? = if (apLat.isBlank() && apLon.isBlank()) {
        null
    } else {
        val lat = apLat.toDoubleOrNull() ?: error("AP lat must be a number")
        val lon = apLon.toDoubleOrNull() ?: error("AP lon must be a number")
        val eye = apEye.toDoubleOrNull() ?: 2.0
        require(lat in -90.0..90.0) { "AP lat out of range" }
        require(lon in -180.0..180.0) { "AP lon out of range" }
        Session.ApSeed(latDeg = lat, lonDeg = lon, eyeHeightM = eye)
    }
    val kin: Session.Kinematics = if (kindStationary) {
        Session.Kinematics.Stationary
    } else {
        val kn = kindMaxKn.toDoubleOrNull() ?: error("Max speed must be a number")
        require(kn > 0.0) { "Max speed must be positive" }
        Session.Kinematics.MaxSpeedKn(kn)
    }
    val sec = retSec.toLongOrNull() ?: error("Retention seconds must be an integer")
    val cap = retCap.toIntOrNull() ?: error("Retention capacity must be an integer")
    require(sec > 0L) { "Retention seconds must be > 0" }
    require(cap > 0) { "Retention capacity must be > 0" }
    ParsedEdits(apSeed = apSeed, kin = kin, retSec = sec, retCap = cap)
}
