# 待办

按里程碑排列，设计见 [docs/design.md](docs/design.md)。完成后从这里删除，并在 README 里记录。

## M0 工具链与骨架

- 在红米 Note 11 Pro 上安装，跑自检和 USB 检测（iQOO 15、小米 Pad 6 已通过）。

## M1、M2 的后续

- `androidTest` 仪器测试：需要模拟器镜像或真机，以及 AndroidX Test 依赖（要下载）。
- 写入时 `onGetSize` 每次都查询 Rust；如果代理文件描述符因此太慢，改为在 Kotlin 里缓存大小。

## 厂商选择器（M3 之后处理）

- vivo OriginOS 6 用自己的选择器响应其他 App 的“打开文件”（`ACTION_OPEN_DOCUMENT`、`ACTION_GET_CONTENT`），里面看不到第三方存储；只指定 DocumentsUI 包名的 intent 也被改送到它，只有写明 DocumentsUI 的 Activity 才行。可选做法：App 内加文件浏览，用“打开方式”“分享”把文件交给其他 App；或实测能否把默认选择器改回 DocumentsUI。小米 HyperOS 也要实测。

## M3 USB

- 插盘意图过滤器、权限、前台服务、安全移除、拔盘处理。
- 三台设备真机读写。

## M4 稳定性与性能

- 边写边拔线的测试，回放日志后 `e2fsck` 干净。
- 传输大小与速度：
  - 默认单次传输用多大：32 KiB 没有遇到过 ENOMEM，64 KiB 偶尔会；现在减半后不再变大，要不要过一段时间再试大的；
  - 改用异步提交（`USBDEVFS_SUBMITURB`，同时挂多个 16–32 KiB 的传输），看能否稳定超过现在同步传输的 100–200 MB/s；
  - 测量改为每次读更多数据，结果才稳定；同时测写入速度。
- 观察前台服务在 OriginOS、HyperOS 上能否长期存活。

## M5 加密

- LUKS 口令、密钥文件解锁；Argon2 在手机上的内存占用实测。
- fscrypt 口令、原始密钥、`fscrypt` 工具保护器。
- Android Keystore 加密保存。

## 以后

- 图片缩略图、搜索。
- 在手机上格式化为 ext4（核心已有 `mkfs`）。
