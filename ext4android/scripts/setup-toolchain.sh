#!/bin/bash
# Install the build tools for ext4android. Run by hand, once.
#
#   - Rust targets for Android (arm64 devices, x86_64 emulator)
#   - cargo-ndk
#   - Android SDK platform android-37.2 (the compileSdk), build tools (36 or
#     newer) and an NDK: what Android Studio already installed is used; only
#     missing parts are installed with its sdkmanager
#   - Gradle, only to generate the Gradle wrapper of the project
#
# Nothing in your shell profile is changed; the variables to add are
# printed at the end.
set -euo pipefail

# must match compileSdk in android/app/build.gradle.kts
PLATFORM_API=37.2
MIN_BUILD_TOOLS=36
DEFAULT_BUILD_TOOLS="build-tools;36.0.0"

# Newest version ("37", "36.1", "28.2.13676358") read from stdin;
# previews such as "37.0.0-rc1" are skipped.
newest() {
    grep -E '^[0-9]+(\.[0-9]+)*$' | sort -u -t. -k1,1n -k2,2n -k3,3n | tail -1 || true
}

# Names of the subdirectories of $1 (nothing if it does not exist).
subdirs() {
    for d in "$1"/*; do
        if [ -d "$d" ]; then basename "$d"; fi
    done
}

# Installed NDK versions (directories with a source.properties).
installed_ndks() {
    for d in "$SDK"/ndk/*; do
        if [ -f "$d/source.properties" ]; then basename "$d"; fi
    done
}

echo "==> Rust targets"
rustup target add aarch64-linux-android x86_64-linux-android

echo "==> cargo-ndk"
if command -v cargo-ndk >/dev/null 2>&1; then
    echo "already installed: $(cargo ndk --version 2>/dev/null || echo cargo-ndk)"
else
    cargo install cargo-ndk --locked
fi

echo "==> Android SDK"
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
if [ ! -d "$SDK" ]; then
    echo "Android SDK not found at $SDK." >&2
    echo "Open Android Studio once (it installs the SDK there), or set ANDROID_HOME, then run this again." >&2
    exit 1
fi
echo "SDK: $SDK"
if [ -z "${JAVA_HOME:-}" ] && [ -d "/Applications/Android Studio.app/Contents/jbr/Contents/Home" ]; then
    export JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home"
fi
SDKMANAGER="$SDK/cmdline-tools/latest/bin/sdkmanager"

MISSING=()

if [ -d "$SDK/platforms/android-$PLATFORM_API" ]; then
    echo "platform: android-$PLATFORM_API (installed)"
else
    MISSING+=("platforms;android-$PLATFORM_API")
fi

BUILD_TOOLS=$(subdirs "$SDK/build-tools" | newest)
if [ -n "$BUILD_TOOLS" ] && [ "${BUILD_TOOLS%%.*}" -ge "$MIN_BUILD_TOOLS" ]; then
    echo "build tools: $BUILD_TOOLS (installed)"
else
    MISSING+=("$DEFAULT_BUILD_TOOLS")
    BUILD_TOOLS=${DEFAULT_BUILD_TOOLS#build-tools;}
fi

NDK_VERSION=$(installed_ndks | newest)
if [ -n "$NDK_VERSION" ]; then
    echo "NDK: $NDK_VERSION (installed)"
fi

if [ "${#MISSING[@]}" -gt 0 ] || [ -z "$NDK_VERSION" ]; then
    if [ ! -x "$SDKMANAGER" ]; then
        echo "sdkmanager not found at $SDKMANAGER." >&2
        echo "In Android Studio: Settings > Languages & Frameworks > Android SDK > SDK Tools," >&2
        echo "check \"Android SDK Command-line Tools (latest)\", apply, then run this again." >&2
        exit 1
    fi
    if [ -z "$NDK_VERSION" ]; then
        # newest stable NDK on offer (previews carry a suffix)
        LIST=$("$SDKMANAGER" --list)
        NDK_VERSION=$(printf '%s\n' "$LIST" | awk '{print $1}' | sed -n 's/^ndk;//p' | newest)
        if [ -z "$NDK_VERSION" ]; then
            echo "Could not find an NDK in 'sdkmanager --list'. Lines mentioning ndk:" >&2
            printf '%s\n' "$LIST" | grep -i ndk | head -20 >&2 || true
            exit 1
        fi
        MISSING+=("ndk;$NDK_VERSION")
    fi
    echo "installing: ${MISSING[*]}"
    # sdkmanager asks you to accept the licenses it has not seen yet
    "$SDKMANAGER" --install "${MISSING[@]}"
fi

echo "==> Gradle (to generate the project's Gradle wrapper)"
if command -v gradle >/dev/null 2>&1; then
    echo "already installed: $(command -v gradle)"
else
    brew install gradle
fi

cat <<EOF

Done: platform android-$PLATFORM_API, build tools $BUILD_TOOLS, NDK $NDK_VERSION.

Add these lines to ~/.zshrc (cargo-ndk and Gradle read them):

  export ANDROID_HOME="$SDK"
  export ANDROID_NDK_HOME="\$ANDROID_HOME/ndk/$NDK_VERSION"
EOF
