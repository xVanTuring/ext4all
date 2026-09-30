#!/bin/bash
# Create a set of ext2/3/4 test images covering different feature
# combinations. Usage: scripts/make-test-images.sh [OUTPUT_DIR] [SIZE_MB]
# Compatible with the system bash 3.2.
set -euo pipefail

OUT="${1:-$(pwd)/test-images}"
SIZE_MB="${2:-256}"
SBIN="${E2FSPROGS_SBIN:-/opt/homebrew/opt/e2fsprogs/sbin}"
MKE2FS="$SBIN/mke2fs"

if [ ! -x "$MKE2FS" ]; then
    echo "error: $MKE2FS not found (brew install e2fsprogs, or set E2FSPROGS_SBIN)" >&2
    exit 1
fi

mkdir -p "$OUT"

# sample content copied into every image
SRC="$(mktemp -d)"
trap 'rm -rf "$SRC"' EXIT
mkdir -p "$SRC/docs/nested/deeper" "$SRC/many"
printf 'hello ext4\n' > "$SRC/hello.txt"
printf '你好\n' > "$SRC/中文.txt"
dd if=/dev/urandom of="$SRC/random.bin" bs=1024 count=3000 2>/dev/null
dd if=/dev/zero of="$SRC/zeros.bin" bs=1024 count=100 2>/dev/null
printf 'deep\n' > "$SRC/docs/nested/deeper/file"
ln -s hello.txt "$SRC/link-to-hello"
i=0
while [ $i -lt 300 ]; do
    printf '%s\n' "$i" > "$SRC/many/file-$i"
    i=$((i + 1))
done

make_image() {
    name="$1"
    shift
    img="$OUT/$name.img"
    rm -f "$img"
    mkfile -n "${SIZE_MB}m" "$img"
    "$MKE2FS" -F -q -L "$name" -d "$SRC" "$@" "$img"
    echo "created $img ($*)"
}

make_image ext4-default -t ext4
make_image ext4-1k -t ext4 -b 1024
make_image ext4-nojournal -t ext4 -O ^has_journal
make_image ext4-nocsum -t ext4 -O ^metadata_csum,^metadata_csum_seed
make_image ext4-no64bit -t ext4 -O ^64bit,^flex_bg
make_image ext4-inline -t ext4 -O inline_data
make_image ext4-metabg -t ext4 -O meta_bg,^resize_inode
make_image ext3 -t ext3
make_image ext2 -t ext2

echo "done: $OUT"
