# Ext4Kit — macOS 上的 ext4 读写支持

[English](README.md) | 简体中文

用 **FSKit（Swift）+ 纯 Rust ext4 实现** 在 macOS 27 上挂载并读写 Linux ext4 磁盘。

- 完整读写：创建 / 删除 / 重命名 / 硬链接 / 符号链接 / 设备节点、读写 / 截断 / 预分配 / 打洞、扩展属性、卷标
- 崩溃一致性：实现 jbd2 日志（Linux 同格式），所有元数据修改以事务方式原子提交；挂载时自动回放未完成的日志
- 兼容现代 Linux 默认格式：`metadata_csum`、`64bit`、`flex_bg`、`extent`、htree 目录、`orphan_file`、`inline_data` 等
- ext3 / ext2（间接块映射）同样支持读写；含 `bigalloc`、`quota`、`casefold` 等特性的卷以**只读**方式挂载
- 格式化：在 Mac 上直接把磁盘抹成 ext4（`diskutil`、磁盘工具、`newfs_fskit`），结果与 Linux 的 mke2fs 一致
- 加密：支持 fscrypt 加密的文件夹（Linux 的 `fscrypt` 工具、`fscryptctl`、安卓）和 LUKS1/LUKS2 整盘加密（cryptsetup）。没有密钥时，加密文件夹里的名字和 Linux 上显示的编码名字一样；口令和密钥在 App 里添加，保存在钥匙串中

## 架构

```
Finder / 应用
   │ VFS
FSKit ──XPC──▶ Ext4FS.appex (Swift, macOS/Ext4FS)
                  │  Ext4FileSystem  : FSUnaryFileSystem（探测 / 加载 / 检查 / 格式化）
                  │  Ext4Volume      : FSVolume.Handler 及读写、xattr、改卷标、预分配、SEEK_DATA/HOLE
                  │  Ext4KernelIOVolume : 可选的内核直通 I/O（KernelOffloadedIOHandler）
                  │  ResourceBlockIO : FSBlockDeviceResource 读写
                  ▼  C ABI（crates/ext4-ffi，cbindgen 生成头文件，静态库）
               ext4-core（纯 Rust，../ext4-core/crates/ext4-core）
                  ├─ ondisk/   superblock、组描述符、inode、extent、目录项、xattr（含校验和）
                  ├─ journal/  jbd2 回放（csum v2/v3、revoke）与提交
                  ├─ cache.rs  元数据块缓存（脏块钉住到提交）
                  ├─ mkfs.rs   创建新的文件系统
                  ├─ crypto/   AES 各模式（XTS、CBC-CTS、CBC-ESSIV）、SipHash、base64
                  ├─ fscrypt/  加密上下文、密钥派生、文件名加解密、fscrypt 工具的保护器
                  ├─ luks/     LUKS1/LUKS2 头、密钥槽、解密块设备
                  └─ fs/       挂载、分配器、extent 树、目录（线性 + htree）、文件读写、孤儿 inode
```

本目录是 ext4all 仓库中的 macOS 部分；Cargo workspace 在仓库根目录。

| 目录 | 内容 |
|---|---|
| `../ext4-core/crates/ext4-core` | ext4 实现本体，只依赖 `BlockDevice` trait |
| `../ext4-core/crates/ext4-tool` | 命令行工具，直接读写镜像文件，便于调试 |
| `crates/ext4-ffi` | 给 Swift 用的 C ABI（`include/ext4_ffi.h`） |
| `macos/` | xcodegen 工程：宿主 App `Ext4Kit`、FSKit 扩展 `Ext4FS`、XCTest |
| `scripts/` | Rust 构建、测试镜像生成、端到端挂载测试 |

## 构建

依赖：Xcode 27、Rust（`aarch64-apple-darwin`）、cbindgen、xcodegen、e2fsprogs（测试用）。

```bash
# Rust 部分，在仓库根目录执行（测试需要 e2fsprogs 的 mke2fs / e2fsck / debugfs）
cargo test --workspace

# 生成 Xcode 工程并构建（构建阶段会自动调用 scripts/build-rust.sh）
cd macos && xcodegen generate
xcodebuild -project Ext4Kit.xcodeproj -scheme Ext4Kit -configuration Release build

# Swift 单元测试
xcodebuild -project Ext4Kit.xcodeproj -scheme Ext4KitTests test
```

## 签名与启用扩展（需要手动完成一次）

FSKit 扩展必须带 `com.apple.developer.fskit.fsmodule` 权限签名，这需要 Apple 开发者账号生成的描述文件：

1. 打开 Xcode › Settings › Accounts，登录开发者账号（团队写在 `macos/project.yml` 的 `DEVELOPMENT_TEAM`，换成你自己的）。
2. 执行 `scripts/install-dev.sh`：签名构建（自动登记本机设备、注册 App ID 和带 FSKit Module 能力的描述文件），安装到 `/Applications`，并取消构建目录里其它副本在系统中的登记（否则「系统设置」里会出现多个 Ext4Kit）。
   如果仍报 FSKit Module 能力相关的错误，用 Xcode 打开 `macos/Ext4Kit.xcodeproj`，在 `Ext4FS` target 的 Signing & Capabilities 中加上 **FSKit Module**，或在开发者网站为 App ID `tech.xvanturing.ext4.fs` 勾选该能力。
3. 打开「系统设置 › 通用 › 登录项与扩展 › 文件系统扩展」，在「按类别」视图中启用 **Ext4Kit**（「按 App」视图里的开关可能无法切换）。
4. 执行 `sudo scripts/install-fs-bundle.sh`，安装文件系统描述包 `/Library/Filesystems/ext4.fs`。`diskutil` 和「磁盘工具」靠它识别 ext4；没有它时卷照样能挂载和读写，但 `diskutil unmount` / `eject` 会拒绝这些卷（直接调用 DiskArbitration 卸载不受影响），「磁盘工具」也不能抹盘为 ext4。`--remove` 可删除。
5. 插入 ext4 磁盘即可自动挂载；也可手动：

```bash
diskutil list                                   # 找到分区，例如 disk4s1
mkdir -p /tmp/ext4 && mount -F -t ext4 disk4s1 /tmp/ext4
mount -F -t ext4 -o ro disk4s1 /tmp/ext4        # 只读挂载
hdiutil attach -nomount linux.img               # 挂载镜像文件前先接成块设备
```

### 可选：内核直通 I/O

默认情况下文件数据经扩展进程转发。打开内核直通 I/O 后，普通 ext4 文件（extent 映射、非内联数据）的数据由内核直接读写磁盘，扩展只提供块映射。在两块 NVMe 上实测，大文件读写快 25%～110%（因硬盘而异），4K 随机写快 45%～120%，大量小文件慢 15%～25%（每个文件多两次映射和完成请求），详见下文“实测”。

它默认关闭：这条路径依赖内核缓存块映射的方式，而这部分行为没有文档。随机操作测试曾经在这里发现过一个会丢数据的问题（见“设计要点”），已修复并加入回归测试，修复后 15 组随机测试（约 1.9 万次操作、每组重新挂载 7 次）全部通过。建议先在自己的数据盘上试用：

```bash
# 打开（写入扩展沙盒容器内的偏好设置；之后新挂载的卷生效）
defaults write ~/Library/Containers/tech.xvanturing.ext4.fs/Data/Library/Preferences/tech.xvanturing.ext4.fs KernelOffloadedIO -bool YES
# 关闭
defaults delete ~/Library/Containers/tech.xvanturing.ext4.fs/Data/Library/Preferences/tech.xvanturing.ext4.fs KernelOffloadedIO
# 打开后仍可对单次挂载关闭
mount -F -t ext4 -o nokoio disk4s1 /tmp/ext4
```

目录、符号链接、内联数据文件和 ext2/ext3 的块映射文件始终走普通读写路径；同一个文件在被系统回收前不会切换路径。

### 可选：并行读取

默认情况下，扩展收到的读请求在卷锁里逐个处理，硬盘同一时刻只收到一个请求。打开并行读取后，读请求交给后台线程处理：只在查找块位置时持锁，读盘在锁外进行，FSKit 同时发来的多个请求可以一起交给硬盘。截断、删除等会释放块的操作要等正在进行的读取全部结束才执行，所以读取不会拿到已被释放、又分给别的文件的块。内联数据文件和 fscrypt 加密文件仍在锁内读取；LUKS 卷照常可用，解密在后台线程里进行。写入不受影响。

在 Union Memory 512 GB NVMe（RTL9210 USB 10 Gbps 硬盘盒）上实测（MB/s，4 GB 文件）：

| 读取方式 | 默认 | 并行读取 | 内核直通 I/O |
|---|---|---|---|
| 不经缓存，每次 5400 KB（Blackmagic Disk Speed Test 的读法） | 542 | 641 | 808 |
| 不经缓存，每次 8 MB | 535 | 704 | 806 |
| 经缓存顺序读（冷缓存） | 662 | 928 | 906 |

不经缓存、每次只读 1 MB 时，三种方式都在 450～470 MB/s，受硬盘单个请求的延迟限制。

它默认关闭，日常使用一段时间没有问题再改默认值：

```bash
# 打开（之后新挂载的卷生效，与内核直通 I/O 可以同时打开）
defaults write ~/Library/Containers/tech.xvanturing.ext4.fs/Data/Library/Preferences/tech.xvanturing.ext4.fs ParallelReads -bool YES
# 关闭
defaults delete ~/Library/Containers/tech.xvanturing.ext4.fs/Data/Library/Preferences/tech.xvanturing.ext4.fs ParallelReads
```

### 诊断

- 每次卸载时，系统日志里会记录这次挂载收到的文件数据请求（读写、块映射、完成、同步的次数、字节数和大小分布），可以看出内核实际走了哪条路径：

  ```bash
  log show --last 10m --info --predicate 'subsystem == "tech.xvanturing.ext4.fs"' | grep -A20 requests
  ```

- 每个请求都有调试级别的日志（`log stream --level debug --predicate 'subsystem == "tech.xvanturing.ext4.fs"'`）。
- 怀疑磁盘本身有问题时，可以让扩展把每次写入立刻读回比对，不一致时记录错误（很慢，只用于排查）：

  ```bash
  defaults write ~/Library/Containers/tech.xvanturing.ext4.fs/Data/Library/Preferences/tech.xvanturing.ext4.fs VerifyWrites -bool YES
  ```

### 格式化为 ext4

格式化会清空整块盘或整个分区。三种方式：

```bash
# 1. diskutil / 磁盘工具（需要已安装 /Library/Filesystems/ext4.fs，见上文 install-fs-bundle.sh）
diskutil eraseDisk ext4 DATA GPT disk4      # 整盘：新建 GPT，分区类型为 Linux 文件系统
diskutil eraseVolume ext4 DATA disk4s2      # 只格式化一个分区
# 2. FSKit 命令行，可带 mke2fs 风格的选项；真实磁盘需要 sudo（设备节点属于 root）
sudo newfs_fskit -t ext4 -L DATA -m 0 /dev/disk4s2
# 3. 磁盘镜像文件（开发用）
cargo run -p ext4-tool -- disk.img mkfs -L DATA
```

- 生成的文件系统与 e2fsprogs 1.47 的 `mke2fs -t ext4` 相同（块大小、inode 数、日志大小和位置都一致），只是不启用 `resize_inode`、`orphan_file`、`metadata_csum_seed`，以便较老的 Linux 内核（4.x）也能读写。
- 支持的选项：`-L` 卷标（超过 16 字节会截断）、`-b` 块大小、`-i` 每个 inode 对应的字节数、`-N` inode 数、`-m` 保留比例、`-U` UUID、`-J size=` 日志大小（MB）、`-O ^has_journal`、`-E root_owner[=uid:gid]`。
- 通过 diskutil / 磁盘工具抹盘时默认不给 root 保留空间（`-m 0`）；其余方式和 mke2fs 一样默认保留 5%。
- 除第 0 组外不清零 inode 表（与 Linux 上 mke2fs 的延迟初始化相同），大盘几秒即可完成；主要耗时是清零日志（最大 1 GB）。
- `newfs_fskit` 之后系统可能还记着“无法识别”的旧探测结果，需要重新插拔才会自动挂载；`diskutil` 抹盘会自己挂载。

### 加密磁盘

支持 Linux 的两种加密方式：

- **fscrypt**（`encrypt` 特性）：只加密某些文件夹，磁盘其余部分不加密。Linux 的 `fscrypt` 工具、`fscryptctl` 和安卓都用它。支持 v1、v2 策略，内容加密 AES-256-XTS 和 AES-128-CBC-ESSIV，文件名加密 AES-256-CTS 和 AES-128-CTS，各种文件名填充长度，IV_INO_LBLK_64/32 标志，以及小于块大小的数据单元。Adiantum、HCTR2、SM4 按没有密钥处理。
- **LUKS1 和 LUKS2**（cryptsetup）：整个分区加密，里面是 ext4。支持 `aes-xts-plain64`（cryptsetup 默认）、`aes-xts-plain`、`aes-cbc-essiv:sha256`、`aes-cbc-plain64`，512 到 4096 字节的扇区，使用 PBKDF2、Argon2i、Argon2id 的密钥槽，以及密钥文件。不支持：分离存放的 LUKS 头、带完整性校验的加密（`--integrity`）、正在重新加密的卷、其它算法（serpent、twofish）。

**没有密钥时**，fscrypt 文件夹的表现和 Linux 上没有密钥时一样：文件名以编码形式显示（加密后文件名的 base64url 编码，和 Linux 上 `ls` 看到的完全相同），符号链接的目标也是这样；读取文件会报“权限不足”；文件和空文件夹可以删除。在里面新建、改名、建链接需要密钥。没有密钥的 LUKS 磁盘能被识别，但不会挂载。

**添加密钥**：打开 Ext4Kit，在“加密磁盘”里添加口令或密钥文件。挂载加密磁盘时，扩展会依次尝试：

- LUKS 的口令或密钥文件，用来打开对应的密钥槽；
- 口令，用来打开 `fscrypt` 工具保存在该磁盘 `/.fscrypt` 里的口令保护器；32 字节的密钥文件，用来打开原始密钥保护器；
- 内容是 16 到 64 字节 fscrypt 主密钥的密钥文件（二进制，或十六进制文本，例如 `fscryptctl` 生成的），直接作为密钥使用。

打开某块磁盘的密钥会按磁盘记住（LUKS 的卷密钥，或 fscrypt 的主密钥），以后挂载不再需要运行密钥派生。LUKS 磁盘第一次解锁需要几秒钟：在 Apple Silicon 上实测，cryptsetup 默认的 Argon2id 参数约需 5.6 秒、870 MB 内存，因为密钥派生是单线程的。密钥在挂载时加载：一直没能解锁的 LUKS 磁盘，添加口令后就可以挂载（重新连接，或执行 `diskutil mount diskNsM`）；已经挂载的、带 fscrypt 文件夹的磁盘需要推出再重新连接。在某块 LUKS 磁盘上失败过的口令不会再对它重试，这块盘之后只会被识别、不会挂载，直到添加了新的口令。终端里也可以操作：

```bash
APP=/Applications/Ext4Kit.app/Contents/MacOS/Ext4Kit
$APP add-passphrase                   # 输入口令时不显示
$APP add-key-file mykey < ~/mykey.bin # 密钥文件从标准输入传入
$APP list                             # 列出已保存的条目及其 id
$APP remove secret:…                  # 删除口令、密钥文件或记住的磁盘密钥
```

说明：

- 所有内容保存在数据保护钥匙串里，所在的访问组只有 App 和它的扩展能用，只保存在这台 Mac 上，不会同步。记住的 LUKS 卷密钥在 Linux 上修改口令后仍能打开磁盘；要让 Mac 忘记这块盘，在 App 里删除对应条目。
- Linux 系统盘的登录口令保护器保存在那台系统的根文件系统里，不在外接盘上，所以这里无法使用；请在 Linux 上为策略添加一个自定义口令保护器（`fscrypt metadata add-protector-to-policy`），或使用原始密钥。安卓的文件加密密钥由设备硬件保管，这类文件夹会保持锁定。
- 加密文件和 LUKS 卷的数据始终经过扩展处理，不使用内核直通 I/O（否则内核读写的是密文）。
- fscrypt 不加密扩展属性（与 Linux 相同）。
- 用 `diskutil` 和磁盘工具可以把解不开的 LUKS 磁盘抹成 ext4（它们会先清除原有内容的标识）；对解不开的 LUKS 设备直接运行 `newfs_fskit` 会被拒绝，因为扩展无法加载它。
- 已在安装好的扩展里用磁盘镜像实测：fscrypt 文件夹的锁定与解锁（口令、原始密钥保护器文件、记住的密钥），LUKS2 的锁定、解锁、用记住的卷密钥再次挂载、附加时自动挂载；经 FSKit 写入的文件在 Linux 上读出正确。

`ext4-tool` 对镜像文件也接受同样的密钥：`--key HEX`、`--key-file FILE`、`--passphrase TEXT`（fscrypt 保护器）、`--luks-passphrase TEXT`、`--luks-key HEX`，另有 `crypt-status PATH`、`encrypt PATH`（加密一个空文件夹）、`luks-dump` 命令。

发布给他人使用时需 Developer ID 签名 + 公证。

## 测试

| 层级 | 内容 | 命令 |
|---|---|---|
| 单元测试 | 磁盘结构解析/序列化/校验和、哈希、分配器、缓存、日志 | `cargo test -p ext4-core --lib` |
| 读路径 | 11 种 mke2fs 特性组合，逐文件与源目录、debugfs 比对 | `cargo test -p ext4-core --test read` |
| 写路径 | 所有操作后 `e2fsck -fn` 必须零错误，再用 debugfs 核对内容 | `cargo test -p ext4-core --test write` |
| 崩溃一致性 | 在提交的每一个写入点模拟断电；随机操作 + 随机断电；用 `e2fsck -fy` 与自身回放两种方式恢复，要求不需要任何修复 | `cargo test -p ext4-core --test crash` |
| 随机模型测试 | proptest 随机操作序列对照内存模型，大目录 htree 分裂 | `cargo test -p ext4-core --test random` |
| 长时间浸泡 | 800 个随机断电种子 | `cargo test --release -p ext4-core --test crash -- --ignored` |
| FFI / CLI | C ABI 全流程、扇区对齐、并发、定时提交；命令行工具 | `cargo test -p ext4-ffi -p ext4-tool` |
| 加密 | 由 Linux 7.2.8、`fscrypt` 工具和 cryptsetup 2.8.8 生成的加密镜像（`../ext4-core/scripts/make-crypt-fixtures.py`，在 Linux 上以 root 运行）：没有密钥时显示的每个名字、有密钥时的每个文件都与 Linux 一致，覆盖 4K、1K 块上的 7 种 fscrypt 策略、该工具的保护器和 5 个 LUKS 卷；我们写入后 `e2fsck` 干净；AES、XTS、CTS、SipHash、Argon2 公开测试向量 | `cargo test -p ext4-core --test crypt` |
| Swift | 桥接层、FSKit 属性转换、Handler 调用、LUKS 与 fscrypt 解锁 | `xcodebuild ... -scheme Ext4KitTests test` |
| 端到端 | 安装并启用扩展后，真实挂载镜像做 cp/rsync/xattr（cp 和 ditto 之后不产生 `._` 文件）/链接/删除等，卸载后 e2fsck | `scripts/e2e-mount-test.sh` |
| 格式化 | 与 mke2fs 对比几何参数和日志位置；各种大小（含随机数据填充、已有 ext4、64 GB 稀疏镜像）格式化后 e2fsck 干净、可挂载并在多个组里写入；选项解析 | `cargo test -p ext4-core --test mkfs` |
| 随机操作（真实卷） | 在已挂载的卷上随机覆盖写、不经缓存写、追加、截断、扩展、预分配、内存映射写、改名覆盖、删除，每轮与内存模型逐字节比对，每 3 轮重新挂载；默认方式和内核直通 I/O 各跑一遍 | `python3 scripts/fsstress.py /Volumes/X/stress 1 15 diskNsM` |
| 并行读取 | 多个线程并行读取，同时另一个线程截断、删除重建、打洞、改写同一批文件；文件里每 8 字节都写着它自己的 inode 号，读到别的文件或元数据的内容即失败（去掉读写锁时这个测试必然失败）；另外与锁内读取逐字节比对，覆盖内联数据、ext3、fscrypt（有无密钥）和 LUKS | `cargo test -p ext4-core --test parallel_read --test crypt` |
| 并行读取（真实卷） | 打开并行读取后，在已挂载的卷上用多个线程不经缓存读取，同时另一个线程截断、删除重建、改写，检查方法同上，结束后 e2fsck | `python3 scripts/read-race.py /Volumes/X/race 60` |

辅助工具：

```bash
scripts/make-test-images.sh ./test-images 256   # 生成各特性组合的测试镜像
cargo run -p ext4-tool -- IMAGE info            # 查看镜像
cargo run -p ext4-tool -- IMAGE put host.txt /a.txt
```

## 设计要点

- **事务**：每个修改操作只改内存中的元数据块；达到阈值、每 5 秒或收到同步请求时提交。和 Linux jbd2 一样，提交只是把事务顺序追加到日志（描述块、元数据副本、提交块，日志为空时才写日志超级块），元数据写回原位置留到检查点：日志用过一半、卸载、`sync`、空闲一个提交周期，或者有仍在日志里的块被释放（它可能马上被分配给文件数据）时进行。检查点只写已提交的内容。任何时刻断电，回放日志后要么是旧状态要么是新状态；日志里可以同时有多个事务，Linux 和 e2fsck 都能回放。
- **同步**：应用调用 `fsync` 时 FSKit 会请求同步；关闭文件本身不会。macOS 的 `cp` 每复制一个文件都会请求一次需要等待的同步。要求等待的同步只做一次日志提交（满足持久化），不等待的同步只唤醒提交线程。
- **延迟释放**：事务中释放的块在提交时才归还位图，避免未提交前被重新分配后覆盖。
- **打开后删除**：`removeItem` 只删除目录项，inode 挂到孤儿链表；FSKit `reclaimItem` 时才真正释放；异常断电后下次挂载自动清理。
- **扩展属性**：macOS 的属性名 `N` 存为 ext4 的 `user.N`，其它命名空间（`security.`、`trusted.`、`system.`）对 macOS 隐藏并原样保留。
- **所有权**：按磁盘上的 uid/gid 原样呈现；外置盘默认由系统忽略所有权（也可 `mount -o noowners`）。
- **操作级回滚**：每个修改操作开始时建立保存点（元数据块撤销日志 + 超级块/组描述符/孤儿等内存状态），失败（空间不足、I/O 错误、损坏）时全部撤销，不会把半完成的修改提交到磁盘。
- **提交失败即中止**：日志提交出错后卷立即转为只读并保留 needs_recovery，下次挂载回放；与 jbd2 的 abort 行为一致。
- **日志特性**：读写挂载时像 Linux 内核一样为日志启用 64 位块号和校验和 v3。
- **系统区域校验**：超级块副本、组描述符、位图、inode 表和日志所在的块被登记为系统区域；任何文件映射、树节点、间接块、xattr 块、释放和分配碰到它都按损坏处理，恶意或损坏的磁盘无法借此覆盖元数据。两个元数据结构共用块（例如两个组指向同一张 inode 表）时拒绝挂载，与 Linux 相同。
- **目录枚举游标**：htree 目录按哈希顺序枚举、使用基于哈希的游标（与 Linux 相同），枚举过程中目录分裂不会漏项或重复；线性目录使用字节偏移游标。
- **挂载前检查**：实现 FSKit 的检查操作，系统在自动挂载块设备前会调用。
- **内核直通 I/O 的块映射**：写映射先分配未写入 extent，部分覆盖的新块先清零，内核报告完成后才转换为已写入并增长文件大小，断电不会暴露旧数据。内核会缓存拿到的映射，之后直接按缓存写盘、只为缺少映射的块再来请求；因此映射请求即使从文件末尾之后开始，也不能去清零末尾块的剩余部分（内核可能正在同一次写入里填充它，早期版本因此丢过数据）。扩展写入时的空隙由内核自己补零；写入失败时由扩展清理末尾块里的残留。
- **内核数据缓存**：没有实现 FSKit 的 `DataCacheHandler`。实测显式授予写回缓存和不实现完全一样（内核本来就缓存读写并合并写入），只是每个文件多一次打开和关闭请求；写穿模式下每次写都立即下发，小块追加慢上百倍；不缓存模式下内核每追加一次就按扇区把整个文件从头读一遍、写一遍。
- **通过 FSKit 格式化**：FSKit 在格式化前会先加载设备（参数和挂载前一样是 `-f`），所以设备里没有可用的 ext4 时加载也要成功：返回一个占位卷，激活和检查时报出原来的错误。以 root 身份运行时，`newfs_fskit` 从 `SUDO_UID` 判断该用哪个用户启用的模块；StorageKit 在用户会话之外以 root 身份运行格式化程序，所以 `ext4.fs` 里的格式化程序把它设成当前登录用户。
- **fscrypt**：文件名在引擎的边界处转换。查找时先把给定的名字加密（或把无密钥名还原成密文），再比较密文；htree 目录和 Linux 一样对密文计算哈希，所以加密目录也保留索引。每个加密 inode 的密钥由它的加密上下文派生（v2 用 HKDF-SHA512，v1 用主密钥的 AES-ECB 加密）并缓存。内容按数据单元加密（块大小，或策略指定的更小单元）；部分块写入、截断、打洞时先解密整块、修改后再加密。新文件继承所在目录的策略并使用新的随机数，并且像 Linux 一样不使用内联数据。
- **LUKS**：解密后的卷作为引擎下面的一个块设备：读取时解密整个扇区，写入时加密（不完整的扇区先读出、修改、再整扇区写回）。扇区的 IV 是以 512 字节为单位的扇区号加上数据段的 IV 偏移，4K 扇区也是如此，与 dm-crypt 对 LUKS2 的处理相同。

## 实测（macOS 27，Apple Silicon）

| 介质 | 顺序写 | 顺序读（冷缓存） | 复制 57 个源码文件 | 3000 个小文件 | 每次 fsync |
|---|---|---|---|---|---|
| NVMe（梵想 S790MAX，USB 10Gbps 硬盘盒） | 753 MB/s（KOIO 925） | 781 MB/s（KOIO 967） | 0.18 秒 | 1.15 秒 | 0.6 毫秒 |
| SD 卡 | 33 MB/s | 37 MB/s（KOIO 相同） | 0.82 秒 | 5.6 秒 | 5.5 毫秒 |
| 入门级 U 盘 | 8 MB/s | 37 MB/s | 约 7 秒 | 约 19 秒 | 受随机写延迟限制 |

慢速介质上瓶颈是硬件本身（入门级 U 盘单次 4K 随机写可能卡 1.5 秒）。

默认方式与内核直通 I/O 的对比（单位：秒，写入类均包含最后的 `sync` 或 `fsync`）。硬件为 Union Memory 512 GB NVMe 装在 RTL9210 USB 10 Gbps 硬盘盒里（括号内是有缺陷的梵想 S790MAX 2TB 的结果，见“已知限制”）：

| 操作 | 默认 | 内核直通 I/O |
|---|---|---|
| 顺序写 1 GB | 1.58（2.59） | 1.25（1.24） |
| 复制 2 GB 随机数据文件 | 5.33（6.0～7.7） | 3.70（4.08） |
| 读 2 GB（冷缓存） | 3.33（2.73） | 2.37（2.22） |
| 64 MB 文件内 2 万次 4K 随机写 | 0.16（0.24） | 0.11（0.11） |
| 16 MB 文件内 5000 次 1000 字节非对齐写 | 0.038（0.057） | 0.025（0.024） |
| 2 万次小块追加 | 0.027（0.027） | 0.027（0.026） |
| 内存映射写 2000 页 | 0.027（0.034） | 0.059（0.056） |
| 创建 3000 个小文件 | 1.16（1.16） | 1.36（1.45） |
| 本地 `git clone` 本项目 | 0.15（0.55） | 0.17（0.61） |

两块盘上，默认方式和内核直通 I/O 下的随机操作测试（`scripts/fsstress.py`，每组 18 轮、每 3 轮重新挂载）全部通过。

正确性方面，用 RK3399 开发板的 SD 卡（Linux 内核写入、带未回放日志）验证过：日志回放后 `e2fsck` 干净；约 6 万个文件和 e2fsprogs 的 `debugfs` 导出逐个比对，内容全部一致。

格式化在 Linux 内核上验证过：本工具格式化的 3 GB 镜像，在 Arch Linux ARM（内核 5.18、e2fsprogs 1.46.5）虚拟机里 `e2fsck` 干净，由内核挂载并写入约 3600 个文件和 300 MB 数据后仍然干净；传回 Mac 后，Linux 写入的 2430 个文件由本引擎读出，SHA-256 全部一致。

加密在 Linux 7.2.8（e2fsprogs 1.47.4、cryptsetup 2.8.8）上做了双向验证：在测试镜像的每种 fscrypt 策略下，Mac 写入的文件、文件夹、长短符号链接和改名，Linux 内核都读得正确；在 Mac 上加密的文件夹（v1、v2 策略）在 Linux 上用对应密钥能打开；Linux 的 `e2fsck` 检查干净，Linux 再往这些文件夹写入后也仍然干净。Mac 写入 LUKS1（XTS、CBC-ESSIV）和 LUKS2（Argon2、PBKDF2、4K 扇区）卷的文件，经 `cryptsetup open` 后读出也都正确。

**容量显示**：和 Linux 一样，总容量不含元数据（inode 表、日志等）；mke2fs 默认保留 5% 给 root，这部分算空闲但不算可用，Finder 会把它显示为“已用”。只存数据的盘可以在卸载状态下执行 `sudo tune2fs -m 0 /dev/diskNsM` 取消保留。注意 e2fsprogs 修改已有文件系统时要用块设备 `/dev/diskNsM`：原始设备 `/dev/rdiskNsM` 要求按扇区对齐读写，`tune2fs` 写超级块时会报 `Invalid argument`。

**自定义分区类型**：开发板镜像（如 Rockchip）常用厂商自定义的分区类型 GUID，macOS 不会自动探测（Linux 桌面同样不会自动挂载）。手动挂载时注意 FSKit 扩展按用户启用，不能用 `sudo mount`；先把设备交给当前用户再挂载：

```bash
sudo chown $USER /dev/disk4s9 /dev/rdisk4s9 && sudo chmod u+w /dev/disk4s9 /dev/rdisk4s9
mkdir -p ~/mnt/sd && mount -F -t ext4 disk4s9 ~/mnt/sd
```

## 已知限制

- 无日志的 ext4 卷在断电后可能需要 `fsck`（与 Linux 相同，挂载期间会标记为未干净卸载）。
- FSKit 没有提供让磁盘把自身写缓存刷到介质的接口。日志提交依赖原始写入按顺序同步完成；如果磁盘在突然断电时丢失了写缓存里的数据，日志可能无法完整回放。拔盘前请先推出。
- 单个操作修改的元数据超过日志容量（极大且极碎片化的文件删除）时，会在标记"未干净"的前提下直接写入原位置。
- 内核直通 I/O 默认关闭（见上文）。已在 macOS 27 上实测：内核确实通过块映射直接读写文件数据，端到端测试 5 种格式全部通过；在包含追加、非对齐写、内存映射、小文件和 `git clone` 的测试里，没有任何写入改走扩展的读写接口。
- **有的硬盘会读到旧数据**：梵想（Fanxiang）S790MAX 2TB NVMe 硬盘（英韧 IG5236 主控，固件 030W0P4W）在短时间内反复覆盖同一个块之后，写入约 1 毫秒后再读这个块，会返回写入前的内容，而且一直如此，直到读过别处足够多的数据（32 MB）为止。不经过 FSKit、直接以 root 读写原始设备同样能复现；装在两个 RTL9210 芯片的 USB 硬盘盒和一个雷电硬盘盒（直接走 PCIe、由 macOS 自带的 NVMe 驱动管理）里结果都一样；另一块 Union Memory 512 GB 硬盘装进出错最多的那个 RTL9210 盒子则完全正常，磁盘镜像上也从不出现，所以是这块硬盘本身的问题，与硬盘盒和文件系统无关。这块盘在雷电盒子里还出现过一次读取超时后从 PCIe 上掉线。默认方式下内核自己缓存文件数据，很少需要从盘上读回刚写的块，在这块盘上做的全部测试都没有出错，`e2fsck` 也干净；但任何需要“先读再改再写”的操作都可能拿到旧内容，不建议用这类硬盘存放重要数据。可以用下面的脚本检查自己的硬盘（只用分区末尾附近的 4 MB，先备份、结束时恢复，分区必须先卸载）：

  ```bash
  diskutil unmount disk4s2
  sudo python3 scripts/check-stale-reads.py /dev/rdisk4s2
  ```
- FSKit 卷上 `fcntl(F_LOG2PHYS)` / `F_LOG2PHYS_EXT` 返回“不支持”（内核没有转发给扩展），开启内核直通 I/O 时也一样。
- ext2/ext3 的文件不支持预分配（`fallocate`，与 Linux 相同，块映射无法表示未写入块）。
- 扩展属性是原生支持的，拷贝时不会产生 AppleDouble 的 `._` 文件；但 ext4 把一个文件的全部属性存放在 inode 和一个块里，超过约一个块大小的属性值（例如 8 KB 的 `com.apple.ResourceFork`）无法保存：`cp` 会复制文件但丢掉这个属性，并报“No space left on device”，`ditto` 则直接失败。存放在独立 inode 里的属性值（`ea_inode`）只能读取，不能写入。
- 只读支持：`bigalloc`、`quota`、`casefold`、`verity`、`ea_inode`、`mmp` 等特性的卷可以读取，但不写入。后续计划见 [TODO.md](TODO.md)。

## 许可证

Copyright (C) 2026 xVanTuring

本程序是自由软件：你可以依据自由软件基金会发布的 GNU 通用公共许可证（GNU General Public License）第 3 版，或（由你选择）任何更新的版本，再发布和修改它。发布本程序是希望它有用，但不提供任何担保，甚至不包括适销性或适用于特定用途的默示担保。详见 [LICENSE](LICENSE)。

SPDX-License-Identifier: GPL-3.0-or-later
