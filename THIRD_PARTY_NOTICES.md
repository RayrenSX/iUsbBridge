# 第三方组件

本项目运行时使用以下开源组件，许可证文本随其安装包提供：

- `pymobiledevice3`：GPL-3.0-or-later。负责 usbmuxd、Lockdown、CoreDevice、RSD 和 Universal HID 客户端。
- `pmd-pytcp`、`pmd-net-addr`、`pmd-net-proto`：GPL-3.0-or-later。负责无需管理员权限的用户态 TCP 隧道。
- `pytun-pmd3`：MIT。由上游导入的 Windows TUN 兼容层；本桥接器固定使用用户态 TCP 隧道，不创建 Wintun 适配器。
- `PyInstaller`：GPL-2.0-or-later with bootloader exception。仅用于生成 Windows 发布程序。
- `httpx`：BSD-3-Clause。用于开发者镜像的异步 HTTP 传输。
- Python 标准库：PSF License。

`src/iostouch/qt/usb.py`、`usbmux_usb.py`、`usbmuxd_server.py` 的参考来源包括
[iPhoneUsbTouch](https://gitee.com/xiaozai-van-liu/iPhoneUsbTouch)。其上游许可资料不完整；
本次同步不将这些文件标记为已取得 MIT 或 GPL 授权。

Python 打包使用调用者提供的 `libusb0.dll` 和 `libusb-1.0.dll`。
再分发时应随运行时保留相应项目的许可证、版权和来源说明。
构建只校验文件身份，不在本仓库存放 DLL，也不安装或替换系统 USB 驱动。

Apple、iOS、iPhone、iPad、Apple Devices 和 usbmuxd 是其各自权利人的商标或产品名称。
本项目不分发 Apple 私有二进制，不安装 Apple 驱动，也不包含专有控制程序。
