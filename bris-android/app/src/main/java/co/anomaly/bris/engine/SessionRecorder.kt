package co.anomaly.bris.engine

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
data class SessionThresholds(
    val targetSigmaNm: Double = 1.0,
    val hardSigmaNm: Double = 5.0,
    val sustainedGreenMs: Long = 3_000,
    val timeoutMs: Long = 5 * 60 * 1_000,
)

/** What the live screen renders to communicate session status. */
sealed interface SessionStatus {
    data object Idle : SessionStatus
    data class Capturing(
        val startedAtMs: Long,
        val lastFix: FfiPublishedFix? = null,
        val lastVerdict: FixVerdict? = null,
        val sustainedGreenSinceMs: Long? = null,
        val nGreen: Int = 0,
        val nYellow: Int = 0,
        val nRed: Int = 0,
    ) : SessionStatus
    data object Saving : SessionStatus
    data class Saved(val sessionDir: java.io.File, val outcome: SessionOutcome) : SessionStatus
    data class Failed(val reason: String) : SessionStatus
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
 *  3. The recorder emits [`SessionStatus.Capturing`] updates
 *     into [`status`] as fixes arrive. When sustained-green
 *     fires it transitions to [`SessionStatus.Saving`], pulls
 *     the contributing-frame bytes from the engine, writes the
 *     sight-log entry, and ends in [`SessionStatus.Saved`].
 *  4. The caller may also invoke [`stop`] to end early. If a
 *     non-red fix has ever been observed the session captures
 *     the best one; otherwise it ends with [`SessionOutcome.NoFix`].
 *  5. The session times out after `thresholds.timeoutMs` with
 *     the same best-effort accept-on-yellow-or-better logic as
 *     manual stop.
 *
 * Engine lifecycle is *not* this class's concern — the engine
 * is constructed once when the live screen composes and lives
 * across many sessions. This class only listens on the
 * pre-existing fix stream.
 */
class SessionRecorder(
    private val engine: EngineWrapper,
    private val sightLog: SightLog,
    private val scope: CoroutineScope,
    private val thresholds: SessionThresholds = SessionThresholds(),
    private val deviceUuidProvider: suspend () -> String,
    private val appVersion: String,
    private val coreVersionProvider: () -> String,
) {

    private val _status = MutableStateFlow<SessionStatus>(SessionStatus.Idle)
    val status: StateFlow<SessionStatus> = _status.asStateFlow()

    private var sessionJob: Job? = null
    private var bestFix: FfiPublishedFix? = null
    private var bestVerdict: FixVerdict = FixVerdict.RED
    private var pbrisLines: MutableList<String> = mutableListOf()
    private var sustainedGreenStartMs: Long? = null
    private var sessionId: String = ""
    private var sessionStartedAtMs: Long = 0L

    /**
     * Begin a session. No-op if a session is already active —
     * the caller (LiveScreen) is responsible for not calling
     * Start while a session is running.
     *
     * Returns the session ID assigned. Caller logs / persists
     * for cross-reference if needed.
     */
    fun start(): String {
        if (sessionJob?.isActive == true) {
            return sessionId
        }
        sessionId = ulid()
        sessionStartedAtMs = System.currentTimeMillis()
        bestFix = null
        bestVerdict = FixVerdict.RED
        pbrisLines.clear()
        sustainedGreenStartMs = null

        _status.value = SessionStatus.Capturing(
            startedAtMs = sessionStartedAtMs,
            lastFix = null,
            lastVerdict = null,
        )

        sessionJob = scope.launch {
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
                    val elapsed = System.currentTimeMillis() - sessionStartedAtMs
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
        return sessionId
    }

    /**
     * Operator pressed Stop. Ends the session and writes
     * whichever best fix the recorder collected, if any.
     */
    fun stop() {
        scope.launch { finalize("operator stopped") }
    }

    /** True when a session is currently active. */
    fun isActive(): Boolean = sessionJob?.isActive == true

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
        val s = _status.value as? SessionStatus.Capturing
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
        // _status as we go, then null sessionJob at the end.
        val job = sessionJob ?: return
        if (!job.isActive) return
        _status.value = SessionStatus.Saving

        val outcome: SessionOutcome = bestFix?.let { fix ->
            SessionOutcome.Captured(fix = fix, verdict = bestVerdict)
        } ?: SessionOutcome.NoFix(reason = reasonForUi)

        // Pull contributing-frame bytes from the engine *now*,
        // before they evict from the ring buffer.
        val frames = mutableMapOf<ULong, SightLog.FrameBytes>()
        if (outcome is SessionOutcome.Captured) {
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
            val sessionDir = sightLog.writeEntry(
                sessionId = sessionId,
                outcome = outcome,
                frames = frames,
                pbrisLines = pbrisLines.toList(),
                deviceUuid = deviceUuid,
                appVersion = appVersion,
                coreVersion = coreVersion,
            )
            _status.value = SessionStatus.Saved(sessionDir = sessionDir, outcome = outcome)
        } catch (t: Throwable) {
            _status.value = SessionStatus.Failed(
                reason = "write failed: ${t.javaClass.simpleName}: ${t.message ?: "?"}",
            )
        } finally {
            // Stop the per-session collector + watchdog. We
            // schedule the cancel in the parent scope so the
            // current coroutine isn't cancelling itself.
            scope.launch { sessionJob?.cancelAndJoin(); sessionJob = null }
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
