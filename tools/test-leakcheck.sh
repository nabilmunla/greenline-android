#!/usr/bin/env bash
# test-leakcheck.sh - self-test for tools/leakcheck.sh and tools/backup.sh.
#
# Usage: bash tools/test-leakcheck.sh
# Prints PASS/FAIL per case; exits non-zero if any case fails.
# Uses a fake deny-list in a temp folder (GL_DENYLIST); never reads or writes ~/.greenline-denylist.
#
# Note: the fake secret is assembled at run time so the literal string never appears in this
# file (otherwise a backup of the repo would legitimately fail its own deny-list check).

set -u
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LC="$ROOT/tools/leakcheck.sh"
BK="$ROOT/tools/backup.sh"
T=$(mktemp -d) || exit 2
trap 'rm -rf "$T"' EXIT

DENYSTR=$(printf 'SECRET-%s-%s' DWG 123)
DL="$T/denylist.txt"
printf '%s\n' "$DENYSTR" >"$DL"

NP=0
NF=0
OUT=""
RC=0

# ---- helpers -----------------------------------------------------------------------------
cat >"$T/mkzip.py" <<'PY'
import os, sys, zipfile
out, src = sys.argv[1:3]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    for base, dirs, files in os.walk(src):
        dirs.sort()
        for f in sorted(files):
            p = os.path.join(base, f)
            z.write(p, os.path.relpath(p, src).replace(os.sep, "/"))
PY

# mkzip <out.zip> <stage-dir>
mkzip() { python3 -I "$T/mkzip.py" "$1" "$2"; }

# stage <name> : fresh stage dir with one clean text file and one clean binary file
stage() {
    rm -rf "${T:?}/$1"
    mkdir -p "$T/$1"
    echo "hello, nothing to see" >"$T/$1/readme.txt"
    head -c 4000 /dev/urandom >"$T/$1/data.bin"
}

# run <deny-list> args... : runs leakcheck, sets OUT/RC, flags a leaked secret in the output
LEAKED=0
run() {
    local dl=$1
    shift
    OUT=$(GL_DENYLIST="$dl" "$LC" "$@" 2>&1)
    RC=$?
    LEAKED=0
    if grep -qiF -e "$DENYSTR" <<<"$OUT"; then LEAKED=1; fi
}

report() { # name ok detail
    if [[ $2 == 1 ]]; then
        echo "PASS: $1"
        NP=$((NP + 1))
    else
        echo "FAIL: $1 -- $3"
        sed 's/^/        | /' <<<"$OUT"
        NF=$((NF + 1))
    fi
}

# expect_pass <name> <target> [deny-list]
expect_pass() {
    run "${3:-$DL}" "$2"
    if ((LEAKED)); then
        report "$1" 0 "output leaked the secret"
    elif ((RC == 0)) && [[ $OUT == "LEAKCHECK PASS" ]]; then
        report "$1" 1 ""
    else
        report "$1" 0 "expected LEAKCHECK PASS (rc=$RC)"
    fi
}

# expect_fail <name> <target> [expected-substring] [deny-list]
expect_fail() {
    run "${4:-$DL}" "$2"
    if ((LEAKED)); then
        report "$1" 0 "output leaked the secret"
    elif ((RC != 1)); then
        report "$1" 0 "expected exit 1, got $RC"
    elif ! grep -q '^LEAKCHECK FAIL: ' <<<"$OUT"; then
        report "$1" 0 "no 'LEAKCHECK FAIL:' line"
    elif [[ -n ${3:-} ]] && ! grep -qF -e "$3" <<<"$OUT"; then
        report "$1" 0 "expected text not in output: $3"
    else
        report "$1" 1 ""
    fi
}

# ---- cases ---------------------------------------------------------------------------------
echo "leakcheck self-test (fake deny-list, temp folder $T)"

# 1. clean zip passes
stage clean
mkzip "$T/clean.zip" "$T/clean"
expect_pass "clean zip passes" "$T/clean.zip"

# 1b. clean zip with a nested clean zip passes
stage nested_clean
mkzip "$T/inner_clean.zip" "$T/clean"
cp "$T/inner_clean.zip" "$T/nested_clean/inner.jar"
mkzip "$T/nested_clean.zip" "$T/nested_clean"
expect_pass "clean zip with nested clean jar passes" "$T/nested_clean.zip"

# 1c. blank lines and CRLF in the deny-list must not make everything match
printf '\r\n\r\n%s\r\n\r\n   \r\n' "$DENYSTR" >"$T/dl_blank.txt"
expect_pass "deny-list with blank lines and CRLF: clean zip still passes" "$T/clean.zip" "$T/dl_blank.txt"

# 2. planted x.pdf
stage pdf
echo "not really a pdf" >"$T/pdf/x.pdf"
mkzip "$T/pdf.zip" "$T/pdf"
expect_fail "zip with planted x.pdf fails" "$T/pdf.zip" "PDF"

# 2b. upper-case extension
stage pdf_upper
echo "x" >"$T/pdf_upper/Y.PdF"
mkzip "$T/pdf_upper.zip" "$T/pdf_upper"
expect_fail "zip with Y.PdF (mixed-case extension) fails" "$T/pdf_upper.zip" "PDF"

# 3. %PDF- content in a .bin name
stage pdfbin
printf '%%PDF-1.7\n%%binary content\n' >"$T/pdfbin/sneaky.bin"
head -c 3000 /dev/urandom >>"$T/pdfbin/sneaky.bin"
mkzip "$T/pdfbin.zip" "$T/pdfbin"
expect_fail "zip with %PDF- content under a .bin name fails" "$T/pdfbin.zip" "by content"

# 4. deny string inside a binary file
stage denybin
{ head -c 3000 /dev/urandom; printf '%s' "$DENYSTR"; head -c 3000 /dev/urandom; } >"$T/denybin/blob.bin"
mkzip "$T/denybin.zip" "$T/denybin"
expect_fail "zip with deny string inside a binary file fails" "$T/denybin.zip" "blob.bin"

# 4b. different case
stage denycase
{ head -c 500 /dev/urandom; printf '%s' "${DENYSTR,,}"; head -c 500 /dev/urandom; } >"$T/denycase/blob.bin"
mkzip "$T/denycase.zip" "$T/denycase"
expect_fail "deny string in lower case (case-insensitive) fails" "$T/denycase.zip" "blob.bin"

# 4c. UTF-16 text (as in compiled Android resources)
stage deny16
{ head -c 200 /dev/urandom; printf '%s' "$DENYSTR" | iconv -f ASCII -t UTF-16LE; head -c 200 /dev/urandom; } >"$T/deny16/res.bin"
mkzip "$T/deny16.zip" "$T/deny16"
expect_fail "deny string as UTF-16 text fails" "$T/deny16.zip" "res.bin"

# 5. deny string in an entry name
stage denyname
echo "innocent content" >"$T/denyname/$DENYSTR-notes.txt"
mkzip "$T/denyname.zip" "$T/denyname"
expect_fail "zip with deny string in an entry name fails" "$T/denyname.zip" "<entry name withheld>"

# 5b. a failing entry whose name is clean is named; the secret is never printed
run "$DL" "$T/denybin.zip"
if grep -qF "blob.bin" <<<"$OUT" && ((LEAKED == 0)); then report "clean entry name is shown, secret is not" 1 ""; else report "clean entry name is shown, secret is not" 0 "bad output"; fi

# 6. missing deny-list
expect_fail "missing deny-list fails (clearly)" "$T/clean.zip" "deny-list is required" "$T/does-not-exist.txt"
# 6b. empty deny-list
: >"$T/dl_empty.txt"
expect_fail "empty deny-list fails (clearly)" "$T/clean.zip" "deny-list is required" "$T/dl_empty.txt"
printf '\n  \n\r\n' >"$T/dl_blank_only.txt"
expect_fail "blank-only deny-list fails (clearly)" "$T/clean.zip" "deny-list is required" "$T/dl_blank_only.txt"

# 7. nested archives
stage n_pdf
mkdir -p "$T/inner_pdf"
echo x >"$T/inner_pdf/doc.pdf"
mkzip "$T/n_pdf/inner.aar" "$T/inner_pdf"
mkzip "$T/n_pdf.zip" "$T/n_pdf"
expect_fail "PDF inside a nested aar fails" "$T/n_pdf.zip" "inner.aar!/doc.pdf"

stage n_deny
mkdir -p "$T/inner_deny"
{ printf 'prefix '; printf '%s' "$DENYSTR"; } >"$T/inner_deny/note.dat"
mkzip "$T/n_deny/inner.jar" "$T/inner_deny"
mkzip "$T/n_deny.zip" "$T/n_deny"
expect_fail "deny string inside a nested jar fails" "$T/n_deny.zip" "note.dat"

stage n2
mkdir -p "$T/l2" "$T/l1"
printf '%%PDF-1.4\n' >"$T/l2/hidden.bin"
mkzip "$T/l1/level2.zip" "$T/l2"
mkzip "$T/n2/level1.bin" "$T/l1"
mkzip "$T/n2.zip" "$T/n2"
expect_fail "PDF content two levels deep (zip magic under a .bin name) fails" "$T/n2.zip" "hidden.bin"

# 8. size limit and allow-list
stage big
truncate -s 21M "$T/big/huge.dat"
mkzip "$T/big.zip" "$T/big"
expect_fail "entry over 20 MB without allow-list entry fails" "$T/big.zip" "huge.dat"
printf '# test allow-list\n\n*/huge.dat\nhuge.dat\n' >"$T/allow.txt"
OUT=$(GL_ALLOWLIST="$T/allow.txt" GL_DENYLIST="$DL" "$LC" "$T/big.zip" 2>&1)
RC=$?
if ((RC == 0)) && [[ $OUT == "LEAKCHECK PASS" ]]; then report "entry over 20 MB passes when matched by the allow-list" 1 ""; else report "entry over 20 MB passes when matched by the allow-list" 0 "rc=$RC"; fi

stage big_default
mkdir -p "$T/big_default/lib/arm64-v8a"
truncate -s 21M "$T/big_default/lib/arm64-v8a/libapp_lib.so"
mkzip "$T/big_default.zip" "$T/big_default"
expect_pass "default allow-list lets lib/arm64-v8a/libapp_lib.so through" "$T/big_default.zip"

stage big_pdf
truncate -s 21M "$T/big_pdf/huge.pdf"
mkzip "$T/big_pdf.zip" "$T/big_pdf"
OUT=$(GL_ALLOWLIST="$T/allow.txt" GL_DENYLIST="$DL" "$LC" "$T/big_pdf.zip" 2>&1)
RC=$?
if ((RC == 1)) && grep -q "PDF" <<<"$OUT"; then report "allow-list never excuses a PDF" 1 ""; else report "allow-list never excuses a PDF" 0 "rc=$RC"; fi

# 9. directory mode and bad targets
expect_pass "clean directory passes" "$T/clean"
expect_fail "directory with planted PDF fails" "$T/pdf" "PDF"
expect_fail "directory with deny string in a file name fails" "$T/denyname" "<entry name withheld>"
echo "this is not a zip" >"$T/garbage.zip"
expect_fail "corrupt archive fails (cannot be inspected)" "$T/garbage.zip" "cannot"
expect_fail "missing target fails" "$T/no-such-file.zip" "not found"

# 10. git history (bundle): a PDF or secret that was committed and later removed
git_commit() { git -c user.name=t -c user.email=t@example.invalid -c commit.gpgsign=false "$@"; }
mkrepo() {
    rm -rf "${T:?}/$1"
    mkdir -p "$T/$1"
    git -C "$T/$1" init -q
    echo "hello" >"$T/$1/a.txt"
    git -C "$T/$1" add a.txt
    git_commit -C "$T/$1" commit -q -m "first"
}

mkrepo repo_pdf
echo "x" >"$T/repo_pdf/old.pdf"
git -C "$T/repo_pdf" add old.pdf
git_commit -C "$T/repo_pdf" commit -q -m "add pdf"
git -C "$T/repo_pdf" rm -q old.pdf
git_commit -C "$T/repo_pdf" commit -q -m "remove pdf"
rm -f "$T/bk_pdf.zip"
OUT=$(GL_REPO="$T/repo_pdf" GL_DENYLIST="$DL" "$BK" "$T/bk_pdf.zip" t 2>&1)
RC=$?
if ((RC != 0)) && [[ ! -e $T/bk_pdf.zip ]] && grep -q "LEAKCHECK FAIL" <<<"$OUT"; then
    report "backup.sh fails and deletes output when history ever held a PDF" 1 ""
else
    report "backup.sh fails and deletes output when history ever held a PDF" 0 "rc=$RC exists=$([[ -e $T/bk_pdf.zip ]] && echo yes || echo no)"
fi

mkrepo repo_deny
printf 'contains %s here\n' "$DENYSTR" >"$T/repo_deny/note.txt"
git -C "$T/repo_deny" add note.txt
git_commit -C "$T/repo_deny" commit -q -m "add note"
git -C "$T/repo_deny" rm -q note.txt
git_commit -C "$T/repo_deny" commit -q -m "remove note"
rm -f "$T/bk_deny.zip"
OUT=$(GL_REPO="$T/repo_deny" GL_DENYLIST="$DL" "$BK" "$T/bk_deny.zip" t 2>&1)
RC=$?
LEAKED=0
grep -qiF -e "$DENYSTR" <<<"$OUT" && LEAKED=1
if ((RC != 0)) && [[ ! -e $T/bk_deny.zip ]] && ((LEAKED == 0)); then
    report "backup.sh fails and deletes output when history ever held a secret (compressed in git objects)" 1 ""
else
    report "backup.sh fails and deletes output when history ever held a secret (compressed in git objects)" 0 "rc=$RC leaked=$LEAKED"
fi

# 11. backup of a small clean repo: untracked files stay out, restore works
mkrepo repo_ok
mkdir -p "$T/repo_ok/corpus"
echo "untracked test data" >"$T/repo_ok/corpus/untracked-test-file.txt"
rm -f "$T/bk_ok.zip"
OUT=$(GL_REPO="$T/repo_ok" GL_DENYLIST="$DL" "$BK" "$T/bk_ok.zip" ok 2>&1)
RC=$?
if ((RC == 0)) && [[ -s $T/bk_ok.zip ]]; then
    mkdir -p "$T/bk_ok_x"
    unzip -qq -o -d "$T/bk_ok_x" "$T/bk_ok.zip"
    inner=$(find "$T/bk_ok_x" -name 'greenline-android-HEAD.zip' | head -n 1)
    mkdir -p "$T/bk_ok_head"
    unzip -qq -o -d "$T/bk_ok_head" "$inner"
    if [[ -f $T/bk_ok_head/a.txt && ! -e $T/bk_ok_head/corpus ]] &&
        ! find "$T/bk_ok_x" -name 'untracked-test-file*' | grep -q .; then
        report "backup.sh (temp repo): committed files in, untracked files out" 1 ""
    else
        report "backup.sh (temp repo): committed files in, untracked files out" 0 "wrong contents"
    fi
else
    report "backup.sh (temp repo): committed files in, untracked files out" 0 "rc=$RC"
fi

# 12. backup.sh on the real repo passes leakcheck, and the bundle restores
rm -f "$T/bk_repo.zip"
OUT=$(GL_DENYLIST="$DL" "$BK" "$T/bk_repo.zip" selftest 2>&1)
RC=$?
if ((RC == 0)) && [[ -s $T/bk_repo.zip ]]; then
    report "backup.sh on the repo produces a zip" 1 ""
    run "$DL" "$T/bk_repo.zip"
    if ((RC == 0)) && [[ $OUT == "LEAKCHECK PASS" ]]; then report "leakcheck passes on the repo backup zip" 1 ""; else report "leakcheck passes on the repo backup zip" 0 "rc=$RC"; fi
    mkdir -p "$T/bk_repo_x"
    unzip -qq -o -d "$T/bk_repo_x" "$T/bk_repo.zip"
    bundle=$(find "$T/bk_repo_x" -name 'greenline-android.bundle' | head -n 1)
    have=$(find "$T/bk_repo_x" -name RESTORE.txt | head -n 1)
    if [[ -n $bundle && -n $have ]] && grep -q 'git clone greenline-android.bundle greenline-android' "$have" &&
        (cd "$T/bk_repo_x" && git clone -q "$bundle" restored >/dev/null 2>&1 && git -C restored log --oneline -1 >/dev/null 2>&1); then
        report "repo backup restores with: git clone <bundle> greenline-android" 1 ""
    else
        OUT=""
        report "repo backup restores with: git clone <bundle> greenline-android" 0 "restore failed"
    fi
else
    report "backup.sh on the repo produces a zip" 0 "rc=$RC"
fi

# 13. backup.sh without a deny-list: fails and leaves no output
rm -f "$T/bk_nodl.zip"
OUT=$(GL_REPO="$T/repo_ok" GL_DENYLIST="$T/nope.txt" "$BK" "$T/bk_nodl.zip" 2>&1)
RC=$?
if ((RC != 0)) && [[ ! -e $T/bk_nodl.zip ]]; then
    report "backup.sh without a deny-list fails and deletes output" 1 ""
else
    report "backup.sh without a deny-list fails and deletes output" 0 "rc=$RC"
fi

# 14. temp files cleaned up by leakcheck
before=$(find "${TMPDIR:-/tmp}" -maxdepth 1 -name 'tmp.*' 2>/dev/null | wc -l)
GL_DENYLIST="$DL" "$LC" "$T/n2.zip" >/dev/null 2>&1
GL_DENYLIST="$DL" "$LC" "$T/clean.zip" >/dev/null 2>&1
after=$(find "${TMPDIR:-/tmp}" -maxdepth 1 -name 'tmp.*' 2>/dev/null | wc -l)
if ((after <= before)); then report "leakcheck leaves no temp folders behind" 1 ""; else OUT=""; report "leakcheck leaves no temp folders behind" 0 "before=$before after=$after"; fi

echo
echo "Result: $NP passed, $NF failed"
((NF == 0)) || exit 1
exit 0
