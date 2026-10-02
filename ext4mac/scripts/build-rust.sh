#!/bin/bash
# Build the ext4-ffi static library for the FSKit extension and install it,
# together with the generated C header and a module map, into
# macos/Vendor. Called by the Xcode "Build Rust" script phase and usable by
# hand. Compatible with the system bash 3.2.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# the Cargo workspace (and its target directory) is the ext4all root
WORKSPACE="$(cd "$ROOT/.." && pwd)"
FFI="$ROOT/crates/ext4-ffi"
VENDOR="$ROOT/macos/Vendor"
TARGET="aarch64-apple-darwin"
PROFILE="${EXT4_RUST_PROFILE:-release}"

# Xcode runs build phases with a minimal PATH
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

# Xcode exports variables meant for Swift/clang that confuse cargo builds of
# host tools; the deployment target is what we actually want to pin.
unset LIBRARY_PATH CPATH || true
export MACOSX_DEPLOYMENT_TARGET="27.0"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found (install Rust from https://rustup.rs)" >&2
    exit 1
fi

cd "$WORKSPACE"
if [ "$PROFILE" = "release" ]; then
    cargo build -p ext4-ffi --release --target "$TARGET"
    OUT_DIR="$WORKSPACE/target/$TARGET/release"
else
    cargo build -p ext4-ffi --target "$TARGET"
    OUT_DIR="$WORKSPACE/target/$TARGET/debug"
fi

mkdir -p "$VENDOR/lib" "$VENDOR/include"

if command -v cbindgen >/dev/null 2>&1; then
    cbindgen --quiet --config "$FFI/cbindgen.toml" --crate ext4-ffi \
        --output "$FFI/include/ext4_ffi.h"
else
    echo "warning: cbindgen not found; using the checked-in header" >&2
fi

cp -f "$OUT_DIR/libext4_ffi.a" "$VENDOR/lib/libext4_ffi.a"
cp -f "$FFI/include/ext4_ffi.h" "$VENDOR/include/ext4_ffi.h"
cat > "$VENDOR/include/module.modulemap" <<'EOF'
module Ext4FFI {
    header "ext4_ffi.h"
    link "ext4_ffi"
    export *
}
EOF

echo "ext4-ffi ($PROFILE) installed into $VENDOR"
