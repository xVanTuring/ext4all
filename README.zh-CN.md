# ext4all

[English](README.md) | 简体中文

在 macOS 和 Android 上读写 Linux ext4 磁盘，共用一套纯 Rust 的 ext4 实现。

| 目录 | 内容 |
|---|---|
| [`ext4-core/`](ext4-core) | 与平台无关的 Rust：`ext4-core`（文件系统本体、jbd2 日志、格式化、fscrypt、LUKS）、`ext4-tool`（读写镜像文件的命令行工具）、`part`（GPT/MBR 分区表） |
| [`ext4mac/`](ext4mac) | macOS 版 Ext4Kit：FSKit 扩展、宿主 App、`ext4-ffi`（给 Swift 用的 C ABI） |
| [`ext4android/`](ext4android) | Android 版 ext4android：不需要 root 读写 USB 磁盘，通过存储访问框架开放给其他 App；`ext4-jni`、`usb-msc` |

所有 Rust crate 在仓库根目录组成一个 Cargo workspace：

```bash
cargo test --workspace      # ext4-core 的测试需要 e2fsprogs（mke2fs、e2fsck、debugfs）
```

各平台目录有自己的 README，写有构建步骤。

## 许可证

GPL-3.0-or-later，见 [LICENSE](LICENSE)。
