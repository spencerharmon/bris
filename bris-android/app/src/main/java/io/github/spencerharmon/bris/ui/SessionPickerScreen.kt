package io.github.spencerharmon.bris.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.OutlinedButton
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
import androidx.compose.runtime.collectAsState
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.engine.Session
import io.github.spencerharmon.bris.engine.SessionStore
import kotlinx.coroutines.launch
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.UUID

/**
 * Operator-facing session list. Three actions:
 *
 *  - "New session" — create a fresh `Untitled <date>`, jump
 *    straight to the edit screen for the operator to refine.
 *  - "Use" — set this session as active and pop back.
 *  - "Edit" — navigate to the edit screen for an existing
 *    session.
 *
 * A session is the operator's UUIDv4 grouping under which
 * captures land. The "active" session is the one new captures
 * will write into; LiveScreen reads `Prefs.activeSessionIdFlow`
 * to decide.
 */
@Composable
fun SessionPickerScreen(
    prefs: Prefs,
    onBack: () -> Unit,
    onEdit: (UUID) -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val store = remember(context) { SessionStore.forApp(context) }
    val activeId by prefs.activeSessionIdFlow.collectAsState(initial = null)
    var sessions by remember { mutableStateOf(store.list()) }

    fun refresh() {
        sessions = store.list()
    }

    LaunchedEffect(Unit) { refresh() }

    Column(modifier = Modifier.fillMaxSize().padding(16.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            OutlinedButton(onClick = onBack) { Text("Back") }
            Spacer(Modifier.height(8.dp))
            Text(
                "  Sessions",
                fontWeight = FontWeight.Bold,
                modifier = Modifier.padding(start = 8.dp),
            )
        }
        Spacer(Modifier.height(12.dp))
        Button(onClick = {
            val s = Session.new("Untitled ${formatDateShort(System.currentTimeMillis())}")
            store.save(s)
            scope.launch { prefs.setActiveSessionId(s.sessionId.toString()) }
            refresh()
            onEdit(s.sessionId)
        }) { Text("+ New session") }
        Spacer(Modifier.height(16.dp))
        if (sessions.isEmpty()) {
            Text("No sessions yet. Tap \"New session\" to create one.")
        } else {
            LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                items(sessions, key = { it.sessionId }) { s ->
                    SessionRow(
                        session = s,
                        isActive = activeId == s.sessionId.toString(),
                        onUse = {
                            scope.launch { prefs.setActiveSessionId(s.sessionId.toString()) }
                        },
                        onEdit = { onEdit(s.sessionId) },
                    )
                }
            }
        }
    }
}

@Composable
private fun SessionRow(
    session: Session,
    isActive: Boolean,
    onUse: () -> Unit,
    onEdit: () -> Unit,
) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    session.title,
                    fontWeight = FontWeight.Bold,
                    modifier = Modifier.weight(1f),
                )
                if (isActive) Text("ACTIVE  ")
            }
            Text(
                "id ${session.sessionId.toString().take(8)}\u2026  " +
                    "created ${formatTimestamp(session.createdUnixMs)}  " +
                    "captures ${session.orderedCaptureIds.size}",
            )
            Text(
                "${session.kinematics.label()}  " +
                    "retain ${session.sightRetentionSeconds}s/${session.sightRetentionCapacity} " +
                    if (session.expectedToFail) " EXPECTED-FAIL" else "",
            )
            HorizontalDivider(modifier = Modifier.padding(vertical = 4.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (!isActive) {
                    Button(onClick = onUse) { Text("Use") }
                } else {
                    OutlinedButton(onClick = onUse) { Text("Use") }
                }
                TextButton(onClick = onEdit) { Text("Edit") }
            }
        }
    }
}

private fun Session.Kinematics.label(): String = when (this) {
    Session.Kinematics.Stationary -> "Stationary"
    is Session.Kinematics.MaxSpeedKn -> "Max ${"%.1f".format(kn)} kn"
}

private val DATE_SHORT_FMT = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm")
    .withZone(ZoneId.systemDefault())

private fun formatTimestamp(unixMs: Long): String =
    DATE_SHORT_FMT.format(Instant.ofEpochMilli(unixMs))

private fun formatDateShort(unixMs: Long): String =
    DATE_SHORT_FMT.format(Instant.ofEpochMilli(unixMs))
