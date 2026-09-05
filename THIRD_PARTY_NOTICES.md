# 第三方组件

本项目运行时使用以下开源组件，许可证文本随其安装包提供：

- `pymobiledevice3`：GPL-3.0-or-later。负责 usbmuxd、Lockdown、CoreDevice、RSD 和 Universal HID 客户端。
- `pmd-pytcp`、`pmd-net-addr`、`pmd-net-proto`：GPL-3.0-or-later。负责无需管理员权限的用户态 TCP 隧道。
- `pytun-pmd3`：MIT。由上游导入的 Windows TUN 兼容层；本桥接器固定使用用户态 TCP 隧道，不创建 Wintun 适配器。
- `PyInstaller`：GPL-2.0-or-later with bootloader exception。仅用于生成 Windows 发布程序。
- Python 标准库：PSF License。

Apple、iOS、iPhone、iPad、Apple Devices 和 usbmuxd 是其各自权利人的商标或产品名称。
本项目不分发 Apple 私有二进制，不安装 Apple 驱动，也不包含专有控制程序。
