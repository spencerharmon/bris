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
import java.util.UUID

class CaptureCatalogTest {

    @get:Rule
    val tmp = TemporaryFolder()

    private fun makeSession(
        id: UUID,
        title: String,
        createdMs: Long,
    ): File {
        val root = tmp.root
        val dir = File(root, "sessions/$id").apply { mkdirs() }
        val s = Session(
            sessionId = id,
            title = title,
            createdUnixMs = createdMs,
            device = Session.DeviceInfo(model = "m"),
        )
        File(dir, "session.json").writeText(s.toJson().toString())
        return dir
    }

    private fun makeBundleCapture(sessionDir: File, capId: String) {
        val d = File(sessionDir, "captures/$capId").apply { mkdirs() }
        File(d, "bundle.json").writeText("{}")
    }

    private fun makeSightLogCapture(sessionDir: File, capId: String) {
        val d = File(sessionDir, "captures/$capId").apply { mkdirs() }
        File(d, "manifest.json").writeText("{}")
    }

    private fun makeUnknownCapture(sessionDir: File, capId: String) {
        val d = File(sessionDir, "captures/$capId").apply { mkdirs() }
        File(d, "random.txt").writeText("x")
    }

    private fun makeOrphan(capId: String, kind: String = "manifest.json") {
        val d = File(tmp.root, "sights/$capId").apply { mkdirs() }
        File(d, kind).writeText("{}")
    }

    private fun catalog() = CaptureCatalog(tmp.root)

    @Test
    fun empty_root_yields_empty_list() {
        assertEquals(emptyList<Any>(), catalog().listGroups())
    }

    @Test
    fun lists_sessions_with_captures() {
        val id = UUID.randomUUID()
        val s = makeSession(id, "moon", 1_700_000_000_000L)
        makeBundleCapture(s, "cap-a")
        makeBundleCapture(s, "cap-b")
        val groups = catalog().listGroups()
        assertEquals(1, groups.size)
        val g = groups[0]
        assertEquals(id, g.sessionId)
        assertEquals("moon", g.title)
        assertEquals(2, g.captures.size)
        assertTrue(g.captures.all { it.kind == CaptureCatalog.CaptureKind.Bundle })
    }

    @Test
    fun orphan_sights_become_their_own_group() {
        makeOrphan("orphan-1")
        makeOrphan("orphan-2")
        val groups = catalog().listGroups()
        assertEquals(1, groups.size)
        val g = groups[0]
        assertNull(g.sessionId)
        assertEquals("(orphan captures)", g.title)
        assertEquals(2, g.captures.size)
        assertTrue(g.captures.all { it.kind == CaptureCatalog.CaptureKind.SightLog })
    }

    @Test
    fun sessions_sorted_newest_first_orphans_last() {
        val older = UUID.randomUUID()
        val newer = UUID.randomUUID()
        makeSession(older, "older", 100L)
        makeSession(newer, "newer", 200L)
        makeOrphan("o-1")
        val groups = catalog().listGroups()
        assertEquals(3, groups.size)
        assertEquals(newer, groups[0].sessionId)
        assertEquals(older, groups[1].sessionId)
        assertNull(groups[2].sessionId)
    }

    @Test
    fun classifies_bundle_sightlog_unknown_capture_kinds() {
        val id = UUID.randomUUID()
        val s = makeSession(id, "t", 0L)
        makeBundleCapture(s, "cap-bundle")
        makeSightLogCapture(s, "cap-sight")
        makeUnknownCapture(s, "cap-unknown")
        val g = catalog().listGroups().single()
        val byId = g.captures.associateBy { it.id }
        assertEquals(CaptureCatalog.CaptureKind.Bundle, byId["cap-bundle"]?.kind)
        assertEquals(CaptureCatalog.CaptureKind.SightLog, byId["cap-sight"]?.kind)
        assertEquals(CaptureCatalog.CaptureKind.Unknown, byId["cap-unknown"]?.kind)
    }

    @Test
    fun session_with_missing_sessionjson_is_skipped() {
        val id = UUID.randomUUID()
        File(tmp.root, "sessions/$id/captures/cap-x").mkdirs()
        // No session.json written. Catalog must skip silently.
        assertEquals(emptyList<Any>(), catalog().listGroups())
    }

    @Test
    fun session_with_corrupt_sessionjson_is_skipped() {
        val id = UUID.randomUUID()
        val dir = File(tmp.root, "sessions/$id").apply { mkdirs() }
        File(dir, "session.json").writeText("{ not json")
        // Catalog must skip silently rather than throw.
        assertEquals(emptyList<Any>(), catalog().listGroups())
    }

    @Test
    fun session_with_no_captures_still_listed() {
        val id = UUID.randomUUID()
        makeSession(id, "empty", 0L)
        val g = catalog().listGroups().single()
        assertEquals(id, g.sessionId)
        assertTrue(g.captures.isEmpty())
    }

    @Test
    fun captures_inside_session_sorted_newest_first_by_mtime() {
        val id = UUID.randomUUID()
        val s = makeSession(id, "t", 0L)
        val older = File(s, "captures/older").apply { mkdirs() }
        File(older, "bundle.json").writeText("{}")
        older.setLastModified(1_000L)
        val newer = File(s, "captures/newer").apply { mkdirs() }
        File(newer, "bundle.json").writeText("{}")
        newer.setLastModified(2_000L)
        val g = catalog().listGroups().single()
        assertEquals("newer", g.captures[0].id)
        assertEquals("older", g.captures[1].id)
    }
}
