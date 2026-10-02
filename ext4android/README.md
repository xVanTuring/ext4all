# ext4android

在 Android 上读写通过 USB 连接的 ext4 硬盘和 U 盘，不需要 root，并通过系统的存储访问框架（SAF）开放给其他 App。

文件系统部分复用同仓库的 [`ext4-core`](../ext4-core)（纯 Rust，macOS 版 [Ext4Kit](../ext4mac) 也用它）。

状态：M0 只差代理文件描述符实验的真机测试；M1（镜像文件、只读的文档提供者）代码已完成，等真机测试。见 [docs/design.md](docs/design.md) 和 [TODO.md](TODO.md)。

## 已完成的部分

- `crates/usb-msc`：USB 大容量存储（Bulk-Only 传输 + SCSI），经 usbdevfs 收发；ENOMEM 时自动减小单次传输。用模拟传输和模拟磁盘做单元测试。
- `../ext4-core/crates/part`：GPT（主表头损坏时用备份）、MBR（含逻辑分区）、无分区表。
- `crates/ext4-jni`：
  - 自检和 USB 检测（只读）；
  - 卷管理：挂载镜像文件（文件系统镜像，或带分区表的磁盘镜像），用编号登记，挂载后的卷用核心的 `SharedFs`；
  - 文档层：路径编码（非 UTF-8 的字节、`%` 和控制字符写成 `%XX`）、列目录、属性、读文件；只跟随卷内的相对符号链接（最多 40 层），断开的、指向绝对路径的、循环的链接和设备文件、FIFO、socket 不显示；
  - 调试用的测试镜像：各种文件名、链接和一个大文件 `big.bin`；把手机上的文件导入镜像。
- App：
  - `Ext4DocumentsProvider`：每个挂载的卷是一个 root（ID 为文件系统 UUID），文档 ID 为 `<root>:<编码路径>`；只读打开时返回代理文件描述符（`openProxyFileDescriptor`），每个打开的文件一个线程；
  - 调试界面：创建、导入、挂载测试镜像；从系统文件选择器选文件交给其他 App 打开；通过文档提供者测顺序读和随机读的速度，并核对数据。

## M0 已有结论

在 iQOO 15 上（未 root，OriginOS 6，Android 16，内核 6.12）：

- 自检通过；App 能对 `UsbDeviceConnection` 的文件描述符执行 usbdevfs ioctl，`claimInterface(intf, true)` 能取得接口。在同一块 RTL9210 NVMe 硬盘盒上完成了识别、读 GPT、只读挂载 ext4、列出根目录、刷新缓存。
- 16 到 128 KiB 的同步传输都没有出现 ENOMEM；传输越大越快，128 KiB 时约 230–260 MB/s，64 KiB 时 170–300 MB/s。
- 检测时没有出现 USB 权限弹窗（小米 Pad 6 上也没有），和原版 Android 的行为不同，做正式插盘流程时再确认。

在小米 Pad 6 上（HyperOS 2.0，Android 14，内核 4.19；App 没有使用 root）：

- 原生库加载正常，自检（在内存里格式化、写入、重新挂载、读回）通过。
- App 能对 `UsbDeviceConnection` 的文件描述符执行 usbdevfs ioctl：在一块 RTL9210 NVMe 硬盘盒上完成了识别、读 GPT、只读挂载 ext4、列出根目录、刷新缓存。
- 系统已检测到这个盘并发出通知后，`claimInterface(intf, true)` 仍能取得接口。
- 同步传输越大，越容易返回 ENOMEM（内核每次要为传输申请一块连续内存）：每种大小测 3 次，128 KiB 每次都失败，64 KiB 失败 1 次，16、32 KiB 没有失败过。遇到 ENOMEM 时自动把单次传输减半重试，读取不再出错。
- 读速度约 100–200 MB/s（RTL9210 NVMe 硬盘盒，每次测量读 16 MiB，波动较大）。

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
