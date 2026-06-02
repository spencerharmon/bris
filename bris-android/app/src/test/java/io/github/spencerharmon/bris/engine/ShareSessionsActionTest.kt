package io.github.spencerharmon.bris.engine

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.util.zip.ZipInputStream

class ShareSessionsActionTest {

    @get:Rule
    val tmp = TemporaryFolder()

    @Test
    fun zips_empty_root_to_just_the_root_entry() {
        val src = tmp.newFolder("sessions")
        val zipFile = File(tmp.root, "out.zip")
        FileOutputStream(zipFile).use { ShareSessionsAction.writeZip(src, it) }

        val names = readEntryNames(zipFile)
        assertEquals(listOf("sessions/"), names)
    }

    @Test
    fun zips_session_with_capture_payload() {
        val src = tmp.newFolder("sessions")
        val s = File(src, "AAAA-BBBB").apply { mkdirs() }
        File(s, "session.json").writeText("{}")
        val cap = File(s, "captures/cap-1").apply { mkdirs() }
        File(cap, "bundle.json").writeText("{}")
        File(cap, "manifest.json").writeText("{}")
        val frames = File(cap, "frames").apply { mkdirs() }
        File(frames, "00000000.pgm").writeBytes(byteArrayOf(1, 2, 3))
        File(frames, "00000000.json").writeText("{\"retention\":\"fix_frame\"}")

        val zipFile = File(tmp.root, "out.zip")
        FileOutputStream(zipFile).use { ShareSessionsAction.writeZip(src, it) }

        val names = readEntryNames(zipFile).toSet()
        assertTrue("session.json present", names.contains("sessions/AAAA-BBBB/session.json"))
        assertTrue(
            "bundle.json present",
            names.contains("sessions/AAAA-BBBB/captures/cap-1/bundle.json"),
        )
        assertTrue(
            "pgm present",
            names.contains("sessions/AAAA-BBBB/captures/cap-1/frames/00000000.pgm"),
        )
        assertTrue(
            "sidecar present",
            names.contains("sessions/AAAA-BBBB/captures/cap-1/frames/00000000.json"),
        )
    }

    @Test
    fun pgm_bytes_round_trip() {
        val src = tmp.newFolder("sessions")
        val cap = File(src, "x/captures/c").apply { mkdirs() }
        val pgm = File(cap, "frames/000.pgm").apply { parentFile.mkdirs() }
        val payload = ByteArray(64) { it.toByte() }
        pgm.writeBytes(payload)

        val zipFile = File(tmp.root, "out.zip")
        FileOutputStream(zipFile).use { ShareSessionsAction.writeZip(src, it) }

        ZipInputStream(FileInputStream(zipFile)).use { zin ->
            var found = false
            while (true) {
                val e = zin.nextEntry ?: break
                if (e.name == "sessions/x/captures/c/frames/000.pgm") {
                    val data = zin.readBytes()
                    assertEquals(payload.size, data.size)
                    for (i in payload.indices) assertEquals(payload[i], data[i])
                    found = true
                }
                zin.closeEntry()
            }
            assertTrue("pgm entry not found", found)
        }
    }

    @Test
    fun directories_are_emitted_for_each_subdir() {
        val src = tmp.newFolder("sessions")
        val cap = File(src, "y/captures/c").apply { mkdirs() }
        File(cap, "frames").mkdir()

        val zipFile = File(tmp.root, "out.zip")
        FileOutputStream(zipFile).use { ShareSessionsAction.writeZip(src, it) }

        val names = readEntryNames(zipFile).toSet()
        assertTrue(names.contains("sessions/"))
        assertTrue(names.contains("sessions/y/"))
        assertTrue(names.contains("sessions/y/captures/"))
        assertTrue(names.contains("sessions/y/captures/c/"))
        assertTrue(names.contains("sessions/y/captures/c/frames/"))
    }

    private fun readEntryNames(zipFile: File): List<String> {
        val out = mutableListOf<String>()
        ZipInputStream(FileInputStream(zipFile)).use { zin ->
            while (true) {
                val e = zin.nextEntry ?: break
                out.add(e.name)
                zin.closeEntry()
            }
        }
        return out
    }
}
