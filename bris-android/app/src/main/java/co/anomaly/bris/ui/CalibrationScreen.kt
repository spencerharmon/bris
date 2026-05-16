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
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.Alignment
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
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
import uniffi.bris_ffi.FfiCalibrationResult
import uniffi.bris_ffi.FfiDiagnosisIssue
import uniffi.bris_ffi.FfiDiagnosisLevel
import uniffi.bris_ffi.FfiFrameOutcome
import uniffi.bris_ffi.detectCalibrationFrame
import uniffi.bris_ffi.runCalibration
import java.io.ByteArrayOutputStream
import java.util.concurrent.atomic.AtomicInteger

/**
 * Calibration capture + solve.
 *
 * Interactive flow (per `progress.md` "calibration UX
 * overhaul"):
 *
 *  1. Operator taps **Capture**.
 *  2. The JPEG is written to `frames/frame_NNNN.jpg`.
 *  3. The bytes are immediately fed to the FFI's
 *     `detectCalibrationFrame()` on a background dispatcher.
 *  4. The result is rendered as a colored chip:
 *       - green: ✓ Detected · N corners · sharpness=…
 *       - amber: ⚠ Wrong grid (found AxB, expected RxC)
 *       - red:   ✗ No board (frame auto-discarded into
 *                              `frames/rejected/`)
 *       - red:   ✗ Decode failed (also auto-discarded)
 *  5. The running tally chip ("Good 12 · NoBoard 3 · Wrong 1")
 *     gives an at-a-glance "am I getting good captures?"
 *     read.
 *  6. Operator may explicitly **Discard last** (moves the
 *     last accepted frame to `rejected/`) if they want to
 *     reject a frame the auto-classifier kept.
 *  7. **Solve** runs the full pipeline; the result panel
 *     renders aggregate stats *plus* a list of diagnosis
 *     cards (warn / error) with operator-actionable
 *     remediation.
 *
 * "No board" auto-rejection: per operator request, frames
 * that the detector can't find a board in are immediately
 * moved into `frames/rejected/` so they don't pollute the
 * solve. If many consecutive captures auto-reject, that
 * itself is a UX signal — the chip and tally make it
 * obvious *why* (e.g., "all 8 of your last captures had no
 * board found; check focus / target / lighting").
 *
 * Camera FOV: ImageCapture is pinned to the same resolution
 * the streaming engine analyzes (CameraConstants.SIZE) and
 * preview + capture share a single ViewPort. The pixel grid
 * the operator sees in the preview is *exactly* the pixel
 * grid that lands in the calibration solver, and *exactly*
 * the pixel grid the engine analyzes during fix capture.
 */
@Composable
@Suppress("LongMethod")
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
    val captureSeq = remember { AtomicInteger(0) }
    var status by remember {
        mutableStateOf("Capture frames of a checkerboard from varied angles.")
    }
    var lastOutcome by remember { mutableStateOf<FrameFeedback?>(null) }
    val tally = remember { mutableStateOf(CaptureTally()) }
    var lastAcceptedSeq by remember { mutableStateOf<Int?>(null) }
    var solveResult by remember { mutableStateOf<FfiCalibrationResult?>(null) }
    var solving by remember { mutableStateOf(false) }
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

    fun resetSession() {
        sessionDir = store.newSession(lensId, CameraConstants.WIDTH, CameraConstants.HEIGHT)
        captureSeq.set(0)
        tally.value = CaptureTally()
        lastOutcome = null
        lastAcceptedSeq = null
        solveResult = null
        status = "New session started."
    }

    Box(modifier = Modifier.fillMaxSize()) {
        // ---- camera preview (fills the whole screen; controls overlay it) ----
        //
        // Mirrors LiveScreen's layout: the preview is the
        // root and *every* control is a translucent overlay
        // on top. Earlier this screen wrapped the preview in
        // a Column with a fixed-height header above and a
        // status panel below, which squeezed the 16:9
        // ViewPort into a near-square slot and made the
        // preview look like a square crop. The captured
        // pixels were always the full 16:9 frame; the
        // squeeze was purely a layout artifact, but
        // visually misleading. Root-level preview + capped
        // bottom overlay restores parity with LiveScreen.
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

        // ---- header (top overlay) ----
        Row(
            modifier = Modifier
                .align(Alignment.TopCenter)
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

        // ---- status + actions (bottom overlay; capped so the
        //      preview keeps most of the screen) ----
        Column(
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .heightIn(max = 360.dp)
                .background(Color(0xCC000000))
                .padding(12.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text(status, color = Color.White)
            TallyChip(tally.value)
            lastOutcome?.let { OutcomeChip(it) }

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
                                return@captureFrame
                            }
                            if (bytes == null) return@captureFrame
                            store.writeFrame(sessionDir, seq, bytes)
                            status = "Captured frame $seq — analyzing…"
                            scope.launch {
                                val outcome = withContext(Dispatchers.Default) {
                                    runCatching {
                                        detectCalibrationFrame(
                                            jpegBytes = bytes,
                                            rows = rows.toUInt(),
                                            cols = cols.toUInt(),
                                            squareSizeMm = sizeMm,
                                        )
                                    }
                                }
                                outcome.onSuccess { o ->
                                    val fb = describeOutcome(seq, o, rows, cols)
                                    lastOutcome = fb
                                    val newTally = tally.value.bump(fb.kind)
                                    tally.value = newTally
                                    if (fb.autoReject) {
                                        // Auto-discard noisy frames so the
                                        // operator doesn't have to babysit.
                                        store.rejectFrame(sessionDir, seq, fb.kind.code)
                                        status = "Frame $seq rejected: ${fb.short}"
                                    } else {
                                        lastAcceptedSeq = seq
                                        status = "Frame $seq kept: ${fb.short}"
                                    }
                                }.onFailure { e ->
                                    lastOutcome = FrameFeedback(
                                        seq = seq,
                                        kind = OutcomeKind.DECODE_FAILED,
                                        short = "analyzer error",
                                        long = "Detector errored: ${e.message?.take(160)}",
                                        autoReject = true,
                                    )
                                    tally.value = tally.value.bump(OutcomeKind.DECODE_FAILED)
                                    store.rejectFrame(sessionDir, seq, "analyzer_error")
                                    status = "Frame $seq rejected: analyzer error"
                                }
                            }
                        }
                    },
                ) { Text("Capture") }

                OutlinedButton(
                    modifier = Modifier.weight(1f),
                    enabled = lastAcceptedSeq != null,
                    onClick = {
                        val s = lastAcceptedSeq ?: return@OutlinedButton
                        store.rejectFrame(sessionDir, s, "manual")
                        tally.value = tally.value.demote()
                        lastAcceptedSeq = null
                        status = "Discarded frame $s."
                    },
                ) { Text("Discard last") }
            }

            Button(
                modifier = Modifier.fillMaxWidth(),
                enabled = !solving && tally.value.good >= MIN_FRAMES_FOR_SOLVE,
                onClick = {
                    if (rows <= 0 || cols <= 0 || sizeMm <= 0.0) {
                        status = "Set valid target dimensions first."
                        return@Button
                    }
                    solving = true
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
                        solving = false
                        result.onSuccess { res ->
                            store.writeIntrinsics(sessionDir, res)
                            solveResult = res
                            status = "Solved: ${res.nFramesUsed}/${res.nFramesTotal} frames, " +
                                "rms=${"%.3f".format(res.rmsPx)} px"
                        }.onFailure { e ->
                            solveResult = null
                            status = "Solve failed: ${e.message?.take(160)}"
                        }
                    }
                },
            ) { Text(if (solving) "Solving…" else "Solve") }

            solveResult?.let { ResultPanel(it) }

            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(
                    modifier = Modifier.weight(1f),
                    onClick = ::resetSession,
                ) { Text("New session") }
                OutlinedButton(modifier = Modifier.weight(1f), onClick = onBack) { Text("Back") }
            }
            // Save / send (always available; the data is
            // operator-owned, sits in app-local external-files,
            // and can be transferred via adb pull regardless
            // of debug mode). The collector POST stays
            // debug-mode-gated because it leaves the device.
            Button(
                modifier = Modifier.fillMaxWidth(),
                enabled = tally.value.good > 0,
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
                    enabled = tally.value.good > 0,
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

// ---- per-capture feedback model ----

/** Coarse classification of a per-capture analysis result. */
private enum class OutcomeKind(val code: String, val color: Color, val label: String) {
    DETECTED("ok", Color(0xFF34A853), "✓"),
    WRONG_GRID("wrong_grid", Color(0xFFFBBC05), "⚠"),
    NO_BOARD("no_board", Color(0xFFEA4335), "✗"),
    DECODE_FAILED("decode_failed", Color(0xFFEA4335), "✗"),
}

/** Per-capture analysis result rendered as a chip. */
private data class FrameFeedback(
    val seq: Int,
    val kind: OutcomeKind,
    /** Short tagline for the chip body. */
    val short: String,
    /** Longer remediation hint shown below the chip. */
    val long: String,
    /** When true, the bytes are moved to `frames/rejected/`
     *  and not offered to the solver. */
    val autoReject: Boolean,
)

private data class CaptureTally(
    val good: Int = 0,
    val noBoard: Int = 0,
    val wrongGrid: Int = 0,
    val decodeFailed: Int = 0,
) {
    fun bump(kind: OutcomeKind): CaptureTally = when (kind) {
        OutcomeKind.DETECTED -> copy(good = good + 1)
        OutcomeKind.NO_BOARD -> copy(noBoard = noBoard + 1)
        OutcomeKind.WRONG_GRID -> copy(wrongGrid = wrongGrid + 1)
        OutcomeKind.DECODE_FAILED -> copy(decodeFailed = decodeFailed + 1)
    }
    /** Reverse a manual discard of a previously-accepted frame. */
    fun demote(): CaptureTally = copy(good = (good - 1).coerceAtLeast(0))
}

private fun describeOutcome(
    seq: Int,
    outcome: FfiFrameOutcome,
    expectedRows: Int,
    expectedCols: Int,
): FrameFeedback = when (outcome) {
    is FfiFrameOutcome.Detected -> {
        val sharpHint = if (outcome.sharpness.isFinite() && outcome.sharpness < 50.0) {
            " (low sharpness — hold steadier?)"
        } else {
            ""
        }
        FrameFeedback(
            seq = seq,
            kind = OutcomeKind.DETECTED,
            short = "${outcome.nCorners} corners, sharpness=${"%.0f".format(outcome.sharpness)}$sharpHint",
            long = "Detected ${outcome.nCorners} of ${expectedRows * expectedCols} expected corners. " +
                "Sharpness (Laplacian variance) = ${"%.1f".format(outcome.sharpness)}; " +
                "values < 50 typically indicate motion blur or defocus.",
            autoReject = false,
        )
    }
    FfiFrameOutcome.NoBoardFound -> FrameFeedback(
        seq = seq,
        kind = OutcomeKind.NO_BOARD,
        short = "no board detected — auto-rejected",
        long = "Detector found nothing chessboard-shaped. Common causes: motion " +
            "blur, severe defocus, board outside the frame, or low contrast " +
            "(check lighting). The frame was moved to frames/rejected/ so it " +
            "doesn't poison the solve.",
        autoReject = true,
    )
    is FfiFrameOutcome.WrongGridSize -> FrameFeedback(
        seq = seq,
        kind = OutcomeKind.WRONG_GRID,
        short = "found ${outcome.foundRows}×${outcome.foundCols}, expected ${outcome.expectedRows}×${outcome.expectedCols}",
        long = "Found a chessboard of ${outcome.foundRows}×${outcome.foundCols} inner " +
            "corners; expected ${outcome.expectedRows}×${outcome.expectedCols}. Either " +
            "the board is partially occluded, or the target dimensions in the " +
            "header don't match the printed board (use Edit to fix).",
        autoReject = false,
    )
    is FfiFrameOutcome.DecodeFailed -> FrameFeedback(
        seq = seq,
        kind = OutcomeKind.DECODE_FAILED,
        short = "decode failed — auto-rejected",
        long = "Captured bytes did not decode as an image: ${outcome.reason.take(120)}",
        autoReject = true,
    )
}

@Composable
private fun OutcomeChip(fb: FrameFeedback) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(fb.kind.color.copy(alpha = 0.15f), RoundedCornerShape(8.dp))
            .padding(10.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text(
            "${fb.kind.label}  Frame ${fb.seq}: ${fb.short}",
            color = fb.kind.color,
            fontWeight = FontWeight.Bold,
        )
        Text(fb.long, color = Color(0xFFD0D0D0))
    }
}

@Composable
private fun TallyChip(t: CaptureTally) {
    val total = t.good + t.noBoard + t.wrongGrid + t.decodeFailed
    val short = "Captured: $total · Good: ${t.good} · NoBoard: ${t.noBoard} · " +
        "Wrong: ${t.wrongGrid} · DecodeErr: ${t.decodeFailed}"
    Text(short, color = Color(0xFFB0B0B0))
}

@Composable
private fun ResultPanel(result: FfiCalibrationResult) {
    val color = when (result.diagnosisOverall) {
        FfiDiagnosisLevel.OK -> Color(0xFF34A853)
        FfiDiagnosisLevel.WARN -> Color(0xFFFBBC05)
        FfiDiagnosisLevel.ERROR -> Color(0xFFEA4335)
    }
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(Color(0xFF1F1F1F), RoundedCornerShape(8.dp))
            .padding(10.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Text(
            "Solve: rms=${"%.3f".format(result.rmsPx)} px · views=${result.nFramesUsed}/${result.nFramesTotal}",
            color = Color.White,
            fontWeight = FontWeight.Bold,
        )
        Text(
            "Diagnosis: ${result.diagnosisOverall.name}",
            color = color,
            fontWeight = FontWeight.Bold,
        )
        if (result.diagnosisIssues.isEmpty()) {
            Text("No issues found.", color = Color(0xFFB0B0B0))
        } else {
            for (issue in result.diagnosisIssues) {
                IssueCard(issue)
            }
        }
        // Top per-view residual offenders (worst 3).
        val worst = result.perViewResiduals
            .filter { it.rmsPx.isFinite() }
            .sortedByDescending { it.rmsPx }
            .take(3)
        if (worst.isNotEmpty()) {
            Spacer(Modifier.height(2.dp))
            Text("Worst per-view residuals:", color = Color(0xFFB0B0B0))
            for (v in worst) {
                Text(
                    "  ${v.source}  rms=${"%.3f".format(v.rmsPx)} px  max=${"%.3f".format(v.maxPx)} px",
                    color = Color(0xFFD0D0D0),
                )
            }
        }
    }
}

@Composable
private fun IssueCard(issue: FfiDiagnosisIssue) {
    val color = when (issue.level) {
        FfiDiagnosisLevel.OK -> Color(0xFF34A853)
        FfiDiagnosisLevel.WARN -> Color(0xFFFBBC05)
        FfiDiagnosisLevel.ERROR -> Color(0xFFEA4335)
    }
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(color.copy(alpha = 0.12f), RoundedCornerShape(6.dp))
            .padding(8.dp),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Text(
            "[${issue.level.name}] ${issue.code}",
            color = color,
            fontWeight = FontWeight.Bold,
        )
        Text(issue.message, color = Color.White)
        Text("→ ${issue.remediation}", color = Color(0xFFB0B0B0))
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

/** Minimum *good* frames before "Solve" is enabled. Zhang's
 *  method needs ≥ 3; we ask for 5 for pose dispersion. */
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
