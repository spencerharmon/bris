package co.anomaly.bris.ui

import android.Manifest
import android.content.pm.PackageManager
import android.util.Size
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import co.anomaly.bris.Prefs
import co.anomaly.bris.engine.CalibrationStore
import co.anomaly.bris.engine.DebugCaptureBuffer
import co.anomaly.bris.engine.EngineWrapper
import co.anomaly.bris.engine.FrameAnalyzer
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.SupervisorJob
import uniffi.bris_ffi.FfiEngineConfig
import uniffi.bris_ffi.FfiIntrinsics
import uniffi.bris_ffi.FfiObserver
import java.util.concurrent.Executors

/**
 * Live camera preview + engine pipeline + diagnostic overlay.
 *
 * Lifecycle: the [`EngineWrapper`] is created with a per-screen
 * `CoroutineScope` and disposed when this composable leaves the
 * composition. Camera binding uses
 * [`ProcessCameraProvider.bindToLifecycle`] tied to the
 * activity's lifecycle owner, so backgrounding the app stops
 * frame delivery automatically.
 *
 * Backpressure: [`ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST`] —
 * see `docs/design/diagnostic_collection.md`.
 *
 * Camera permission is requested on first composition. While
 * permission is denied, the screen renders an explainer with
 * a "grant" button.
 *
 * Intrinsics: until the operator runs calibration, the engine
 * receives placeholder intrinsics. Per `plan.org` Phase 2.5,
 * absolute altitudes are off by the calibration factor in this
 * state — the diagnostic overlay still surfaces the engine's
 * per-stage σ, queue depths, and classifier verdict honestly,
 * which is what the spike needs for corpus capture.
 */
@Composable
fun LiveScreen(
    debugMode: Boolean,
    onOpenSettings: () -> Unit,
    onSendFix: () -> Unit,
    onOpenCalibration: () -> Unit,
) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current

    var hasCameraPermission by remember {
        mutableStateOf(
            ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) ==
                PackageManager.PERMISSION_GRANTED
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

    // Debug-capture buffer is app-scoped (lives across screens
    // and even across activity recreations) so the captured
    // frames survive a `rotate` or backgrounding.
    val prefs = remember(context) { Prefs(context) }
    val debugCaptureEnabled by prefs.debugCaptureFlow.collectAsState(initial = false)
    val debugBuffer = remember(context) { DebugCaptureBuffer.forApp(context) }
    val calStore = remember(context) { CalibrationStore.forApp(context) }
    // Snapshot persisted intrinsics once at composition; new
    // calibrations require leaving + re-entering the screen.
    val persistedIntrinsics = remember(context) { calStore.latestIntrinsics() }

    // One engine per screen. Disposed via DisposableEffect.
    // The $PBRIS sink only writes when debug capture is on, so
    // operators who never enable capture pay zero disk I/O.
    val engine = remember {
        EngineWrapper.create(
            config = defaultEngineConfig(),
            scope = CoroutineScope(SupervisorJob()),
            pbrisSink = { line ->
                if (debugCaptureEnabled) debugBuffer.appendPbris(line)
            },
        )
    }
    DisposableEffect(engine) {
        onDispose { engine.close() }
    }

    val snapshot by engine.snapshot.collectAsState()

    Box(modifier = Modifier.fillMaxSize()) {
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
                    val analysis = ImageAnalysis.Builder()
                        .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                        .setResolutionSelector(
                            ResolutionSelector.Builder()
                                .setResolutionStrategy(
                                    ResolutionStrategy(
                                        Size(1280, 720),
                                        ResolutionStrategy.FALLBACK_RULE_CLOSEST_LOWER_THEN_HIGHER,
                                    ),
                                )
                                .build(),
                        )
                        .build()
                    analysis.setAnalyzer(
                        Executors.newSingleThreadExecutor(),
                        FrameAnalyzer(
                            engine = engine,
                            intrinsicsProvider = {
                                intrinsicsForResolution(
                                    persistedIntrinsics,
                                    targetWidth = LIVE_VIEW_WIDTH,
                                    targetHeight = LIVE_VIEW_HEIGHT,
                                )
                            },
                            debugCaptureProvider = { debugCaptureEnabled },
                            debugBuffer = debugBuffer,
                        ),
                    )
                    provider.unbindAll()
                    provider.bindToLifecycle(
                        lifecycleOwner,
                        CameraSelector.DEFAULT_BACK_CAMERA,
                        preview,
                        analysis,
                    )
                }, ContextCompat.getMainExecutor(ctx))
                previewView
            },
        )

        Column(
            modifier = Modifier.padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            val s = snapshot
            if (s == null) {
                Text("waiting for first frame…")
            } else {
                val calibLabel = persistedIntrinsics?.let {
                    if (it.width == LIVE_VIEW_WIDTH && it.height == LIVE_VIEW_HEIGHT) {
                        "calib: rms ${"%.2f".format(it.rmsPx)} px"
                    } else {
                        "calib mismatch (${it.width}×${it.height} on ${LIVE_VIEW_WIDTH}×${LIVE_VIEW_HEIGHT})"
                    }
                } ?: "calib: PLACEHOLDER (run calibration)"
                Text(calibLabel)
                Text("frames pushed: ${s.framesPushed}  dropped: ${s.framesDropped}")
                Text("classifier: ${s.lastClassification ?: "—"}")
                Text(
                    "queues  body=${s.bodyQueueDepth}  horizon=${s.horizonQueueDepth}" +
                        "  ring=${s.ringBufferDepth}  sights=${s.sightWindowDepth}",
                )
            }
            Spacer(Modifier.height(12.dp))
            OutlinedButton(onClick = onOpenSettings) { Text("Settings") }
            OutlinedButton(onClick = onOpenCalibration) { Text("Calibration") }
            if (debugMode) {
                Button(onClick = onSendFix) { Text("Send fix (debug)") }
            }
        }
    }
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
)

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

private const val LIVE_VIEW_WIDTH = 1280
private const val LIVE_VIEW_HEIGHT = 720

/**
 * Placeholder intrinsics. Until calibration is wired through
 * the FFI and a per-camera intrinsics file is loaded, this
 * returns fx = fy = 1000 (the same defaults the Rust
 * `Intrinsics::placeholder` uses), which makes absolute
 * altitudes wrong by the calibration factor but keeps the
 * pixel-space pipeline behavior honest.
 *
 * Resolution-aware: principal point is the image center.
 * Wired to the actual analyzer resolution once the analyzer
 * surfaces the chosen size; today we hard-code 1280×720 to
 * match `LiveScreen`'s `ResolutionStrategy`.
 */
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
