#!/bin/bash
# Build the signed release APK: aarch64 / arm64-v8a only, minSdk 30, 16 KB page-aligned .so.
#
# usage: tools/build-apk.sh
# Output: dist/greenline-<version>.apk, copied there only after leakcheck passes.
#
# Steps: cargo tauri android build (release, --apk, --target aarch64) -> 16 KB ELF check ->
# tools/sign-apk.sh (zipalign -P 16, apksigner, verify) -> tools/leakcheck.sh -> dist/.
set -euo pipefail
. "$HOME/.cargo/env"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Same cross-compile env as the engine build and spike/ (CC/CXX/AR/RANLIB, linker, bindgen sysroot),
# exported here so Tauri's Gradle-driven cargo build sees it too.
. "$ROOT/tools/android-env.sh"
export ANDROID_HOME NDK_HOME
export ANDROID_SDK_ROOT="$ANDROID_HOME"
export JAVA_HOME="/usr/lib/jvm/java-17-openjdk-amd64"
export PATH="$JAVA_HOME/bin:$PATH"
export CARGO_BUILD_JOBS=8
# 16 KB ELF segment alignment for Android 15+ devices (checked below with llvm-readelf).
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384 --remap-path-prefix=$HOME=/build --remap-path-prefix=$ROOT=/src"
# No build-machine paths in the native library: the remaps above cover Rust (the last match wins, so
# the repo maps to /src), and the prefix maps below cover the C code (MuPDF, fdstream.c).
export CFLAGS_aarch64_linux_android="${CFLAGS_aarch64_linux_android:-} -ffile-prefix-map=$HOME=/build -ffile-prefix-map=$ROOT=/src"
export CXXFLAGS_aarch64_linux_android="${CXXFLAGS_aarch64_linux_android:-} -ffile-prefix-map=$HOME=/build -ffile-prefix-map=$ROOT=/src"

VERSION="$(sed -n 's/^ *"version": *"\([^"]*\)".*/\1/p' app/src-tauri/tauri.conf.json | head -n 1)"
[ -n "$VERSION" ] || { echo "build-apk: no version in tauri.conf.json" >&2; exit 1; }

GEN="$ROOT/app/src-tauri/gen/android"
OUTDIR="$GEN/app/build/outputs/apk"
rm -rf "$OUTDIR"

echo "build-apk: cargo tauri android build --apk --target aarch64 (release, v$VERSION)"
(cd app/src-tauri && cargo tauri android build --apk --target aarch64 --ci)

UNSIGNED="$OUTDIR/universal/release/app-universal-release-unsigned.apk"
[ -f "$UNSIGNED" ] || UNSIGNED="$(find "$OUTDIR" -name '*-release*.apk' | head -n 1)"
[ -n "$UNSIGNED" ] && [ -f "$UNSIGNED" ] || { echo "build-apk: no release APK under $OUTDIR" >&2; exit 1; }
echo "build-apk: unsigned APK $UNSIGNED"

STAGE="$ROOT/target/apk"
mkdir -p "$STAGE"
SIGNED="$STAGE/greenline-$VERSION-signed.apk"
rm -f "$SIGNED"

# No home-folder paths in native libraries (they would publish the build account name).
HOMEHITS=0
while IFS= read -r so; do
  n=$(unzip -p "$UNSIGNED" "$so" | grep -aoF "$HOME/" | wc -l || true)
  [ "$n" -eq 0 ] || { echo "build-apk: $so holds $n home-folder paths" >&2; HOMEHITS=1; }
done < <(unzip -Z1 "$UNSIGNED" | grep '\.so$')
[ "$HOMEHITS" -eq 0 ] || { echo "build-apk: FAIL home-folder paths in native libraries" >&2; exit 1; }

# Every native lib must be arm64-v8a and every LOAD segment aligned to at least 16 KB.
READELF="$NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf"
CHK="$(mktemp -d)"
trap 'rm -rf "$CHK"' EXIT
LIBS="$(unzip -Z1 "$UNSIGNED" 'lib/*' 2>/dev/null || true)"
[ -n "$LIBS" ] || { echo "build-apk: no native libs in the APK" >&2; exit 1; }
if grep -v '^lib/arm64-v8a/' <<<"$LIBS"; then
    echo "build-apk: native libs outside lib/arm64-v8a/ (listed above)" >&2; exit 1
fi
unzip -q -o "$UNSIGNED" 'lib/*' -d "$CHK"
while read -r so; do
    aligns="$("$READELF" -lW "$CHK/$so" | awk '$1=="LOAD" { print $NF }')"
    [ -n "$aligns" ] || { echo "build-apk: $so has no LOAD segments" >&2; exit 1; }
    for a in $aligns; do
        if (( a < 16384 )); then
            echo "build-apk: $so has a LOAD segment aligned to $a (< 0x4000)" >&2; exit 1
        fi
    done
    echo "build-apk: $so LOAD p_align $(echo $aligns | tr ' ' ',') OK (>= 0x4000)"
done <<<"$LIBS"

"$ROOT/tools/sign-apk.sh" "$UNSIGNED" "$SIGNED"

echo "build-apk: leakcheck $SIGNED"
"$ROOT/tools/leakcheck.sh" "$SIGNED"

mkdir -p "$ROOT/dist"
FINAL="$ROOT/dist/greenline-$VERSION.apk"
cp -f "$SIGNED" "$FINAL"
echo "build-apk: OK $FINAL ($(stat -c %s "$FINAL") bytes)"
