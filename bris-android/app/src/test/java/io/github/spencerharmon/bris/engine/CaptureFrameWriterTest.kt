package io.github.spencerharmon.bris.engine

import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

class CaptureFrameWriterTest {

    @get:Rule
    val tmp = TemporaryFolder()

    private fun mkDir(name: String): File = tmp.newFolder(name)

    @Test
    fun empty_writer_reports_zero_frames() {
        val w = CaptureFrameWriter(mkDir("cap1"))
        assertEquals(0, w.frameCount())
        assertNull(w.startedUnixMs())
        assertNull(w.endedUnixMs())
        w.close()
    }

    @Test
    fun appends_pgm_and_sidecar_per_frame() {
        val dir = mkDir("cap2")
        val w = CaptureFrameWriter(dir)
        val pix = ByteArray(4 * 3) { (it + 10).toByte() }
        val pgm = w.appendFrame(width = 4, height = 3, pixels = pix, capturedUnixMs = 100L)
        w.close()

        assertTrue("pgm exists", pgm.exists())
        assertEquals(File(dir, "frames/00000000.pgm").canonicalPath, pgm.canonicalPath)
        val sidecar = File(dir, "frames/00000000.json")
        assertTrue("sidecar exists", sidecar.exists())
        val j = JSONObject(sidecar.readText())
        assertEquals(0, j.getInt("seq"))
        assertEquals(100L, j.getLong("captured_unix_ms"))
        assertEquals(4, j.getInt("width"))
        assertEquals(3, j.getInt("height"))

        // PGM header + payload byte-exact.
        val raw = pgm.readBytes()
        val header = "P5\n4 3\n255\n".toByteArray()
        assertArrayEquals(header, raw.copyOfRange(0, header.size))
        assertArrayEquals(pix, raw.copyOfRange(header.size, raw.size))
    }

    @Test
    fun index_jsonl_grows_per_append() {
        val dir = mkDir("cap3")
        val w = CaptureFrameWriter(dir)
        val pix = ByteArray(2 * 2)
        w.appendFrame(2, 2, pix, 1_000L)
        w.appendFrame(2, 2, pix, 2_000L)
        w.appendFrame(2, 2, pix, 3_000L)
        w.close()

        val index = File(dir, "index.jsonl").readText().trim().lines()
        assertEquals(3, index.size)
        val rows = index.map { JSONObject(it) }
        assertEquals(listOf(0, 1, 2), rows.map { it.getInt("seq") })
        assertEquals(listOf(1_000L, 2_000L, 3_000L), rows.map { it.getLong("captured_unix_ms") })
        assertTrue(rows.all { it.getLong("pgm_bytes") > 0 })
    }

    @Test
    fun tracks_first_and_last_unix_ms() {
        val w = CaptureFrameWriter(mkDir("cap4"))
        val pix = ByteArray(4)
        w.appendFrame(2, 2, pix, 555L)
        w.appendFrame(2, 2, pix, 999L)
        w.appendFrame(2, 2, pix, 777L)
        assertEquals(555L, w.startedUnixMs())
        assertEquals(777L, w.endedUnixMs()) // last appended, not max
        assertEquals(3, w.frameCount())
        w.close()
    }

    @Test
    fun diagnostic_snapshot_round_trips_into_sidecar() {
        val dir = mkDir("cap5")
        val w = CaptureFrameWriter(dir)
        val snap = JSONObject()
            .put("frames_pushed", 17)
            .put("ring_buffer_depth", 4)
        w.appendFrame(2, 2, ByteArray(4), 0L, snap)
        w.close()

        val sidecar = JSONObject(File(dir, "frames/00000000.json").readText())
        val parsed = sidecar.getJSONObject("diagnostic_snapshot")
        assertEquals(17, parsed.getInt("frames_pushed"))
        assertEquals(4, parsed.getInt("ring_buffer_depth"))
    }

    @Test
    fun mismatched_pixel_size_throws() {
        val w = CaptureFrameWriter(mkDir("cap6"))
        val e = assertThrows(IllegalArgumentException::class.java) {
            w.appendFrame(4, 4, ByteArray(3), 0L)
        }
        assertTrue(e.message!!.contains("!="))
        w.close()
    }

    @Test
    fun append_after_close_throws() {
        val w = CaptureFrameWriter(mkDir("cap7"))
        w.close()
        assertThrows(IllegalStateException::class.java) {
            w.appendFrame(2, 2, ByteArray(4), 0L)
        }
    }

    @Test
    fun close_is_idempotent() {
        val w = CaptureFrameWriter(mkDir("cap8"))
        w.appendFrame(2, 2, ByteArray(4), 0L)
        w.close()
        w.close() // no throw
    }

    @Test
    fun writes_create_capture_dir_if_missing() {
        val parent = mkDir("parent")
        val captureDir = File(parent, "deeply/nested/cap")
        assertTrue("not created yet", !captureDir.exists())
        val w = CaptureFrameWriter(captureDir)
        w.appendFrame(2, 2, ByteArray(4), 0L)
        w.close()
        assertTrue(captureDir.exists())
        assertNotNull(File(captureDir, "frames/00000000.pgm"))
    }

    @Test
    fun seq_zero_padded_to_eight_digits() {
        val dir = mkDir("cap9")
        val w = CaptureFrameWriter(dir)
        val pix = ByteArray(4)
        // Cheap: just check the first frame's name.
        val pgm = w.appendFrame(2, 2, pix, 0L)
        assertTrue(pgm.name == "00000000.pgm")
        w.close()
    }
}
