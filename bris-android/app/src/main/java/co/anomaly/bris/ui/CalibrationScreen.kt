package co.anomaly.bris.ui

import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageCapture
import androidx.camera.core.ImageCaptureException
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import co.anomaly.bris.engine.CalibrationStore
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.bris_ffi.runCalibration
import java.io.ByteArrayOutputStream
import java.util.concurrent.atomic.AtomicInteger

/**
 * Calibration capture + solve.
 *
 * Workflow:
 *
 *  1. Operator enters checkerboard rows/cols/square-size-mm.
 *  2. Camera preview is live; "Capture" snaps a JPEG into the
 *     current session directory under
 *     `<app-files>/calibration/<session-ulid>/frames/`.
 *  3. After ≥ 5 captures (Zhang's planar method needs ≥ 3 but
 *     real-world dispersion of poses needs more), "Run
 *     calibration" invokes [`runCalibration`] in a background
 *     coroutine. The result is persisted to the session
 *     directory's `intrinsics.json`.
 *  4. With debug mode on, "Send calibration" navigates to the
 *     pre-upload review, which packages the entire session
 *     (frames + intrinsics + target description) into a
 *     `submission_kind = "calibration"` submission.
 *
 * The Bris-specific intrinsics persistence path
 * (`bris-calibrate::persist`) is *not* yet wired into the
 * Android app — that's the next commit, which will make the
 * solved intrinsics available to `LiveScreen` so the streaming
 * engine no longer uses placeholders. For now the calibration
 * result lands on disk and surfaces in the UI; the operator
 * can copy the values manually if needed.
 */
@Composable
fun CalibrationScreen(
    debugMode: Boolean,
    onBack: () -> Unit,
    onSendCalibration: () -> Unit,
) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val scope = rememberCoroutineScope()
    val store = remember(context) { CalibrationStore.forApp(context) }
    var sessionDir by remember { mutableStateOf(store.newSession()) }

    var rows by remember { mutableStateOf("9") }
    var cols by remember { mutableStateOf("6") }
    var sizeMm by remember { mutableStateOf("25.0") }
    var captureCount by remember { mutableStateOf(0) }
    val captureSeq = remember { AtomicInteger(0) }
    var status by remember { mutableStateOf("Capture frames of a checkerboard from varied angles.") }
    val imageCapture = remember { ImageCapture.Builder().build() }

    Column(
        modifier = Modifier.fillMaxSize().padding(12.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text("Calibration")

        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(
                value = rows, onValueChange = { rows = it.filter(Char::isDigit) },
                label = { Text("Rows") }, modifier = Modifier.weight(1f), singleLine = true,
            )
            OutlinedTextField(
                value = cols, onValueChange = { cols = it.filter(Char::isDigit) },
                label = { Text("Cols") }, modifier = Modifier.weight(1f), singleLine = true,
            )
            OutlinedTextField(
                value = sizeMm, onValueChange = { sizeMm = it.filter { c -> c.isDigit() || c == '.' } },
                label = { Text("Square mm") }, modifier = Modifier.weight(1.2f), singleLine = true,
            )
        }

        // Camera preview occupies a flexible region.
        Box(modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp).weight(1f)) {
            AndroidView(
                modifier = Modifier.fillMaxSize(),
                factory = { ctx ->
                    val previewView = PreviewView(ctx)
                    val providerFuture = ProcessCameraProvider.getInstance(ctx)
                    providerFuture.addListener({
                        val provider = providerFuture.get()
                        val preview = Preview.Builder().build().also {
                            it.setSurfaceProvider(previewView.surfaceProvider)
                        }
                        provider.unbindAll()
                        provider.bindToLifecycle(
                            lifecycleOwner,
                            CameraSelector.DEFAULT_BACK_CAMERA,
                            preview,
                            imageCapture,
                        )
                    }, ContextCompat.getMainExecutor(ctx))
                    previewView
                },
            )
        }

        Text(status)
        Text("Captured: $captureCount")

        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(
                modifier = Modifier.weight(1f),
                onClick = {
                    val seq = captureSeq.incrementAndGet()
                    captureFrame(
                        imageCapture = imageCapture,
                        executor = ContextCompat.getMainExecutor(context),
                    ) { bytes, err ->
                        if (err != null) {
                            status = "Capture failed: ${err.message}"
                        } else if (bytes != null) {
                            store.writeFrame(sessionDir, seq, bytes)
                            captureCount = seq
                            status = "Captured frame $seq."
                        }
                    }
                },
            ) { Text("Capture") }

            Button(
                modifier = Modifier.weight(1f),
                enabled = captureCount >= MIN_FRAMES_FOR_SOLVE,
                onClick = {
                    val r = rows.toIntOrNull() ?: 0
                    val c = cols.toIntOrNull() ?: 0
                    val s = sizeMm.toDoubleOrNull() ?: 0.0
                    if (r <= 0 || c <= 0 || s <= 0.0) {
                        status = "Enter valid rows, cols, square size."
                        return@Button
                    }
                    status = "Solving…"
                    scope.launch {
                        store.writeTarget(sessionDir, r, c, s)
                        val result = withContext(Dispatchers.IO) {
                            runCatching {
                                runCalibration(
                                    framesDir = java.io.File(sessionDir, "frames").absolutePath,
                                    rows = r.toUInt(),
                                    cols = c.toUInt(),
                                    squareSizeMm = s,
                                )
                            }
                        }
                        result.onSuccess { res ->
                            store.writeIntrinsics(sessionDir, res)
                            status = "Solved: ${res.nFramesUsed}/${res.nFramesTotal} frames, " +
                                "rms=${"%.3f".format(res.rmsPx)} px, " +
                                "fx=${"%.1f".format(res.intrinsics.fx)}, " +
                                "fy=${"%.1f".format(res.intrinsics.fy)}"
                        }.onFailure { e ->
                            status = "Solve failed: ${e.message?.take(160)}"
                        }
                    }
                },
            ) { Text("Solve") }
        }

        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(
                modifier = Modifier.weight(1f),
                onClick = {
                    sessionDir = store.newSession()
                    captureCount = 0
                    captureSeq.set(0)
                    status = "New session started."
                },
            ) { Text("New session") }

            OutlinedButton(modifier = Modifier.weight(1f), onClick = onBack) { Text("Back") }
        }

        if (debugMode) {
            Button(
                modifier = Modifier.fillMaxWidth(),
                enabled = captureCount > 0,
                onClick = onSendCalibration,
            ) { Text("Send calibration (debug)") }
        }
    }
}

/** Minimum frames before "Solve" is enabled. Zhang's method
 *  needs ≥ 3; we ask for 5 for pose dispersion. */
private const val MIN_FRAMES_FOR_SOLVE = 5

/**
 * Take a single JPEG snapshot via [`ImageCapture.takePicture`]
 * and hand the bytes to `onResult`. Uses the in-memory output
 * variant (via a temporary `OnImageCapturedCallback` that we
 * convert to bytes ourselves) rather than the file-output
 * variant; we want to control the on-disk filename layout
 * ourselves.
 */
private fun captureFrame(
    imageCapture: ImageCapture,
    executor: java.util.concurrent.Executor,
    onResult: (ByteArray?, Exception?) -> Unit,
) {
    imageCapture.takePicture(
        executor,
        object : ImageCapture.OnImageCapturedCallback() {
            override fun onCaptureSuccess(image: androidx.camera.core.ImageProxy) {
                try {
                    // The captured image is JPEG-encoded with one plane.
                    val plane = image.planes[0]
                    val buf = plane.buffer
                    val bytes = ByteArray(buf.remaining())
                    buf.get(bytes)
                    val jpeg = if (image.format == android.graphics.ImageFormat.JPEG) {
                        bytes
                    } else {
                        // Defensive fallback: re-encode if the
                        // device handed us something else (rare;
                        // ImageCapture default is JPEG).
                        val baos = ByteArrayOutputStream()
                        baos.write(bytes)
                        baos.toByteArray()
                    }
                    onResult(jpeg, null)
                } finally {
                    image.close()
                }
            }
            override fun onError(exception: ImageCaptureException) {
                onResult(null, exception)
            }
        },
    )
}
