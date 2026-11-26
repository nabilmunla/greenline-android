package com.greenline.glfile

import com.greenline.glfile.UpdateCore.Asset
import com.greenline.glfile.UpdateCore.Rejected
import com.greenline.glfile.UpdateCore.Release
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class UpdateCoreTest {
    private val base = UpdateCore.DOWNLOAD_PREFIX
    private fun apk(v: String, size: Long = 13L shl 20, url: String = base + "v$v/greenline-$v.apk") =
        Asset("greenline-$v.apk", url, size)
    private fun rel(tag: String, vararg a: Asset, draft: Boolean = false, pre: Boolean = false) =
        Release(tag, draft, pre, a.toList())
    private fun rejected(code: String, f: () -> Unit) {
        try {
            f()
            fail("expected $code")
        } catch (e: Rejected) {
            assertEquals(code, e.code)
        }
    }

    @Test fun versions() {
        assertEquals(listOf(0, 1, 9), UpdateCore.parseVersion("v0.1.9")!!.toList())
        assertEquals(listOf(0, 1, 10), UpdateCore.parseVersion("0.1.10")!!.toList())
        assertNull(UpdateCore.parseVersion("v0.1"))
        assertNull(UpdateCore.parseVersion("v0.1.9-beta"))
        assertNull(UpdateCore.parseVersion(null))
        assertTrue(UpdateCore.isNewer(intArrayOf(0, 1, 10), intArrayOf(0, 1, 9)))
        assertTrue(UpdateCore.isNewer(intArrayOf(1, 0, 0), intArrayOf(0, 9, 9)))
        assertFalse(UpdateCore.isNewer(intArrayOf(0, 1, 9), intArrayOf(0, 1, 9)))
        assertFalse(UpdateCore.isNewer(intArrayOf(0, 1, 8), intArrayOf(0, 1, 9)))
    }

    @Test fun offersANewerRelease() {
        val o = UpdateCore.offer(rel("v0.1.9", apk("0.1.9")), "0.1.8")!!
        assertEquals("0.1.9", o.version)
        assertEquals(base + "v0.1.9/greenline-0.1.9.apk", o.url)
        assertEquals(13L shl 20, o.size)
    }

    @Test fun nothingWhenNotNewerOrNotFinal() {
        assertNull(UpdateCore.offer(rel("v0.1.8", apk("0.1.8")), "0.1.8"))
        assertNull(UpdateCore.offer(rel("v0.1.7", apk("0.1.7")), "0.1.8"))
        assertNull(UpdateCore.offer(rel("v0.1.9", apk("0.1.9"), draft = true), "0.1.8"))
        assertNull(UpdateCore.offer(rel("v0.1.9", apk("0.1.9"), pre = true), "0.1.8"))
    }

    @Test fun refusesAnythingNotFromThisRepositorysRelease() {
        // Another host, another repository, another tag's folder.
        rejected("E_UPDATE_BAD_RELEASE") { UpdateCore.offer(rel("v0.1.9", apk("0.1.9", url = "https://example.com/greenline-0.1.9.apk")), "0.1.8") }
        rejected("E_UPDATE_BAD_RELEASE") {
            UpdateCore.offer(rel("v0.1.9", apk("0.1.9", url = "https://github.com/someone/greenline-android/releases/download/v0.1.9/greenline-0.1.9.apk")), "0.1.8")
        }
        rejected("E_UPDATE_BAD_RELEASE") { UpdateCore.offer(rel("v0.1.9", apk("0.1.9", url = base + "v0.1.8/greenline-0.1.9.apk")), "0.1.8") }
        rejected("E_UPDATE_BAD_RELEASE") { UpdateCore.offer(rel("v0.1.9", apk("0.1.9", url = "http://github.com/" + UpdateCore.REPO + "/releases/download/v0.1.9/greenline-0.1.9.apk")), "0.1.8") }
        // No APK for this version, or a differently named one.
        rejected("E_UPDATE_BAD_RELEASE") { UpdateCore.offer(rel("v0.1.9", apk("0.1.8")), "0.1.8") }
        rejected("E_UPDATE_BAD_RELEASE") { UpdateCore.offer(rel("v0.1.9", Asset("other.apk", base + "v0.1.9/other.apk", 13L shl 20)), "0.1.8") }
        rejected("E_UPDATE_BAD_RELEASE") { UpdateCore.offer(rel("v0.1.9"), "0.1.8") }
        // A tag that is not a plain version.
        rejected("E_UPDATE_BAD_RELEASE") { UpdateCore.offer(rel("latest", apk("0.1.9")), "0.1.8") }
    }

    @Test fun refusesOddSizes() {
        rejected("E_UPDATE_SIZE") { UpdateCore.offer(rel("v0.1.9", apk("0.1.9", size = 1000)), "0.1.8") }
        rejected("E_UPDATE_SIZE") { UpdateCore.offer(rel("v0.1.9", apk("0.1.9", size = 65L shl 20)), "0.1.8") }
    }

    @Test fun picksTheRightApkAmongSeveral() {
        val o = UpdateCore.offer(rel("v0.1.9", Asset("notes.txt", base + "v0.1.9/notes.txt", 10), apk("0.1.9")), "0.1.8")!!
        assertEquals(base + "v0.1.9/greenline-0.1.9.apk", o.url)
    }

    @Test fun signersMustMatchExactly() {
        val a = setOf("aa11"); val b = setOf("bb22")
        assertTrue(UpdateCore.sameSigners(a, setOf("aa11")))
        assertFalse(UpdateCore.sameSigners(a, b))
        assertFalse(UpdateCore.sameSigners(a, setOf("aa11", "bb22")))
        assertFalse(UpdateCore.sameSigners(emptySet(), emptySet()))
        assertEquals("00ff10", UpdateCore.hex(byteArrayOf(0, -1, 16)))
    }
}
