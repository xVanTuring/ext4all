#!/bin/bash
# Build, Developer-ID sign, notarize and package Ext4Kit.app.
#
# One-time setup (by you):
#   1. Sign in to Xcode with the developer account (team T8F5T6HKG8) so the
#      FSKit Module capability can be provisioned.
#   2. On the developer website, create two Developer ID provisioning profiles
#      on the Developer ID Application certificate whose private key is in
#      this Mac's keychain, and install them:
#        "Ext4Kit"   for tech.xvanturing.ext4
#        "Ext4KitFS" for tech.xvanturing.ext4.fs (with FSKit Module)
#      (Export uses manual signing: the account has no cloud signing access.)
#   3. Store notarization credentials in the keychain:
#        xcrun notarytool store-credentials notary \
#            --apple-id YOUR_APPLE_ID --team-id T8F5T6HKG8 --password APP_SPECIFIC_PASSWORD
#
# Usage: scripts/release.sh [VERSION]   (e.g. 0.2.0; default: project version)
# Bash 3.2 compatible.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MACOS="$ROOT/macos"
BUILD="$MACOS/build/release"
TEAM="T8F5T6HKG8"
PROFILE="${NOTARY_PROFILE:-notary}"
APP_PROFILE="${APP_PROFILE:-Ext4Kit}"
FS_PROFILE="${FS_PROFILE:-Ext4KitFS}"
VERSION="${1:-}"

cd "$MACOS"
command -v xcodegen >/dev/null || { echo "error: xcodegen not found" >&2; exit 1; }
xcodegen generate

VERSION_ARGS=()
if [ -n "$VERSION" ]; then
    VERSION_ARGS=("MARKETING_VERSION=$VERSION")
fi

rm -rf "$BUILD"
mkdir -p "$BUILD"

# Run the whole test suite first; never ship a failing build.
(cd "$ROOT" && cargo test --workspace --release)
xcodebuild -project Ext4Kit.xcodeproj -scheme Ext4KitTests -derivedDataPath "$BUILD/dd" test

xcodebuild -project Ext4Kit.xcodeproj -scheme Ext4Kit -configuration Release \
    -derivedDataPath "$BUILD/dd" -archivePath "$BUILD/Ext4Kit.xcarchive" \
    -allowProvisioningUpdates "${VERSION_ARGS[@]+"${VERSION_ARGS[@]}"}" archive

cat > "$BUILD/ExportOptions.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>method</key>
    <string>developer-id</string>
    <key>teamID</key>
    <string>$TEAM</string>
    <key>signingStyle</key>
    <string>manual</string>
    <key>signingCertificate</key>
    <string>Developer ID Application</string>
    <key>provisioningProfiles</key>
    <dict>
        <key>tech.xvanturing.ext4</key>
        <string>$APP_PROFILE</string>
        <key>tech.xvanturing.ext4.fs</key>
        <string>$FS_PROFILE</string>
    </dict>
</dict>
</plist>
EOF

xcodebuild -exportArchive -archivePath "$BUILD/Ext4Kit.xcarchive" \
    -exportOptionsPlist "$BUILD/ExportOptions.plist" -exportPath "$BUILD/export"

APP="$BUILD/export/Ext4Kit.app"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -d --entitlements - "$APP/Contents/Extensions/Ext4FS.appex" 2>/dev/null | grep -q fskit.fsmodule \
    || { echo "error: extension lacks the FSKit entitlement" >&2; exit 1; }

ZIP="$BUILD/Ext4Kit.zip"
ditto -c -k --keepParent "$APP" "$ZIP"
xcrun notarytool submit "$ZIP" --keychain-profile "$PROFILE" --wait
xcrun stapler staple "$APP"
spctl --assess --type execute --verbose "$APP"

FINAL_VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")"
OUT="$BUILD/Ext4Kit-$FINAL_VERSION.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$OUT"
echo "release ready: $OUT"
