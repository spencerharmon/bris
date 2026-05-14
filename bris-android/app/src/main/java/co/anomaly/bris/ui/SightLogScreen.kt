package co.anomaly.bris.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import co.anomaly.bris.engine.SightLog
import org.json.JSONObject
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * List view of captured sight-log entries.
 *
 * Reads `<external-files>/sights/` via [`SightLog.list`] and
 * renders one row per entry, sorted oldest-first (ULID-by-name
 * sort = chronological). Each row shows the entry's submission
 * id, captured-at timestamp from the manifest, and the fix
 * verdict / reason. Tapping a row opens
 * [`SightLogDetailScreen`].
 *
 * Soft-deleted entries (under `.trash/`) are not surfaced.
 */
@Composable
fun SightLogScreen(
    onBack: () -> Unit,
    onOpen: (String) -> Unit,
) {
    val context = LocalContext.current
    val sightLog = remember(context) { SightLog.forApp(context) }
    var rows by remember { mutableStateOf<List<SightLogRow>>(emptyList()) }
    LaunchedEffect(sightLog) {
        rows = sightLog.list().mapNotNull { dir -> SightLogRow.fromDir(dir) }
    }

    Column(modifier = Modifier.fillMaxSize().padding(16.dp)) {
        Text(
            "Sight log (${rows.size})",
            style = androidx.compose.material3.MaterialTheme.typography.titleLarge,
        )
        if (rows.isEmpty()) {
            Text(
                "No captures yet. Use Start capture on the live screen.",
                modifier = Modifier.padding(top = 12.dp),
            )
        } else {
            LazyColumn(
                modifier = Modifier.fillMaxWidth().padding(top = 8.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                items(rows) { row ->
                    Column(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(vertical = 4.dp),
                    ) {
                        Text(
                            "${row.capturedAt}   ${row.verdictLabel}",
                            style = androidx.compose.material3.MaterialTheme.typography.bodyLarge,
                        )
                        Text(
                            row.summary,
                            style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
                        )
                        OutlinedButton(onClick = { onOpen(row.dirName) }) {
                            Text("Open ${row.dirName.take(8)}…")
                        }
                        HorizontalDivider()
                    }
                }
            }
        }
        OutlinedButton(onClick = onBack, modifier = Modifier.padding(top = 12.dp)) {
            Text("Back")
        }
    }
}

/**
 * One sight-log entry's detail view: dump the manifest's fix
 * summary + media listing, with delete-images and
 * delete-entry affordances.
 *
 * Spike-grade: text dump only. A future commit adds frame
 * thumbnails, a map preview of the lat/lon + uncertainty
 * ellipse, and a "Send to collector" button (debug-mode only).
 * For now the operator can also pull the entry directly via
 * `adb pull` from the path shown at the top of the screen.
 */
@Composable
fun SightLogDetailScreen(
    dirName: String,
    onBack: () -> Unit,
    onDeleted: () -> Unit,
) {
    val context = LocalContext.current
    val sightLog = remember(context) { SightLog.forApp(context) }
    val sessionDir = remember(context, dirName) { File(sightLog.list().firstOrNull { it.name == dirName }?.absolutePath ?: "/dev/null") }
    var manifestJson by remember { mutableStateOf<String?>(null) }
    var mediaFiles by remember { mutableStateOf<List<File>>(emptyList()) }
    var statusText by remember { mutableStateOf<String?>(null) }

    LaunchedEffect(sessionDir) {
        manifestJson = readOrNull(File(sessionDir, "manifest.json"))
        val mediaDir = File(sessionDir, "media")
        mediaFiles = mediaDir.listFiles()?.sortedBy { it.name } ?: emptyList()
    }

    Column(modifier = Modifier.fillMaxSize().padding(16.dp)) {
        Text(
            "Sight: $dirName",
            style = androidx.compose.material3.MaterialTheme.typography.titleLarge,
        )
        Text(
            sessionDir.absolutePath,
            style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
            modifier = Modifier.padding(top = 4.dp),
        )
        statusText?.let { Text(it, modifier = Modifier.padding(top = 8.dp)) }

        Text(
            "Manifest",
            style = androidx.compose.material3.MaterialTheme.typography.titleMedium,
            modifier = Modifier.padding(top = 12.dp),
        )
        Text(
            manifestJson ?: "(no manifest)",
            style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
            modifier = Modifier.padding(top = 4.dp),
        )

        Text(
            "Media (${mediaFiles.size})",
            style = androidx.compose.material3.MaterialTheme.typography.titleMedium,
            modifier = Modifier.padding(top = 12.dp),
        )
        for (f in mediaFiles.take(MAX_MEDIA_LISTED)) {
            Text(
                "${f.name}  (${f.length()} bytes)",
                style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
            )
        }
        if (mediaFiles.size > MAX_MEDIA_LISTED) {
            Text(
                "… and ${mediaFiles.size - MAX_MEDIA_LISTED} more.",
                style = androidx.compose.material3.MaterialTheme.typography.bodySmall,
            )
        }

        Column(
            modifier = Modifier.padding(top = 16.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedButton(onClick = {
                val n = sightLog.deleteImages(sessionDir)
                statusText = "Deleted $n image file(s); manifest + diagnostics retained."
                mediaFiles = File(sessionDir, "media").listFiles()?.sortedBy { it.name } ?: emptyList()
            }) {
                Text("Delete images only")
            }
            Button(onClick = {
                if (sightLog.softDelete(sessionDir)) {
                    onDeleted()
                } else {
                    statusText = "Delete failed."
                }
            }) {
                Text("Delete entry")
            }
            OutlinedButton(onClick = onBack) { Text("Back") }
        }
    }
}

private const val MAX_MEDIA_LISTED = 50

private data class SightLogRow(
    val dirName: String,
    val capturedAt: String,
    val verdictLabel: String,
    val summary: String,
) {
    companion object {
        fun fromDir(dir: File): SightLogRow? {
            val manifestFile = File(dir, "manifest.json")
            if (!manifestFile.exists()) return null
            val text = readOrNull(manifestFile) ?: return null
            return try {
                val json = JSONObject(text)
                val capturedAt = json.optString("captured_at", "—")
                val fix = json.optJSONObject("fix")
                val verdict = fix?.optString("verdict", "—") ?: "—"
                val summary = if (fix != null) {
                    val n = fix.optLong("n_sights", -1)
                    val sigma = fix.optDouble("sigma_major_nm", Double.NaN)
                    if (sigma.isNaN()) {
                        "outcome: ${fix.optString("session_outcome", "?")}"
                    } else {
                        "σ=${"%.2f".format(sigma)} nm  sights=$n"
                    }
                } else {
                    "(malformed manifest)"
                }
                SightLogRow(
                    dirName = dir.name,
                    capturedAt = formatIso(capturedAt),
                    verdictLabel = verdict.uppercase(),
                    summary = summary,
                )
            } catch (_: Throwable) {
                null
            }
        }
    }
}

private fun readOrNull(f: File): String? = try {
    if (f.exists()) f.readText() else null
} catch (_: Throwable) {
    null
}

private fun formatIso(iso: String): String = try {
    val ms = java.time.Instant.parse(iso).toEpochMilli()
    SimpleDateFormat("yyyy-MM-dd HH:mm", Locale.US).format(Date(ms))
} catch (_: Throwable) {
    iso
}
