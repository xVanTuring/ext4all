# ext4android 设计

## 目标

- 不需要 root，在 Android 11 及以上的设备上读写通过 USB 连接的 ext4 硬盘和 U 盘（ext2、ext3 同样支持）。
- 通过 `DocumentsProvider`（存储访问框架，SAF）把盘里的文件开放给其他 App。
- 文件系统部分直接复用 `ext4-core`（同仓库的 `../ext4-core`，macOS 版 Ext4Kit 也用它）：日志、校验和、只读特性判断、LUKS 和 fscrypt 都已经在里面。

不做：

- 系统级挂载（出现在 `/storage/XXXX-XXXX` 路径下）。这需要 root 并改动系统服务，见方案 A，不在本项目范围内。
- 媒体库（MediaStore）扫描、内核直通 I/O。

## 其他 App 能用到什么程度

| 访问方式 | 能否使用 |
|---|---|
| 系统文件选择器里打开、保存文件（`ACTION_OPEN_DOCUMENT`、`ACTION_CREATE_DOCUMENT`） | 可以 |
| 授权整个文件夹或整个盘（`ACTION_OPEN_DOCUMENT_TREE`），之后长期读写 | 可以 |
| 随机读写：视频拖动进度、编辑器原地保存 | 可以，通过 `openProxyFileDescriptor` 返回可定位的文件描述符 |
| 系统“文件”App（DocumentsUI）浏览、复制、移动 | 可以 |
| 小米、vivo 自带的文件管理器 | 不一定列出第三方存储 |
| 只按路径访问、或只查媒体库的 App（例如系统相册） | 看不到 |

## 整体结构

```
其他 App / 系统文件选择器（DocumentsUI）
   │  ContentResolver（Binder 调用）
Ext4DocumentsProvider（Kotlin）
   │  列目录、新建、删除、改名、移动 → JNI 直接调用
   │  打开文件 → StorageManager.openProxyFileDescriptor
   │              └ ProxyFileDescriptorCallback（onRead/onWrite/onFsync/onRelease）→ JNI
   ▼
libext4android.so（Rust，crates/ext4-jni）
   ├─ 卷管理：已挂载卷表、提交线程、打开文件计数
   ├─ ext4-core（../ext4-core，见“与核心、macOS 项目的关系”）
   ├─ part：GPT / MBR 分区表
   └─ usb-msc：USB 大容量存储 Bulk-Only 传输 + SCSI 命令，经 usbdevfs ioctl 收发
        ▲ 文件描述符、端点地址、接口号
UsbDeviceConnection（Kotlin：申请权限、claimInterface）
```

Kotlin 只负责 Android 框架相关的部分（USB 权限、前台服务、DocumentsProvider、界面），所有磁盘读写和文件系统逻辑都在 Rust 里，可以在 Mac 上用 `cargo test` 测试。

## 模块设计

### 1. USB 传输（`crates/usb-msc`）

Kotlin 侧：

- 通过 `UsbManager` 找到大容量存储接口：接口类 8（Mass Storage）、子类 6（SCSI 透明命令集）、协议 0x50（Bulk-Only）。
- 申请权限，`openDevice` 后调用 `claimInterface(intf, true)`。参数 `true` 会让内核的 usb-storage 驱动释放这个接口。
- 把 `UsbDeviceConnection.getFileDescriptor()`、接口号、bulk IN/OUT 端点地址交给 Rust。

Rust 侧：

- 直接对这个文件描述符调用 usbdevfs 的 ioctl（`USBDEVFS_BULK`、`USBDEVFS_CONTROL`、`USBDEVFS_CLEAR_HALT`）。libusb 在 Android 上不需要 root 的工作方式也是这样，每次读写都不必经过 JNI。
- Bulk-Only 传输：31 字节的 CBW、数据阶段、13 字节的 CSW；出错时按规范做复位恢复（Bulk-Only Mass Storage Reset，再清除两个端点的 HALT 状态）。
- SCSI 命令：
  - INQUIRY、TEST UNIT READY、REQUEST SENSE；
  - READ CAPACITY(10)，容量超过 2 TiB 时改用 (16)；
  - READ(10)/WRITE(10)，超过 2 TiB 时改用 (16)；
  - SYNCHRONIZE CACHE(10) 作为 `BlockDevice::flush` 的写屏障。部分转接芯片不支持这条命令，需要能识别出来并记录下来。
- 单条命令的传输长度先按 64 KiB 实现，M4 再实测调整。
- 扇区大小取 READ CAPACITY 的结果（512 或 4096），对齐交给 `ext4-core` 现有的 `AlignedDevice`。
- 先只支持 LUN 0；GET MAX LUN 大于 0 时在界面上提示。
- 收发通过一个 `Transport` trait 抽象出来，单元测试用模拟实现。如果实测发现某些系统不允许 App 对这个描述符做 ioctl，就换成 Kotlin 的 `bulkTransfer` 加 JNI 回调实现同一个 trait。
- 只实现 Bulk-Only，不做 UAS。常见的 SATA/NVMe 转接盒在备用设置 0 上都保留了 Bulk-Only。

### 2. 分区表（`../ext4-core/crates/part`）

macOS 上由系统提供分区设备（`disk4s1`），所以 `ext4-core` 里没有分区表解析，Android 版要自己做：

- GPT：先读主表头，校验失败时用备份表头；保护性 MBR 只用来识别 GPT。
- MBR：主分区和扩展分区里的逻辑分区。
- 没有分区表、整个盘就是一个文件系统的情况。
- 每个分区包装成一个带偏移的 `BlockDevice`，逐个检测 ext2/3/4 超级块或 LUKS 头。

### 3. 卷管理与 JNI（`crates/ext4-jni`）

- 挂载后的卷用核心里的 `SharedFs`（`ext4_core::shared`）：`Fs` 放在互斥锁里，配一个提交线程；操作中发生 panic 时停用这个卷。macOS 的 `ext4-ffi` 也用它。
- 提交策略：
  - 和 macOS 版一样，每 5 秒把新的修改提交到日志，空闲时做 checkpoint；
  - 额外规定：写过的文件被关闭（`onRelease`）或调用 `onFsync` 时，立即提交并发送写屏障。手机上不点“安全移除”就直接拔线的情况很常见，修改要尽快落盘。
- 错误转换：`ext4_core::Error` 的 `errno()` 用的是 macOS 的编号，Android 的不同（例如 `ENOTEMPTY` 在 macOS 是 66，在 Linux 是 39）。JNI 层直接按错误种类转换成 Java 异常（`FileNotFoundException`、`IOException` 等），不使用 `errno()`。
- 文件名：两侧都按字节数组传递，由 Kotlin 侧按 UTF-8 解码后显示。
- 打开文件表：记录每个 inode 被打开的次数。挂载时开启 `set_defer_unlinked(true)`，文件被删除时如果仍有打开的描述符，就等最后一次 `onRelease` 后再调用 `reclaim`，和 Linux 上“删除后仍可读写到关闭为止”的行为一致。

### 4. DocumentsProvider

- authority：`tech.xvanturing.ext4android.documents`。
- 每个挂载的卷是一个 root：
  - `rootId` 用卷 UUID；
  - 标题用卷标，没有卷标时显示“ext4 磁盘（容量）”；
  - `availableBytes` 取自 `statfs`；
  - 标志：`FLAG_LOCAL_ONLY`、`FLAG_SUPPORTS_CREATE`、`FLAG_SUPPORTS_IS_CHILD`（文件夹授权需要）。
- 文档 ID 格式为 `<卷 UUID>:<卷内路径>`，路径里的 `%` 和非 UTF-8 字节用百分号编码。这和系统自带的 `ExternalStorageProvider` 做法一致：判断父子关系很简单；改名、移动后 ID 会变，由 `renameDocument`、`moveDocument` 返回新 ID。
- 文件类型：
  - 普通文件按扩展名取 MIME 类型；
  - 目录用 `Document.MIME_TYPE_DIR`；
  - 符号链接如果指向卷内的文件或目录，就按目标显示（限制解析深度），无法解析的不显示；
  - 设备文件、FIFO、socket 不显示。
- 能力标志：可写的卷给出写入、删除、改名、移动、复制、在目录中新建；只读的卷（例如带 `quota`、`casefold` 特性）都不给。
- `openDocument`：
  - 通过 `StorageManager.openProxyFileDescriptor` 返回描述符；
  - 模式按 `ParcelFileDescriptor.parseMode` 的语义处理，其中 `w` 和 `wt` 都会截断文件；
  - 回调运行在 Handler 线程上，每个打开的文件一个 `HandlerThread`，避免一个文件的慢读写拖住其他文件。
- 新建文件的属主取父目录的 uid/gid，权限为 0644，目录为 0755。
- 变更通知：
  - 目录游标设置 `setNotificationUri`；
  - 修改后对父目录调用 `notifyChange`；
  - 挂载、卸载时通知 roots URI。
- 同一个卷内的 `copyDocument` 在 Rust 里直接读写复制；`moveDocument` 用 rename 实现。跨卷或跨 provider 的复制由 DocumentsUI 通过读写流完成。
- 后续再做：图片缩略图（`openDocumentThumbnail`）、搜索（`querySearchDocuments`）。

### 5. 生命周期

- 插盘：
  - 清单里给一个 Activity 声明 `USB_DEVICE_ATTACHED` 意图过滤器，设备过滤条件为接口类 8、子类 6、协议 0x50；
  - 用户在系统弹窗里选择“默认用此应用打开”后，以后插同一个盘不再询问权限。
- 挂载期间运行前台服务：
  - 类型为 `connectedDevice`，Android 14 起需要声明 `FOREGROUND_SERVICE_CONNECTED_DEVICE` 权限；
  - 通知栏显示已挂载的卷，并提供“安全移除”按钮。
- 拔盘（`USB_DEVICE_DETACHED`）：
  - 立即对这个卷调用 `abandon`，不再写入任何数据；
  - 移除对应的 root 并发出通知；
  - 尚未提交的修改会丢失，但日志保证盘上的文件系统是一致的。
- 进程被系统结束：效果等同于拔盘。下次挂载时回放日志。
- 系统检测到这个盘时，可能仍会弹出“不支持此设备”的通知。需要实测：我们 `claimInterface` 之后，这个通知是否会消失。

### 6. 加密（M5）

- LUKS：在 App 里输入口令或选择密钥文件，在后台线程派生密钥。Argon2id 按 cryptsetup 的默认参数在 Mac 上用了约 870 MB 内存，在手机上可能因为内存不足被系统结束进程，需要实测；必要时在界面上说明，并允许直接输入卷密钥。
- fscrypt：和 macOS 版一样，支持口令、原始密钥和 `fscrypt` 工具的保护器。
- 口令和密钥用 Android Keystore 里的 AES-GCM 密钥加密后，保存在 App 私有目录。

### 7. 界面与多语言

- 界面只有几页：已连接的盘、挂载状态和安全移除、密钥管理、设置，用 Jetpack Compose 实现。
- 和 macOS 项目的约定一样，从第一天就做中英双语：`values/strings.xml`（英文）和 `values-b+zh+Hans/strings.xml`（简体中文），代码里不写死界面文字。用按文字区分的 `b+zh+Hans`，而不是 `zh-rCN`，是为了让地区设为新加坡等的简体中文系统也能匹配到中文。

## 工程结构

```
ext4all/ext4android/
├─ README.md
├─ TODO.md
├─ docs/design.md
├─ scripts/
│  ├─ setup-toolchain.sh     安装 Android 构建工具（手动执行）
│  ├─ fetch-deps.sh          下载 Rust 和 Gradle 依赖（手动执行）
│  └─ build-rust.sh          cargo-ndk 编译 .so 到 jniLibs
├─ crates/                    属于 ext4all 根目录的 Cargo workspace
│  ├─ usb-msc/                Bulk-Only + SCSI
│  └─ ext4-jni/               cdylib：JNI 接口、卷管理
└─ android/                   Gradle 工程
   └─ app/src/main/
      ├─ java/tech/xvanturing/ext4android/
      │  ├─ usb/               插盘处理、权限、claimInterface
      │  ├─ service/           前台服务、卷生命周期
      │  ├─ provider/          Ext4DocumentsProvider、ProxyFileDescriptorCallback
      │  ├─ jni/               JNI 声明（native 是 Java 关键字，不能做包名）
      │  └─ ui/                Compose 界面
      ├─ res/values/、res/values-b+zh+Hans/
      └─ jniLibs/              构建产物，不提交
```

## 构建

- Rust：`scripts/build-rust.sh` 调用 `cargo ndk -t arm64-v8a -t x86_64 --platform 30 build --release`，输出到 `android/app/src/main/jniLibs`。三台测试设备都是 arm64，x86_64 只给模拟器用。Rust 部分始终按 release 编译（加密和校验计算在 debug 下太慢）。
- Gradle 的 `preBuild` 依赖 `buildRust` 任务，由它调用上面的脚本。Android Studio 不读 shell 配置，所以 NDK 路径由 Gradle 根据 `local.properties` 的 `sdk.dir`（或 `ANDROID_HOME`）和 `ndkVersion` 算出来传给脚本。
- 版本（2026-10）：
  - Android Gradle 插件 9.4.1，Gradle 9.8.0（wrapper）；
  - 使用插件内置的 Kotlin 支持，不再应用 `org.jetbrains.kotlin.android`；Compose 编译器插件 2.4.20 放在根工程的 `apply false` 里，同时把 Kotlin Gradle 插件升到 2.4.20；
  - Compose BOM 2026.09.00，activity-compose 1.13.0；
  - NDK 30.0.16248370，JDK 21（Android Studio 自带）。
- `minSdk` 30（Android 11），`compileSdk` 37.2（Compose 1.12 要求至少 37），`targetSdk` 36。`compileSdk` 只决定编译时能用哪些 API，App 在设备上的行为由 `targetSdk` 决定。
- 包名 `tech.xvanturing.ext4android`。
- 依赖由 `scripts/fetch-deps.sh` 下载（手动执行）；之后的构建可以离线进行。

## 测试

- Rust 单元测试（在 Mac 上、仓库根目录运行 `cargo test -p usb-msc -p part -p ext4-jni`）：
  - `usb-msc` 用模拟传输覆盖正常读写、端点 STALL、读到的数据比请求的短、CSW 状态为 phase error、复位恢复；
  - `part` 用构造的 GPT/MBR 样本；
  - 卷管理用 `MemDevice` 和 `FileDevice`。
- 镜像文件模式（仅调试版）：挂载 App 私有目录下的 ext4 镜像（用 adb 推上去）。不需要 USB 就能在模拟器上测试 DocumentsProvider，并写成 `androidTest` 仪器测试。
- 真机：先在 Pad 6 上测（不跑服务），再测 iQOO 15 和红米。用 Union Memory（UNION）那块盘，Fanxiang S790MAX 有读到旧数据的问题，不用于测试。
- 交叉验证：手机写过的盘，拿到 Mac 上运行 `e2fsck -fn`，或者在 Linux 虚拟机里挂载读写。
- 拔盘测试：边写边拔线，然后回放日志并运行 `e2fsck`，结果必须干净。

## 里程碑

| 阶段 | 内容 | 完成标准 |
|---|---|---|
| M0 | 工具链、工程骨架、加载 Rust 库；三个先行验证（见下） | 三台设备都能装上并调用 Rust 函数；先行验证有结论 |
| M1 | 镜像文件模式 + 只读 DocumentsProvider | 模拟器上能在系统文件选择器里浏览镜像、打开图片和视频（可拖动进度） |
| M2 | 写操作：新建、删除、改名、移动、复制、写入；提交策略；变更通知 | 写过的镜像拿到 Mac 上 `e2fsck -fn` 干净 |
| M3 | USB：Bulk-Only/SCSI、分区表、插盘流程、前台服务、安全移除、拔盘处理 | 三台设备上插真实硬盘能读写，其他 App 能通过文件选择器访问 |
| M4 | 稳定性与性能：拔盘测试、`e2fsck` 交叉验证、传输长度调优 | 拔盘测试多轮通过；记录读写速度 |
| M5 | LUKS 与 fscrypt 解锁、Keystore 保存密钥 | 加密测试镜像能在手机上解锁读写 |

### M0 里先做的三个小实验

1. 在 iQOO 15（未 root，OriginOS）上，App 能否对 `UsbDeviceConnection` 的描述符执行 usbdevfs ioctl。不行的话改用 Kotlin `bulkTransfer`。
2. 系统已经检测过这个盘、弹出“不支持”通知之后，`claimInterface(intf, true)` 能否成功拿到接口。
3. `openProxyFileDescriptor` 的读写速度，以及常用 App（视频播放器拖动进度、编辑器保存）在它上面是否正常。

另外，前台服务在 OriginOS、HyperOS 的后台管理下能否长期存活，在 M3 观察。

## 与核心、macOS 项目的关系

- 三个项目在同一个 ext4all 仓库、同一个 Cargo workspace 里（根目录的 `Cargo.toml`）：
  - `ext4-core/`：和平台无关的 `ext4-core`、`ext4-tool`、`part`（分区表解析，最早写在本项目里，已移过去）；
  - `ext4mac/`：macOS 的 FSKit 扩展和 `ext4-ffi`；
  - `ext4android/`：本项目，`ext4-jni` 和 `usb-msc`。
- 提交线程和互斥锁封装已移进核心（`ext4_core::shared::SharedFs`），macOS 和 Android 共用。
- 待整理：核心的 `Error::errno()` 是 macOS 编号。Android 不用它，按错误种类直接转换成 Java 异常；以后视情况把 macOS 的映射移到 `ext4-ffi`。
