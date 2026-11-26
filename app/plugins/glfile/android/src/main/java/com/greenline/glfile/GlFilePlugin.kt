// Greenline task 2b.5: pick a PDF through the Storage Access Framework and hand Rust a detached fd.
// Task 2b.7: saveText, "save as" for the numbers-only measurement export.
//
// Tasks 2bf.10-12: one open path for the picker, files sent by other apps and recent files.
//
// CONFIDENTIALITY: the document's display name, its full URI and any path must never be logged (no
// Log.*, no println) or put into an error message. Downloads URIs contain the file name. Opening hands
// Rust only the fd number, a timing, the URI authority (the provider), a random recent-file id and the
// incoming flag. The one place names leave this class is listRecents, for display on the start screen
// (the app passes that answer through without reading it); they never reach exports or measurement
// records. Every rejection is a fixed code with a fixed message.
package com.greenline.glfile

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.SystemClock
import android.provider.OpenableColumns
import android.webkit.WebView
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

// Task 2b.7: arguments of saveText. `text` is the numbers-only measurement export; `name` is the
// suggested file name (greenline-results-YYYYMMDD-HHMM.jsonl). Neither is logged.
@InvokeArg
class SaveTextArgs {
    var name: String = "greenline-results.jsonl"
    var mime: String = "application/octet-stream"
    var text: String = ""
}

@TauriPlugin
class GlFilePlugin(private val activity: Activity) : Plugin(activity) {

    // Task 2b.7: "save as" through the Storage Access Framework (ACTION_CREATE_DOCUMENT), so the user
    // can save the export into any provider (for example a Drive folder). Only a byte count goes
    // back to Rust; the chosen URI and display name never leave this class.
    @Command
    fun saveText(invoke: Invoke) {
        val args = try {
            invoke.parseArgs(SaveTextArgs::class.java)
        } catch (e: Exception) {
            invoke.reject("bad arguments", "BAD_ARGS")
            return
        }
        val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = args.mime
            putExtra(Intent.EXTRA_TITLE, args.name)
        }
        startActivityForResult(invoke, intent, "onSaveTarget")
    }

    @ActivityCallback
    fun onSaveTarget(invoke: Invoke, result: ActivityResult) {
        if (result.resultCode != Activity.RESULT_OK) {
            invoke.reject("save cancelled", "CANCELLED")
            return
        }
        val uri = result.data?.data
        if (uri == null) {
            invoke.reject("no document created", "NO_URI")
            return
        }
        val args = try {
            invoke.parseArgs(SaveTextArgs::class.java)
        } catch (e: Exception) {
            invoke.reject("bad arguments", "BAD_ARGS")
            return
        }
        // Cloud providers may block while writing: keep it off the UI thread.
        Thread({
            val bytes = args.text.toByteArray(Charsets.UTF_8)
            val ok = writeAll(uri, "wt", bytes) || writeAll(uri, "w", bytes)
            if (ok) {
                val ret = JSObject()
                ret.put("bytes", bytes.size)
                invoke.resolve(ret)
            } else {
                invoke.reject("could not write the document", "WRITE_FAILED")
            }
        }, "gl-saf-save").start()
    }

    // Some providers refuse mode "wt" (truncate); the document is new, so "w" is the fallback.
    private fun writeAll(uri: android.net.Uri, mode: String, bytes: ByteArray): Boolean = try {
        activity.contentResolver.openOutputStream(uri, mode)?.use { out ->
            out.write(bytes)
            out.flush()
            true
        } ?: false
    } catch (e: Exception) {
        // e.message can contain the URI: drop it.
        false
    }

    @Command
    fun pickPdf(invoke: Invoke) {
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "application/pdf"
        }
        startActivityForResult(invoke, intent, "onPdfPicked")
    }

    @ActivityCallback
    fun onPdfPicked(invoke: Invoke, result: ActivityResult) {
        if (result.resultCode != Activity.RESULT_OK) {
            invoke.reject("picker cancelled", "CANCELLED")
            return
        }
        val uri = result.data?.data
        if (uri == null) {
            invoke.reject("picker returned no document", "NO_URI")
            return
        }
        openUri(invoke, uri, incoming = false, recentId = null)
    }

    // ---------------------------------------------------------------------------------------------
    // Task 2bf.11: PDFs sent by other apps (VIEW and SEND intent filters on MainActivity). The newest
    // waiting Uri is kept here; the page pulls it with takeIncoming (on load and whenever it becomes
    // visible), so a cold start never races the WebView. The intent is cleared once its Uri is taken
    // in, so recreating the activity does not open the file again.

    private val pending = PendingSlot<Uri>()
    private val recents by lazy { RecentsStore(activity) }

    override fun load(webView: WebView) {
        super.load(webView)
        catchIntent(activity.intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        catchIntent(intent)
    }

    private fun catchIntent(intent: Intent?) {
        if (intent == null) return
        val uri: Uri? = when (intent.action) {
            Intent.ACTION_VIEW -> intent.data
            Intent.ACTION_SEND -> if (android.os.Build.VERSION.SDK_INT >= 33) {
                intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra(Intent.EXTRA_STREAM)
            }
            else -> null
        }
        // content: only (2bf review finding 8): file: URIs are blocked between apps, and an explicit
        // intent could hand anything else.
        if (uri == null || uri.scheme != "content") return
        pending.set(uri)
        activity.intent = Intent(Intent.ACTION_MAIN)
        trigger("incoming", JSObject())
    }

    @Command
    fun takeIncoming(invoke: Invoke) {
        val uri = pending.take()
        if (uri == null) {
            invoke.reject("nothing waiting", "NONE")
            return
        }
        openUri(invoke, uri, incoming = true, recentId = null)
    }

    // ---------------------------------------------------------------------------------------------
    // Task 2bf.12: recent files. Only files whose read permission could be made persistable are listed
    // (the picker always grants one; other apps usually do not), so every listed file reopens.

    @Command
    fun openRecent(invoke: Invoke) {
        val args = parse(invoke, IdArgs::class.java) ?: return
        val uri = recents.uri(args.id)
        if (uri == null) {
            invoke.reject("no such recent file", "NONE")
            return
        }
        val held = activity.contentResolver.persistedUriPermissions.any { it.uri == uri && it.isReadPermission }
        if (!held) {
            invoke.reject("permission gone", "NO_PERMISSION")
            return
        }
        recents.touch(args.id)
        openUri(invoke, uri, incoming = false, recentId = args.id)
    }

    @Command
    fun listRecents(invoke: Invoke) {
        val ret = JSObject()
        ret.put("items", recents.list())
        invoke.resolve(ret)
    }

    @Command
    fun removeRecent(invoke: Invoke) {
        val args = parse(invoke, IdArgs::class.java) ?: return
        recents.remove(args.id)
        invoke.resolve(JSObject())
    }

    @Command
    fun setLastPage(invoke: Invoke) {
        val args = parse(invoke, PageArgs::class.java) ?: return
        recents.setPages(args.id, args.page, args.pages)
        invoke.resolve(JSObject())
    }

    @Command
    fun saveThumb(invoke: Invoke) {
        val args = parse(invoke, ThumbArgs::class.java) ?: return
        val ret = JSObject()
        ret.put("saved", recents.saveThumb(args.id, args.data))
        invoke.resolve(ret)
    }

    // Task 2bf.11: Android lets only the user make an app the default ("Always"); this only reports it.
    @Command
    fun isDefaultPdfApp(invoke: Invoke) {
        val probe = Intent(Intent.ACTION_VIEW).setDataAndType(Uri.parse("content://greenline.probe/x.pdf"), "application/pdf")
        val pkg = try {
            activity.packageManager.resolveActivity(probe, PackageManager.MATCH_DEFAULT_ONLY)?.activityInfo?.packageName
        } catch (e: Exception) {
            null
        }
        val ret = JSObject()
        ret.put("is_default", pkg == activity.packageName)
        invoke.resolve(ret)
    }

    // Back from a file another app sent: return the user to that app.
    @Command
    fun moveToBack(invoke: Invoke) {
        activity.runOnUiThread { activity.moveTaskToBack(true) }
        invoke.resolve(JSObject())
    }

    // ---------------------------------------------------------------------------------------------

    private fun <T> parse(invoke: Invoke, cls: Class<T>): T? = try {
        invoke.parseArgs(cls)
    } catch (e: Exception) {
        invoke.reject("bad arguments", "BAD_ARGS")
        null
    }

    // One open path for every source (task 2bf.10): make the read permission persistable when the
    // provider allows it (then the file goes into the recent list), and hand Rust a detached fd.
    // openFileDescriptor can block while a cloud provider downloads the file: keep it off the UI
    // thread. Invoke.resolve/reject may be called from any thread.
    private fun openUri(invoke: Invoke, uri: Uri, incoming: Boolean, recentId: String?) {
        val authority = uri.authority ?: ""
        Thread({
            var id = recentId
            var isNew = false
            if (id == null) {
                val persisted = try {
                    activity.contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION)
                    true
                } catch (e: Exception) {
                    false
                }
                if (persisted) {
                    isNew = !recents.has(uri)
                    id = recents.add(uri, displayName(uri))
                }
            }
            val t0 = SystemClock.elapsedRealtimeNanos()
            var noPermission = false
            val fd = try {
                activity.contentResolver.openFileDescriptor(uri, "r")?.detachFd()
            } catch (e: SecurityException) {
                noPermission = true
                null
            } catch (e: Exception) {
                // e.message can contain the URI: drop it.
                null
            }
            val ms = (SystemClock.elapsedRealtimeNanos() - t0) / 1.0e6
            if (fd == null || fd < 0) {
                // A file that never opened is not offered again (2bf review finding 3).
                if (isNew && id != null) recents.remove(id)
                if (noPermission) invoke.reject("permission gone", "NO_PERMISSION")
                else invoke.reject("could not open the document", "OPEN_FAILED")
            } else {
                val ret = JSObject()
                ret.put("fd", fd)
                ret.put("pick_to_fd_ms", ms)
                ret.put("authority", authority)
                ret.put("incoming", incoming)
                if (id != null) ret.put("recent_id", id)
                ret.put("recent_new", isNew)
                invoke.resolve(ret)
            }
        }, "gl-saf-open").start()
    }

    private fun displayName(uri: Uri): String = try {
        activity.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) c.getString(0) else null
        } ?: "PDF"
    } catch (e: Exception) {
        "PDF"
    }
}

@InvokeArg
class IdArgs {
    var id: String = ""
}

@InvokeArg
class PageArgs {
    var id: String = ""
    var page: Int = -1
    var pages: Int = -1
}

@InvokeArg
class ThumbArgs {
    var id: String = ""
    var data: String = ""
}
