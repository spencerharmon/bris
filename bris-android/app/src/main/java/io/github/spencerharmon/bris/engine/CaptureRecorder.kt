package io.github.spencerharmon.bris.engine

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import uniffi.bris_ffi.FfiPublishedFix
import uniffi.bris_ffi.formatPbris

/**
 * Threshold defaults for the operator-driven sight-capture
 * session.
 *
 * Mirrors plan.org Phase 7 ("Sight session UX"):
 *
 *  * `targetSigmaNm` is the σ_major below which a fix is
 *    "green" — auto-accept-eligible after sustained.
 *  * `hardSigmaNm` is the σ_major above which a fix is "red" —
 *    never accepted, even at session timeout.
 *  * `sustainedGreenMs` is the duration of consecutive green
 *    fixes before auto-accept fires.
 *  * `timeoutMs` is the hard wall-clock cap on session length.
 *
 * Hard-coded for the developer-iteration version; an operator-
 * facing settings UI will surface them under Phase 7's session
 * UX work item.
 */
data class CaptureThresholds(
    val targetSigmaNm: Double = 1.0,
    val hardSigmaNm: Double = 5.0,
    val sustainedGreenMs: Long = 3_000,
    val timeoutMs: Long = 5 * 60 * 1_000,
)

/** What the live screen renders to communicate session status. */
sealed interface CaptureStatus {
    data object Idle : CaptureStatus
    data class Capturing(
        val startedAtMs: Long,
        val lastFix: FfiPublishedFix? = null,
        val lastVerdict: FixVerdict? = null,
        val sustainedGreenSinceMs: Long? = null,
        val nGreen: Int = 0,
        val nYellow: Int = 0,
        val nRed: Int = 0,
    ) : CaptureStatus
    data object Saving : CaptureStatus
    data class Saved(val captureDir: java.io.File, val outcome: CaptureOutcome) : CaptureStatus
    data class Failed(val reason: String) : CaptureStatus
}

/**
 * Drives one operator-initiated capture session.
 *
 * Lifecycle:
 *
 *  1. Caller (LiveScreen) constructs a recorder bound to an
 *     [`EngineWrapper`], a [`SightLog`], and a [`CoroutineScope`]
 *     tied to the screen's composition.
 *  2. Caller invokes [`start`] when the operator taps **Start
 *     capture** — the recorder begins consuming the engine's
 *     fix stream and the analyzer (caller-managed) is
 *     simultaneously bound on the camera side.
 *  3. The recorder emits [`CaptureStatus.Capturing`] updates
 *     into [`status`] as fixes arrive. When sustained-green
 *     fires it transitions to [`CaptureStatus.Saving`], pulls
 *     the contributing-frame bytes from the engine, writes the
 *     sight-log entry, and ends in [`CaptureStatus.Saved`].
 *  4. The caller may also invoke [`stop`] to end early. If a
 *     non-red fix has ever been observed the session captures
 *     the best one; otherwise it ends with [`CaptureOutcome.NoFix`].
 *  5. The session times out after `thresholds.timeoutMs` with
 *     the same best-effort accept-on-yellow-or-better logic as
 *     manual stop.
 *
 * Engine lifecycle is *not* this class's concern — the engine
 * is constructed once when the live screen composes and lives
 * across many sessions. This class only listens on the
 * pre-existing fix stream.
 */
class CaptureRecorder(
    private val engine: EngineWrapper,
    private val sightLog: SightLog,
    private val scope: CoroutineScope,
    private val thresholds: CaptureThresholds = CaptureThresholds(),
    private val deviceUuidProvider: suspend () -> String,
    private val appVersion: String,
    private val coreVersionProvider: () -> String,
    private val onCaptureSaved: ((String) -> Unit)? = null,
    /**
     * Returns the on-disk directory the capture's bundle
     * payload should be written to. The recorder writes
     * `frames/`, `index.jsonl`, and `bundle.json` here
     * (in addition to the sight-log entry produced by
     * `SightLog.writeEntry`). `null` disables the streaming
     * frame-tap and bundle write (legacy behavior).
     */
    private val captureDirProvider: ((String) -> java.io.File)? = null,
    /**
     * Builds the [`DebugBundleWriter.Inputs`] used for the
     * per-capture `bundle.json` write at finalize. Called
     * once at finalize, after the frame writer is closed.
     */
    private val bundleInputsProvider: (() -> DebugBundleWriter.Inputs)? = null,
) {

    private val _status = MutableStateFlow<CaptureStatus>(CaptureStatus.Idle)
    val status: StateFlow<CaptureStatus> = _status.asStateFlow()

    private var captureJob: Job? = null
    private var bestFix: FfiPublishedFix? = null
    private var bestVerdict: FixVerdict = FixVerdict.RED
    private var pbrisLines: MutableList<String> = mutableListOf()
    private var sustainedGreenStartMs: Long? = null
    private var captureId: String = ""
    private var captureStartedAtMs: Long = 0L
    private var frameWriter: CaptureFrameWriter? = null
    private var captureDir: java.io.File? = null

    /**
     * Per-analyzer-frame tap. LiveScreen wires this as
     * `FrameAnalyzer.captureFrameTap` so every frame seen
     * during Start→Stop lands in the capture directory's
     * `frames/` regardless of fix outcome. No-op when the
     * recorder isn't currently capturing.
     */
    fun onAnalyzerFrame(frame: uniffi.bris_ffi.FfiFrame) {
        val writer = frameWriter ?: return
        try {
            writer.appendFrame(
                width = frame.width.toInt(),
                height = frame.height.toInt(),
                pixels = frame.pixels,
                capturedUnixMs = frame.capturedUnixMs.toLong(),
            )
        } catch (t: Throwable) {
            android.util.Log.w(
                TAG,
                "frame tap drop: ${t.javaClass.simpleName}: ${t.message ?: "?"}",
            )
        }
    }

    /**
     * Begin a session. No-op if a session is already active —
     * the caller (LiveScreen) is responsible for not calling
     * Start while a session is running.
     *
     * Returns the session ID assigned. Caller logs / persists
     * for cross-reference if needed.
     */
    fun start(): String {
        if (captureJob?.isActive == true) {
            return captureId
        }
        captureId = ulid()
        captureStartedAtMs = System.currentTimeMillis()
        bestFix = null
        bestVerdict = FixVerdict.RED
        pbrisLines.clear()
        sustainedGreenStartMs = null

        // Open the streaming frame writer for this capture.
        // captureDirProvider == null → legacy mode, no
        // frame tap, no bundle.json.
        captureDir = captureDirProvider?.invoke(captureId)?.also { dir ->
            dir.mkdirs()
            frameWriter = CaptureFrameWriter(dir)
        }

        _status.value = CaptureStatus.Capturing(
            startedAtMs = captureStartedAtMs,
            lastFix = null,
            lastVerdict = null,
        )

        captureJob = scope.launch {
            // Two parallel concerns:
            //   * consume engine.fixes, score and update state.
            //   * watch for sustained-green / timeout end
            //     conditions.
            val collector = launch {
                engine.fixes.collect { fix ->
                    onFix(fix)
                }
            }
            val watchdog = launch {
                while (isActive) {
                    val elapsed = System.currentTimeMillis() - captureStartedAtMs
                    if (elapsed >= thresholds.timeoutMs) {
                        finalize("timeout after ${thresholds.timeoutMs} ms")
                        return@launch
                    }
                    val sustainStart = sustainedGreenStartMs
                    if (sustainStart != null) {
                        val sustained = System.currentTimeMillis() - sustainStart
                        if (sustained >= thresholds.sustainedGreenMs) {
                            finalize("sustained green for ${sustained} ms")
                            return@launch
                        }
                    }
                    delay(WATCHDOG_TICK_MS)
                }
            }
            // Suspend until the watchdog finalize calls cancel
            // ourselves via stop().
            collector.join()
            watchdog.cancelAndJoin()
        }
        return captureId
    }

    /**
     * Operator pressed Stop. Ends the session and writes
     * whichever best fix the recorder collected, if any.
     */
    fun stop() {
        scope.launch { finalize("operator stopped") }
    }

    /** True when a session is currently active. */
    fun isActive(): Boolean = captureJob?.isActive == true

    private suspend fun onFix(fix: FfiPublishedFix) {
        val verdict = score(fix)
        val now = System.currentTimeMillis()
        // Update sustained-green tracking.
        sustainedGreenStartMs = if (verdict == FixVerdict.GREEN) {
            sustainedGreenStartMs ?: now
        } else {
            null
        }
        // Track best-so-far (lowest σ_major ever observed,
        // among non-red fixes).
        if (verdict != FixVerdict.RED) {
            val current = bestFix
            if (current == null || fix.sigmaMajorNm < current.sigmaMajorNm) {
                bestFix = fix
                bestVerdict = verdict
            }
        }
        // Format $PBRIS,FIX for the rolling per-session log.
        for (line in formatPbrisOrEmpty(fix)) {
            pbrisLines.add(line)
        }
        // Update counters in status.
        val s = _status.value as? CaptureStatus.Capturing
        if (s != null) {
            _status.value = s.copy(
                lastFix = fix,
                lastVerdict = verdict,
                sustainedGreenSinceMs = sustainedGreenStartMs,
                nGreen = s.nGreen + (if (verdict == FixVerdict.GREEN) 1 else 0),
                nYellow = s.nYellow + (if (verdict == FixVerdict.YELLOW) 1 else 0),
                nRed = s.nRed + (if (verdict == FixVerdict.RED) 1 else 0),
            )
        }
    }

    private suspend fun finalize(reasonForUi: String) {
        // Cancel the session job's children. The job itself
        // continues executing this coroutine; we replace
        // _status as we go, then null captureJob at the end.
        val job = captureJob ?: return
        if (!job.isActive) return
        _status.value = CaptureStatus.Saving

        val outcome: CaptureOutcome = bestFix?.let { fix ->
            CaptureOutcome.Captured(fix = fix, verdict = bestVerdict)
        } ?: CaptureOutcome.NoFix(reason = reasonForUi)

        // Pull contributing-frame bytes from the engine *now*,
        // before they evict from the ring buffer.
        val frames = mutableMapOf<ULong, SightLog.FrameBytes>()
        if (outcome is CaptureOutcome.Captured) {
            for (frameId in outcome.fix.contributingFrameIds) {
                val ff = engine.frameById(frameId) ?: continue
                frames[frameId] = SightLog.FrameBytes(
                    width = ff.width.toInt(),
                    height = ff.height.toInt(),
                    pixelsLe = ff.pixels,
                    capturedAt = java.time.Instant.ofEpochMilli(ff.capturedUnixMs),
                )
            }
        }

        try {
            val deviceUuid = deviceUuidProvider()
            val coreVersion = coreVersionProvider()
            val sightDir = sightLog.writeEntry(
                captureId = captureId,
                outcome = outcome,
                frames = frames,
                pbrisLines = pbrisLines.toList(),
                deviceUuid = deviceUuid,
                appVersion = appVersion,
                coreVersion = coreVersion,
            )

            // Close the streaming frame writer (if any) and
            // emit `bundle.json` into the capture directory.
            // This is what makes the capture self-contained:
            // every analyzer frame seen during Start→Stop is
            // already on disk (via the frame tap), and now
            // the manifest gets written alongside.
            val writer = frameWriter
            val dir = captureDir
            if (writer != null && dir != null) {
                writer.close()
                frameWriter = null
                val inputs = bundleInputsProvider?.invoke()
                if (inputs != null) {
                    val frameCount = writer.frameCount()
                    val startedMs = writer.startedUnixMs() ?: captureStartedAtMs
                    val endedMs = writer.endedUnixMs() ?: System.currentTimeMillis()
                    val snap = DebugCaptureBuffer.CaptureSnapshot(
                        frameCount = frameCount.toLong(),
                        startedUnixMs = startedMs,
                        endedUnixMs = endedMs,
                        // CaptureRecorder doesn't compute a frame
                        // hash today; bundle integrity verification
                        // via blake3 is reserved for the share-
                        // capture export path. Empty string =
                        // "not recorded" downstream.
                        firstFrameBlake3 = "",
                        firstFrameWidth = 0,
                        firstFrameHeight = 0,
                    )
                    DebugBundleWriter.write(
                        bundleDir = dir,
                        bundleId = captureId,
                        snapshot = snap,
                        inputs = inputs,
                    )
                }
            }

            _status.value = CaptureStatus.Saved(captureDir = sightDir, outcome = outcome)
            // ALWAYS append, fix-or-no-fix. The capture
            // happened on disk regardless of outcome.
            onCaptureSaved?.invoke(captureId)
        } catch (t: Throwable) {
            _status.value = CaptureStatus.Failed(
                reason = "write failed: ${t.javaClass.simpleName}: ${t.message ?: "?"}",
            )
            runCatching { frameWriter?.close() }
            frameWriter = null
        } finally {
            // Stop the per-session collector + watchdog. We
            // schedule the cancel in the parent scope so the
            // current coroutine isn't cancelling itself.
            scope.launch { captureJob?.cancelAndJoin(); captureJob = null }
        }
    }

    private fun score(fix: FfiPublishedFix): FixVerdict {
        val s = fix.sigmaMajorNm
        return when {
            s <= thresholds.targetSigmaNm -> FixVerdict.GREEN
            s <= thresholds.hardSigmaNm -> FixVerdict.YELLOW
            else -> FixVerdict.RED
        }
    }

    private fun formatPbrisOrEmpty(fix: FfiPublishedFix): List<String> = try {
        formatPbris(fix)
    } catch (_: Throwable) {
        emptyList()
    }

    companion object {
        private const val WATCHDOG_TICK_MS = 200L
        private const val TAG = "CaptureRecorder"
    }
}

/** kotlinx.coroutines doesn't export this from a top-level. */
private suspend fun Job.cancelAndJoin() {
    cancel()
    join()
}

/**
 * Tiny ULID-ish session identifier. Same shape used by
 * `CalibrationStore.ulid` — millisecond-precision time prefix
 * + UUID bits, lexicographically sortable.
 */
private fun ulid(): String {
    val ms = System.currentTimeMillis()
    val r = java.util.UUID.randomUUID().leastSignificantBits
    return "%013x%016x".format(ms, r)
}
