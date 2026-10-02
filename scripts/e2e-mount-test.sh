#!/bin/bash
# End-to-end test of the installed FSKit extension: build ext4 images,
# attach them as block devices, mount them through FSKit, exercise the file
# system with standard tools, unmount, and verify with e2fsck.
#
# Prerequisites: Ext4Kit.app installed (signed) and the "Ext4Kit" file
# system extension enabled in System Settings. Usage:
#   scripts/e2e-mount-test.sh [PROFILE...]   (default: all profiles)
# Compatible with the system bash 3.2.
set -euo pipefail

SBIN="${E2FSPROGS_SBIN:-/opt/homebrew/opt/e2fsprogs/sbin}"
WORK="$(mktemp -d /tmp/ext4-e2e.XXXXXX)"
PASS=0
FAIL=0

cleanup() {
    if [ -n "${MNT:-}" ] && mount | grep -q " $MNT "; then
        umount "$MNT" 2>/dev/null || diskutil unmount force "$MNT" >/dev/null 2>&1 || true
    fi
    if [ -n "${DEV:-}" ]; then
        hdiutil detach "$DEV" >/dev/null 2>&1 || hdiutil detach -force "$DEV" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

profile_args() {
    case "$1" in
        default) echo "-t ext4" ;;
        1k) echo "-t ext4 -b 1024" ;;
        nojournal) echo "-t ext4 -O ^has_journal" ;;
        nocsum) echo "-t ext4 -O ^metadata_csum,^metadata_csum_seed" ;;
        inline) echo "-t ext4 -O inline_data" ;;
        *) echo "unknown profile $1" >&2; return 1 ;;
    esac
}

check() {
    # check "description" command...
    desc="$1"
    shift
    if "$@"; then
        echo "  ok   $desc"
    else
        echo "  FAIL $desc"
        return 1
    fi
}

run_profile() {
    profile="$1"
    img="$WORK/$profile.img"
    echo "== profile $profile"
    mkfile -n 512m "$img"
    # shellcheck disable=SC2046
    "$SBIN/mke2fs" -F -q -L "e2e$profile" $(profile_args "$profile") "$img"

    DEV="$(hdiutil attach -nomount -imagekey diskimage-class=CRawDiskImage "$img" | awk 'NR==1{print $1}')"
    MNT="$WORK/mnt-$profile"
    mkdir -p "$MNT"
    mount -F -t ext4 "$DEV" "$MNT"

    src="$WORK/src"
    rm -rf "$src"
    mkdir -p "$src/tree/a/b/c"
    dd if=/dev/urandom of="$src/big.bin" bs=1m count=64 2>/dev/null
    i=0
    while [ $i -lt 500 ]; do
        printf 'file %s\n' "$i" > "$src/tree/a/f$i"
        i=$((i + 1))
    done

    ok=0
    check "copy big file" cp "$src/big.bin" "$MNT/big.bin" || ok=1
    check "big file matches" cmp -s "$src/big.bin" "$MNT/big.bin" || ok=1
    check "copy tree" cp -R "$src/tree" "$MNT/tree" || ok=1
    check "tree matches" diff -r "$src/tree" "$MNT/tree" || ok=1
    check "rsync" rsync -a "$src/tree/" "$MNT/rsynced/" || ok=1
    check "mkdir -p" mkdir -p "$MNT/x/y/z" || ok=1
    check "symlink" ln -s ../big.bin "$MNT/x/link" || ok=1
    check "readlink" test "$(readlink "$MNT/x/link")" = "../big.bin" || ok=1
    check "hard link" ln "$MNT/big.bin" "$MNT/x/hard" || ok=1
    check "link count" test "$(stat -f %l "$MNT/big.bin")" = "2" || ok=1
    check "rename" mv "$MNT/tree/a/f1" "$MNT/x/moved" || ok=1
    check "chmod" chmod 600 "$MNT/x/moved" || ok=1
    check "mode" test "$(stat -f %Lp "$MNT/x/moved")" = "600" || ok=1
    check "xattr write" xattr -w com.example.test hello "$MNT/x/moved" || ok=1
    check "xattr read" test "$(xattr -p com.example.test "$MNT/x/moved")" = "hello" || ok=1
    # native extended attributes: copies get no AppleDouble ._ companions
    printf tagged > "$src/tagged"
    xattr -w com.example.copy yes "$src/tagged"
    check "cp keeps xattrs" sh -c "cp '$src/tagged' '$MNT/x/' && ditto '$src/tagged' '$MNT/x/ditto'" || ok=1
    check "xattr copied" test "$(xattr -p com.example.copy "$MNT/x/ditto")" = "yes" || ok=1
    check "no ._ files" test -z "$(find "$MNT" -name '._*')" || ok=1
    check "truncate" truncate -s 100 "$MNT/x/moved" 2>/dev/null || : > "$MNT/x/moved" || ok=1
    check "append" sh -c "printf more >> '$MNT/x/moved'" || ok=1
    check "delete tree" rm -rf "$MNT/rsynced" || ok=1
    check "unlink open file" sh -c "exec 3<'$MNT/x/hard'; rm '$MNT/x/hard'; cat <&3 >/dev/null" || ok=1
    check "df reports" df -k "$MNT" >/dev/null || ok=1
    check "sync" sync || ok=1

    umount "$MNT"
    hdiutil detach "$DEV" >/dev/null
    DEV=""
    MNT=""
    check "e2fsck -fn clean" "$SBIN/e2fsck" -fn "$img" >/dev/null || ok=1
    if [ $ok -eq 0 ]; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); fi
}

if [ $# -eq 0 ]; then
    set -- default 1k nojournal nocsum inline
fi
for p in "$@"; do
    run_profile "$p" || FAIL=$((FAIL + 1))
done

echo "profiles passed: $PASS, failed: $FAIL"
rm -rf "$WORK"
[ $FAIL -eq 0 ]
