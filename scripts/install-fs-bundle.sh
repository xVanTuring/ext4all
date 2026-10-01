#!/bin/bash
# Install (or remove with --remove) the file system description bundle
# /Library/Filesystems/ext4.fs so diskutil and Disk Utility recognize ext4
# volumes mounted by the FSKit extension (without it, `diskutil unmount`
# and `diskutil eject` refuse them). Needs root:
#   sudo scripts/install-fs-bundle.sh [--remove]
# Compatible with the system bash 3.2.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST=/Library/Filesystems/ext4.fs

if [ "$(id -u)" -ne 0 ]; then
    echo "run with sudo" >&2
    exit 1
fi

rm -rf "$DEST"
if [ "${1:-}" != "--remove" ]; then
    ditto "$ROOT/macos/Support/ext4.fs" "$DEST"
    chown -R root:wheel "$DEST"
    echo "installed $DEST"
else
    echo "removed $DEST"
fi
# storagekitd caches the file system list; launchd restarts it on demand
killall storagekitd 2>/dev/null || true
