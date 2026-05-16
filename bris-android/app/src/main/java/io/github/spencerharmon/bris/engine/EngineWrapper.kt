package io.github.spencerharmon.bris.engine

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.onEach
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import uniffi.bris_ffi.DiagnosticSnapshot
import uniffi.bris_ffi.Engine as RustEngine
import uniffi.bris_ffi.FfiEngineConfig
import uniffi.bris_ffi.FfiFrame
import uniffi.bris_ffi.FfiPublishedFix
import uniffi.bris_ffi.FixSubscriber
import uniffi.bris_ffi.engineNew
import uniffi.bris_ffi.formatPbris

/**
 * Kotlin-friendly wrapper around the UniFFI-generated `Engine`.
 *
 * Three jobs:
 *
 *  1. Construct the Rust engine ([`engineNew`]) and own its
 *     `Arc<Engine>`-equivalent handle.
 *  2. Expose the engine's published-fix stream as a Kotlin
 *     `Flow<FfiPublishedFix>` consumable by Compose UI and
 *     coroutine scopes.
 *  3. Drive a periodic diagnostic-snapshot poll into a
 *     `StateFlow<DiagnosticSnapshot?>` so the live view can
 *     render queue depths, classifier verdict, etc., without
 *     each composition pulling the FFI itself.
 *
 * The wrapper is constructed once per session (typically by a
 * `ViewModel`) and `close()`d when the session ends. Closing
 * cancels the polling coroutine; the underlying Rust engine is
 * dropped when no Kotlin references remain (UniFFI's `Arc`
 * release semantics).
 */
class EngineWrapper private constructor(
    private val rust: RustEngine,
    private val scope: CoroutineScope,
    private val pbrisSink: ((String) -> Unit)? = null,
) : AutoCloseable {

    private val _snapshot = MutableStateFlow<DiagnosticSnapshot?>(null)

    /** Latest diagnostic snapshot, or `null` before the first poll. */
    val snapshot: StateFlow<DiagnosticSnapshot?> = _snapshot.asStateFlow()

    private val _fixes = MutableSharedFlow<FfiPublishedFix>(
        replay = 0,
        extraBufferCapacity = 64,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    /** Stream of fixes published by the engine after subscription. */
    val fixes: Flow<FfiPublishedFix> = _fixes.asSharedFlow()

    private val subscriber = object : FixSubscriber {
        override fun onFix(fix: FfiPublishedFix) {
            _fixes.tryEmit(fix)
        }
        override fun onClosed() {
            // Subscription ends when the engine is dropped; we
            // emit no synthetic terminal value because the
            // SharedFlow contract has no "end of stream"
            // notion. Consumers that care use `close()` on the
            // wrapper to know.
        }
    }

    init {
        rust.subscribeFixes(subscriber)
        scope.launch(Dispatchers.Default) {
            while (isActive) {
                _snapshot.value = rust.snapshot()
                kotlinx.coroutines.delay(SNAPSHOT_POLL_INTERVAL_MS)
            }
        }
        // If a $PBRIS sink is configured, format every fix and
        // forward to the sink. Used by the debug-capture buffer
        // to maintain its rolling pbris.log alongside the
        // captured frames.
        if (pbrisSink != null) {
            fixes.onEach { fix ->
                for (line in formatPbris(fix)) {
                    pbrisSink.invoke(line)
                }
            }.launchIn(scope)
        }
    }

    /** Push a captured frame to the engine. */
    fun pushFrame(frame: FfiFrame) {
        rust.pushFrame(frame)
    }

    /**
     * Look up a previously-pushed frame by its engine-assigned
     * ID. Returns null when the frame has been evicted from
     * the ring buffer.
     *
     * Used by the session recorder to copy contributing-frame
     * pixel bytes out of the engine into a sight-log entry,
     * after a fix publishes and before the sight window ages
     * past those frames.
     */
    fun frameById(id: ULong): FfiFrame? = rust.frameById(id)

    override fun close() {
        // Cancel the polling coroutine. The Rust engine stays
        // alive as long as `rust` holds a reference; UniFFI
        // releases it when the wrapper goes out of scope.
        scope.coroutineContext[kotlinx.coroutines.Job]?.cancel()
    }

    companion object {
        /**
         * How often to refresh `snapshot`. 100 ms gives a
         * smooth UI without thrashing the engine's mutex.
         */
        private const val SNAPSHOT_POLL_INTERVAL_MS = 100L

        /**
         * Construct a new engine + wrapper.
         *
         * @param config Engine configuration. Provide via
         *               [`FfiEngineConfig`].
         * @param scope  Coroutine scope owning the diagnostic
         *               poll and the optional `$PBRIS` sink
         *               coroutine. Typically the host
         *               `ViewModel`'s `viewModelScope`.
         * @param pbrisSink Optional callback receiving every
         *                  formatted `$PBRIS` line published
         *                  for a fix. The wrapper invokes the
         *                  sink on a background dispatcher;
         *                  the sink must be thread-safe.
         */
        fun create(
            config: FfiEngineConfig,
            scope: CoroutineScope,
            pbrisSink: ((String) -> Unit)? = null,
        ): EngineWrapper {
            val rust = engineNew(config)
            return EngineWrapper(rust, scope, pbrisSink)
        }
    }
}
