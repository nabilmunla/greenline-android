#!/bin/bash
# Sign a release APK for sideloading: zipalign (16 KB .so pages), apksigner, then verify both.
#
# usage: tools/sign-apk.sh <unsigned.apk> <signed-out.apk>
#
# The keystore lives outside the repo in ~/.greenline-keys/. On first use it is created once
# (RSA 4096, 10000 days, alias "greenline") with a random password stored in
# ~/.greenline-keys/keystore.properties (chmod 600). The password is never printed and never
# put on a command line: keytool and apksigner read it from an environment variable.
set -euo pipefail
umask 077

die() { echo "sign-apk: $*" >&2; exit 1; }
[ $# -eq 2 ] || die "usage: $0 <unsigned.apk> <signed-out.apk>"
IN="$1"
OUT="$2"
[ -f "$IN" ] || die "input APK not found: $IN"

export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export JAVA_HOME="${JAVA_HOME:-/usr/lib/jvm/java-17-openjdk-amd64}"
export PATH="$JAVA_HOME/bin:$PATH"
# zipalign -P (16 KB page alignment) needs build-tools 35+; use the newest installed.
BT=""
for d in $(ls -d "$ANDROID_HOME"/build-tools/*/ 2>/dev/null | sort -V); do BT="${d%/}"; done
[ -n "$BT" ] && [ -x "$BT/zipalign" ] && [ -x "$BT/apksigner" ] || die "no build-tools with zipalign and apksigner under $ANDROID_HOME/build-tools"
ZA_USAGE="$("$BT/zipalign" 2>&1 || true)"
[[ $ZA_USAGE == *"-P <pagesize_kb>"* ]] || die "$BT/zipalign has no -P flag; install build-tools 35 or newer"

KEYDIR="$HOME/.greenline-keys"
KS="$KEYDIR/greenline-release.jks"
PROPS="$KEYDIR/keystore.properties"
ALIAS=greenline

if [ ! -f "$KS" ]; then
    # Never mint a new password next to an old one: a stray properties file means a lost keystore.
    [ ! -e "$PROPS" ] || die "$PROPS exists but $KS is missing; restore the keystore instead of making a new key"
    mkdir -p "$KEYDIR"
    chmod 700 "$KEYDIR"
    PASS="$(head -c 64 /dev/urandom | base64 -w 0 | tr -dc 'A-Za-z0-9')"
    PASS="${PASS:0:40}"
    [ ${#PASS} -ge 32 ] || die "could not generate a keystore password"
    {
        echo "# Greenline release keystore. Never commit, never print."
        echo "storeFile=$KS"
        echo "keyAlias=$ALIAS"
        echo "storePassword=$PASS"
        echo "keyPassword=$PASS"
    } > "$PROPS"
    chmod 600 "$PROPS"
    echo "sign-apk: creating release keystore $KS (one time)"
    GL_KS_PASS="$PASS" keytool -genkeypair -noprompt \
        -keystore "$KS" -storetype PKCS12 \
        -alias "$ALIAS" -keyalg RSA -keysize 4096 -validity 10000 \
        -dname "CN=Greenline" \
        -storepass:env GL_KS_PASS -keypass:env GL_KS_PASS >/dev/null
    chmod 600 "$KS"
    unset PASS
fi

[ -f "$PROPS" ] || die "$PROPS missing: cannot read the keystore password"
GL_KS_PASS="$(grep -m 1 '^storePassword=' "$PROPS" || true)"
GL_KS_PASS="${GL_KS_PASS#storePassword=}"
[ -n "$GL_KS_PASS" ] || die "no storePassword in $PROPS"
export GL_KS_PASS

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$(dirname "$OUT")"

echo "sign-apk: zipalign -P 16 -f 4"
"$BT/zipalign" -P 16 -f 4 "$IN" "$WORK/aligned.apk"

echo "sign-apk: apksigner sign ($(basename "$BT"))"
"$BT/apksigner" sign \
    --ks "$KS" --ks-key-alias "$ALIAS" \
    --ks-pass env:GL_KS_PASS --key-pass env:GL_KS_PASS \
    --out "$WORK/signed.apk" "$WORK/aligned.apk"
unset GL_KS_PASS

echo "sign-apk: apksigner verify"
"$BT/apksigner" verify --verbose "$WORK/signed.apk" | grep -E '^Verifies|^Verified using' || true
"$BT/apksigner" verify "$WORK/signed.apk"

echo "sign-apk: zipalign -c -P 16 4"
"$BT/zipalign" -c -P 16 4 "$WORK/signed.apk"

mv -f "$WORK/signed.apk" "$OUT"
chmod 644 "$OUT"
echo "sign-apk: OK $OUT"
