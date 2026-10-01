#!/bin/bash
# Build the signed Debug app, install it to /Applications and keep only that
# copy registered with the system (Xcode registers every build product, which
# shows up as duplicate entries in System Settings).
#
# Requires the developer account in Xcode (see README). Usage:
#   scripts/install-dev.sh
# Compatible with the system bash 3.2.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LSREG=/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister
APP_DIR=build/signed/Build/Products/Debug

cd "$ROOT/macos"
xcodegen generate >/dev/null
xcodebuild -project Ext4Kit.xcodeproj -scheme Ext4Kit -configuration Debug \
    -derivedDataPath build/signed -allowProvisioningUpdates -allowProvisioningDeviceRegistration \
    -quiet build
codesign --verify --deep --strict "$APP_DIR/Ext4Kit.app"

# Running extension instances keep the old code; stop them unless a volume
# is mounted (that would cut it off).
if mount | grep -q "(ext4,"; then
    echo "note: an ext4 volume is mounted; unmount it and run this again to use the new build" >&2
else
    pkill -9 -x Ext4FS 2>/dev/null || true
fi

rm -rf /Applications/Ext4Kit.app
ditto "$APP_DIR/Ext4Kit.app" /Applications/Ext4Kit.app

for d in build/dd/Build/Products/Debug build/dd/Build/Products/Release "$APP_DIR"; do
    if [ -d "$d/Ext4Kit.app" ]; then
        "$LSREG" -u "$ROOT/macos/$d/Ext4Kit.app" 2>/dev/null || true
    fi
done
"$LSREG" -f -R /Applications/Ext4Kit.app
echo "installed /Applications/Ext4Kit.app"
