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
import io.github.spencerharmon.bris.engine.CaptureRecorder
import io.github.spencerharmon.bris.engine.CaptureStatus
import io.github.spencerharmon.bris.engine.SightLog
import io.github.spencerharmon.bris.engine.resolveCalibration
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
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
 * `docs/design/capture.md`): the streaming engine is
 * constructed once when this screen composes and lives until
 * the screen leaves composition. There is no per-session
 * engine reset; the engine has no notion of session.
 *
 * Capture lifecycle: independently driven by the operator's
 * Start / Stop buttons. While the session is active the
 * CameraX [`ImageAnalysis`] use case is bound and the engine
 * receives frames; while idle, only the preview is bound and
 * the engine is silent. The [`CaptureRecorder`] consumes the
 * engine's published-fix stream during a session and decides
 * the session's outcome (sustained-green auto-accept, operator
 * stop, or timeout).
 *
 * Storage: captured fixes land in a sight-log entry under
 * `<external-files>/sights/<capture-id>/` (see [`SightLog`]).
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
    onOpenSessions: () -> Unit,
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
    val sessionStore = remember(context) {
        io.github.spencerharmon.bris.engine.SessionStore.forApp(context)
    }
    val activeSessionId by prefs.activeSessionIdFlow.collectAsState(initial = null)
    val activeSessionUuid = activeSessionId?.let {
        runCatching { java.util.UUID.fromString(it) }.getOrNull()
    }
    // Sight-log root: per-session when an active session exists
    // (writes captures under sessions/<uuid>/captures/), else the
    // legacy orphan path <external-files>/sights/.
    val sightLog = remember(context, activeSessionUuid) {
        if (activeSessionUuid != null) {
            SightLog.forSession(context, activeSessionUuid)
        } else {
            SightLog.forApp(context)
        }
    }

    val engineScope = remember { CoroutineScope(SupervisorJob()) }
    val engine = remember(context, activeSessionUuid) {
        SessionHolder.acquire(
            context = context,
            activeSessionId = activeSessionUuid,
            configFactory = { storeRoot ->
                val hemi = runBlocking { prefs.coarseHemisphereFlow.first() }
                val s = activeSessionUuid?.let { sessionStore.loadOrNull(it) }
                defaultEngineConfig(
                    coarseHemisphere = hemi,
                    session = s,
                    storeDataRoot = storeRoot,
                )
            },
            pbrisSink = { line ->
                if (debugCaptureEnabled) debugBuffer.appendPbris(line)
            },
        )
    }
    // Shared bundle.json inputs builder. Used by both the
    // streaming capture-recorder write at Stop and the
    // (now-legacy) DebugBuffer "Save buffer" path. Capturing
    // the same inputs at both call sites keeps the two
    // writers honest against each other.
    val bundleInputsBuilder: () -> io.github.spencerharmon.bris.engine.DebugBundleWriter.Inputs = {
        io.github.spencerharmon.bris.engine.DebugBundleWriter.Inputs(
            observer = io.github.spencerharmon.bris.engine.DebugBundleWriter.ObserverFix(
                latitudeDeg = 0.0,
                longitudeDeg = 0.0,
                eyeHeightM = 2.0,
            ),
            apProvenance = "operator_entered",
            lensId = effectiveLensId,
            captureWidth = captureSize.width,
            captureHeight = captureSize.height,
            calibration = calibration,
            gpsTruth = if (debugMode) {
                io.github.spencerharmon.bris.engine.DebugBundleWriter.maybeGpsTruth(context)
            } else null,
            sessionId = activeSessionUuid?.toString(),
        )
    }

    val recorder = remember(engine, sightLog, prefs, activeSessionUuid) {
        CaptureRecorder(
            engine = engine,
            sightLog = sightLog,
            scope = engineScope,
            deviceUuidProvider = { prefs.deviceUuid() },
            appVersion = BuildConfig.BRIS_APP_VERSION,
            coreVersionProvider = { version().brisFfi },
            onCaptureSaved = { captureId ->
                // Append captureId to the active session's
                // ordered_capture_ids so `bris replay --session`
                // walks them in chronological order.
                activeSessionUuid?.let { sessionStore.appendCapture(it, captureId) }
            },
            captureDirProvider = { capId ->
                // Active session set → canonical path:
                // <files>/sessions/<UUID>/captures/<capId>/
                // Else → orphan path: <files>/sights/<capId>/
                val externalRoot = context.getExternalFilesDir(null) ?: context.filesDir
                val capturesRoot = if (activeSessionUuid != null) {
                    java.io.File(
                        java.io.File(
                            java.io.File(externalRoot, "sessions"),
                            activeSessionUuid.toString(),
                        ),
                        "captures",
                    )
                } else {
                    java.io.File(externalRoot, "sights")
                }
                java.io.File(capturesRoot, capId)
            },
            bundleInputsProvider = bundleInputsBuilder,
        )
    }
    val snapshot by engine.snapshot.collectAsState()
    val captureStatus by recorder.status.collectAsState()

    // ----- Pool / last-persisted / current-fix UI state -----
    var recoveredFix by remember { mutableStateOf<FfiPublishedFix?>(null) }
    var showRecoveredBanner by remember { mutableStateOf(false) }
    var liveFix by remember { mutableStateOf<FfiPublishedFix?>(null) }
    var poolSights by remember { mutableStateOf<List<FfiSight>>(emptyList()) }

    // On screen entry, fetch the most-recent persisted fix off
    // the UI thread and show the recovery banner for 10 s. The
    // recovered value renders into a *separate* overlay state
    // (yellow ellipse, RECOVERED badge) so an operator can't
    // mistake it for a live solution.
    LaunchedEffect(engine) {
        val recovered = withContext(Dispatchers.IO) {
            runCatching { engine.lastPersistedFix() }.getOrNull()
        }
        if (recovered != null) {
            recoveredFix = recovered
            showRecoveredBanner = true
            delay(RECOVERED_BANNER_MS)
            showRecoveredBanner = false
        }
    }

    // Every new published fix refreshes the live-fix overlay
    // and re-reads the in-memory sight pool. Both `pool_sights()`
    // and the collect itself originate on the engine's worker
    // thread, but state writes happen on Main; explicitly hop
    // to IO for the JNI getter so the Main thread never blocks
    // on the engine mutex.
    LaunchedEffect(engine) {
        engine.fixes.collect { fix ->
            liveFix = fix
            recoveredFix = null
            poolSights = withContext(Dispatchers.IO) {
                runCatching { engine.poolSights() }.getOrDefault(emptyList())
            }
        }
    }
    val captureActive = captureStatus is CaptureStatus.Capturing ||
        captureStatus is CaptureStatus.Saving
    val bufferState by debugBuffer.stateFlow.collectAsState()
    val snackbarHost = remember { SnackbarHostState() }
    val saveAction = rememberDebugSaveAction(
        buffer = debugBuffer,
        prefs = prefs,
        snackbarHost = snackbarHost,
        bundleInputsProvider = bundleInputsBuilder,
    )

    Box(modifier = Modifier.fillMaxSize()) {
        // Top-right confidence-ellipse HUD. Sits above the
        // diagnostic column so the ellipse is always visible
        // even when the column scrolls; align manually because
        // the column is left-aligned at (0,0).
        ConfidenceEllipseOverlay(
            fix = liveFix ?: recoveredFix,
            sights = poolSights,
            recovered = liveFix == null && recoveredFix != null,
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
            captureFrameTap = recorder::onAnalyzerFrame,
        )

        Column(
            modifier = Modifier.padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            DiagnosticOverlay(
                captureStatus = captureStatus,
                lastRawClassification = snapshot?.lastRawClassification,
                lastDispatchedCondition = snapshot?.lastDispatchedCondition,
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
            RecoveredFixBanner(visible = showRecoveredBanner, fix = recoveredFix)
            PoolSummaryChip(sights = poolSights)
            ProvenanceBadge(fix = liveFix ?: recoveredFix)
            Spacer(Modifier.height(12.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (captureActive) {
                    Button(onClick = { recorder.stop() }) { Text("Stop capture") }
                } else {
                    Button(onClick = {
                        // Default-session fallback: if no active
                        // session is set, auto-create one named
                        // "Untitled <date>" so this capture is
                        // never an orphan on disk. The operator
                        // can rename later from the Sessions
                        // screen.
                        if (activeSessionUuid == null) {
                            val now = System.currentTimeMillis()
                            val s = io.github.spencerharmon.bris.engine.Session.new(
                                "Untitled " + java.time.format.DateTimeFormatter
                                    .ofPattern("yyyy-MM-dd HH:mm")
                                    .withZone(java.time.ZoneId.systemDefault())
                                    .format(java.time.Instant.ofEpochMilli(now)),
                            )
                            sessionStore.save(s)
                            engineScope.launch {
                                prefs.setActiveSessionId(s.sessionId.toString())
                            }
                            // Note: this recorder is the one bound
                            // to the prior `activeSessionUuid` via
                            // `remember(...)`. Recomposition after
                            // setActiveSessionId rebuilds a new
                            // recorder rooted at the new session
                            // path. The first capture's frames
                            // land in the orphan path; subsequent
                            // captures use the new session. This
                            // is a known UX wart and goes away
                            // once Phase 7 collapses the path
                            // choice into the recorder's
                            // per-capture lookup.
                        }
                        recorder.start()
                    }) { Text("Start capture") }
                }
                OutlinedButton(onClick = onOpenSightLog) { Text("Sight log") }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = onOpenSettings) { Text("Settings") }
                OutlinedButton(onClick = onOpenCalibration) { Text("Calibration") }
                OutlinedButton(onClick = onOpenSessions) { Text("Sessions") }
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
    captureFrameTap: ((uniffi.bris_ffi.FfiFrame) -> Unit)? = null,
) {
    val context = LocalContext.current
    val previewView = remember(context) { PreviewView(context) }
    val analyzerExecutor = remember { Executors.newSingleThreadExecutor() }
    val cameraSelector = remember(lensId) { LensCatalog.selectorFor(lensId) }

    // Track the current display rotation reactively. We update
    // it on every display-change event (rotate-lock toggle,
    // physical rotation while unlocked) and pass it as a key to
    // the bind LaunchedEffect so the use cases rebind with the
    // correct targetRotation. Both Preview (ViewPort) and
    // ImageAnalysis honor the value: the analyzer's incoming
    // ImageProxy.imageInfo.rotationDegrees becomes the
    // rotation-from-sensor-to-display delta, and FrameAnalyzer
    // rotates the Y plane accordingly before pushing to the
    // engine.
    // Source of analyzer's target rotation: the device's
    // physical orientation derived from the accelerometer,
    // not `display.rotation`. The latter is pinned by the
    // system rotate-lock; the former always reflects how the
    // operator is actually holding the phone. Cat S62 Pro
    // verification 2026-06-01 showed `display.rotation`
    // stayed stale through a manual phone rotation while
    // rotate-lock was on, causing 42 of 74 frames to be
    // stored with sideways gravity.
    val orientationSource = remember(context) {
        io.github.spencerharmon.bris.engine.DeviceOrientationSource.forContext(context)
    }
    androidx.compose.runtime.DisposableEffect(orientationSource) {
        orientationSource.start()
        onDispose { orientationSource.stop() }
    }
    val displayRotation by orientationSource.rotation.collectAsState()

    LaunchedEffect(captureActive, lifecycleOwner, cameraSelector, captureSize, displayRotation) {
        val provider = ProcessCameraProvider.getInstance(context).get()
        provider.unbindAll()
        val preview = Preview.Builder()
            .setTargetRotation(displayRotation)
            .build()
            .also { it.setSurfaceProvider(previewView.surfaceProvider) }
        // Pin every use case to the same crop rectangle. Without
        // a shared ViewPort, PreviewView shows the full sensor
        // crop while ImageAnalysis sees a different one — the
        // operator would line up the horizon visually but the
        // analyzer would be analyzing a different frame.
        val viewport = ViewPort.Builder(
            CameraConstants.aspectRatioOf(captureSize),
            displayRotation,
        )
            .setScaleType(ViewPort.FIT)
            .build()
        if (captureActive) {
            val analysis = ImageAnalysis.Builder()
                .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                .setTargetRotation(displayRotation)
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
                    captureFrameTap = captureFrameTap,
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
    captureStatus: CaptureStatus,
    lastRawClassification: String?,
    lastDispatchedCondition: String?,
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
        when (val s = captureStatus) {
            is CaptureStatus.Idle -> {
                Text("Idle. Tap Start capture to begin a session.", color = Color.White)
            }
            is CaptureStatus.Capturing -> {
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
            is CaptureStatus.Saving -> Text("Saving…", color = Color.White)
            is CaptureStatus.Saved -> {
                val msg = when (val o = s.outcome) {
                    is io.github.spencerharmon.bris.engine.CaptureOutcome.Captured ->
                        "Captured ${o.verdict.name.lowercase()} fix " +
                            "(σ=${"%.2f".format(o.fix.sigmaMajorNm)} nm). Saved to ${s.captureDir.name}."
                    is io.github.spencerharmon.bris.engine.CaptureOutcome.NoFix ->
                        "No fix recorded (${o.reason})."
                }
                Text(msg, color = Color.White)
            }
            is CaptureStatus.Failed -> Text("Failed: ${s.reason}", color = Color(0xFFE57373))
        }
        Text(
            "classifier: dispatched=${lastDispatchedCondition ?: "—"}  " +
                "raw=${lastRawClassification ?: "—"}",
            color = Color.White,
        )
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
 *
 * When [session] is non-null, session-level retention overrides
 * apply: `sight_window_seconds` / `sight_window_capacity` come
 * from the session, and `kinematics`→`assumed_max_speed_kn`
 * is plumbed through the FFI (paired with the replay-side
 * `apply_session_overlay`, this closes the live↔replay
 * symmetry).
 */
private fun defaultEngineConfig(
    coarseHemisphere: String? = null,
    session: io.github.spencerharmon.bris.engine.Session? = null,
    storeDataRoot: String? = null,
): FfiEngineConfig = FfiEngineConfig(
    observer = FfiObserver(
        latitudeDeg = session?.apSeed?.latDeg ?: 0.0,
        longitudeDeg = session?.apSeed?.lonDeg ?: 0.0,
        eyeHeightM = session?.apSeed?.eyeHeightM ?: 2.0,
        eyeHeightSigmaM = 0.5,
    ),
    stitchingWindowSeconds = 2.0,
    sightWindowSeconds = session?.sightRetentionSeconds?.toDouble() ?: 600.0,
    sightWindowCapacity = session?.sightRetentionCapacity?.toUInt() ?: 10u,
    minFixPublicationIntervalMs = 1000u,
    inputRingCapacity = 120u,
    segmentationModelPath = null,
    horizonAnalysisWidth = null,
    horizonAnalysisHeight = null,
    horizonAnalysisMaxLongEdgePx = 1280u,
    coldStartCoarseHemisphere = coarseHemisphere,
    assumedMaxSpeedKn = session?.kinematics?.let {
        when (it) {
            io.github.spencerharmon.bris.engine.Session.Kinematics.Stationary -> 0.0
            is io.github.spencerharmon.bris.engine.Session.Kinematics.MaxSpeedKn -> it.kn
        }
    },
    storeDataRoot = storeDataRoot,
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
