package io.github.spencerharmon.bris.engine

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.io.FileOutputStream
import java.util.zip.ZipInputStream

/**
 * Pure-JVM unit tests for the file-walking contract that
 * [DebugBufferActions.saveAll] uses to drive its SAF
 * `DocumentFile` writes. SAF itself is end-to-end tested by
 * the operator following the acceptance-criteria walkthrough.
 */
class DebugBufferActionsTest {

    @get:Rule
    val tmp = TemporaryFolder()

    @Test
    fun enumerateSources_emptyRoot_isEmpty() {
        val root = tmp.newFolder("buf")
        val s = DebugBufferActions.enumerateSources(root)
        assertEquals(0, s.frameFiles.size)
        assertEquals(0, s.topLevelFiles.size)
        assertEquals(0, s.pgmFrameCount)
        assertEquals(0L, s.totalBytes)
    }

    @Test
    fun enumerateSources_collectsFramesAndTopLevelFilesSorted() {
        val root = tmp.newFolder("buf")
        val frames = File(root, "frames").apply { mkdirs() }
        File(frames, "000000000002.pgm").writeBytes(ByteArray(10))
        File(frames, "000000000002.json").writeText("{}") // 2 bytes
        File(frames, "000000000001.pgm").writeBytes(ByteArray(20))
        File(frames, "000000000001.json").writeText("{}")
        File(root, "index.jsonl").writeText("a\nb\n") // 4 bytes
        File(root, "pbris.log").writeText("\$PBRIS\n") // 7 bytes

        val s = DebugBufferActions.enumerateSources(root)

        // Sorted lexicographically → 001.json, 001.pgm, 002.json, 002.pgm.
        assertEquals(
            listOf(
                "000000000001.json",
                "000000000001.pgm",
                "000000000002.json",
                "000000000002.pgm",
            ),
            s.frameFiles.map { it.name },
        )
        assertEquals(2, s.pgmFrameCount)
        assertEquals(setOf("index.jsonl", "pbris.log"), s.topLevelFiles.map { it.name }.toSet())
        // 10 + 2 + 20 + 2 + 4 + 7 = 45
        assertEquals(45L, s.totalBytes)
    }

    @Test
    fun enumerateSources_skipsMissingTopLevelFiles() {
        val root = tmp.newFolder("buf")
        File(root, "frames").mkdirs()
        File(root, "pbris.log").writeText("x")
        val s = DebugBufferActions.enumerateSources(root)
        assertEquals(listOf("pbris.log"), s.topLevelFiles.map { it.name })
        assertTrue(s.frameFiles.isEmpty())
    }

    @Test
    fun writeZipBundle_producesSingleZipWithExpectedEntries() {
        val root = tmp.newFolder("buf")
        val frames = File(root, "frames").apply { mkdirs() }
        File(frames, "000000000001.pgm").writeBytes(ByteArray(20) { 0x55 })
        File(frames, "000000000001.json").writeText("{\"a\":1}")
        File(frames, "000000000002.pgm").writeBytes(ByteArray(20) { 0x33 })
        File(frames, "000000000002.json").writeText("{\"a\":2}")
        File(root, "index.jsonl").writeText("a\nb\n")
        File(root, "pbris.log").writeText("\$PBRIS\n")

        val sources = DebugBufferActions.enumerateSources(root)
        val zipOut = tmp.newFile("bundle.zip")
        val bundleName = "bris-debug-test"
        val frameCount = FileOutputStream(zipOut).use { fos ->
            DebugBufferActions.writeZipBundle(fos, bundleName, sources)
        }
        assertEquals(2, frameCount)
        assertTrue(zipOut.isFile)
        assertTrue(zipOut.length() > 0L)

        val entries = mutableListOf<String>()
        ZipInputStream(zipOut.inputStream()).use { zin ->
            while (true) {
                val e = zin.nextEntry ?: break
                entries.add(e.name)
                zin.closeEntry()
            }
        }
        assertEquals(
            listOf(
                "$bundleName/frames/000000000001.json",
                "$bundleName/frames/000000000001.pgm",
                "$bundleName/frames/000000000002.json",
                "$bundleName/frames/000000000002.pgm",
                "$bundleName/index.jsonl",
                "$bundleName/pbris.log",
            ),
            entries,
        )
}
