#!/usr/bin/env bash
# leakcheck.sh - leak guard for Greenline Android.
#
# Usage: tools/leakcheck.sh <file.apk|file.zip|directory>
#
# Run it on every APK, release zip and backup zip before it leaves the PC.
# Exit 0 and "LEAKCHECK PASS", or exit 1 and one or more "LEAKCHECK FAIL: <reason>" lines.
#
# It fails when anything inside (nested zip/jar/aar/apk are unpacked, up to 5 levels):
#   - is a PDF, by extension (case-insensitive) or by content (starts with the PDF header);
#   - is larger than 20 MiB and not matched by a glob in tools/leakcheck-allow.txt;
#   - contains a line of the deny-list file (case-insensitive) in an entry name, in the
#     content (binary included; UTF-16 text is caught too), or in a git bundle's history;
#   - is a symlink, or a special file;
#   - cannot be inspected (encrypted or corrupt nested archive): it fails closed.
#
# Deny-list: $GL_DENYLIST, default ~/.greenline-denylist. One confidential string per line.
# It is required (missing or empty = FAIL). The matched line is never printed, and an entry
# name is printed only when that name is not itself a deny-list hit.
#
# Optional env: GL_ALLOWLIST (default: leakcheck-allow.txt next to this script).

set -u
umask 077
export LC_ALL=C

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
MAX_BYTES=$((20 * 1024 * 1024))
MAX_DEPTH=5
DENYLIST=${GL_DENYLIST:-$HOME/.greenline-denylist}
ALLOWLIST=${GL_ALLOWLIST:-$SCRIPT_DIR/leakcheck-allow.txt}
PDF_MAGIC='%PDF-'
WORK=""
DENY=""
NONASCII=0
FAILS=()
ALLOW=()

die() {
    echo "LEAKCHECK FAIL: $1"
    exit 1
}

fail() {
    local m
    for m in "${FAILS[@]}"; do [[ $m == "$1" ]] && return 0; done
    FAILS+=("$1")
}

cleanup() {
    if [[ -n $WORK && -d $WORK ]]; then
        chmod -R u+rwX "$WORK" 2>/dev/null
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ---------------------------------------------------------------- deny-list matching
# dg [grep opts] file : 0 = hit, 1 = no hit, 2 = error. Never prints.
dg() {
    local rc
    LC_ALL=C grep -a -q -F -i -f "$DENY" "$@"
    rc=$?
    if ((rc == 1 && NONASCII)); then
        LC_ALL=C.UTF-8 grep -a -q -F -i -f "$DENY" "$@"
        rc=$?
    fi
    return $rc
}

# deny_content file : as dg, also catches UTF-16 text (NULs stripped).
deny_content() {
    local rc
    dg "$1"
    rc=$?
    ((rc == 1)) || return $rc
    tr -d '\0' <"$1" | LC_ALL=C grep -a -q -F -i -f "$DENY"
}

# safe_name name : prints the name, or a placeholder if the name is itself a deny-list hit.
safe_name() {
    local f="$WORK/safe_name.tmp" s
    printf '%s\n' "$1" >"$f"
    dg "$f"
    if (($? != 1)); then
        printf '%s' '<entry name withheld>'
    else
        s=${1//[^[:print:]]/?}
        printf '%s' "$s"
    fi
}

# check_names nulfile context-prefix : deny-list hit in any of the NUL-separated names.
check_names() {
    local rc where=""
    dg -z "$1"
    rc=$?
    if ((rc == 0)); then
        [[ -n $2 ]] && where=" in archive $(safe_name "${2%!/}")"
        fail "deny-list string found in an entry name${where}: <entry name withheld>"
    elif ((rc != 1)); then
        fail "scan error while checking entry names"
    fi
}

# ---------------------------------------------------------------- allow-list
load_allowlist() {
    local line
    [[ -f $ALLOWLIST ]] || return 0
    while IFS= read -r line || [[ -n $line ]]; do
        line=${line%$'\r'}
        line="${line#"${line%%[![:space:]]*}"}"
        line="${line%"${line##*[![:space:]]}"}"
        [[ -z $line || $line == \#* ]] && continue
        ALLOW+=("$line")
    done <"$ALLOWLIST"
}

allowed() {
    local pat
    for pat in "${ALLOW[@]}"; do
        # shellcheck disable=SC2053
        [[ $1 == $pat ]] && return 0
    done
    return 1
}

# ---------------------------------------------------------------- git history
# scan_git gitdir label : history of a git repository / bundle clone.
scan_git() {
    local gd=$1 rel=$2 objs="$WORK/gitobjs.$RANDOM" stream="$WORK/gitstream.$RANDOM" rc lbl
    gg() { git -c safe.directory='*' --git-dir="$gd" "$@"; }
    lbl=$(safe_name "$rel")
    if ! gg rev-list --objects --all >"$objs" 2>/dev/null; then
        fail "cannot read git history of $lbl"
        rm -f "$objs"
        return
    fi
    sed -n 's/^[0-9a-f]* //p' "$objs" | tr '\n' '\0' >"$objs.names"
    if LC_ALL=C grep -z -a -q -i -E '\.pdf[[:space:].]*$' "$objs.names"; then
        fail "git history of $lbl contains a PDF path"
    fi
    check_names "$objs.names" "$rel!/"
    rm -f "$objs" "$objs.names"
    if gg cat-file --batch-all-objects --batch-check='%(objecttype) %(objectsize)' 2>/dev/null |
        awk -v m="$MAX_BYTES" '$1=="blob" && $2+0>m {f=1} END {exit f?0:1}'; then
        fail "git history of $lbl contains a blob over 20 MB"
    fi
    if ! gg cat-file --batch-all-objects --batch >"$stream" 2>/dev/null; then
        fail "cannot read git objects of $lbl"
        rm -f "$stream"
        return
    fi
    if LC_ALL=C grep -a -q -E "^$PDF_MAGIC" "$stream"; then
        fail "git history of $lbl contains PDF content"
    fi
    deny_content "$stream"
    rc=$?
    if ((rc == 0)); then
        fail "deny-list string found in git history (file names, contents or messages) of $lbl"
    elif ((rc != 1)); then
        fail "scan error while checking git history of $lbl"
    fi
    rm -f "$stream"
}

scan_bundle() {
    local f=$1 rel=$2 d
    d=$(mktemp -d "$WORK/b.XXXXXX")
    if ! git clone -q --mirror "$f" "$d/r.git" </dev/null >/dev/null 2>&1; then
        fail "cannot read git bundle (incomplete or corrupt): $(safe_name "$rel")"
        return
    fi
    scan_git "$d/r.git" "$rel"
}

# ---------------------------------------------------------------- tree / archive scan
# scan_one file rel depth
scan_one() {
    local f=$1 rel=$2 depth=$3 lower size hex rc lbl
    if [[ -L $f ]]; then
        fail "symlink entry: $(safe_name "$rel")"
        return
    fi
    if [[ ! -f $f ]]; then
        fail "special (non-regular) file: $(safe_name "$rel")"
        return
    fi
    NFILES=$((NFILES + 1))
    lbl=$(safe_name "$rel")

    lower=${rel,,}
    if [[ $lower =~ \.pdf[[:space:].]*$ ]]; then
        fail "PDF file (by extension): $lbl"
    fi
    # Content starts with the PDF header (leading whitespace or a UTF-8 BOM tolerated).
    if [[ $(head -c 32 "$f" | tr -d ' \t\r\n\357\273\277\0' | head -c 5) == "$PDF_MAGIC" ]]; then
        fail "PDF file (by content): $lbl"
    fi

    size=$(stat -c %s "$f")
    if ((size > MAX_BYTES)) && ! allowed "$rel"; then
        fail "file over 20 MB and not on the allow-list: $lbl ($((size / 1048576)) MB)"
    fi

    deny_content "$f"
    rc=$?
    if ((rc == 0)); then
        fail "deny-list string found in the content of: $lbl"
    elif ((rc != 1)); then
        fail "scan error while reading: $lbl"
    fi

    hex=$(head -c 4 "$f" | od -An -tx1 | tr -d ' \n')
    if [[ $hex == 504b0304 ]]; then
        unpack_and_scan "$f" "$rel!/" $((depth + 1))
    elif head -c 20 "$f" | LC_ALL=C grep -a -q -E '^# v[23] git bundle'; then
        scan_bundle "$f" "$rel"
    fi
}

# scan_tree dir prefix depth
scan_tree() {
    local root=$1 prefix=$2 depth=$3 names f gd
    names="$WORK/names.$RANDOM.$RANDOM"
    if ! find "$root" -mindepth 1 -printf '%P\0' >"$names" 2>/dev/null; then
        fail "cannot list folder contents"
    fi
    check_names "$names" "$prefix"
    rm -f "$names"
    while IFS= read -r -d '' f; do
        scan_one "$f" "${prefix}${f#"$root"/}" "$depth"
    done < <(find "$root" -mindepth 1 ! -type d -print0)
    while IFS= read -r -d '' gd; do
        scan_git "$gd" "${prefix}${gd#"$root"/}"
    done < <(find "$root" -mindepth 1 -type d -name .git -prune -print0)
}

# unpack_and_scan archive prefix depth
unpack_and_scan() {
    local archive=$1 prefix=$2 depth=$3 d raw rc
    if ((depth > MAX_DEPTH)); then
        fail "archives nested more than $MAX_DEPTH levels deep at: $(safe_name "${prefix%!/}")"
        return
    fi
    d=$(mktemp -d "$WORK/u.XXXXXX")
    raw="$d.names"
    if ! unzip -Z1 "$archive" </dev/null >"$raw" 2>/dev/null; then
        fail "cannot read archive (corrupt or not a zip): $(safe_name "${prefix%!/}")"
        return
    fi
    # Two entries with the same path: unzip -o keeps only the last, so the earlier one would never be
    # content-scanned. Fail closed (the name is not printed).
    if [[ -n $(LC_ALL=C sort "$raw" | LC_ALL=C uniq -d | head -n 1) ]]; then
        rm -f "$raw"
        fail "archive has two entries with the same path, so it cannot be inspected: $(safe_name "${prefix%!/}")"
        return
    fi
    tr '\n' '\0' <"$raw" >"$raw.0"
    check_names "$raw.0" "$prefix"
    rm -f "$raw" "$raw.0"
    unzip -qq -o -d "$d" "$archive" </dev/null >/dev/null 2>&1
    rc=$?
    # Any non-zero status fails closed: 1 means "success with warnings" (members skipped, renamed or
    # partly written), and a member that was not extracted would never be content-scanned.
    if ((rc != 0)); then
        fail "cannot unpack archive cleanly (encrypted, corrupt or unzip warnings), so it cannot be inspected: $(safe_name "${prefix%!/}")"
        return
    fi
    chmod -R u+rwX "$d" 2>/dev/null
    scan_tree "$d" "$prefix" "$depth"
}

# ---------------------------------------------------------------- main
(($# == 1)) || die "usage: tools/leakcheck.sh <file.apk|file.zip|directory>"
TARGET=$1
[[ -e $TARGET ]] || die "target not found: $TARGET"

WORK=$(mktemp -d) || die "cannot create a temp folder"

if [[ ! -f $DENYLIST || ! -r $DENYLIST ]]; then
    die "the deny-list is required but is missing or unreadable: $DENYLIST (one confidential string per line; set GL_DENYLIST to use another path)"
fi
DENY="$WORK/deny.txt"
# Strip BOM and CRs, drop blank lines (an empty pattern would match everything).
sed '1s/^\xEF\xBB\xBF//' "$DENYLIST" | tr -d '\r' | grep -a -v -E '^[[:space:]]*$' >"$DENY"
if [[ ! -s $DENY ]]; then
    die "the deny-list is required but is empty: $DENYLIST (one confidential string per line)"
fi
if grep -a -q -v -E '^[[:print:]]*$' "$DENY"; then NONASCII=1; fi

load_allowlist

NFILES=0
if [[ -d $TARGET ]]; then
    scan_tree "${TARGET%/}" "" 0
elif [[ -f $TARGET ]]; then
    unpack_and_scan "$TARGET" "" 1
else
    die "target is neither a file nor a folder: $TARGET"
fi

if ((${#FAILS[@]} == 0)); then
    echo "LEAKCHECK PASS"
    exit 0
fi
i=0
for m in "${FAILS[@]}"; do
    echo "LEAKCHECK FAIL: $m"
    i=$((i + 1))
    if ((i >= 25 && ${#FAILS[@]} > 25)); then
        echo "LEAKCHECK FAIL: ... and $((${#FAILS[@]} - 25)) more problem(s)"
        break
    fi
done
exit 1
