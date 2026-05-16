package co.anomaly.bris.ui

import androidx.camera.core.ImageCapture
import androidx.camera.core.ImageCaptureException
import androidx.camera.core.Preview
import androidx.camera.core.UseCaseGroup
import androidx.camera.core.ViewPort
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import co.anomaly.bris.engine.CalibrationStore
import co.anomaly.bris.engine.CameraConstants
import co.anomaly.bris.engine.Exporter
import co.anomaly.bris.engine.LensCatalog
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.bris_ffi.runCalibration
import java.io.ByteArrayOutputStream
import java.util.concurrent.atomic.AtomicInteger

/**
 * Calibration capture + solve.
 *
 * The screen layout is split into three rigid regions so the
 * camera preview never collides with the controls:
 *
 *  ┌─ header (fixed) ────────────────────────┐
 *  │  Calibration · 9×6 @ 25.0mm · [Edit]    │
 *  ├─────────────────────────────────────────┤
 *  │                                         │
 *  │       Camera preview (fills middle)     │
 *  │                                         │
 *  ├─ status + actions (fixed) ──────────────┤
 *  │  status text                            │
 *  │  [Capture] [Solve] [New session] [Back] │
 *  │  [Save calibration locally] (debug)     │
 *  │  [Send calibration (debug)] (debug)     │
 *  └─────────────────────────────────────────┘
 *
 * Target dimensions (rows / cols / square mm) are edited
 * through a dialog rather than always-visible inputs so the
 * controls don't compete with the preview for vertical space
 * (the operator sets them once per session, not continuously).
 *
 * Camera FOV: ImageCapture is pinned to the same resolution
 * the streaming engine analyzes (CameraConstants.SIZE) and
 * preview + capture share a single ViewPort. The pixel grid
 * the operator sees in the preview is *exactly* the pixel
 * grid that lands in the calibration solver, and *exactly*
 * the pixel grid the engine analyzes during fix capture. This
 * is a load-bearing invariant: a calibration solved against
 * a different resolution silently produces wrong altitudes
 * when applied to live frames.
 */
@Composable
fun CalibrationScreen(
    debugMode: Boolean,
    lensId: String,
    onBack: () -> Unit,
    onSendCalibration: () -> Unit,
) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val scope = rememberCoroutineScope()
    val store = remember(context) { CalibrationStore.forApp(context) }
    val exporter = remember(context) { Exporter.forApp(context) }
    var sessionDir by remember(lensId) {
        mutableStateOf(store.newSession(lensId, CameraConstants.WIDTH, CameraConstants.HEIGHT))
    }
    val cameraSelector = remember(lensId) { LensCatalog.selectorFor(lensId) }

    var rows by remember { mutableStateOf(9) }
    var cols by remember { mutableStateOf(6) }
    var sizeMm by remember { mutableStateOf(25.0) }
    var captureCount by remember { mutableStateOf(0) }
    val captureSeq = remember { AtomicInteger(0) }
    var status by remember { mutableStateOf("Capture frames of a checkerboard from varied angles.") }
    var showTargetDialog by remember { mutableStateOf(false) }

    val imageCapture = remember {
        ImageCapture.Builder()
            .setResolutionSelector(
                ResolutionSelector.Builder()
                    .setResolutionStrategy(
                        ResolutionStrategy(
                            CameraConstants.SIZE,
                            ResolutionStrategy.FALLBACK_RULE_CLOSEST_LOWER_THEN_HIGHER,
                        ),
                    )
                    .build(),
            )
            .build()
    }

    Column(modifier = Modifier.fillMaxSize()) {
        // ---- header ----
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .background(Color(0xCC000000))
                .padding(horizontal = 12.dp, vertical = 8.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            Text(
                "Target: ${rows}×${cols} @ ${"%.1f".format(sizeMm)} mm",
                color = Color.White,
            )
            TextButton(onClick = { showTargetDialog = true }) {
                Text("Edit", color = Color(0xFF8AB4F8))
            }
        }

        // ---- camera preview (fills middle) ----
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .weight(1f),
        ) {
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
                        val viewport = ViewPort.Builder(
                            android.util.Rational(
                                CameraConstants.WIDTH,
                                CameraConstants.HEIGHT,
                            ),
                            preview.targetRotation,
                        )
                            .setScaleType(ViewPort.FIT)
                            .build()
                        val group = UseCaseGroup.Builder()
                            .setViewPort(viewport)
                            .addUseCase(preview)
                            .addUseCase(imageCapture)
                            .build()
                        provider.unbindAll()
                        provider.bindToLifecycle(
                            lifecycleOwner,
                            cameraSelector,
                            group,
                        )
                    }, ContextCompat.getMainExecutor(ctx))
                    previewView
                },
            )
        }

        // ---- status + actions (fixed) ----
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .background(Color(0xCC000000))
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text("$status  ·  Captured: $captureCount", color = Color.White)
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
                        if (rows <= 0 || cols <= 0 || sizeMm <= 0.0) {
                            status = "Set valid target dimensions first."
                            return@Button
                        }
                        status = "Solving…"
                        scope.launch {
                            store.writeTarget(sessionDir, rows, cols, sizeMm)
                            val result = withContext(Dispatchers.IO) {
                                runCatching {
                                    runCalibration(
                                        framesDir = java.io.File(sessionDir, "frames").absolutePath,
                                        rows = rows.toUInt(),
                                        cols = cols.toUInt(),
                                        squareSizeMm = sizeMm,
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
                        sessionDir = store.newSession(lensId, CameraConstants.WIDTH, CameraConstants.HEIGHT)
                        captureCount = 0
                        captureSeq.set(0)
                        status = "New session started."
                    },
                ) { Text("New session") }
                OutlinedButton(modifier = Modifier.weight(1f), onClick = onBack) { Text("Back") }
            }
            // ---- save / send (always available; the data is
            // operator-owned, sits in app-local external-files,
            // and can be transferred via adb pull regardless
            // of debug mode). The collector POST stays
            // debug-mode-gated because it leaves the device.
            Button(
                modifier = Modifier.fillMaxWidth(),
                enabled = captureCount > 0,
                onClick = {
                    scope.launch {
                        val dest = withContext(Dispatchers.IO) {
                            exporter.exportCalibrationSession(sessionDir)
                        }
                        status = "Saved to ${dest.absolutePath}"
                    }
                },
            ) { Text("Save calibration to phone") }
            if (debugMode) {
                Button(
                    modifier = Modifier.fillMaxWidth(),
                    enabled = captureCount > 0,
                    onClick = onSendCalibration,
                ) { Text("Send calibration to collector (debug)") }
            }
        }
    }

    if (showTargetDialog) {
        TargetDialog(
            initialRows = rows,
            initialCols = cols,
            initialSizeMm = sizeMm,
            onDismiss = { showTargetDialog = false },
            onConfirm = { r, c, s ->
                rows = r
                cols = c
                sizeMm = s
                showTargetDialog = false
            },
        )
    }
}

@Composable
private fun TargetDialog(
    initialRows: Int,
    initialCols: Int,
    initialSizeMm: Double,
    onDismiss: () -> Unit,
    onConfirm: (Int, Int, Double) -> Unit,
) {
    var rowsText by remember { mutableStateOf(initialRows.toString()) }
    var colsText by remember { mutableStateOf(initialCols.toString()) }
    var sizeText by remember { mutableStateOf(initialSizeMm.toString()) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Checkerboard target") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    value = rowsText,
                    onValueChange = { rowsText = it.filter(Char::isDigit) },
                    label = { Text("Inner rows") },
                    singleLine = true,
                )
                OutlinedTextField(
                    value = colsText,
                    onValueChange = { colsText = it.filter(Char::isDigit) },
                    label = { Text("Inner cols") },
                    singleLine = true,
                )
                OutlinedTextField(
                    value = sizeText,
                    onValueChange = { sizeText = it.filter { c -> c.isDigit() || c == '.' } },
                    label = { Text("Square size (mm)") },
                    singleLine = true,
                )
                Spacer(Modifier.height(4.dp))
                Text(
                    "Inner-corner counts: a 10×7 squares board has 9×6 inner corners.",
                    color = Color.Gray,
                )
            }
        },
        confirmButton = {
            TextButton(onClick = {
                val r = rowsText.toIntOrNull() ?: 0
                val c = colsText.toIntOrNull() ?: 0
                val s = sizeText.toDoubleOrNull() ?: 0.0
                if (r > 0 && c > 0 && s > 0.0) onConfirm(r, c, s)
            }) {
                Text("OK")
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
        },
    )
}

/** Minimum frames before "Solve" is enabled. Zhang's method
 *  needs ≥ 3; we ask for 5 for pose dispersion. */
private const val MIN_FRAMES_FOR_SOLVE = 5

/**
 * Take a single JPEG snapshot via [`ImageCapture.takePicture`]
 * and hand the bytes to `onResult`. Uses the in-memory output
 * variant rather than the file-output variant; we want to
 * control the on-disk filename layout ourselves.
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
                    val plane = image.planes[0]
                    val buf = plane.buffer
                    val bytes = ByteArray(buf.remaining())
                    buf.get(bytes)
                    val jpeg = if (image.format == android.graphics.ImageFormat.JPEG) {
                        bytes
                    } else {
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
