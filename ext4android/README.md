# ext4android

在 Android 上读写通过 USB 连接的 ext4 硬盘和 U 盘，不需要 root，并通过系统的存储访问框架（SAF）开放给其他 App。

文件系统部分复用同仓库的 [`ext4-core`](../ext4-core)（纯 Rust，macOS 版 [Ext4Kit](../ext4mac) 也用它）。

状态：M0（工具链、骨架、先行实验）进行中，见 [docs/design.md](docs/design.md) 和 [TODO.md](TODO.md)。

## M0 已有结论

在小米 Pad 6 上（HyperOS 2.0，Android 14，内核 4.19；App 没有使用 root）：

- 原生库加载正常，自检（在内存里格式化、写入、重新挂载、读回）通过。
- App 能对 `UsbDeviceConnection` 的文件描述符执行 usbdevfs ioctl：在一块 RTL9210 NVMe 硬盘盒上完成了识别、读 GPT、只读挂载 ext4、列出根目录、刷新缓存。
- 系统已检测到这个盘并发出通知后，`claimInterface(intf, true)` 仍能取得接口。
- 单次 64 KiB 以上的同步传输失败，返回 ENOMEM；小块读取正常。原因和对策见 TODO。

## 构建

1. 安装构建工具，只需一次：
   ```bash
   scripts/setup-toolchain.sh
   ```
   会安装 Rust 的 Android 目标、cargo-ndk、缺少的 Android SDK 组件和 Gradle。
2. 下载依赖。第一次构建前执行，以后依赖有变动时再执行：
   ```bash
   scripts/fetch-deps.sh
   ```
   会下载 Rust crate、Gradle、Android Gradle 插件和 AndroidX 库。
3. 构建与安装（Gradle 会先调用 `scripts/build-rust.sh` 编译 `libext4android.so`）：

   ```bash
   cd android
   ./gradlew :app:assembleDebug          # 输出 app/build/outputs/apk/debug/app-debug.apk
   ./gradlew :app:installDebug           # 安装到 adb 连接的设备
   ```

只改 Rust 时也可以单独编译：`scripts/build-rust.sh arm64-v8a`。Rust 单元测试在 Mac 上、仓库根目录运行：`cargo test -p usb-msc -p part -p ext4-jni`。
