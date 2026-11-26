// Greenline task 2bf.12: the recent-files list and the incoming-file slot, as plain Kotlin with no
// Android types so they can be unit-tested on the JVM (src/test). RecentsStore.kt persists RecentsCore;
// GlFilePlugin.kt uses PendingSlot for files sent by other apps (task 2bf.11).
//
// CONFIDENTIALITY: entries hold the document's URI and display name. They stay in app-private storage
// (excluded from backup) and go to the start screen for display only: never logged, never in an
// error message, never in exports or measurement records.
package com.greenline.glfile

/** One recent file. `lastPage` is 0-based; `pages` and `lastPage` are -1 until known. */
data class Recent(
    val id: String,
    val uri: String,
    var name: String,
    var pages: Int = -1,
    var lastPage: Int = -1,
    var opened: Long = 0,
)

/**
 * Recent files, newest first, at most [max]. The same URI is one entry (opening it again moves it to
 * the front). Entries pushed out are returned to the caller, which releases their URI permission and
 * deletes their thumbnail.
 */
class RecentsCore(private val max: Int = 20, private val newId: () -> String) {
    val items: MutableList<Recent> = mutableListOf()

    /** Add or refresh `uri`. Returns the entry and any entries dropped to stay within [max]. */
    fun add(uri: String, name: String, now: Long): Pair<Recent, List<Recent>> {
        val i = items.indexOfFirst { it.uri == uri }
        val r = if (i >= 0) items.removeAt(i).also { it.name = name } else Recent(newId(), uri, name)
        r.opened = now
        items.add(0, r)
        val dropped = mutableListOf<Recent>()
        while (items.size > max) dropped.add(items.removeAt(items.size - 1))
        return r to dropped
    }

    fun find(id: String): Recent? = items.firstOrNull { it.id == id }

    /** Move `id` to the front with a new open time. */
    fun touch(id: String, now: Long): Recent? {
        val i = items.indexOfFirst { it.id == id }
        if (i < 0) return null
        val r = items.removeAt(i)
        r.opened = now
        items.add(0, r)
        return r
    }

    fun remove(id: String): Recent? {
        val i = items.indexOfFirst { it.id == id }
        return if (i < 0) null else items.removeAt(i)
    }
}

/** One waiting value: the newest set wins, and take() hands it out once. Thread-safe. */
class PendingSlot<T> {
    private var value: T? = null

    @Synchronized
    fun set(v: T) {
        value = v
    }

    @Synchronized
    fun take(): T? = value.also { value = null }

    @Synchronized
    fun isSet(): Boolean = value != null
}
