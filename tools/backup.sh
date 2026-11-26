#!/usr/bin/env bash
# backup.sh - build a backup zip of the repo from git only (never from the working folder).
#
# Usage: tools/backup.sh <out.zip> [label]
#
# The zip holds:
#   greenline-android.bundle        git bundle of every branch and tag (full history)
#   greenline-android-HEAD.zip      git archive of the committed files at HEAD
#   RESTORE.txt                     plain-English restore steps
# Only committed work is backed up, so untracked files (corpus/, test PDFs, keys) cannot slip in.
# The result is checked with tools/leakcheck.sh; if the check fails the output is deleted.
#
# Optional env: GL_REPO (repo to back up; default: the repo this script lives in),
#               GL_DENYLIST (see leakcheck.sh).

set -u
umask 077

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=${GL_REPO:-$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)}
WORK=""
OUT=""
DONE=0

cleanup() {
    [[ -n $WORK && -d $WORK ]] && rm -rf "$WORK"
    # An output that was never finished and checked must not be left behind.
    if ((DONE == 0)) && [[ -n $OUT && -e $OUT ]]; then rm -f "$OUT"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

err() {
    echo "backup.sh: $1" >&2
    exit 1
}

(($# >= 1 && $# <= 2)) || err "usage: tools/backup.sh <out.zip> [label]"
[[ -n $REPO && -d $REPO ]] || err "cannot find the git repo"
OUTARG=$1
LABEL=${2:-backup}
LABEL=${LABEL//[^A-Za-z0-9._-]/_}

mkdir -p "$(dirname "$OUTARG")" || err "cannot create the output folder"
OUT="$(cd "$(dirname "$OUTARG")" && pwd)/$(basename "$OUTARG")"
[[ -e $OUT ]] && { OUT=""; err "output already exists, refusing to overwrite: $OUTARG"; }

git -C "$REPO" rev-parse --verify -q HEAD >/dev/null || err "the repo has no commits yet"
WORK=$(mktemp -d) || err "cannot create a temp folder"
STAGE="$WORK/stage"
mkdir "$STAGE"

SHA=$(git -C "$REPO" rev-parse HEAD)
BRANCH=$(git -C "$REPO" symbolic-ref -q --short HEAD || echo "(detached)")
NOW=$(date '+%Y-%m-%d %H:%M:%S %z')

git -C "$REPO" bundle create "$STAGE/greenline-android.bundle" --all >/dev/null 2>&1 \
    || err "git bundle failed"
git -C "$REPO" bundle verify "$STAGE/greenline-android.bundle" >/dev/null 2>&1 \
    || err "git bundle verify failed"
git -C "$REPO" archive --format=zip -o "$STAGE/greenline-android-HEAD.zip" HEAD \
    || err "git archive failed"

cat >"$STAGE/RESTORE.txt" <<EOF
GREENLINE ANDROID - BACKUP
==========================
Label:   $LABEL
Made:    $NOW
Branch:  $BRANCH
Commit:  $SHA

What is in this zip
-------------------
greenline-android.bundle      The whole git history (every branch and tag) in one file.
greenline-android-HEAD.zip    A plain snapshot of the files as committed at the commit above.
RESTORE.txt                   This page.

How to restore (needs git)
--------------------------
1. Unzip this backup anywhere (for example into a new empty folder).
2. Open a terminal in that folder.
3. Run:   git clone greenline-android.bundle greenline-android
   This creates a normal project folder called greenline-android with the full history.
   The branch above is checked out. Other branches show up as origin/<name>;
   "git switch <name>" makes a local copy of one.
4. Optional: "git remote remove origin" inside the new folder, so it does not point at the bundle.
5. To check the bundle first: git bundle verify greenline-android.bundle

If git is not available, unzip greenline-android-HEAD.zip instead. It has the files
but no history.

What is NOT in this backup
--------------------------
Only committed work is saved. Uncommitted edits, untracked files, the test PDF corpus,
signing keys and the deny-list are deliberately left out. Keep those separately.
EOF

python3 -I - "$OUT" "$STAGE" "greenline-android-backup-$LABEL" <<'PY' || err "could not build the zip"
import os, sys, zipfile
out, stage, top = sys.argv[1:4]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    for name in ("RESTORE.txt", "greenline-android.bundle", "greenline-android-HEAD.zip"):
        z.write(os.path.join(stage, name), top + "/" + name)
PY

"$SCRIPT_DIR/leakcheck.sh" "$OUT"
rc=$?
if ((rc != 0)); then
    rm -f "$OUT"
    DONE=1
    echo "backup.sh: leak check failed, the backup was deleted: $OUTARG" >&2
    exit 1
fi
DONE=1
echo "backup.sh: wrote $OUT"
exit 0
