#!/bin/bash
# Install an Android 16 system image and create the emulator used to test
# USB disks without a phone. Run by hand, once.
#
#   - system image android-36, Google APIs, arm64-v8a (about 1.5 GB)
#   - AVD "ext4android" (Pixel 8 profile) using it
#
# scripts/emulator.sh starts it with a disk image plugged in over USB.
set -euo pipefail

IMAGE="system-images;android-36;google_apis;arm64-v8a"
AVD=ext4android
DEVICE=pixel_8

SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
if [ -z "${JAVA_HOME:-}" ] && [ -d "/Applications/Android Studio.app/Contents/jbr/Contents/Home" ]; then
    export JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home"
fi
SDKMANAGER="$SDK/cmdline-tools/latest/bin/sdkmanager"
AVDMANAGER="$SDK/cmdline-tools/latest/bin/avdmanager"
if [ ! -x "$SDKMANAGER" ] || [ ! -x "$SDK/emulator/emulator" ]; then
    echo "sdkmanager or the emulator is missing under $SDK (see scripts/setup-toolchain.sh)." >&2
    exit 1
fi

echo "==> $IMAGE"
# sdkmanager asks you to accept the licenses it has not seen yet
"$SDKMANAGER" --install "$IMAGE"

echo "==> AVD $AVD"
if "$SDK/emulator/emulator" -list-avds | grep -qx "$AVD"; then
    echo "already exists"
else
    # "no": do not create a custom hardware profile
    echo no | "$AVDMANAGER" create avd -n "$AVD" -k "$IMAGE" -d "$DEVICE"
fi

echo
echo "Done. Start it with a USB disk: scripts/emulator.sh <disk image>"
