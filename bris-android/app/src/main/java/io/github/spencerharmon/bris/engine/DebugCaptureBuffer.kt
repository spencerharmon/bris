package io.github.spencerharmon.bris.engine

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.time.Instant
import java.util.concurrent.atomic.AtomicLong
import uniffi.bris_ffi.DiagnosticSnapshot
import uniffi.bris_ffi.FfiFrame

/**
 * On-device debug-capture rolling buffer.
 *
 * When the operator enables Debug capture in settings, this
 * class persists every frame the engine processes plus the
 * accompanying diagnostic snapshot to local app storage. The
 * buffer is capped by total disk usage (default 1 GB);
 * oldest entries evict first when the cap is exceeded.
 *
 * On-disk layout, under
 * `<app-files-dir>/debug-capture/`:
 *
 *   index.jsonl                one line per persisted frame
 *   frames/<seq>.pgm           raw P5 grayscale frame data
 *   frames/<seq>.json          DiagnosticSnapshot at capture
 *   pbris.log                  rolling $PBRIS sentence window
 *
 * `<seq>` is a zero-padded 12-digit decimal sequence number;
 * monotonically increasing across the lifetime of the buffer
 * so that lexicographic sort = chronological sort. The
 * sequence persists across restarts via a 1-line `.seq` file
 * so that resuming after a crash doesn't reuse numbers.
 *
 * PGM (P5) is the chosen frame format because it round-trips
 * trivially through both the existing Bris regression-test
 * harness (which already consumes pgm/png) and Python tooling
 * (one-line decode), without requiring a PNG encoder dependency
 * on the Android side.
 *
 * Export flow (primary, operator-visible): the operator's
 * "Save buffer" action in Settings / Live / PreUploadReview
 * funnels through [`DebugBufferActions.saveAll`], which copies
 * the on-disk buffer to a Storage Access Framework destination.
 * This is the canonical local-export path and is available
 * whether or not a fix was produced.
 *
 * Collector-upload flow (secondary, gated by
 * `BuildConfig.ENABLE_REMOTE_SUBMIT`, off in this build):
 * [`PreUploadReviewScreen`] queries [`recentEntries`] and the
 * Submitter uploads each as a
 * [`io.github.spencerharmon.bris.upload.MediaSummary`].
 *
 * Threading: this class is `Sendable`-equivalent via internal
 * synchronization. There is one process-wide instance per
 * application context (see [`forApp`]), so all UI sites that
 * read [`stateFlow`] observe the same counters as the analyzer
 * thread that writes them.
 */
class DebugCaptureBuffer(
    private val rootDir: File,
    private val maxBytes: Long = DEFAULT_MAX_BYTES,
) {
    private val framesDir = File(rootDir, "frames").apply { mkdirs() }
    private val indexFile = File(rootDir, "index.jsonl")
    private val seqFile = File(rootDir, ".seq")
    private val seq = AtomicLong(loadSeq())
    private val totalBytes = AtomicLong(walkTotalBytes())
    private val frameCount = AtomicLong(countFramesFromIndex())
    private val lastAppendUnixMs = AtomicLong(0L)
    private val oldestFrameUnixMs = AtomicLong(0L)
    private val newestFrameUnixMs = AtomicLong(0L)
    private val evictedSinceClear = AtomicLong(0L)

    private val _stateFlow: MutableStateFlow<BufferState>

    /**
     * Observable buffer state. Emits on every append, eviction
     * and clear. Reads are cheap (in-memory counters); the
     * initial value is computed by scanning `index.jsonl` at
     * construction.
     */
    val stateFlow: StateFlow<BufferState>

    init {
        rootDir.mkdirs()
        seedTimestampsFromIndex()
        _stateFlow = MutableStateFlow(snapshotState())
        stateFlow = _stateFlow.asStateFlow()
    }

    /**
     * Persist one frame + snapshot. Synchronous (writes to
     * disk before returning); the camera analyzer thread may
     * call this on every frame.
     *
     * Returns the assigned sequence number, or `-1` if the
     * write failed (logged; the buffer continues operating).
     */
    @Synchronized
    fun appendFrame(frame: FfiFrame, snapshot: DiagnosticSnapshot?): Long {
        val n = seq.getAndIncrement()
        val tag = "%012d".format(n)
        val pgm = File(framesDir, "$tag.pgm")
        val json = File(framesDir, "$tag.json")
        try {
            writePgm(pgm, frame)
            writeSnapshot(json, n, frame, snapshot)
            appendIndex(n, frame, pgm.length(), json.length())
            persistSeq(n + 1)
            totalBytes.addAndGet(pgm.length() + json.length())
            frameCount.incrementAndGet()
            val capturedMs = frame.capturedUnixMs.toLong()
            lastAppendUnixMs.set(System.currentTimeMillis())
            if (oldestFrameUnixMs.get() == 0L) oldestFrameUnixMs.set(capturedMs)
            newestFrameUnixMs.set(capturedMs)
            evictIfOverCap()
            emitState()
            return n
        } catch (e: Exception) {
            android.util.Log.w(TAG, "appendFrame($tag) failed: $e")
            // Best-effort: clean up partials.
            pgm.delete()
            json.delete()
            return -1
        }
    }

    /**
     * Append one `$PBRIS` sentence to the rolling log. The log
     * is line-oriented; the submitter clips the last N lines
     * around a fix's timestamp at upload time.
     */
    @Synchronized
    fun appendPbris(line: String) {
        val log = File(rootDir, "pbris.log")
        try {
            val bytes = (line + "\n").toByteArray()
            FileOutputStream(log, true).use { it.write(bytes) }
            totalBytes.addAndGet(bytes.size.toLong())
        } catch (e: Exception) {
            android.util.Log.w(TAG, "appendPbris failed: $e")
        }
        lastAppendUnixMs.set(System.currentTimeMillis())
        emitState()
    }

    /**
     * Most-recent entries up to `limit`, oldest-first. Used by
     * the submitter to populate the manifest's `media` array.
     */
    @Synchronized
    fun recentEntries(limit: Int): List<Entry> {
        if (!indexFile.exists()) return emptyList()
        val lines = indexFile.readLines().takeLast(limit)
        return lines.mapNotNull { line ->
            try {
                val obj = JSONObject(line)
                val n = obj.getLong("seq")
                val tag = "%012d".format(n)
                Entry(
                    seq = n,
                    capturedUnixMs = obj.getLong("captured_unix_ms"),
                    width = obj.getInt("width"),
                    height = obj.getInt("height"),
                    framePath = File(framesDir, "$tag.pgm"),
                    snapshotPath = File(framesDir, "$tag.json"),
                )
            } catch (_: Exception) {
                null
            }
        }
    }

    /** Total bytes currently used by the buffer. */
    fun totalBytes(): Long = totalBytes.get()

    /** Wipe every persisted artifact. Called from settings. */
    @Synchronized
    fun clear() {
        framesDir.listFiles()?.forEach { it.delete() }
        indexFile.delete()
        File(rootDir, "pbris.log").delete()
        seqFile.delete()
        seq.set(0)
        totalBytes.set(0)
        frameCount.set(0)
        lastAppendUnixMs.set(0)
        oldestFrameUnixMs.set(0)
        newestFrameUnixMs.set(0)
        evictedSinceClear.set(0)
        emitState()
    }

    /** Root directory on disk; exposed for export tooling. */
    fun rootDir(): File = rootDir

    private fun emitState() {
        _stateFlow.value = snapshotState()
    }

    private fun snapshotState(): BufferState = BufferState(
        frameCount = frameCount.get().toInt(),
        totalBytes = totalBytes.get(),
        lastAppendUnixMs = lastAppendUnixMs.get().takeIf { it > 0L },
        oldestFrameUnixMs = oldestFrameUnixMs.get().takeIf { it > 0L },
        newestFrameUnixMs = newestFrameUnixMs.get().takeIf { it > 0L },
        evictedSinceClear = evictedSinceClear.get(),
    )

    private fun countFramesFromIndex(): Long =
        if (indexFile.exists()) indexFile.useLines { it.count().toLong() } else 0L

    private fun seedTimestampsFromIndex() {
        if (!indexFile.exists()) return
        try {
            val lines = indexFile.readLines()
            if (lines.isEmpty()) return
            val first = JSONObject(lines.first()).optLong("captured_unix_ms", 0L)
            val last = JSONObject(lines.last()).optLong("captured_unix_ms", 0L)
            if (first > 0L) oldestFrameUnixMs.set(first)
            if (last > 0L) newestFrameUnixMs.set(last)
        } catch (_: Exception) {
            // index unreadable — leave seeds at 0
        }
    }

    private fun loadSeq(): Long = try {
        if (seqFile.exists()) seqFile.readText().trim().toLong() else 0L
    } catch (_: Exception) {
        0L
    }

    private fun persistSeq(next: Long) {
        try {
            seqFile.writeText(next.toString())
        } catch (e: Exception) {
            android.util.Log.w(TAG, "persistSeq failed: $e")
        }
    }

    private fun walkTotalBytes(): Long {
        var sum = 0L
        if (framesDir.exists()) {
            sum += framesDir.listFiles()?.sumOf { it.length() } ?: 0L
        }
        // Top-level files (index.jsonl, pbris.log, .seq) also
        // count toward the on-disk cap so unbounded $PBRIS log
        // growth eventually triggers eviction.
        if (rootDir.exists()) {
            sum += rootDir.listFiles { f -> f.isFile }?.sumOf { it.length() } ?: 0L
        }
        return sum
    }

    private fun writePgm(path: File, frame: FfiFrame) {
        // PGM P5: "P5\n<w> <h>\n255\n" + raw bytes.
        // Frame is FfiPixelFormat.GRAY8 — pixels is already
        // a width*height byte buffer ready to write.
        FileOutputStream(path).use { out ->
            val header = "P5\n${frame.width} ${frame.height}\n255\n"
            out.write(header.toByteArray())
            out.write(frame.pixels)
        }
    }

    private fun writeSnapshot(
        path: File,
        n: Long,
        frame: FfiFrame,
        snapshot: DiagnosticSnapshot?,
    ) {
        val obj = JSONObject()
            .put("seq", n)
            .put("captured_unix_ms", frame.capturedUnixMs)
            .put("width", frame.width.toLong())
            .put("height", frame.height.toLong())
        if (snapshot != null) {
            val stages = JSONArray()
            for (s in snapshot.stages) {
                stages.put(
                    JSONObject()
                        .put("name", s.name)
                        .put("entered", s.entered.toLong())
                        .put("produced", s.produced.toLong())
                        .put("failed", s.failed.toLong())
                        .put("skipped", s.skipped.toLong()),
                )
            }
            val snap = JSONObject()
                .put("frames_pushed", snapshot.framesPushed.toLong())
                .put("frames_dropped", snapshot.framesDropped.toLong())
                .put("body_queue_depth", snapshot.bodyQueueDepth.toLong())
                .put("horizon_queue_depth", snapshot.horizonQueueDepth.toLong())
                .put("ring_buffer_depth", snapshot.ringBufferDepth.toLong())
                .put("sight_window_depth", snapshot.sightWindowDepth.toLong())
                .put("last_classification", snapshot.lastClassification ?: JSONObject.NULL)
                .put("stages", stages)
            obj.put("diagnostic_snapshot", snap)
        }
        path.writeText(obj.toString())
    }

    private fun appendIndex(n: Long, frame: FfiFrame, pgmBytes: Long, jsonBytes: Long) {
        val obj = JSONObject()
            .put("seq", n)
            .put("captured_unix_ms", frame.capturedUnixMs)
            .put("width", frame.width.toLong())
            .put("height", frame.height.toLong())
            .put("pgm_bytes", pgmBytes)
            .put("json_bytes", jsonBytes)
        FileOutputStream(indexFile, true).use { it.write((obj.toString() + "\n").toByteArray()) }
    }

    /**
     * Evict oldest frames until the buffer is back under cap.
     *
     * Reads the index file, deletes the corresponding files in
     * order, and rewrites the index without the evicted lines.
     * Index rewrite is atomic (write to .tmp, rename).
     */
    /**
     * Run the eviction pass without appending a new frame.
     * Test-only hook so unit tests can exercise the
     * empty-remainder cleanup path without constructing an
     * `FfiFrame` (which needs the native library).
     */
    @Synchronized
    internal fun evictForTests() {
        evictIfOverCap()
    }

    private fun evictIfOverCap() {
        if (totalBytes.get() <= maxBytes) return
        val lines = indexFile.readLines()
        var freed = 0L
        var evictCount = 0
        val target = totalBytes.get() - maxBytes
        for (line in lines) {
            if (freed >= target) break
            try {
                val obj = JSONObject(line)
                val n = obj.getLong("seq")
                val tag = "%012d".format(n)
                val pgm = File(framesDir, "$tag.pgm")
                val json = File(framesDir, "$tag.json")
                freed += pgm.length() + json.length()
                pgm.delete()
                json.delete()
                evictCount++
            } catch (_: Exception) {
                evictCount++
            }
        }
        if (evictCount > 0) {
            val remaining = lines.drop(evictCount).filter { it.isNotBlank() }
            if (remaining.isEmpty()) {
                indexFile.delete()
            } else {
                val tmp = File(rootDir, "index.jsonl.tmp")
                tmp.writeText(remaining.joinToString("\n", postfix = "\n"))
                tmp.renameTo(indexFile)
            }
            totalBytes.addAndGet(-freed)
            frameCount.addAndGet(-evictCount.toLong())
            evictedSinceClear.addAndGet(evictCount.toLong())
            // Refresh oldest from new index head; newest is unchanged.
            oldestFrameUnixMs.set(
                remaining.firstOrNull()?.let {
                    try { JSONObject(it).optLong("captured_unix_ms", 0L) } catch (_: Exception) { 0L }
                } ?: 0L,
            )
            if (frameCount.get() == 0L) newestFrameUnixMs.set(0L)
            emitState()
        }
    }

    /** Snapshot of buffer-state fields surfaced to the UI. */
    data class BufferState(
        val frameCount: Int,
        val totalBytes: Long,
        val lastAppendUnixMs: Long?,
        val oldestFrameUnixMs: Long?,
        val newestFrameUnixMs: Long?,
        val evictedSinceClear: Long,
    )

    /** One persisted frame's location and metadata. */
    data class Entry(
        val seq: Long,
        val capturedUnixMs: Long,
        val width: Int,
        val height: Int,
        val framePath: File,
        val snapshotPath: File,
    ) {
        /** ISO-8601 capture time. */
        fun capturedAt(): Instant = Instant.ofEpochMilli(capturedUnixMs)
    }

    companion object {
        private const val TAG = "DebugCaptureBuffer"

        /** Default cap: 1 GiB. */
        const val DEFAULT_MAX_BYTES: Long = 1L * 1024 * 1024 * 1024

        @Volatile
        private var instance: DebugCaptureBuffer? = null

        /**
         * Return the process-wide buffer for this application.
         *
         * Multiple UI Composables (LiveScreen, SettingsScreen,
         * PreUploadReviewScreen) read [`stateFlow`] and call
         * [`clear`] / [`appendFrame`] against the *same* on-disk
         * tree; constructing independent instances per call
         * site would let their in-memory counters
         * (`totalBytes`, `frameCount`, timestamps) drift out of
         * sync with each other and with reality after a clear,
         * corrupting the eviction cap. Keyed on
         * `applicationContext.filesDir` implicitly via the
         * application singleton.
         */
        fun forApp(context: Context): DebugCaptureBuffer {
            val app = context.applicationContext
            return getOrCreate(File(app.filesDir, "debug-capture"))
        }

        /**
         * Internal singleton creation hook. Visible for tests
         * so they can exercise the same caching code path
         * without a real Android `Context`.
         */
        internal fun getOrCreate(rootDir: File): DebugCaptureBuffer {
            instance?.let { return it }
            return synchronized(this) {
                instance ?: DebugCaptureBuffer(rootDir).also { instance = it }
            }
        }

        /** Test-only: drop the cached singleton so each test
         *  using a fresh temp dir gets a fresh buffer. */
        internal fun resetForTests() {
            synchronized(this) { instance = null }
        }
    }
}
