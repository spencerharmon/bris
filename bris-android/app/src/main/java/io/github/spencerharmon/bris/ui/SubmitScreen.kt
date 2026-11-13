package io.github.spencerharmon.bris.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import io.github.spencerharmon.bris.upload.CollectorSettings
import io.github.spencerharmon.bris.upload.PendingReview
import io.github.spencerharmon.bris.upload.Upload
import io.github.spencerharmon.bris.upload.UploadCoordinator
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * "Submit to collector…": the one-screen pre-upload review of
 * exactly what will leave the device. Nothing is queued until the
 * operator presses Approve; the queue is then flushed by a
 * network-constrained background worker.
 */
@Composable
fun SubmitScreen(bundleDir: String, onBack: () -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val settings by remember(context) { Upload.settings(context) }.flow
        .collectAsState(initial = CollectorSettings())
    var coordinator by remember { mutableStateOf<UploadCoordinator?>(null) }
    var review by remember { mutableStateOf<PendingReview?>(null) }
    var status by remember { mutableStateOf<String?>(null) }
    var counts by remember { mutableStateOf(0 to 0) }

    LaunchedEffect(bundleDir) {
        try {
            withContext(Dispatchers.IO) {
                val c = Upload.coordinator(context)
                coordinator = c
                counts = c.counts()
                review = c.review(bundleDir)
            }
        } catch (t: Throwable) {
            status = "Cannot prepare submission: ${t.message}"
        }
    }

    Column(
        modifier = Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("Submit to collector", style = MaterialTheme.typography.titleLarge)
        if (!settings.configured) {
            Text("Collector not configured: Settings \u2192 Debug mode \u2192 Collector.")
        } else {
            Text("Collector: ${settings.baseUrl}" + if (settings.registered) " (device registered)" else "")
        }
        Text("Queue: ${counts.first} pending, ${counts.second} rejected")
        settings.lastError?.let { Text("Last error: $it") }
        review?.let { r ->
            Text("Kind: ${r.kind}", style = MaterialTheme.typography.titleMedium)
            for (line in r.lines) Text("${line.label}: ${line.value}", style = MaterialTheme.typography.bodySmall)
        }
        status?.let { Text(it) }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(
                enabled = review != null && settings.configured,
                onClick = {
                    val r = review ?: return@Button
                    val c = coordinator ?: return@Button
                    scope.launch {
                        try {
                            val id = withContext(Dispatchers.IO) { c.approve(r) }
                            review = null
                            Upload.scheduleFlush(context)
                            counts = withContext(Dispatchers.IO) { c.counts() }
                            status = "Queued $id; uploading in background."
                        } catch (t: Throwable) {
                            status = "Enqueue failed: ${t.message}"
                        }
                    }
                },
            ) { Text("Approve & upload") }
            OutlinedButton(onClick = onBack) { Text("Cancel") }
        }
    }
}
