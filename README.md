# ext4all

English | [简体中文](README.zh-CN.md)

Linux ext4 disks on macOS and Android, with one ext4 implementation in pure Rust.

| Directory | Contents |
|---|---|
| [`ext4-core/`](ext4-core) | Platform-neutral Rust: `ext4-core` (the file system, jbd2 journal, mkfs, fscrypt, LUKS), `ext4-tool` (command-line tool for images), `part` (GPT/MBR partition tables) |
| [`ext4mac/`](ext4mac) | Ext4Kit for macOS: FSKit extension, host app, `ext4-ffi` (C ABI for Swift) |
| [`ext4android/`](ext4android) | ext4android: USB disks without root, shared with other apps through the Storage Access Framework; `ext4-jni`, `usb-msc` |

All Rust crates form one Cargo workspace at the repository root:

```bash
cargo test --workspace      # the ext4-core tests need e2fsprogs (mke2fs, e2fsck, debugfs)
```

Each platform directory has its own README with build steps.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE).
