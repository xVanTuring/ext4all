#!/bin/bash
# Build libext4android.so into android/app/src/main/jniLibs with cargo-ndk.
#
#   scripts/build-rust.sh              arm64-v8a (devices) and x86_64 (emulator)
#   scripts/build-rust.sh arm64-v8a    only the listed ABIs
#
# Gradle runs this before every build. Android Studio does not read the
# shell profile, so the tool paths are added here and Gradle passes
# ANDROID_NDK_HOME.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# the Cargo workspace is the ext4all root
WORKSPACE="$(cd "$ROOT/.." && pwd)"
OUT="$ROOT/android/app/src/main/jniLibs"
MIN_API=30

export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    echo "ANDROID_NDK_HOME is not set (see scripts/setup-toolchain.sh)." >&2
    exit 1
fi

ABIS=("$@")
if [ "${#ABIS[@]}" -eq 0 ]; then
    ABIS=(arm64-v8a x86_64)
fi
TARGETS=()
for abi in "${ABIS[@]}"; do
    TARGETS+=(-t "$abi")
done

cd "$WORKSPACE"
cargo ndk "${TARGETS[@]}" --platform "$MIN_API" -o "$OUT" build --release -p ext4-jni
