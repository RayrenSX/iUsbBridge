# 第三方组件

当前 Rust bridge 使用下列第三方项目：

- `jkcoxson/idevice`，固定提交 `e98264c4194e6980173c576ac79a58adce95492b`，MIT License。构建时应用 `patches/idevice-compat.patch`。
- Cargo.lock 中列出的 Rust crates，分别遵循各自的软件许可证。
- `libusb0.dll` 是可选的 QuickTime USB 兼容后备运行时；bridge 只在系统或程序目录已有该 DLL 时动态加载，不在本仓库分发该文件。

仓库保留的旧 Python 实现使用以下组件：

- `pymobiledevice3`：GPL-3.0-or-later。
- `pmd-pytcp`、`pmd-net-addr`、`pmd-net-proto`：GPL-3.0-or-later。
- `pytun-pmd3`：MIT。
- `PyInstaller`：GPL-2.0-or-later with bootloader exception。

Apple、iOS、iPhone、iPad、Apple Devices 和 usbmuxd 是其各自权利人的商标或产品名称。本项目不分发 Apple 私有二进制或 Developer Disk Image。
