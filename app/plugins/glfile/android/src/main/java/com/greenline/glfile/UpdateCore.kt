// Greenline task 2b.9: in-app updater, the decisions only (pure Kotlin, JVM-tested in UpdateCoreTest).
//
// The app asks GitHub for the latest release of its own public repository and offers it only when:
// - the release is not a draft or pre-release;
// - its tag is a plain version (v1.2.3) newer than the installed version;
// - it carries an APK named greenline-<that version>.apk, at the exact GitHub download address of that
//   release in this repository (no other host or repository is ever downloaded from);
// - the APK size is between 1 MB and 64 MB.
// After the download, GlUpdater also checks that the APK is this app (package name), has a higher
// version code, and is signed with exactly the installed app's certificates (sameSigners).
package com.greenline.glfile

object UpdateCore {
    const val REPO = "nabilmunla/greenline-android"
    const val API_URL = "https://api.github.com/repos/$REPO/releases/latest"
    const val DOWNLOAD_PREFIX = "https://github.com/$REPO/releases/download/"
    const val MIN_BYTES = 1L shl 20
    const val MAX_BYTES = 64L shl 20

    private val VERSION = Regex("^v?(\\d{1,4})\\.(\\d{1,4})\\.(\\d{1,4})$")
    private val ASSET = Regex("^greenline-(\\d{1,4})\\.(\\d{1,4})\\.(\\d{1,4})\\.apk$")

    class Asset(val name: String, val url: String, val size: Long)

    class Release(val tag: String, val draft: Boolean, val prerelease: Boolean, val assets: List<Asset>)

    data class Offer(val version: String, val url: String, val size: Long)

    /** A rejected release; [code] is a fixed E_UPDATE_* code. */
    class Rejected(val code: String) : Exception(code)

    /** "v0.1.9" or "0.1.9" -> [0, 1, 9]; anything else -> null. */
    fun parseVersion(s: String?): IntArray? {
        val m = VERSION.matchEntire((s ?: "").trim()) ?: return null
        return m.groupValues.drop(1).map { it.toInt() }.toIntArray()
    }

    fun isNewer(a: IntArray, b: IntArray): Boolean {
        for (i in 0..2) {
            if (a[i] != b[i]) return a[i] > b[i]
        }
        return false
    }

    /**
     * The update to offer, or null when the latest release is not newer (or is a draft / pre-release).
     * Throws [Rejected] (E_UPDATE_BAD_RELEASE) when a newer release cannot be used safely.
     */
    fun offer(r: Release, installed: String): Offer? {
        if (r.draft || r.prerelease) return null
        val tag = parseVersion(r.tag) ?: throw Rejected("E_UPDATE_BAD_RELEASE")
        val cur = parseVersion(installed) ?: throw Rejected("E_UPDATE_BAD_RELEASE")
        if (!isNewer(tag, cur)) return null
        val version = tag.joinToString(".")
        val asset = r.assets.firstOrNull { a ->
            val m = ASSET.matchEntire(a.name)
            m != null && m.groupValues.drop(1).map { it.toInt() }.joinToString(".") == version
        } ?: throw Rejected("E_UPDATE_BAD_RELEASE")
        if (asset.url != DOWNLOAD_PREFIX + r.tag.trim() + "/" + asset.name) throw Rejected("E_UPDATE_BAD_RELEASE")
        if (asset.size < MIN_BYTES || asset.size > MAX_BYTES) throw Rejected("E_UPDATE_SIZE")
        return Offer(version, asset.url, asset.size)
    }

    /** The downloaded APK may be installed only if it is signed by exactly the installed certificates. */
    fun sameSigners(installed: Set<String>, apk: Set<String>): Boolean = installed.isNotEmpty() && installed == apk

    fun hex(b: ByteArray): String = b.joinToString("") { "%02x".format(it.toInt() and 0xff) }
}
