// Greenline task 2b.9: in-app updater, the Android side. Decisions live in UpdateCore (JVM-tested).
//
// check():    GET the latest release of this app's own GitHub repository (the only network request the
//             app makes), and keep the offer if UpdateCore accepts it.
// download(): fetch that APK into the app's private cache (update/greenline-update.apk), exact size,
//             then verify() it.
// install():  verify() again, then open Android's installer on it (the user taps Install). Without the
//             "install unknown apps" permission, opens that settings screen instead (E_UPDATE_PERMISSION).
// verify():   same package name, higher version code, and signed by exactly the installed certificates.
//
// Nothing about the user or their files is sent; the request carries a fixed User-Agent only. Every
// failure is a fixed E_UPDATE_* code; nothing is logged.
package com.greenline.glfile

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.provider.Settings
import androidx.core.content.FileProvider
import org.json.JSONObject
import java.io.File
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import javax.net.ssl.HttpsURLConnection

/** The FileProvider that hands the downloaded APK to Android's installer (its own class, so it does not
 *  clash with Tauri's FileProvider entry in the merged manifest). */
class GlUpdateProvider : FileProvider()

class GlUpdater(private val activity: Activity) {
    @Volatile var done = 0L
        private set
    @Volatile var total = 0L
        private set
    @Volatile private var busy = false
    @Volatile private var offer: UpdateCore.Offer? = null
    @Volatile private var ready = false

    private val pm get() = activity.packageManager
    private val file get() = File(File(activity.cacheDir, "update"), "greenline-update.apk")

    private fun installed(): PackageInfo = pm.getPackageInfo(activity.packageName, PackageManager.GET_SIGNING_CERTIFICATES)

    /** The offer, or null when nothing newer is published. */
    fun check(): UpdateCore.Offer? {
        val body = String(getApi(UpdateCore.API_URL, 1L shl 20), Charsets.UTF_8)
        val release = try {
            val o = JSONObject(body)
            val arr = o.optJSONArray("assets")
            val assets = ArrayList<UpdateCore.Asset>()
            if (arr != null) {
                for (i in 0 until arr.length()) {
                    val a = arr.getJSONObject(i)
                    assets.add(UpdateCore.Asset(a.optString("name"), a.optString("browser_download_url"), a.optLong("size", -1)))
                }
            }
            UpdateCore.Release(o.optString("tag_name"), o.optBoolean("draft"), o.optBoolean("prerelease"), assets)
        } catch (e: Exception) {
            throw UpdateCore.Rejected("E_UPDATE_BAD_RELEASE")
        }
        val found = UpdateCore.offer(release, installed().versionName ?: "")
        if (found != offer) ready = false
        offer = found
        return found
    }

    /** Download the offered APK and verify it. Blocking; call off the UI thread. */
    fun download() {
        val o = offer ?: throw UpdateCore.Rejected("E_UPDATE_NO_DOWNLOAD")
        synchronized(this) {
            if (busy) throw UpdateCore.Rejected("E_UPDATE_BUSY")
            busy = true
        }
        try {
            ready = false
            done = 0
            total = o.size
            val dir = file.parentFile!!
            dir.mkdirs()
            dir.listFiles()?.forEach { it.delete() }
            val part = File(dir, "greenline-update.part")
            val bytes = getFile(o.url, UpdateCore.MAX_BYTES, part)
            if (bytes != o.size) {
                part.delete()
                throw UpdateCore.Rejected("E_UPDATE_SIZE")
            }
            if (!part.renameTo(file)) throw UpdateCore.Rejected("E_UPDATE_FAILED")
            verify()
            ready = true
        } catch (e: UpdateCore.Rejected) {
            file.delete()
            throw e
        } finally {
            busy = false
        }
    }

    /** Open Android's installer on the verified APK. False if the permission screen was opened instead. */
    fun install(): Boolean {
        if (!ready || !file.isFile) throw UpdateCore.Rejected("E_UPDATE_NO_DOWNLOAD")
        if (!pm.canRequestPackageInstalls()) {
            val i = Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:" + activity.packageName))
            activity.runOnUiThread { activity.startActivity(i) }
            return false
        }
        verify()
        val uri = FileProvider.getUriForFile(activity, activity.packageName + ".glupdate", file)
        val i = Intent(Intent.ACTION_VIEW)
            .setDataAndType(uri, "application/vnd.android.package-archive")
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        activity.runOnUiThread { activity.startActivity(i) }
        return true
    }

    /** This app, a higher version code, the same signing certificates. Else the file is deleted. */
    private fun verify() {
        val me = installed()
        val apk = pm.getPackageArchiveInfo(file.path, PackageManager.GET_SIGNING_CERTIFICATES)
        val ok = apk != null &&
            apk.packageName == activity.packageName &&
            apk.longVersionCode > me.longVersionCode &&
            UpdateCore.sameSigners(signers(me), signers(apk))
        if (!ok) {
            file.delete()
            ready = false
            throw UpdateCore.Rejected("E_UPDATE_SIGNATURE")
        }
    }

    private fun signers(p: PackageInfo): Set<String> {
        val sigs = p.signingInfo?.apkContentsSigners ?: return emptySet()
        return sigs.map { UpdateCore.hex(MessageDigest.getInstance("SHA-256").digest(it.toByteArray())) }.toSet()
    }

    /** HTTPS GET with a size cap, into a file (returns the byte count). */
    private fun getFile(url: String, cap: Long, into: File): Long = fetch(url, cap, false, into).first

    /** HTTPS GET of the GitHub API with a size cap, into memory. */
    private fun getApi(url: String, cap: Long): ByteArray = fetch(url, cap, true, null).second!!

    private fun fetch(url: String, cap: Long, api: Boolean, into: File?): Pair<Long, ByteArray?> {
        var c: HttpURLConnection? = null
        try {
            c = URL(url).openConnection() as HttpURLConnection
            if (c !is HttpsURLConnection) throw UpdateCore.Rejected("E_UPDATE_BAD_RELEASE")
            c.connectTimeout = 15_000
            c.readTimeout = 30_000
            c.instanceFollowRedirects = true // GitHub sends release downloads on to its file host (HTTPS)
            c.setRequestProperty("User-Agent", "Greenline-Android")
            if (api) c.setRequestProperty("Accept", "application/vnd.github+json")
            val code = c.responseCode
            if (code != 200) throw UpdateCore.Rejected(if (api && code == 404) "E_UPDATE_BAD_RELEASE" else "E_UPDATE_NETWORK")
            if (c.url.protocol != "https") throw UpdateCore.Rejected("E_UPDATE_BAD_RELEASE")
            c.inputStream.use { input ->
                return if (into != null) Pair(copy(input, into, cap), null) else Pair(0L, read(input, cap))
            }
        } catch (e: UpdateCore.Rejected) {
            throw e
        } catch (e: Exception) {
            throw UpdateCore.Rejected("E_UPDATE_NETWORK")
        } finally {
            c?.disconnect()
        }
    }

    private fun read(input: InputStream, cap: Long): ByteArray {
        val out = java.io.ByteArrayOutputStream()
        val buf = ByteArray(16 * 1024)
        while (true) {
            val n = input.read(buf)
            if (n < 0) break
            out.write(buf, 0, n)
            if (out.size() > cap) throw UpdateCore.Rejected("E_UPDATE_SIZE")
        }
        return out.toByteArray()
    }

    private fun copy(input: InputStream, to: File, cap: Long): Long {
        var n = 0L
        to.outputStream().use { out ->
            val buf = ByteArray(64 * 1024)
            while (true) {
                val k = input.read(buf)
                if (k < 0) break
                n += k
                if (n > cap) throw UpdateCore.Rejected("E_UPDATE_SIZE")
                out.write(buf, 0, k)
                done = n
            }
        }
        return n
    }
}
