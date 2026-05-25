package io.github.spencerharmon.bris.engine

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

/**
 * Pure-JVM unit tests for [DebugCaptureBuffer.stateFlow].
 *
 * Avoids constructing UniFFI `FfiFrame` values (which require
 * the native library); instead drives the buffer through its
 * file-system side effects (`appendPbris`, `clear`, and
 * pre-seeded `index.jsonl`).
 */
class DebugCaptureBufferStateFlowTest {

    @get:Rule
    val tmp = TemporaryFolder()

    private fun seedIndex(root: File, capturedMs: List<Long>) {
        val frames = File(root, "frames").apply { mkdirs() }
        val index = File(root, "index.jsonl")
        index.bufferedWriter().use { w ->
            for ((i, ms) in capturedMs.withIndex()) {
                val tag = "%012d".format(i.toLong())
                // Write small placeholder files so totalBytes is non-zero
                // and a real path exists for each entry.
                File(frames, "$tag.pgm").writeText("PGM")
                File(frames, "$tag.json").writeText("{}")
                val obj = JSONObject()
                    .put("seq", i.toLong())
                    .put("captured_unix_ms", ms)
                    .put("width", 100)
                    .put("height", 100)
                    .put("pgm_bytes", 3L)
                    .put("json_bytes", 2L)
                w.write(obj.toString())
                w.newLine()
            }
        }
    }

    @Test
    fun emptyBuffer_initialStateIsZero() {
        val buffer = DebugCaptureBuffer(tmp.newFolder("buf"))
        val s = buffer.stateFlow.value
        assertEquals(0, s.frameCount)
        assertEquals(0L, s.totalBytes)
        assertNull(s.lastAppendUnixMs)
        assertNull(s.oldestFrameUnixMs)
        assertNull(s.newestFrameUnixMs)
        assertEquals(0L, s.evictedSinceClear)
    }

    @Test
    fun seedsTimestampsFromExistingIndex() {
        val root = tmp.newFolder("buf")
        seedIndex(root, listOf(1_000L, 2_000L, 3_000L))
        val buffer = DebugCaptureBuffer(root)
        val s = buffer.stateFlow.value
        assertEquals(3, s.frameCount)
        assertEquals(1_000L, s.oldestFrameUnixMs)
        assertEquals(3_000L, s.newestFrameUnixMs)
        // totalBytes counts files under frames/, not index.
        assertTrue(s.totalBytes > 0L)
    }

    @Test
    fun appendPbris_bumpsLastAppendMs() {
        val buffer = DebugCaptureBuffer(tmp.newFolder("buf"))
        assertNull(buffer.stateFlow.value.lastAppendUnixMs)
        val before = System.currentTimeMillis()
        buffer.appendPbris("\$PBRIS,…")
        val after = System.currentTimeMillis()
        val ts = buffer.stateFlow.value.lastAppendUnixMs
        assertNotNull(ts)
        assertTrue(ts!! in before..after)
    }

    @Test
    fun clear_resetsCountersAndEmits() {
        val root = tmp.newFolder("buf")
        seedIndex(root, listOf(10L, 20L))
        val buffer = DebugCaptureBuffer(root)
        assertEquals(2, buffer.stateFlow.value.frameCount)

        buffer.clear()

        val s = buffer.stateFlow.value
        assertEquals(0, s.frameCount)
        assertEquals(0L, s.totalBytes)
        assertNull(s.oldestFrameUnixMs)
        assertNull(s.newestFrameUnixMs)
        assertEquals(0L, s.evictedSinceClear)
        // frames dir was wiped; index removed.
        assertEquals(0, File(root, "frames").listFiles().orEmpty().size)
    }
}
