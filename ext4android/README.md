# ext4android

在 Android 上读写通过 USB 连接的 ext4 硬盘和 U 盘，不需要 root，并通过系统的存储访问框架（SAF）开放给其他 App。

文件系统部分复用同仓库的 [`ext4-core`](../ext4-core)（纯 Rust，macOS 版 [Ext4Kit](../ext4mac) 也用它）。

状态：M0–M2 完成，并在 iQOO 15 上测过（视频播放器、文本编辑器、系统文件管理器界面的手动测试还没做）；下一步是 M3（USB 盘接入文档提供者）。见 [docs/design.md](docs/design.md) 和 [TODO.md](TODO.md)。

## 已完成的部分

- `crates/usb-msc`：USB 大容量存储（Bulk-Only 传输 + SCSI），经 usbdevfs 收发；ENOMEM 时自动减小单次传输。用模拟传输和模拟磁盘做单元测试。
- `../ext4-core/crates/part`：GPT（主表头损坏时用备份）、MBR（含逻辑分区）、无分区表。
- `crates/ext4-jni`：
  - 自检和 USB 检测（只读）；
  - 卷管理：挂载镜像文件（文件系统镜像，或带分区表的磁盘镜像），用编号登记，挂载后的卷用核心的 `SharedFs`；
  - 文档层：路径编码（非 UTF-8 的字节、`%` 和控制字符写成 `%XX`）、列目录、属性、读文件；只跟随卷内的相对符号链接（最多 40 层），断开的、指向绝对路径的、循环的链接和设备文件、FIFO、socket 不显示；
  - 写操作：新建（重名时加 ` (1)` 等编号，属主跟随所在目录）、删除（目录连同内容；删除链接只删链接本身）、改名、移动、复制（目录连同内容，每 1 MiB 释放一次锁）、通过文件描述符写入、截断、fsync；
  - 打开文件表：打开期间被删除的文件仍可读写，最后一次关闭时才释放；写过的文件关闭时立即提交日志；
  - 错误按 Linux 编号报给 Kotlin（`Ext4Exception`）；
  - 调试用的测试镜像：各种文件名、链接和一个大文件 `big.bin`；把手机上的文件导入镜像。
  - 一连串写操作后用 e2fsprogs 的 `e2fsck -fn` 检查镜像，结果干净（单元测试）。
- App：
  - `Ext4DocumentsProvider`：每个挂载的卷是一个 root（ID 为文件系统 UUID），文档 ID 为 `<root>:<编码路径>`；打开文件时返回代理文件描述符（`openProxyFileDescriptor`），每个打开的文件一个线程；可写的卷支持新建、删除、改名、移动、复制和写模式打开（`w`、`wt` 截断，`rw` 原地改写，`wa` 追加）；跨卷的移动和复制交给系统文件管理器按字节复制；修改后通知所在目录刷新；
  - 调试界面：创建、导入、挂载（只读或可写）测试镜像；在系统的 DocumentsUI 里选文件交给其他 App 打开；通过文档提供者测读取速度，以及用其他 App 会用的系统接口（`DocumentsContract`、`ContentResolver`）测试全部写操作。

## M1、M2 真机测试（iQOO 15）

- 经文档提供者（代理文件描述符）读测试镜像里的 32 MiB 文件：顺序读 155 MB/s；随机读 4 KiB 平均 0.07 ms；数据正确。
- DocumentsUI 里出现卷（标题为卷标，副标题“ext4 · 可用 …”），目录列表、特殊文件名（`100%.txt`、非 UTF-8 的名字显示为 U+FFFD）、经过链接的目录和文件都正常，断开的、指向绝对路径的、循环的链接和 FIFO 不显示。
- 写操作测试 14 步全部通过：新建文件夹和文件（按 MIME 类型补扩展名）、`wt` 写入 3 MiB 并读回、`w` 截断、`rw` 原地改写、`wt` 截断、`wa` 追加、中文改名、复制、移动、列目录、递归删除。
- 写过的镜像拉回 Mac，`e2fsck -fn` 干净。
- vivo OriginOS 6 上，其他 App 的“打开文件”默认弹出 vivo 自己的选择器，看不到本 App 的卷（见 TODO）。

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
