# ext4android

在 Android 上读写通过 USB 连接的 ext4 硬盘和 U 盘，不需要 root，并通过系统的存储访问框架（SAF）开放给其他 App。

文件系统部分复用同仓库的 [`ext4-core`](../ext4-core)（纯 Rust，macOS 版 [Ext4Kit](../ext4mac) 也用它）。

状态：M0–M2 完成，并在 iQOO 15 上测过；M3（USB 盘接入文档提供者）的功能已完成，在模拟器上用模拟的 USB 硬盘测过，还差真机测试。见 [docs/design.md](docs/design.md) 和 [TODO.md](TODO.md)。

## 已完成的部分

- `crates/usb-msc`：USB 大容量存储（Bulk-Only 传输 + SCSI），经 usbdevfs 收发；数据阶段一次排队 4 个传输（异步传输，每个默认 64 KiB），一个传输结束时控制器手里已经有下一个；ENOMEM 时自动减小单次传输。用模拟传输和模拟磁盘做单元测试。
- `../ext4-core/crates/part`：GPT（主表头损坏时用备份）、MBR（含逻辑分区）、无分区表。
- `crates/ext4-jni`：
  - 自检和 USB 检测（只读）；
  - 卷管理：挂载镜像文件（文件系统镜像，或带分区表的磁盘镜像），用编号登记，挂载后的卷用核心的 `SharedFs`；
  - USB 盘：经 usbdevfs 打开，盘上每个 ext2/3/4 分区各挂载为一个卷（没有分区表时整个盘作为一个卷），只有 LUKS 卷时提示暂不支持；拔盘时放弃这些卷，不再写入任何数据（核心的 `SharedFs::abandon`），已提交的修改由日志保证一致；
  - 文档层：路径编码（非 UTF-8 的字节、`%` 和控制字符写成 `%XX`）、列目录、属性、读文件；只跟随卷内的相对符号链接（最多 40 层），断开的、指向绝对路径的、循环的链接和设备文件、FIFO、socket 不显示；
  - 写操作：新建（重名时加 ` (1)` 等编号，属主跟随所在目录）、删除（目录连同内容；删除链接只删链接本身）、改名、移动、复制（目录连同内容，每 1 MiB 释放一次锁）、通过文件描述符写入、截断、fsync；
  - 打开文件表：每个描述符一个编号；打开期间被删除的文件仍可读写，最后一次关闭时才释放；写过的文件关闭时立即提交日志；
  - 预读：一个描述符按顺序读时，在另一个线程上提前读下一段 1 MiB，盘的读取和 App 的请求同时进行；文件内容被改写或截断后，已预读的数据作废；
  - 错误按 Linux 编号报给 Kotlin（`Ext4Exception`）；
  - 调试用的测试镜像：各种文件名、链接和一个大文件 `big.bin`；把手机上的文件导入镜像。
  - 一连串写操作后用 e2fsprogs 的 `e2fsck -fn` 检查镜像，结果干净（单元测试）。
- App：
  - 插盘：`MainActivity` 声明 `USB_DEVICE_ATTACHED` 意图过滤器（接口类 8、子类 6、协议 0x50）；用户在系统弹窗里同意后，自动以可写方式挂载；
  - 前台服务 `MountService`（类型 `connectedDevice`）：挂载期间运行，通知栏列出已挂载的卷（卷标、只读或可写、可用空间），每个盘一个“安全移除”按钮；安全移除时提交并卸载所有卷，再把设备交还系统；
  - 拔盘：收到 `USB_DEVICE_DETACHED` 后立即放弃这个盘的卷，移除对应的 root；以可写方式挂载时发通知提醒最近几秒的修改可能没有保存；
  - 界面的“USB 磁盘”一栏：列出连接的盘（插拔时自动刷新），可写或只读挂载、安全移除，挂载后可在系统文件选择器里选文件、测试写操作；原来的只读检测（M0 实验）保留；
  - `Ext4DocumentsProvider`：每个挂载的卷是一个 root（ID 为文件系统 UUID），文档 ID 为 `<root>:<编码路径>`；打开文件时返回代理文件描述符（`openProxyFileDescriptor`），每个打开的文件一个线程；可写的卷支持新建、删除、改名、移动、复制和写模式打开（`w`、`wt` 截断，`rw` 原地改写，`wa` 追加）；跨卷的移动和复制交给系统文件管理器按字节复制；修改后通知所在目录刷新；
  - 调试界面：创建、导入、挂载（只读或可写）测试镜像；在系统的 DocumentsUI 里选文件交给其他 App 打开；通过文档提供者测读取速度；对任何已挂载的卷测读写速度：经文档提供者写入、读回 256 MiB 并校验（报告每次请求的大小、时间有多少用在文件系统和盘上），不经过文档提供者再读一遍同一个文件，不经过文档提供者按每次 128 KiB、1 MiB、4 MiB 写入，直接读盘，USB 单次传输的大小和减半次数，以及不碰盘的两项对照：只经过系统代理描述符的读写、换成管道的写入；以及用其他 App 会用的系统接口（`DocumentsContract`、`ContentResolver`）测试全部写操作。

## M3 模拟器测试

没有真机时用模拟器测：QEMU 的 `usb-storage` 把 Mac 上的一个磁盘镜像文件模拟成 USB 硬盘插进模拟器，App 收到的是真实的 USB 设备，读写走的也是同一套 usbdevfs 路径（见下文“在模拟器上测试 USB 硬盘”）。Android 16（API 36，arm64），256 MiB 磁盘镜像，MBR 分区表加一个 1 KiB 块的 ext4 分区：

- App 列出“QEMU USB HARDDRIVE”；点“挂载（可写）”后系统弹出 USB 权限对话框，同意后挂载，卡片和通知栏显示“EMUDISK”、可写、可用空间。
- 写操作测试 14 步全部通过（和 M2 相同的步骤，这次经过 USB）。
- 经文档提供者读回盘上原有的文件：5 MB 随机数据的校验和一致，中文文件名和内容正确；系统文件 App（DocumentsUI）的“其他应用中的文件”里出现 EMUDISK，能浏览目录。
- 点通知栏的“安全移除”：前台服务结束，通知消失，界面显示“已安全移除”；镜像在 Mac 上 `e2fsck -fn` 干净。
- 拔出再插入：系统弹出“用 ext4android 打开”，确认后自动挂载。模拟的盘每次插入序列号都不同，所以勾选“始终打开”后仍会再问；真实的盘需要在真机上确认。
- 写入文件后立即拔出：服务结束，root 消失，弹出“未经安全移除就被拔下”的通知；镜像带 `needs_recovery`，`e2fsck -fy` 回放日志后干净，文件内容是新写入的；再插入由 App 自己回放日志，安全移除后 `e2fsck -fn` 干净。
- 中文界面检查过（`cmd locale set-app-locales`）。

## 读写速度（真机）

RTL9210 硬盘盒（卷标 SSD），经文档提供者写入、读回 256 MiB：

| | 速度 | 每次 128 KiB 请求的耗时 | 其中在文件系统和盘上 |
|---|---|---|---|
| 经文档提供者写入 | 69 MB/s | 1.9 ms | 0.6 ms（32%） |
| 不经过文档提供者，经文件系统写入（每次 128 KiB / 1 MiB / 4 MiB） | 204 / 247 / 261 MB/s | | |
| 不经过文档提供者，经文件系统读同一个文件 | 252 MB/s | | |
| 直接读盘的前 256 MiB | 262 MB/s | | |

- 盘这一段在这台手机上约 250 MB/s，读写都差不多。每次写得更大只快一点，所以把小写入攒成大块帮助不大。
- 经文档提供者写入慢，主要慢在文件系统和盘之外：每个写请求在外面要 1.3 ms，读请求只要约 0.17 ms。下一步先测“只经过系统代理描述符”的写速度，看是代理本身慢，还是代理往返和写盘一个等一个造成的（见 TODO）。
- 这次读回显示 666 MB/s，是当时的计时方法有误：只计了每次 `read()` 调用内部的时间，没算两次调用之间校验数据的时间，而这段时间里内核和预读还在后台继续读。已改为按整段计时，真实值应在 250 MB/s 左右。

## 读写速度（模拟器）

模拟器上的数字只用来比较改动前后：那里的 USB 控制器和硬盘都是软件模拟的，真实速度要在真机上用“测读写速度”测。2 GiB 磁盘镜像，经文档提供者写入、读回 256 MiB：

| | 改动前 | 加预读 | 再加排队传输 |
|---|---|---|---|
| 写入 | 137 MB/s | 140 MB/s | 155 MB/s |
| 读回 | 158 MB/s | 220 MB/s | 247 MB/s |
| 直接读盘（不经过文件系统和文档提供者） | 313 MB/s | 330 MB/s | 460–476 MB/s |

- 系统每次只交给文档提供者 128 KiB 的读或写请求。
- 读回时只有 3% 的时间在文件系统和盘上，其余都是系统代理描述符的往返，所以模拟器上 247 MB/s 已经是这条路径的上限。
- 写入时 58% 的时间在文件系统和盘上，其余是代理的往返；两者可以重叠，见 TODO。
- 测试镜像上随机读 4 KiB 平均 0.04 ms，预读不影响随机读。
- 写入途中拔盘（排队传输进行中）：App 收到 I/O 错误，进程正常；再插入回放日志、安全移除后 `e2fsck -fn` 干净。

## M1、M2 真机测试（iQOO 15）

- 经文档提供者（代理文件描述符）读测试镜像里的 32 MiB 文件：顺序读 155 MB/s；随机读 4 KiB 平均 0.07 ms；数据正确。
- DocumentsUI 里出现卷（标题为卷标，副标题“ext4 · 可用 …”），目录列表、特殊文件名（`100%.txt`、非 UTF-8 的名字显示为 U+FFFD）、经过链接的目录和文件都正常，断开的、指向绝对路径的、循环的链接和 FIFO 不显示。
- 写操作测试 14 步全部通过：新建文件夹和文件（按 MIME 类型补扩展名）、`wt` 写入 3 MiB 并读回、`w` 截断、`rw` 原地改写、`wt` 截断、`wa` 追加、中文改名、复制、移动、列目录、递归删除。
- 写过的镜像拉回 Mac，`e2fsck -fn` 干净。
- 导入卷里的 38 MB 视频，在 DocumentsUI 里选中后用其他播放器打开，播放和来回拖动进度都流畅，日志里没有读取错误。
- Material Files 通过“授权文件夹”（`ACTION_OPEN_DOCUMENT_TREE`）接入卷：从手机存储复制照片进来、删除文件、用它的文本编辑器修改并保存 `README.txt` 都正常；之后 `e2fsck -fn` 干净，`debugfs` 读出的内容和改动一致。
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

### 在模拟器上测试 USB 硬盘

1. 安装系统镜像并创建模拟器，只需一次（下载约 1.7 GB）：
   ```bash
   scripts/setup-emulator.sh
   ```
2. 准备一个磁盘镜像（例如 MBR 分区表加一个 ext4 分区，`mke2fs -E offset=1048576` 可直接在镜像里的分区位置建文件系统），然后启动。脚本一直运行到模拟器关闭，加 `--window` 显示窗口：
   ```bash
   scripts/emulator.sh start disk.img
   ```
3. 第一次启动后执行一次：模拟器的系统镜像没有声明 USB 主机功能，App 看不到任何 USB 设备。这一步在模拟器的可写系统分区里补上声明并重启：
   ```bash
   scripts/emulator.sh enable-usb-host
   ```
4. 安装 App 后即可测试。`scripts/emulator.sh unplug`、`plug` 模拟拔插，`stop` 关闭模拟器。
5. 修改直接写进镜像文件。拔出或关闭模拟器后在 Mac 上检查：`dd if=disk.img of=part.img bs=1m skip=1` 取出分区，再 `e2fsck -fn part.img`。

只改 Rust 时也可以单独编译：`scripts/build-rust.sh arm64-v8a`。Rust 单元测试在 Mac 上、仓库根目录运行：`cargo test -p usb-msc -p part -p ext4-jni`。
