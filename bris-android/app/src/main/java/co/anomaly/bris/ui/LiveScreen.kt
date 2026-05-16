package co.anomaly.bris.ui

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
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
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
import co.anomaly.bris.BuildConfig
import co.anomaly.bris.Prefs
import co.anomaly.bris.engine.CalibrationStore
import co.anomaly.bris.engine.CameraConstants
import co.anomaly.bris.engine.DebugCaptureBuffer
import co.anomaly.bris.engine.EngineWrapper
import co.anomaly.bris.engine.FixVerdict
import co.anomaly.bris.engine.FrameAnalyzer
import co.anomaly.bris.engine.LensCatalog
import co.anomaly.bris.engine.SessionRecorder
import co.anomaly.bris.engine.SessionStatus
import co.anomaly.bris.engine.SightLog
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import uniffi.bris_ffi.FfiEngineConfig
import uniffi.bris_ffi.FfiIntrinsics
import uniffi.bris_ffi.FfiObserver
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
    val debugBuffer = remember(context) { DebugCaptureBuffer.forApp(context) }
    val calStore = remember(context) { CalibrationStore.forApp(context) }
    val persistedIntrinsics = remember(context, effectiveLensId) {
        calStore.latestIntrinsicsFor(
            lensId = effectiveLensId,
            width = CameraConstants.WIDTH,
            height = CameraConstants.HEIGHT,
        )
    }
    val sightLog = remember(context) { SightLog.forApp(context) }

    val engineScope = remember { CoroutineScope(SupervisorJob()) }
    val engine = remember {
        EngineWrapper.create(
            config = defaultEngineConfig(),
            scope = engineScope,
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
        onDispose { engine.close() }
    }

    val snapshot by engine.snapshot.collectAsState()
    val sessionStatus by recorder.status.collectAsState()
    val captureActive = sessionStatus is SessionStatus.Capturing ||
        sessionStatus is SessionStatus.Saving

    Box(modifier = Modifier.fillMaxSize()) {
        CameraSurface(
            lifecycleOwner = lifecycleOwner,
            captureActive = captureActive,
            engine = engine,
            persistedIntrinsics = persistedIntrinsics,
            debugCaptureEnabled = debugCaptureEnabled,
            debugBuffer = debugBuffer,
            lensId = effectiveLensId,
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
                ringBufferDepth = snapshot?.ringBufferDepth ?: 0u,
                sightWindowDepth = snapshot?.sightWindowDepth ?: 0u,
                persistedIntrinsics = persistedIntrinsics,
                lensLabel = lensLabelFor(context, effectiveLensId),
            )
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
                Button(onClick = onSendFix) { Text("Send fix (debug)") }
            }
        }
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
) {
    val context = LocalContext.current
    val previewView = remember(context) { PreviewView(context) }
    val analyzerExecutor = remember { Executors.newSingleThreadExecutor() }
    val cameraSelector = remember(lensId) { LensCatalog.selectorFor(lensId) }

    LaunchedEffect(captureActive, lifecycleOwner, cameraSelector) {
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
            android.util.Rational(CameraConstants.WIDTH, CameraConstants.HEIGHT),
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
                                CameraConstants.SIZE,
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
                            targetWidth = CameraConstants.WIDTH,
                            targetHeight = CameraConstants.HEIGHT,
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
    ringBufferDepth: UInt,
    sightWindowDepth: UInt,
    persistedIntrinsics: CalibrationStore.PersistedIntrinsics?,
    lensLabel: String,
) {
    val calibLabel = persistedIntrinsics?.let {
        if (it.width == CameraConstants.WIDTH && it.height == CameraConstants.HEIGHT) {
            "calib: rms ${"%.2f".format(it.rmsPx)} px"
        } else {
            "calib mismatch (${it.width}×${it.height} on ${CameraConstants.WIDTH}×${CameraConstants.HEIGHT})"
        }
    } ?: "calib: PLACEHOLDER (run calibration)"

    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(Color(0xCC000000))
            .padding(8.dp),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Text(calibLabel, color = Color.White)
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
                    is co.anomaly.bris.engine.SessionOutcome.Captured ->
                        "Captured ${o.verdict.name.lowercase()} fix " +
                            "(σ=${"%.2f".format(o.fix.sigmaMajorNm)} nm). Saved to ${s.sessionDir.name}."
                    is co.anomaly.bris.engine.SessionOutcome.NoFix ->
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
    }
}

/**
 * Pick intrinsics for the analyzer's resolution. Prefer
 * persisted calibration when available *and* the resolution
 * matches; otherwise fall back to placeholder defaults.
 *
 * Calibration data is keyed by camera + resolution. Applying a
 * 640×480 calibration to 1280×720 frames silently produces
 * wrong altitudes, so the resolution gate is mandatory; on
 * mismatch we degrade to placeholder and the diagnostic
 * overlay flags it.
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
    return placeholderIntrinsicsForCurrentResolution()
}

private fun placeholderIntrinsicsForCurrentResolution(): FfiIntrinsics =
    FfiIntrinsics(
        fx = 1000.0,
        fy = 1000.0,
        cx = 640.0,
        cy = 360.0,
        k1 = 0.0,
        k2 = 0.0,
        k3 = 0.0,
        p1 = 0.0,
        p2 = 0.0,
    )

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
