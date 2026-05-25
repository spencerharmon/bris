package io.github.spencerharmon.bris.ui

import android.text.format.Formatter
import android.Manifest
import android.content.pm.PackageManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.core.UseCaseGroup
import androidx.camera.core.ViewPort
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Snackbar
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import io.github.spencerharmon.bris.BuildConfig
import io.github.spencerharmon.bris.Prefs
import io.github.spencerharmon.bris.engine.CalibrationSource
import io.github.spencerharmon.bris.engine.CalibrationStore
import io.github.spencerharmon.bris.engine.CameraConstants
import io.github.spencerharmon.bris.engine.DebugCaptureBuffer
import io.github.spencerharmon.bris.engine.EngineWrapper
import io.github.spencerharmon.bris.engine.FixVerdict
import io.github.spencerharmon.bris.engine.FrameAnalyzer
import io.github.spencerharmon.bris.engine.LensCatalog
import io.github.spencerharmon.bris.engine.SessionHolder
import io.github.spencerharmon.bris.engine.SessionRecorder
import io.github.spencerharmon.bris.engine.SessionStatus
import io.github.spencerharmon.bris.engine.SightLog
import io.github.spencerharmon.bris.engine.resolveCalibration
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.bris_ffi.FfiEngineConfig
import uniffi.bris_ffi.FfiIntrinsics
import uniffi.bris_ffi.FfiObserver
import uniffi.bris_ffi.FfiPublishedFix
import uniffi.bris_ffi.FfiSight
import uniffi.bris_ffi.version
import java.util.concurrent.Executors

/**
 * Live camera + engine + sight-capture session UI.
 *
 * Engine lifecycle (per the design discussion in
 * `docs/design/sight_session.md`): the streaming engine is
 * constructed once when this screen composes and lives until
 * the screen leaves composition. There is no per-session
 * engine reset; the engine has no notion of session.
 *
 * Capture lifecycle: independently driven by the operator's
 * Start / Stop buttons. While the session is active the
 * CameraX [`ImageAnalysis`] use case is bound and the engine
 * receives frames; while idle, only the preview is bound and
 * the engine is silent. The [`SessionRecorder`] consumes the
 * engine's published-fix stream during a session and decides
 * the session's outcome (sustained-green auto-accept, operator
 * stop, or timeout).
 *
 * Storage: captured fixes land in a sight-log entry under
 * `<external-files>/sights/<session-ulid>/` (see [`SightLog`]).
 * Operators pull entries via plain `adb pull` / MTP; the
 * collector network path is a separate, debug-mode-gated
 * affordance.
 *
 * Backpressure: see `docs/design/diagnostic_collection.md` —
 * `STRATEGY_KEEP_ONLY_LATEST` on CameraX, copy-out at the
 * analyzer boundary so the engine and CameraX never compete
 * for ownership of the same buffer.
 */
@Composable
@Suppress("LongMethod") // The screen wires together camera + engine + recorder + UI; further extraction would create cross-cutting helpers that obscure the lifecycle.
fun LiveScreen(
    debugMode: Boolean,
    onOpenSettings: () -> Unit,
    onSendFix: () -> Unit,
    onOpenCalibration: () -> Unit,
    onOpenSightLog: () -> Unit,
) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current

    var hasCameraPermission by remember {
        mutableStateOf(
            ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) ==
                PackageManager.PERMISSION_GRANTED,
        )
    }

    val permissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted -> hasCameraPermission = granted }

    if (!hasCameraPermission) {
        Column(
            modifier = Modifier.fillMaxSize().padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text("Bris needs camera access to detect the horizon and celestial bodies.")
            Button(onClick = { permissionLauncher.launch(Manifest.permission.CAMERA) }) {
                Text("Grant camera access")
            }
            OutlinedButton(onClick = onOpenSettings) { Text("Settings") }
        }
        return
    }

    val prefs = remember(context) { Prefs(context) }
    val debugCaptureEnabled by prefs.debugCaptureFlow.collectAsState(initial = false)
    val selectedLensId by prefs.selectedLensIdFlow.collectAsState(initial = null)
    val defaultBackId = remember(context) {
        LensCatalog.defaultBackCameraId(context) ?: LensCatalog.FALLBACK_LENS_ID
    }
    val effectiveLensId = selectedLensId ?: defaultBackId
    // The capture resolution is the chosen lens's *native
    // maximum* for the analyzer pixel format. Per-stage
    // downsampling happens inside the engine via
    // FramePyramid + Intrinsics::scaled_to (plan.org Phase 2
    // per-stage-resolution architecture); upstream of the
    // engine we always feed the highest pixel count the
    // sensor delivers so plate-solve / centroiding get
    // everything they can. Falls back to a conservative
    // 1280×720 if the device's StreamConfigurationMap can't
    // be enumerated.
    val captureSize = remember(context, effectiveLensId) {
        CameraConstants.maxOutputSizeFor(context, effectiveLensId)
            ?: android.util.Size(1280, 720)
    }
    val debugBuffer = remember(context) { DebugCaptureBuffer.forApp(context) }
    val calStore = remember(context) { CalibrationStore.forApp(context) }
    val calibration = remember(context, effectiveLensId, captureSize) {
        resolveCalibration(
            store = calStore,
            lensId = effectiveLensId,
            width = captureSize.width,
            height = captureSize.height,
        )
    }
    val sightLog = remember(context) { SightLog.forApp(context) }

    val engineScope = remember { CoroutineScope(SupervisorJob()) }
    val engine = remember(context) {
        SessionHolder.acquire(
            context = context,
            configFactory = { defaultEngineConfig() },
            pbrisSink = { line ->
                if (debugCaptureEnabled) debugBuffer.appendPbris(line)
            },
        )
    }
    val recorder = remember(engine, sightLog, prefs) {
        SessionRecorder(
            engine = engine,
            sightLog = sightLog,
            scope = engineScope,
            deviceUuidProvider = { prefs.deviceUuid() },
            appVersion = BuildConfig.BRIS_APP_VERSION,
            coreVersionProvider = { version().brisFfi },
        )
    }
    DisposableEffect(engine) {
        onDispose {
            // Engine is owned by SessionHolder for process lifetime;
            // closing here would tear it down on navigation, defeating
            // the SightLog screen's recent_sights() lookup.
        }
    }

    val snapshot by engine.snapshot.collectAsState()
    val sessionStatus by recorder.status.collectAsState()

    // ----- Pool / last-persisted / current-fix UI state -----
    var lastPersistedFix by remember { mutableStateOf<FfiPublishedFix?>(null) }
    var showRecoveredBanner by remember { mutableStateOf(false) }
    var liveFix by remember { mutableStateOf<FfiPublishedFix?>(null) }
    var poolSights by remember { mutableStateOf<List<FfiSight>>(emptyList()) }

    // On screen entry, fetch the most-recent persisted fix off
    // the UI thread and show the recovery banner for 10 s.
    LaunchedEffect(engine) {
        val recovered = withContext(Dispatchers.IO) {
            runCatching { engine.lastPersistedFix() }.getOrNull()
        }
        if (recovered != null) {
            lastPersistedFix = recovered
            liveFix = liveFix ?: recovered
            showRecoveredBanner = true
            delay(RECOVERED_BANNER_MS)
            showRecoveredBanner = false
        }
    }

    // Every new published fix refreshes the live-fix overlay
    // and re-reads the in-memory sight pool. `pool_sights()` is
    // cheap (in-memory) so it's safe at fix cadence.
    LaunchedEffect(engine) {
        engine.fixes.collect { fix ->
            liveFix = fix
            poolSights = runCatching { engine.poolSights() }.getOrDefault(emptyList())
        }
    }
    val captureActive = sessionStatus is SessionStatus.Capturing ||
        sessionStatus is SessionStatus.Saving
    val bufferState by debugBuffer.stateFlow.collectAsState()
    val snackbarHost = remember { SnackbarHostState() }
    val saveAction = rememberDebugSaveAction(
        buffer = debugBuffer,
        prefs = prefs,
        snackbarHost = snackbarHost,
    )

    Box(modifier = Modifier.fillMaxSize()) {
        // Top-right confidence-ellipse HUD. Sits above the
        // diagnostic column so the ellipse is always visible
        // even when the column scrolls; align manually because
        // the column is left-aligned at (0,0).
        ConfidenceEllipseOverlay(
            fix = liveFix,
            sights = poolSights,
            modifier = Modifier
                .align(Alignment.TopEnd)
                .padding(12.dp),
        )

        CameraSurface(
            lifecycleOwner = lifecycleOwner,
            captureActive = captureActive,
            engine = engine,
            persistedIntrinsics = persistedIntrinsicsFor(calibration),
            debugCaptureEnabled = debugCaptureEnabled,
            debugBuffer = debugBuffer,
            lensId = effectiveLensId,
            captureSize = captureSize,
        )

        Column(
            modifier = Modifier.padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            DiagnosticOverlay(
                sessionStatus = sessionStatus,
                lastClassification = snapshot?.lastClassification,
                framesPushed = snapshot?.framesPushed ?: 0u,
                framesDropped = snapshot?.framesDropped ?: 0u,
                bodyQueueDepth = snapshot?.bodyQueueDepth ?: 0u,
                horizonQueueDepth = snapshot?.horizonQueueDepth ?: 0u,
                horizonProvenance = snapshot?.lastHorizonProvenance,
                horizonAltitudeSigmaArcmin = snapshot?.lastHorizonAltitudeSigmaArcmin,
                ringBufferDepth = snapshot?.ringBufferDepth ?: 0u,
                sightWindowDepth = snapshot?.sightWindowDepth ?: 0u,
                calibrationSource = calibration,
                lensLabel = lensLabelFor(context, effectiveLensId),
                captureSize = captureSize,
                debugCaptureEnabled = debugCaptureEnabled,
                captureActive = captureActive,
                bufferState = bufferState,
            )
            RecoveredFixBanner(visible = showRecoveredBanner, fix = lastPersistedFix)
            PoolSummaryChip(sights = poolSights)
            Spacer(Modifier.height(12.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (captureActive) {
                    Button(onClick = { recorder.stop() }) { Text("Stop capture") }
                } else {
                    Button(onClick = { recorder.start() }) { Text("Start capture") }
                }
                OutlinedButton(onClick = onOpenSightLog) { Text("Sight log") }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = onOpenSettings) { Text("Settings") }
                OutlinedButton(onClick = onOpenCalibration) { Text("Calibration") }
            }
            if (debugMode) {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(onClick = onSendFix) { Text("Send fix (debug)") }
                    OutlinedButton(onClick = saveAction) { Text("Save buffer") }
                }
            }
        }

        SnackbarHost(
            hostState = snackbarHost,
            modifier = Modifier.align(Alignment.BottomCenter).padding(12.dp),
        ) { data -> Snackbar(snackbarData = data) }
    }
}

/**
 * CameraX preview that always shows the camera surface and
 * conditionally binds the analyzer based on `captureActive`.
 *
 * The two use-case sets we toggle between:
 *   * idle:      [Preview]
 *   * capturing: [Preview, ImageAnalysis]
 *
 * `bindToLifecycle` is called inside a `LaunchedEffect` keyed
 * on `captureActive` so flipping the flag triggers a rebind
 * (CameraX requires `unbindAll` + `bindToLifecycle` to swap
 * use cases on a live camera).
 */
@Composable
private fun CameraSurface(
    lifecycleOwner: androidx.lifecycle.LifecycleOwner,
    captureActive: Boolean,
    engine: EngineWrapper,
    persistedIntrinsics: CalibrationStore.PersistedIntrinsics?,
    debugCaptureEnabled: Boolean,
    debugBuffer: DebugCaptureBuffer,
    lensId: String,
    captureSize: android.util.Size,
) {
    val context = LocalContext.current
    val previewView = remember(context) { PreviewView(context) }
    val analyzerExecutor = remember { Executors.newSingleThreadExecutor() }
    val cameraSelector = remember(lensId) { LensCatalog.selectorFor(lensId) }

    LaunchedEffect(captureActive, lifecycleOwner, cameraSelector, captureSize) {
        val provider = ProcessCameraProvider.getInstance(context).get()
        provider.unbindAll()
        val preview = Preview.Builder().build().also {
            it.setSurfaceProvider(previewView.surfaceProvider)
        }
        // Pin every use case to the same crop rectangle. Without
        // a shared ViewPort, PreviewView shows the full sensor
        // crop while ImageAnalysis sees a different one — the
        // operator would line up the horizon visually but the
        // analyzer would be analyzing a different frame.
        val viewport = ViewPort.Builder(
            CameraConstants.aspectRatioOf(captureSize),
            preview.targetRotation,
        )
            .setScaleType(ViewPort.FIT)
            .build()
        if (captureActive) {
            val analysis = ImageAnalysis.Builder()
                .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                .setResolutionSelector(
                    ResolutionSelector.Builder()
                        .setResolutionStrategy(
                            ResolutionStrategy(
                                captureSize,
                                ResolutionStrategy.FALLBACK_RULE_CLOSEST_LOWER_THEN_HIGHER,
                            ),
                        )
                        .build(),
                )
                .build()
            analysis.setAnalyzer(
                analyzerExecutor,
                FrameAnalyzer(
                    engine = engine,
                    intrinsicsProvider = {
                        intrinsicsForResolution(
                            persistedIntrinsics,
                            targetWidth = captureSize.width,
                            targetHeight = captureSize.height,
                        )
                    },
                    debugCaptureProvider = { debugCaptureEnabled },
                    debugBuffer = debugBuffer,
                ),
            )
            val group = UseCaseGroup.Builder()
                .setViewPort(viewport)
                .addUseCase(preview)
                .addUseCase(analysis)
                .build()
            provider.bindToLifecycle(
                lifecycleOwner,
                cameraSelector,
                group,
            )
        } else {
            val group = UseCaseGroup.Builder()
                .setViewPort(viewport)
                .addUseCase(preview)
                .build()
            provider.bindToLifecycle(
                lifecycleOwner,
                cameraSelector,
                group,
            )
        }
    }

    AndroidView(
        modifier = Modifier.fillMaxSize(),
        factory = { previewView },
    )
}

/**
 * Inline HUD chip explaining the debug-capture state.
 *
 * Shown only when Debug capture is enabled. While the session
 * is idle (toggle on but Start capture not yet pressed) the
 * chip reads "Debug armed" to make the toggle/capture
 * relationship obvious; while capturing it shows a pulsing
 * red `REC` dot plus frame count + on-disk size; while paused
 * after a recent append the dot is static grey.
 */
@Composable
private fun DebugBufferChip(
    bufferState: DebugCaptureBuffer.BufferState,
    captureActive: Boolean,
) {
    val context = LocalContext.current
    if (!captureActive) {
        Text(
            "Debug armed \u2014 press Start capture to record",
            color = Color(0xFFB0B0B0),
        )
        return
    }
    val recentMs = bufferState.lastAppendUnixMs ?: 0L
    val isLive = System.currentTimeMillis() - recentMs < 1500L
    // Only pulse when actively recording; the static grey
    // dot for the "paused but armed" state is intentionally
    // not animated (drawing attention to it would mislead).
    val dotAlpha = if (isLive) {
        val transition = rememberInfiniteTransition(label = "rec-dot")
        transition.animateFloat(
            initialValue = 1.0f,
            targetValue = 0.3f,
            animationSpec = infiniteRepeatable(
                animation = tween(durationMillis = 700, easing = LinearEasing),
                repeatMode = RepeatMode.Reverse,
            ),
            label = "rec-dot-alpha",
        ).value
    } else {
        1.0f
    }
    val dotColor = if (isLive) Color(0xFFE53935) else Color(0xFF808080)
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Box(
            modifier = Modifier
                .size(10.dp)
                .background(dotColor.copy(alpha = dotAlpha), CircleShape),
        )
        val size = Formatter.formatShortFileSize(context, bufferState.totalBytes)
        Text(
            "REC  ${bufferState.frameCount} frames \u00b7 $size",
            color = Color.White,
        )
    }
}

/**
 * Top-of-screen translucent panel with the engine + session
 * diagnostics. While idle: calibration state + "Tap Start
 * capture to begin." While capturing: elapsed seconds, last
 * verdict color + σ_major, fix counts. While saving / saved:
 * brief outcome message.
 */
@Composable
private fun DiagnosticOverlay(
    sessionStatus: SessionStatus,
    lastClassification: String?,
    framesPushed: ULong,
    framesDropped: ULong,
    bodyQueueDepth: UInt,
    horizonQueueDepth: UInt,
    horizonProvenance: String?,
    horizonAltitudeSigmaArcmin: Double?,
    ringBufferDepth: UInt,
    sightWindowDepth: UInt,
    calibrationSource: CalibrationSource,
    lensLabel: String,
    captureSize: android.util.Size,
    debugCaptureEnabled: Boolean,
    captureActive: Boolean,
    bufferState: DebugCaptureBuffer.BufferState,
) {
    // Provenance-honest calibration label. Operator-run
    // sessions are the gold standard; factory profiles are
    // a good-enough day-one fallback (operator can override);
    // placeholder means altitudes are not trustworthy.
    val calibLabel = when (calibrationSource) {
        is CalibrationSource.Operator -> {
            val i = calibrationSource.intrinsics
            if (i.width == captureSize.width && i.height == captureSize.height) {
                "calib: operator rms ${"%.2f".format(i.rmsPx)} px"
            } else {
                "calib mismatch (${i.width}×${i.height} on ${captureSize.width}×${captureSize.height})"
            }
        }
        is CalibrationSource.Factory -> {
            val i = calibrationSource.intrinsics
            "calib: factory (${calibrationSource.label}, rms ${"%.2f".format(i.rmsPx)} px)"
        }
        CalibrationSource.Placeholder -> "calib: PLACEHOLDER (run calibration)"
    }

    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(Color(0x80000000))
            .padding(8.dp),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        if (debugCaptureEnabled) {
            DebugBufferChip(
                bufferState = bufferState,
                captureActive = captureActive,
            )
        }
        Text(calibLabel, color = Color.White)
        Text("capture: ${captureSize.width}×${captureSize.height}", color = Color.White)
        Text("lens: $lensLabel", color = Color.White)
        when (val s = sessionStatus) {
            is SessionStatus.Idle -> {
                Text("Idle. Tap Start capture to begin a session.", color = Color.White)
            }
            is SessionStatus.Capturing -> {
                val elapsed = (System.currentTimeMillis() - s.startedAtMs) / 1000
                val verdict = s.lastVerdict
                val verdictColor = when (verdict) {
                    FixVerdict.GREEN -> Color(0xFF35D673)
                    FixVerdict.YELLOW -> Color(0xFFFFC107)
                    FixVerdict.RED -> Color(0xFFE57373)
                    null -> Color.LightGray
                }
                val sigmaText = s.lastFix?.let { "σ=${"%.2f".format(it.sigmaMajorNm)} nm" } ?: "σ=—"
                Text(
                    "Capturing ${elapsed}s  $sigmaText  " +
                        "${s.nGreen}G ${s.nYellow}Y ${s.nRed}R",
                    color = verdictColor,
                )
            }
            is SessionStatus.Saving -> Text("Saving…", color = Color.White)
            is SessionStatus.Saved -> {
                val msg = when (val o = s.outcome) {
                    is io.github.spencerharmon.bris.engine.SessionOutcome.Captured ->
                        "Captured ${o.verdict.name.lowercase()} fix " +
                            "(σ=${"%.2f".format(o.fix.sigmaMajorNm)} nm). Saved to ${s.sessionDir.name}."
                    is io.github.spencerharmon.bris.engine.SessionOutcome.NoFix ->
                        "No fix recorded (${o.reason})."
                }
                Text(msg, color = Color.White)
            }
            is SessionStatus.Failed -> Text("Failed: ${s.reason}", color = Color(0xFFE57373))
        }
        Text("classifier: ${lastClassification ?: "—"}", color = Color.White)
        Text(
            "frames pushed: $framesPushed  dropped: $framesDropped",
            color = Color.White,
        )
        Text(
            "queues  body=$bodyQueueDepth  horizon=$horizonQueueDepth" +
                "  ring=$ringBufferDepth  sights=$sightWindowDepth",
            color = Color.White,
        )
        // Horizon provenance + 1σ (arcmin). Visible whenever
        // the engine has produced a horizon on the most
        // recent processed frame; em-dash placeholder
        // otherwise. Standard HUD typography — no new style.
        val horizonLine = if (horizonProvenance != null) {
            val sigmaText = horizonAltitudeSigmaArcmin
                ?.let { "%.2f".format(it) + "'" }
                ?: "—"
            "horizon: $horizonProvenance  σ=$sigmaText"
        } else {
            "horizon: —"
        }
        Text(horizonLine, color = Color.White)
    }
}

/**
 * Pick intrinsics for the analyzer's resolution. Prefer
 * persisted calibration when available *and* the resolution
 * matches; otherwise fall back to placeholder defaults sized
 * for the current capture resolution.
 *
 * Calibration data is keyed by camera + resolution. Applying
 * a 640×480 calibration to a 4032×3024 frame silently produces
 * wrong altitudes, so the resolution gate is mandatory; on
 * mismatch we degrade to placeholder and the diagnostic
 * overlay flags it. The placeholder is intended to keep the
 * pipeline alive in the un-calibrated case; quantitative
 * altitudes from a placeholder-intrinsics fix are not
 * trustworthy and the operator is expected to run calibration
 * before relying on the numbers.
 */
private fun intrinsicsForResolution(
    persisted: CalibrationStore.PersistedIntrinsics?,
    targetWidth: Int,
    targetHeight: Int,
): FfiIntrinsics {
    if (persisted != null && persisted.width == targetWidth && persisted.height == targetHeight) {
        return FfiIntrinsics(
            fx = persisted.fx, fy = persisted.fy,
            cx = persisted.cx, cy = persisted.cy,
            k1 = persisted.k1, k2 = persisted.k2, k3 = persisted.k3,
            p1 = persisted.p1, p2 = persisted.p2,
        )
    }
    return placeholderIntrinsicsFor(targetWidth, targetHeight)
}

/**
 * Project a [`CalibrationSource`] back down to a nullable
 * [`CalibrationStore.PersistedIntrinsics`] for code that
 * pre-dates the sealed-source refactor (engine binding,
 * `intrinsicsForResolution`). `Placeholder` maps to `null`;
 * both `Operator` and `Factory` map to their carried
 * intrinsics — the engine doesn't need to know the
 * provenance, only the diagnostic overlay does.
 */
private fun persistedIntrinsicsFor(source: CalibrationSource): CalibrationStore.PersistedIntrinsics? =
    when (source) {
        is CalibrationSource.Operator -> source.intrinsics
        is CalibrationSource.Factory -> source.intrinsics
        CalibrationSource.Placeholder -> null
    }

/**
 * Placeholder intrinsics sized to the supplied resolution.
 * Principal point at the image center; focal length scaled
 * so the field of view is roughly that of a "normal" phone
 * camera (a 50°-ish horizontal FOV). The values are *not*
 * accurate for any specific lens — the operator must run
 * calibration to get quantitative altitudes. The placeholder
 * exists only so the pipeline doesn't refuse to start on a
 * fresh install.
 */
private fun placeholderIntrinsicsFor(width: Int, height: Int): FfiIntrinsics {
    // f ≈ width / (2 · tan(FOV/2)); for FOV ≈ 60° horizontal,
    // f ≈ width / 1.1547. Round to a friendly value per pixel
    // grid: 720p → ~1000 px, matching the historical placeholder.
    val focal = width.toDouble() / 1.1547
    return FfiIntrinsics(
        fx = focal,
        fy = focal,
        cx = width / 2.0,
        cy = height / 2.0,
        k1 = 0.0,
        k2 = 0.0,
        k3 = 0.0,
        p1 = 0.0,
        p2 = 0.0,
    )
}

/**
 * Placeholder engine config. Observer is the dev default
 * (equator/Greenwich, 2 m eye height); real callers will read
 * the operator's stored observer settings.
 */
private fun defaultEngineConfig(): FfiEngineConfig = FfiEngineConfig(
    observer = FfiObserver(
        latitudeDeg = 0.0,
        longitudeDeg = 0.0,
        eyeHeightM = 2.0,
        eyeHeightSigmaM = 0.5,
    ),
    stitchingWindowSeconds = 2.0,
    sightWindowSeconds = 600.0,
    sightWindowCapacity = 10u,
    minFixPublicationIntervalMs = 1000u,
    inputRingCapacity = 120u,
    segmentationModelPath = null,
    // Per-stage analysis resolution (plan.org Phase 2 / Per-
    // stage-resolution step 3b + the long-edge follow-up):
    // leave the explicit width/height pair null so the engine
    // falls back to its long-edge cap, which the core defaults
    // to 1280 px. Horizon detectors saturate well below 1280
    // on the long edge — gradient SNR is set by the sky-sea
    // contrast, not pixel count, and segmentation gets worse
    // above its training resolution. Passing the cap
    // explicitly here keeps Kotlin honest about the contract
    // (and would let a future Settings toggle override it).
    horizonAnalysisWidth = null,
    horizonAnalysisHeight = null,
    horizonAnalysisMaxLongEdgePx = 1280u,
)

/**
 * Resolve a human-readable label for the given lens id by
 * consulting [`LensCatalog`]. Falls back to the id itself if
 * the catalog can't enumerate (no Camera2 access yet, etc.).
 */
private fun lensLabelFor(context: android.content.Context, lensId: String): String {
    val match = LensCatalog.enumerate(context).firstOrNull { it.id == lensId }
    return match?.label ?: lensId
}

/** How long the "recovered from previous session" banner stays up. */
private const val RECOVERED_BANNER_MS = 10_000L
