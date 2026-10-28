#!/bin/bash
# The emulator of scripts/setup-emulator.sh with a disk image plugged in as
# a USB disk: QEMU's usb-storage on an xHCI controller, so the app talks to
# it through usbdevfs as to a real disk.
#
#   scripts/emulator.sh start <disk image> [--window]
#       run the emulator (writable system, no snapshots) with the image
#       plugged in, until it is stopped; QEMU's monitor listens on
#       127.0.0.1:45454
#   scripts/emulator.sh enable-usb-host
#       once, on a running emulator: the system images declare no USB host
#       support, so apps see no USB devices; this adds the feature to the
#       emulator's writable system partition and reboots
#   scripts/emulator.sh unplug       pull the disk out
#   scripts/emulator.sh plug         plug it in again
#   scripts/emulator.sh stop
#
# Changes go straight to the image file: check it on the Mac afterwards
# (e2fsck on the partition), with the disk unplugged or the emulator stopped.
set -euo pipefail

AVD=ext4android
MONITOR_PORT=45454
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
EMULATOR="$SDK/emulator/emulator"
ADB="$SDK/platform-tools/adb"
USB_DEVICE="usb-storage,bus=xhci.0,drive=usbdisk,id=usbstick"

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
}

# One command to QEMU's human monitor.
monitor() {
    printf '%s\n' "$1" | nc -w 2 127.0.0.1 "$MONITOR_PORT" >/dev/null
}

wait_boot() {
    "$ADB" wait-for-device
    until [ "$("$ADB" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = "1" ]; do
        sleep 2
    done
}

case "${1:-}" in
start)
    [ $# -ge 2 ] || usage
    IMAGE="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
    [ -f "$IMAGE" ] || { echo "no such file: $IMAGE" >&2; exit 1; }
    WINDOW=(-no-window)
    if [ "${3:-}" = "--window" ]; then
        WINDOW=()
    fi
    # -drive is opened late by the emulator's QEMU, after usb-storage needs
    # it; -blockdev nodes are there at once (and outlive device_del)
    "$EMULATOR" -avd "$AVD" -writable-system -no-snapshot -no-boot-anim -no-audio ${WINDOW[@]+"${WINDOW[@]}"} \
        -qemu \
        -device qemu-xhci,id=xhci \
        -blockdev "driver=file,node-name=usbfile,filename=$IMAGE" \
        -blockdev driver=raw,node-name=usbdisk,file=usbfile \
        -device "$USB_DEVICE" \
        -monitor "telnet:127.0.0.1:$MONITOR_PORT,server,nowait" &
    pid=$!
    wait_boot
    echo "booted, $IMAGE plugged in"
    wait "$pid"
    ;;
enable-usb-host)
    "$ADB" root >/dev/null
    "$ADB" wait-for-device
    if ! "$ADB" remount 2>&1 | grep -q "Remount succeeded"; then
        # the first time verity is turned off, which needs a reboot
        "$ADB" reboot
        wait_boot
        "$ADB" root >/dev/null
        "$ADB" wait-for-device
        "$ADB" remount
    fi
    "$ADB" shell "printf '%s\n' '<permissions>' '    <feature name=\"android.hardware.usb.host\" />' '</permissions>' \
        > /system/etc/permissions/android.hardware.usb.host.xml"
    "$ADB" reboot
    wait_boot
    "$ADB" shell pm list features | grep usb.host
    ;;
unplug)
    monitor "device_del usbstick"
    ;;
plug)
    monitor "device_add $USB_DEVICE"
    ;;
stop)
    "$ADB" emu kill
    ;;
*)
    usage
    ;;
esac
