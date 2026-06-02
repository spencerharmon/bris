package io.github.spencerharmon.bris.engine

import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream

/**
 * Streaming writer for a single capture's frame payload.
 *
 * Lifecycle:
 *  - constructed at Start with the capture's root directory
 *    `<external-files>/sessions/<UUID>/captures/<cap-id>/`.
 *  - [appendFrame] is called per analyzer frame (Debug ON
 *    only). Writes one PGM + one sidecar JSON per frame to
 *    `frames/NNNNNNNN.{pgm,json}` with sidecar
 *    `retention: "debug"`, and appends a row to
 *    `index.jsonl`.
 *  - [writeFixFrame] is called from `CaptureRecorder.finalize`
 *    for each contributing-fix frame. If the frame is already
 *    on disk (Debug ON path), promotes the existing sidecar's
 *    `retention` from `"debug"` to `"fix_frame"` in place \u2014
 *    no file copy. If not present (Debug OFF path), writes
 *    the PGM + sidecar fresh, then appends an index row.
 *  - [close] flushes and is idempotent.
 *
 * Retention class lives in the sidecar so the same `frames/`
 * directory holds both fix-frames (always-kept) and
 * debug-frames (eligible for future purge). No `media/`
 * mirror.
 *
 * Pure-JVM. Concurrency: caller serializes calls (the
 * analyzer executor is single-threaded in `LiveScreen`;
 * `CaptureRecorder.finalize` runs after the analyzer has
 * stopped feeding this writer).
 */
class CaptureFrameWriter(
    private val captureDir: File,
) {

    private val framesDir: File = File(captureDir, "frames").apply { mkdirs() }
    private val indexFile: File = File(captureDir, "index.jsonl")
    private val indexStream: FileOutputStream = FileOutputStream(indexFile, /* append */ true)
    private var seq: Int = 0
    private var firstUnixMs: Long? = null
    private var lastUnixMs: Long? = null
    private var closed: Boolean = false

    /**
     * Frame retention class. Written into the sidecar
     * (`retention` field). A future debug-data purge action
     * walks `frames/` sidecar files and deletes files whose sidecar
     * carries `Debug`; `FixFrame` always kept.
     */
    enum class Retention { Debug, FixFrame }

    init {
        captureDir.mkdirs()
    }

    /**
     * Append one analyzer frame with `retention: "debug"`.
     * Returns the on-disk PGM file just written.
     */
    fun appendFrame(
        width: Int,
        height: Int,
        pixels: ByteArray,
        capturedUnixMs: Long,
        diagnosticSnapshot: JSONObject? = null,
    ): File = writeFrame(
        width = width,
        height = height,
        pixels = pixels,
        capturedUnixMs = capturedUnixMs,
        retention = Retention.Debug,
        diagnosticSnapshot = diagnosticSnapshot,
    )

    /**
     * Write or promote a fix-frame.
     *
     * If a sidecar already exists for `frameSeq` (Debug ON
     * pre-wrote it), rewrites its `retention` to
     * `"fix_frame"` in place. Otherwise writes a fresh PGM +
     * sidecar.
     *
     * `frameSeq` is the seq this frame had at write time
     * (Debug ON), or `null` to assign a fresh seq.
     */
    fun writeFixFrame(
        width: Int,
        height: Int,
        pixels: ByteArray,
        capturedUnixMs: Long,
        diagnosticSnapshot: JSONObject? = null,
        frameSeq: Int? = null,
    ): File {
        check(!closed) { "CaptureFrameWriter already closed" }
        if (frameSeq != null) {
            val name = "%08d".format(frameSeq)
            val pgm = File(framesDir, "$name.pgm")
            val side = File(framesDir, "$name.json")
            if (pgm.isFile && side.isFile) {
                // Promote in place: rewrite sidecar only.
                val obj = JSONObject(side.readText())
                obj.put("retention", "fix_frame")
                side.writeText(obj.toString())
                return pgm
            }
        }
        return writeFrame(
            width = width,
            height = height,
            pixels = pixels,
            capturedUnixMs = capturedUnixMs,
            retention = Retention.FixFrame,
            diagnosticSnapshot = diagnosticSnapshot,
            explicitSeq = frameSeq,
        )
    }

    private fun writeFrame(
        width: Int,
        height: Int,
        pixels: ByteArray,
        capturedUnixMs: Long,
        retention: Retention,
        diagnosticSnapshot: JSONObject?,
        explicitSeq: Int? = null,
    ): File {
        check(!closed) { "CaptureFrameWriter already closed" }
        require(pixels.size == width * height) {
            "pixels.size=${pixels.size} != width*height=${width * height}"
        }
        val mySeq = explicitSeq ?: seq
        val name = "%08d".format(mySeq)
        val pgmFile = File(framesDir, "$name.pgm")
        val jsonFile = File(framesDir, "$name.json")

        writePgm(pgmFile, width, height, pixels)

        val sidecar = JSONObject()
            .put("seq", mySeq)
            .put("captured_unix_ms", capturedUnixMs)
            .put("width", width)
            .put("height", height)
            .put("retention", retentionLabel(retention))
        if (diagnosticSnapshot != null) {
            sidecar.put("diagnostic_snapshot", diagnosticSnapshot)
        }
        jsonFile.writeText(sidecar.toString())

        val indexRow = JSONObject()
            .put("seq", mySeq)
            .put("captured_unix_ms", capturedUnixMs)
            .put("width", width)
            .put("height", height)
            .put("pgm_bytes", pgmFile.length())
            .put("json_bytes", jsonFile.length())
            .put("retention", retentionLabel(retention))
        indexStream.write((indexRow.toString() + "\n").toByteArray())
        indexStream.flush()

        if (firstUnixMs == null) firstUnixMs = capturedUnixMs
        lastUnixMs = capturedUnixMs
        if (explicitSeq == null) seq += 1
        return pgmFile
    }

    /** Total frames written via [appendFrame] (assigned seqs). */
    fun frameCount(): Int = seq

    /** Earliest frame timestamp, or null if no frames. */
    fun startedUnixMs(): Long? = firstUnixMs

    /** Latest frame timestamp, or null if no frames. */
    fun endedUnixMs(): Long? = lastUnixMs

    /** Close the index stream. Idempotent. */
    fun close() {
        if (closed) return
        closed = true
        runCatching { indexStream.close() }
    }

    private companion object {
        fun writePgm(file: File, w: Int, h: Int, pixels: ByteArray) {
            FileOutputStream(file).use { out ->
                out.write("P5\n$w $h\n255\n".toByteArray())
                out.write(pixels)
            }
        }

        fun retentionLabel(r: Retention): String = when (r) {
            Retention.Debug -> "debug"
            Retention.FixFrame -> "fix_frame"
        }
    }
}
