// Task 2bf.11 / 2bf.12 acceptance on the JVM: the recents list and the incoming-file slot.
// Synthetic URIs and names only.
package com.greenline.glfile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class RecentsCoreTest {
    private var n = 0
    private fun core(max: Int = 20) = RecentsCore(max) { "id" + (n++) }

    @Test
    fun sameFileTwiceIsOneEntryMovedToFront() {
        val c = core()
        val (a, _) = c.add("content://p/a", "A.pdf", 1)
        c.add("content://p/b", "B.pdf", 2)
        val (a2, dropped) = c.add("content://p/a", "A renamed.pdf", 3)
        assertEquals(a.id, a2.id)
        assertEquals(2, c.items.size)
        assertEquals("content://p/a", c.items[0].uri)
        assertEquals("A renamed.pdf", c.items[0].name)
        assertEquals(3L, c.items[0].opened)
        assertTrue(dropped.isEmpty())
    }

    @Test
    fun twentyFirstDropsTheOldest() {
        val c = core()
        for (i in 0 until 20) c.add("content://p/$i", "F$i.pdf", i.toLong())
        val (_, dropped) = c.add("content://p/new", "New.pdf", 99)
        assertEquals(20, c.items.size)
        assertEquals(1, dropped.size)
        assertEquals("content://p/0", dropped[0].uri) // the oldest
        assertEquals("content://p/new", c.items[0].uri)
    }

    @Test
    fun idsAreFromTheGeneratorAndUnique() {
        val c = RecentsCore(20) { java.util.UUID.randomUUID().toString() }
        val ids = (0 until 20).map { c.add("content://p/$it", "F.pdf", 0).first.id }.toSet()
        assertEquals(20, ids.size)
        ids.forEach { assertFalse(it.contains("content")) }
    }

    @Test
    fun touchAndRemove() {
        val c = core()
        val (a, _) = c.add("content://p/a", "A.pdf", 1)
        c.add("content://p/b", "B.pdf", 2)
        assertEquals(a.id, c.touch(a.id, 5)!!.id)
        assertEquals(a.id, c.items[0].id)
        assertNull(c.touch("nope", 6))
        assertEquals(a.id, c.remove(a.id)!!.id)
        assertNull(c.find(a.id))
        assertEquals(1, c.items.size)
    }

    @Test
    fun pendingSlotNewestWinsAndIsTakenOnce() {
        val s = PendingSlot<String>()
        assertNull(s.take())
        s.set("first")
        s.set("second")
        assertTrue(s.isSet())
        assertEquals("second", s.take())
        assertNull(s.take())
        assertFalse(s.isSet())
    }
}
