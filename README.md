# Greenline for Android

A PDF viewer for large drawings on Android phones. It is the phone companion to the [Greenline PDF Editor](https://github.com/nabilmunla/greenline-pdf-editor) desktop app.

Greenline is built for big, dense sheets: architectural, structural and MEP drawings with very dense line work.

## Features
- **Opens large PDFs** through Android's file picker, "Open with" and Share. MuPDF reads each file in pieces; the whole file is never copied into memory.
- **Open with Greenline:** Greenline appears in Android's "Open with" and Share menus for PDFs. You can make it your default PDF app.
- **Touch viewing:**
  - pinch to zoom, from the whole sheet up to 1600 %;
  - drag to pan;
  - swipe up or down to change pages (Settings > Page direction switches to sideways);
  - double-tap to zoom in or back out.
- **Sharp when you stop:** when you stop moving, the visible area is redrawn at your screen's own resolution. Very heavy sheets that exceed the memory limit sharpen only up to 400 %.
- **Start screen:**
  - recent files with thumbnails;
  - a "Continue" card that reopens the last file at the page you were on.

  Files opened from attachments usually can't be reopened later, so they don't appear in Recent.
- **Memory:** the app's memory limits follow your phone's RAM, and it releases memory when Android asks.
- **Updates:** once a day the app checks this repository for a newer release and offers it on the start screen. One tap downloads it; Android then asks you to install. The download is checked first: it must be this app, a newer version, and signed with the same key. Settings > Updates turns the daily check off.

Markup, comments, measuring and search are planned.

## Install
1. Download the APK from [Releases](../../releases) and open it on your phone.
2. Android asks you to allow installs from that source. Allow it, then tap Install.

Updates are signed with the same key and install over the previous version. From 0.1.9 on, the app offers new versions itself; you can also use the Releases page or an updater such as Obtainium.

Requirements: Android 11 or later, on a 64-bit ARM phone.

## Privacy
- Greenline has no account and no analytics.
- Its only network connection is the update check. About once a day while the app is open (and when you tap Check now) it asks GitHub for this repository's latest release. It downloads an update, from GitHub, only when you tap Update. The app sends nothing about you, your phone or your files; GitHub sees your internet address (IP), as any website does. You can turn the daily check off in Settings > Updates.
- Your files stay where they are; Greenline only reads them.
- The recent-files list and its thumbnails stay on your phone. They are excluded from backups and from device-to-device transfers.

## Building from source
**Tools:**
- Rust (stable) with the `aarch64-linux-android` target
- Android SDK with build-tools 35 or later
- NDK 27.2.12479018
- JDK 17
- `tauri-cli` 2

**Build:** `tools/build-apk.sh` builds, aligns and signs the APK. Two things to know before you run it:
- It refuses to run without a non-empty leak-check list at `~/.greenline-denylist`: one word or phrase per line that must never appear in a build.
- On its first run it creates a signing key in `~/.greenline-keys/`.

**Engine tests:** `cargo test --release -p greenline-engine`.

## Licence
- AGPL-3.0-or-later (see `LICENSE`).
- Greenline uses [MuPDF](https://mupdf.com) (AGPL) through the `mupdf` Rust crate, and [Tauri](https://tauri.app).
