// Greenline task 2bf.12: RecentsCore kept in app-private SharedPreferences (the app has
// android:allowBackup="false", so this never reaches a cloud backup), thumbnails as small files in
// app-private storage. Entries pushed out of the list give back their URI permission.
//
// CONFIDENTIALITY: holds URIs and display names. Nothing here logs; errors are swallowed without
// their messages (which can contain the URI).
package com.greenline.glfile

import android.content.Context
import android.content.Intent
import android.net.Uri
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.util.UUID

class RecentsStore(private val context: Context) {
    private val prefs = context.getSharedPreferences("gl_recents", Context.MODE_PRIVATE)
    private val thumbs = File(context.filesDir, "gl_thumbs").apply { mkdirs() }
    private val core = RecentsCore(MAX) { UUID.randomUUID().toString() }

    init {
        load()
    }

    @Synchronized
    fun add(uri: Uri, name: String): String {
        val (r, dropped) = core.add(uri.toString(), name, System.currentTimeMillis())
        dropped.forEach { forget(it) }
        save()
        return r.id
    }

    @Synchronized
    fun has(uri: Uri): Boolean = core.items.any { it.uri == uri.toString() }

    @Synchronized
    fun uri(id: String): Uri? = core.find(id)?.let { Uri.parse(it.uri) }

    @Synchronized
    fun touch(id: String) {
        core.touch(id, System.currentTimeMillis())
        save()
    }

    @Synchronized
    fun remove(id: String) {
        core.remove(id)?.let { forget(it) }
        save()
    }

    @Synchronized
    fun setPages(id: String, lastPage: Int, pages: Int) {
        val r = core.find(id) ?: return
        if (pages > 0) r.pages = pages
        if (lastPage >= 0) r.lastPage = lastPage
        save()
    }

    /** `data` is a data: URL (small JPEG made by the page from the fit image). */
    @Synchronized
    fun saveThumb(id: String, data: String): Boolean {
        if (core.find(id) == null || data.length > MAX_THUMB || !data.startsWith("data:image/")) return false
        return try {
            File(thumbs, "$id.txt").writeText(data)
            true
        } catch (e: Exception) {
            false
        }
    }

    /** Newest first, for the start screen only. */
    @Synchronized
    fun list(): JSONArray {
        val out = JSONArray()
        for (r in core.items) {
            val o = JSONObject()
            o.put("id", r.id)
            o.put("name", r.name)
            o.put("pages", if (r.pages > 0) r.pages else JSONObject.NULL)
            o.put("last_page", if (r.lastPage >= 0) r.lastPage else JSONObject.NULL)
            o.put("opened", r.opened)
            val f = File(thumbs, r.id + ".txt")
            o.put("thumb", if (f.isFile) (try { f.readText() } catch (e: Exception) { null }) ?: JSONObject.NULL else JSONObject.NULL)
            out.put(o)
        }
        return out
    }

    private fun forget(r: Recent) {
        try {
            context.contentResolver.releasePersistableUriPermission(Uri.parse(r.uri), Intent.FLAG_GRANT_READ_URI_PERMISSION)
        } catch (e: Exception) {
            // not held any more: nothing to release
        }
        File(thumbs, r.id + ".txt").delete()
    }

    private fun load() {
        val raw = prefs.getString("list", null) ?: return
        try {
            val a = JSONArray(raw)
            for (i in 0 until a.length()) {
                val o = a.getJSONObject(i)
                core.items.add(
                    Recent(
                        o.getString("id"), o.getString("uri"), o.optString("name", "PDF"),
                        o.optInt("pages", -1), o.optInt("last_page", -1), o.optLong("opened", 0),
                    ),
                )
            }
        } catch (e: Exception) {
            core.items.clear()
        }
    }

    private fun save() {
        val a = JSONArray()
        for (r in core.items) {
            val o = JSONObject()
            o.put("id", r.id)
            o.put("uri", r.uri)
            o.put("name", r.name)
            o.put("pages", r.pages)
            o.put("last_page", r.lastPage)
            o.put("opened", r.opened)
            a.put(o)
        }
        prefs.edit().putString("list", a.toString()).apply()
    }

    companion object {
        const val MAX = 20
        const val MAX_THUMB = 40_000
    }
}
