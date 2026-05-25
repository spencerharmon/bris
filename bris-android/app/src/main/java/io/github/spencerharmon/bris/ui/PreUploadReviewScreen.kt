package io.github.spencerharmon.bris.ui

import android.widget.Toast
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import io.github.spencerharmon.bris.BuildConfig
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.engine.CalibrationStore
import io.github.spencerharmon.bris.engine.DebugBufferActions
import io.github.spencerharmon.bris.engine.DebugCaptureBuffer
import io.github.spencerharmon.bris.engine.Exporter
import io.github.spencerharmon.bris.engine.SaveResult
import io.github.spencerharmon.bris.location.CoarseLocation
import io.github.spencerharmon.bris.upload.ManifestBuilder
import io.github.spencerharmon.bris.upload.MediaPart
import io.github.spencerharmon.bris.upload.MediaSummary
import io.github.spencerharmon.bris.upload.SubmitResult
import io.github.spencerharmon.bris.upload.Submitter
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import uniffi.bris_ffi.version
import java.time.Instant

/**
 * One-screen pre-upload review.
 *
 * Per AGENTS.md: every send action shows a one-screen review of
 * exactly what is about to leave the device. Send and Cancel are
 * the only two buttons. There is no per-field opt-out — debug-
 * mode-on is the consent.
 *
 * Spike scaffold today: the manifest is built from in-memory
 * metadata (versions, device, kind-specific JSON summary). The
 * full retained-frames + `$PBRIS` log payload arrives once the
 * on-device debug-capture buffer is wired (next commit).
 */
@Composable
fun PreUploadReviewScreen(
    kind: String,
    onBack: () -> Unit,
    onSend: () -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val prefs = androidx.compose.runtime.remember(context) { Prefs(context) }
    val collectorBase by prefs.collectorBaseFlow.collectAsState(initial = "")
    val debugBuffer = androidx.compose.runtime.remember(context) {
        DebugCaptureBuffer.forApp(context)
    }
    // Snapshot the most recent buffered frames once when the
    // review screen opens; the operator's review is against
    // *these* bytes, not whatever the buffer holds at Send time.
    val entries = androidx.compose.runtime.remember(debugBuffer) {
        debugBuffer.recentEntries(limit = MAX_FRAMES_PER_SUBMISSION)
    }
    // Same one-shot snapshot semantics for GPS: capture-time =
    // review-screen-open-time. `null` if no permission, no
    // provider, or no cached fix.
    val gps = androidx.compose.runtime.remember(context) { CoarseLocation.getLastKnown(context) }
    var note by remember { mutableStateOf("") }

    Column(
        modifier = Modifier.fillMaxSize().padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("Review submission: $kind")
        Text("Collector: ${if (collectorBase.isBlank()) "(not configured)" else collectorBase}")
        Text("The following will be uploaded:")
        Text("• Manifest (timestamps, device, versions)")
        Text("• ${entries.size} debug-capture frame(s) + diagnostics")
        Text("• \$PBRIS log window (when wired)")
        Text(
            "• GPS: " + (gps?.let { "${it.latDeg}, ${it.lonDeg} ±${it.horizontalAccuracyM} m (${it.source})" }
                ?: "(none — no permission or no cached fix)"),
        )
        Text("• Operator note (below)")

        OutlinedTextField(
            value = note,
            onValueChange = { note = it },
            label = { Text("Note (optional, plaintext)") },
            modifier = Modifier.fillMaxWidth(),
            minLines = 2,
            maxLines = 5,
        )

        Button(
            enabled = BuildConfig.ENABLE_REMOTE_SUBMIT && collectorBase.isNotBlank(),
            onClick = {
                scope.launch {
                    val deviceUuid = prefs.deviceUuid()
                    val coreVer = version().brisFfi
                    val builder = ManifestBuilder(
                        deviceUuid = deviceUuid,
                        appVersion = BuildConfig.BRIS_APP_VERSION,
                        brisCoreVersion = coreVer,
                    )
                    // Build media list. For "fix" / "debug_capture" we
                    // pull from the debug-capture buffer's snapshotted
                    // entries. For "calibration" we pull from the
                    // latest CalibrationStore session.
                    val media = mutableListOf<MediaSummary>()
                    val parts = mutableListOf<MediaPart>()

                    if (kind == "calibration") {
                        val calStore = CalibrationStore.forApp(context)
                        val sess = calStore.latestSession()
                        if (sess != null) {
                            for (f in calStore.framesIn(sess)) {
                                val bytes = withContext(Dispatchers.IO) { f.readBytes() }
                                media.add(
                                    MediaSummary(
                                        filename = f.name,
                                        role = "calibration_frame",
                                        sizeBytes = bytes.size.toLong(),
                                    )
                                )
                                parts.add(MediaPart(f.name, "image/jpeg", bytes))
                            }
                            val intrinsicsFile = java.io.File(sess, "intrinsics.json")
                            if (intrinsicsFile.exists()) {
                                val b = withContext(Dispatchers.IO) { intrinsicsFile.readBytes() }
                                media.add(
                                    MediaSummary(
                                        filename = "intrinsics.json",
                                        role = "calibration_intrinsics",
                                        sizeBytes = b.size.toLong(),
                                    )
                                )
                                parts.add(MediaPart("intrinsics.json", "application/json", b))
                            }
                            val targetFile = java.io.File(sess, "target.json")
                            if (targetFile.exists()) {
                                val b = withContext(Dispatchers.IO) { targetFile.readBytes() }
                                media.add(
                                    MediaSummary(
                                        filename = "target.json",
                                        role = "calibration_target",
                                        sizeBytes = b.size.toLong(),
                                    )
                                )
                                parts.add(MediaPart("target.json", "application/json", b))
                            }
                        }
                    } else {
                        for (e in entries) {
                            val frameName = "frame_${"%012d".format(e.seq)}.pgm"
                            val snapName = "frame_${"%012d".format(e.seq)}.json"
                            val frameBytes = withContext(Dispatchers.IO) { e.framePath.readBytes() }
                            val snapBytes = withContext(Dispatchers.IO) { e.snapshotPath.readBytes() }
                            media.add(
                                MediaSummary(
                                    filename = frameName,
                                    role = "fix_frame",
                                    sizeBytes = frameBytes.size.toLong(),
                                    frameIndex = e.seq.toInt(),
                                    capturedAt = e.capturedAt(),
                                )
                            )
                            media.add(
                                MediaSummary(
                                    filename = snapName,
                                    role = "frame_diagnostic",
                                    sizeBytes = snapBytes.size.toLong(),
                                    frameIndex = e.seq.toInt(),
                                    capturedAt = e.capturedAt(),
                                )
                            )
                            parts.add(MediaPart(frameName, "image/x-portable-graymap", frameBytes))
                            parts.add(MediaPart(snapName, "application/json", snapBytes))
                        }
                        // Attach the rolling $PBRIS log if present.
                        val pbrisFile = java.io.File(context.filesDir, "debug-capture/pbris.log")
                        if (pbrisFile.exists() && pbrisFile.length() > 0) {
                            val pbrisBytes = withContext(Dispatchers.IO) { pbrisFile.readBytes() }
                            media.add(
                                MediaSummary(
                                    filename = "pbris.log",
                                    role = "pbris_log",
                                    sizeBytes = pbrisBytes.size.toLong(),
                                )
                            )
                            parts.add(MediaPart("pbris.log", "text/plain", pbrisBytes))
                        }
                    }

                    val summary = JSONObject().put(
                        "note",
                        "spike: kind=$kind frames=${entries.size}",
                    )
                    val manifestJson = when (kind) {
                        "fix" -> builder.fix(
                            capturedAt = entries.firstOrNull()?.capturedAt() ?: Instant.now(),
                            gps = gps,
                            note = note.takeIf { it.isNotBlank() },
                            fixSummary = summary,
                            media = media,
                        )
                        "calibration" -> builder.calibration(
                            capturedAt = Instant.now(),
                            gps = gps,
                            note = note.takeIf { it.isNotBlank() },
                            calibrationSummary = summary,
                            media = media,
                        )
                        else -> builder.debugCapture(
                            capturedAt = entries.firstOrNull()?.capturedAt() ?: Instant.now(),
                            gps = gps,
                            note = note.takeIf { it.isNotBlank() },
                            debugSummary = summary,
                            media = media,
                        )
                    }

                    val token = BuildConfig.BRIS_COLLECTOR_BEARER_TOKEN
                    val submitter = Submitter(collectorBase, token)
                    val result = withContext(Dispatchers.IO) {
                        submitter.submit(manifestJson, parts)
                    }
                    val msg = when (result) {
                        is SubmitResult.Accepted -> "submitted: ${result.id}"
                        is SubmitResult.Rejected -> "rejected ${result.statusCode}: ${result.detail.take(120)}"
                        is SubmitResult.TransientFailure -> "failed: ${result.message.take(120)}"
                    }
                    Toast.makeText(context, msg, Toast.LENGTH_LONG).show()
                    if (result is SubmitResult.Accepted) onSend()
                }
            },
        ) {
            if (BuildConfig.ENABLE_REMOTE_SUBMIT) {
                Text("Send")
            } else {
                Text("Send (disabled in this build)")
            }
        }
        Button(
            // Local save mirrors the on-device debug buffer to
            // the operator's chosen Storage Access Framework
            // destination via DebugBufferActions (PR #1 of the
            // debug-UI fix). Available regardless of debug mode
            // because saving locally does not transmit anything
            // off-device; collector upload stays gated by
            // BuildConfig.ENABLE_REMOTE_SUBMIT above.
            onClick = {
                scope.launch {
                    val savedUriStr = prefs.debugSaveLocationFlow.first()
                    if (kind == "calibration") {
                        val sess = CalibrationStore.forApp(context).latestSession()
                        val dest = withContext(Dispatchers.IO) {
                            sess?.let { Exporter.forApp(context).exportCalibrationSession(it) }
                        }
                        val msg = dest?.let { "Saved to ${it.absolutePath}" }
                            ?: "Nothing to save (no calibration session)."
                        Toast.makeText(context, msg, Toast.LENGTH_LONG).show()
                    } else {
                        val uri = savedUriStr?.let { android.net.Uri.parse(it) }
                        if (uri == null) {
                            Toast.makeText(
                                context,
                                "Pick a save location in Settings \u2192 Change save location first.",
                                Toast.LENGTH_LONG,
                            ).show()
                        } else {
                            val r = DebugBufferActions.saveAll(context, debugBuffer, uri)
                            val msg = when (r) {
                                is SaveResult.Ok -> "Saved ${r.frameCount} frames to ${r.destinationDisplay}"
                                is SaveResult.Failed -> "Save failed: ${r.message}"
                                SaveResult.NeedLocation -> "Pick a save location."
                            }
                            Toast.makeText(context, msg, Toast.LENGTH_LONG).show()
                        }
                    }
                }
            },
        ) { Text("Save to phone") }
        OutlinedButton(onClick = onBack) { Text("Cancel") }
    }
}

/**
 * Hard cap on per-submission frame count to keep upload size
 * bounded. At 1280×720 PGM (≈900 KiB/frame) plus snapshot JSON,
 * 60 frames ≈ 55 MiB — comfortably under the collector's 512
 * MiB request limit and a phone-uplink-friendly transfer.
 */
private const val MAX_FRAMES_PER_SUBMISSION = 60
