#!/bin/bash
# Cross-compile the engine crate for the phone: aarch64-linux-android, API 30, release.
# Same env recipe as spike/ (tools/android-env.sh); later builds source that file too.
set -e
. "$HOME/.cargo/env"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$ROOT/tools/android-env.sh"
cd "$ROOT"
cargo build --release -p greenline-engine --target aarch64-linux-android -j 8
ls -la target/aarch64-linux-android/release/libgreenline_engine.rlib
echo ENGINE_ANDROID_BUILD_DONE
