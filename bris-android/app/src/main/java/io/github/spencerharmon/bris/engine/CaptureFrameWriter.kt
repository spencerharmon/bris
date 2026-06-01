package io.github.spencerharmon.bris.engine

import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.time.Instant

/**
 * Streaming writer for a single capture's frame payload.
 *
 * Lifecycle:
 *  - constructed at Start with the capture's root directory
 *    (`<external-files>/sessions/<UUID>/captures/<cap-id>/`
 *     when an active session exists; or
 *    `<external-files>/sights/<cap-id>/` when orphan).
 *  - [appendFrame] is called for **every** analyzer frame
 *    the engine sees during the capture, regardless of
 *    fix outcome. Writes one PGM + one sidecar JSON per
 *    frame to `frames/NNNNNNNN.{pgm,json}` and appends a
 *    row to `index.jsonl`.
 *  - [close] flushes the index and is idempotent.
 *
 * This replaces the parallel `DebugCaptureBuffer` path. The
 * Start/Stop window is the recording boundary; no app-
 * lifetime accumulation; no separate flat-layout zip.
 *
 * Pure-JVM. Concurrency: caller must serialize calls
 * (the analyzer executor is single-threaded in `LiveScreen`).
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

    init {
        captureDir.mkdirs()
    }

    /**
     * Append one frame to the capture.
     *
     * @param pixels Y-plane bytes, length must equal `width*height`.
     * @return The on-disk PGM file just written.
     */
    fun appendFrame(
        width: Int,
        height: Int,
        pixels: ByteArray,
        capturedUnixMs: Long,
        diagnosticSnapshot: JSONObject? = null,
    ): File {
        check(!closed) { "CaptureFrameWriter already closed" }
        require(pixels.size == width * height) {
            "pixels.size=${pixels.size} != width*height=${width * height}"
        }
        val name = "%08d".format(seq)
        val pgmFile = File(framesDir, "$name.pgm")
        val jsonFile = File(framesDir, "$name.json")

        writePgm(pgmFile, width, height, pixels)

        val sidecar = JSONObject()
            .put("seq", seq)
            .put("captured_unix_ms", capturedUnixMs)
            .put("width", width)
            .put("height", height)
        if (diagnosticSnapshot != null) {
            sidecar.put("diagnostic_snapshot", diagnosticSnapshot)
        }
        jsonFile.writeText(sidecar.toString())

        val indexRow = JSONObject()
            .put("seq", seq)
            .put("captured_unix_ms", capturedUnixMs)
            .put("width", width)
            .put("height", height)
            .put("pgm_bytes", pgmFile.length())
            .put("json_bytes", jsonFile.length())
        indexStream.write((indexRow.toString() + "\n").toByteArray())
        indexStream.flush()

        if (firstUnixMs == null) firstUnixMs = capturedUnixMs
        lastUnixMs = capturedUnixMs
        seq += 1
        return pgmFile
    }

    /** Total frames written. */
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
    }
}
