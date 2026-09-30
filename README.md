# Ext4Kit — macOS 上的 ext4 读写支持

用 **FSKit（Swift）+ 纯 Rust ext4 实现** 在 macOS 27 上挂载并读写 Linux ext4 磁盘。

- 完整读写：创建 / 删除 / 重命名 / 硬链接 / 符号链接 / 设备节点、读写 / 截断 / 预分配 / 打洞、扩展属性、卷标
- 崩溃一致性：实现 jbd2 日志（Linux 同格式），所有元数据修改以事务方式原子提交；挂载时自动回放未完成的日志
- 兼容现代 Linux 默认格式：`metadata_csum`、`64bit`、`flex_bg`、`extent`、htree 目录、`orphan_file`、`inline_data` 等
- ext3 / ext2（间接块映射）同样支持读写；含 `bigalloc`、`quota`、`encrypt`、`casefold` 等特性的卷以**只读**方式挂载

## 架构

```
Finder / 应用
   │ VFS
FSKit ──XPC──▶ Ext4FS.appex (Swift, macOS/Ext4FS)
                  │  Ext4FileSystem  : FSUnaryFileSystem（探测 / 加载）
                  │  Ext4Volume      : FSVolume.Handler 及读写、xattr、改卷标、预分配、SEEK_DATA/HOLE
                  │  Ext4KernelIOVolume : 可选的内核直通 I/O（KernelOffloadedIOHandler）
                  │  ResourceBlockIO : FSBlockDeviceResource 读写
                  ▼  C ABI（crates/ext4-ffi，cbindgen 生成头文件，静态库）
               ext4-core（纯 Rust，crates/ext4-core）
                  ├─ ondisk/   superblock、组描述符、inode、extent、目录项、xattr（含校验和）
                  ├─ journal/  jbd2 回放（csum v2/v3、revoke）与提交
                  ├─ cache.rs  元数据块缓存（脏块钉住到提交）
                  └─ fs/       挂载、分配器、extent 树、目录（线性 + htree）、文件读写、孤儿 inode
```

| 目录 | 内容 |
|---|---|
| `crates/ext4-core` | ext4 实现本体，只依赖 `BlockDevice` trait |
| `crates/ext4-ffi` | 给 Swift 用的 C ABI（`include/ext4_ffi.h`） |
| `crates/ext4-tool` | 命令行工具，直接读写镜像文件，便于调试 |
| `macos/` | xcodegen 工程：宿主 App `Ext4Kit`、FSKit 扩展 `Ext4FS`、XCTest |
| `scripts/` | Rust 构建、测试镜像生成、端到端挂载测试 |

## 构建

依赖：Xcode 27、Rust（`aarch64-apple-darwin`）、cbindgen、xcodegen、e2fsprogs（测试用）。

```bash
# Rust 部分（测试需要 e2fsprogs 的 mke2fs / e2fsck / debugfs）
cargo test --workspace

# 生成 Xcode 工程并构建（构建阶段会自动调用 scripts/build-rust.sh）
cd macos && xcodegen generate
xcodebuild -project Ext4Kit.xcodeproj -scheme Ext4Kit -configuration Release build

# Swift 单元测试
xcodebuild -project Ext4Kit.xcodeproj -scheme Ext4KitTests test
```

## 签名与启用扩展（需要手动完成一次）

FSKit 扩展必须带 `com.apple.developer.fskit.fsmodule` 权限签名，这需要 Apple 开发者账号生成的描述文件：

1. 打开 Xcode › Settings › Accounts，登录开发者账号（团队 `T8F5T6HKG8`，已写在 `macos/project.yml`）。
2. 用 Xcode 打开 `macos/Ext4Kit.xcodeproj`，在 `Ext4FS` target 的 Signing & Capabilities 中确认有 **FSKit Module** 能力（Xcode 会自动为 `tech.xvanturing.ext4` / `tech.xvanturing.ext4.fs` 注册 App ID 和描述文件）。
   命令行等价做法：`xcodebuild ... -allowProvisioningUpdates build`。
3. 运行 `Ext4Kit.app`（放到 `/Applications` 更稳妥），按界面提示打开「系统设置 › 通用 › 登录项与扩展 › 文件系统扩展」，启用 **Ext4Kit**。
4. 插入 ext4 磁盘即可自动挂载；也可手动：

```bash
diskutil list                                   # 找到分区，例如 disk4s1
mkdir -p /tmp/ext4 && mount -F -t ext4 disk4s1 /tmp/ext4
mount -F -t ext4 -o ro disk4s1 /tmp/ext4        # 只读挂载
hdiutil attach -nomount linux.img               # 挂载镜像文件前先接成块设备
```

### 可选：内核直通 I/O

默认情况下文件数据经扩展进程转发。打开内核直通 I/O 后，普通 ext4 文件（extent 映射、非内联数据）的数据由内核直接读写磁盘，扩展只提供块映射，吞吐量更高。该模式需要签名后在真实设备上验证，因此默认关闭：

```bash
# 打开（写入扩展沙盒容器内的偏好设置；之后新挂载的卷生效）
defaults write ~/Library/Containers/tech.xvanturing.ext4.fs/Data/Library/Preferences/tech.xvanturing.ext4.fs KernelOffloadedIO -bool YES
# 关闭
defaults delete ~/Library/Containers/tech.xvanturing.ext4.fs/Data/Library/Preferences/tech.xvanturing.ext4.fs KernelOffloadedIO
# 打开后仍可对单次挂载关闭
mount -F -t ext4 -o nokoio disk4s1 /tmp/ext4
```

目录、符号链接、内联数据文件和 ext2/ext3 的块映射文件始终走普通读写路径；同一个文件在被系统回收前不会切换路径。

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
| Swift | 桥接层、FSKit 属性转换、Handler 调用 | `xcodebuild ... -scheme Ext4KitTests test` |
| 端到端 | 安装并启用扩展后，真实挂载镜像做 cp/rsync/xattr/链接/删除等，卸载后 e2fsck | `scripts/e2e-mount-test.sh` |

辅助工具：

```bash
scripts/make-test-images.sh ./test-images 256   # 生成各特性组合的测试镜像
cargo run -p ext4-tool -- IMAGE info            # 查看镜像
cargo run -p ext4-tool -- IMAGE put host.txt /a.txt
```

## 设计要点

- **事务**：每个修改操作只改内存中的元数据块；达到阈值、每 5 秒或 `sync` 时提交。提交顺序：刷数据 → 日志超级块指向新事务 → 写描述块和元数据副本 → 刷盘 → 写提交块 → 刷盘 → 写回原位置 → 日志清空。任何时刻断电，恢复后要么是旧状态要么是新状态。
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

## 已知限制

- 无日志的 ext4 卷在断电后可能需要 `fsck`（与 Linux 相同，挂载期间会标记为未干净卸载）。
- 单个操作修改的元数据超过日志容量（极大且极碎片化的文件删除）时，会在标记"未干净"的前提下直接写入原位置。
- 内核直通 I/O 默认关闭（见上文）。引擎侧写映射先分配未写入 extent、部分覆盖的新块先清零，完成后才转换并增长文件大小，断电不会暴露旧数据；这部分有单元测试，但内核一侧的行为只能在签名后实测。
- ext2/ext3 的文件不支持预分配（`fallocate`，与 Linux 相同，块映射无法表示未写入块）。
- 只读支持：`bigalloc`、`quota`、`encrypt`、`casefold`、`verity`、`ea_inode`、`mmp` 等特性的卷可以读取，但不写入。
