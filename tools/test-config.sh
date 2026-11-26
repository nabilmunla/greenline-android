#!/bin/bash
# Config checks for app/src-tauri/tauri.conf.json (recent-file thumbnails are data: images).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
python3 - <<'PY'
import json, sys
csp = json.load(open("app/src-tauri/tauri.conf.json"))["app"]["security"]["csp"]
d = {}
for part in csp.split(";"):
    w = part.split()
    if w: d[w[0]] = w[1:]
fails = []
img = d.get("img-src", d.get("default-src", []))
if "data:" not in img: fails.append("img-src does not allow data: (recent-file thumbnails are blocked)")
if "'self'" not in img: fails.append("img-src does not allow 'self'")
for k, v in d.items():
    if "*" in v or "'unsafe-eval'" in v: fails.append(k + " is too open")
    if k != "img-src" and "data:" in v: fails.append(k + " allows data:")
for f in fails: print("FAIL:", f)
print("test-config:", "FAIL" if fails else "PASS")
sys.exit(1 if fails else 0)
PY
