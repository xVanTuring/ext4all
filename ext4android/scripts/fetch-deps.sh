#!/bin/bash
# Download the project's dependencies. Run by hand, again after a
# dependency change; builds can then run offline.
#
#   - Rust crates (cargo fetch)
#   - the Gradle version of the wrapper, the Android Gradle plugin, Kotlin
#     and the AndroidX libraries: a debug build downloads all of them,
#     including what AGP and Kotlin fetch only when a task first runs
#     (aapt2, the Kotlin and Compose compilers)
#
# If the build fails at compiling after the downloads finished, the
# dependencies are in place all the same.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

echo "==> Rust crates (the whole ext4all workspace)"
(cd "$ROOT/.." && cargo fetch)

echo "==> Gradle, Android Gradle plugin and libraries"
cd "$ROOT/android"
./gradlew :app:assembleDebug
