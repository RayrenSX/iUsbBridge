# iUsbBridge

免越狱、免自签、无需额外硬件的 iPhone/iPad 本地控制桥接器。

iUsbBridge 通过 Apple 官方设备服务，在 Windows 上以纯用户态方式控制 iPhone 和
iPad：支持 USB 有线连接，也支持同一局域网内的本地网络连接。控制数据只在本机与
设备之间传输，不依赖云端中转，不需要采集卡、专用 USB 控制器或其他外部硬件。

项目支持触控、五指多点触控和键盘输入，可作为独立组件集成到其他桌面程序中。
设备无需越狱，也无需为 iPhone/iPad 安装或签名额外 App；首次连接仍需解锁设备、
信任此电脑，并在 iOS 18 及以上版本开启开发者模式。网络模式需要设备已启用 Apple
Wi-Fi 同步，USB 模式使用数据线直连。

## 许可

本项目采用 iUsbBridge 非商业使用许可：个人、学习、研究和其他非商业用途可永久、
无限制使用、修改和分发；任何商业用途必须事先联系作者取得单独许可。再发布时必须
保留作者署名和项目链接。完整条款见 [LICENSE](LICENSE)。

版权所有 (c) 2026 RayrenSX

项目地址：https://github.com/RayrenSX/iUsbBridge

商业许可联系：renxiang080104@gmail.com

本协议属于源码公开许可，不是 OSI 定义的严格开源许可证。

## 系统要求

本项目仅支持 iOS 18 及以上版本。iOS 17 及更早版本不在支持范围内，触控、键盘
控制和 Personalized DDI 流程均不保证可用；项目不会将这些系统版本宣传为受支持版本。

独立的 iPhone/iPad USB 触控与键盘控制组件。它不依赖 iPhoneMirror 主程序，
通过 Apple usbmuxd、CoreDevice 隧道和 Universal HID 服务向设备发送输入。

## 目录

```text
iUsbBridge/
├─ src/usb_touch_bridge.py       # bridge 运行时（stdin/stdout IPC）
├─ demo/                         # 可选 WinForms 鼠标触控演示
├─ iUsbBridge.spec               # PyInstaller 打包定义
├─ requirements.txt              # 构建依赖
├─ build.ps1                     # Windows 构建脚本
└─ docs/TECHNICAL.md             # 协议、架构和排障
```

## 快速开始

设备需安装 Apple Devices 或 iTunes 提供的 Apple Mobile Device Support，
通过数据线连接、解锁并信任此电脑。开发运行：

```powershell
python -m venv .venv
.\.venv\Scripts\Activate.ps1
pip install -r requirements.txt
python src\usb_touch_bridge.py --usb
```

对于 iOS 18 及以后版本，还需要在设备上开启开发者模式，并挂载与设备匹配的
Personalized DDI。安装包不携带 DDI；未传入 `--ddi-dir` 时，桥接器通过
从 GitHub 官方 API 动态解析当前 DDI commit 和文件清单，下载后同时校验 Git blob
身份与本地计算的 SHA-256，再通过 Apple 个性化流程自动挂载它。下载的
`BuildManifest.plist` 必须与当前 `pymobiledevice3` build 匹配；首次使用需要联网，
离线时可显式传入一个本地、官方镜像目录：

```powershell
python src\usb_touch_bridge.py --usb --ddi-dir C:\path\to\official-ddi
```

该目录必须包含 `Image.dmg`、`BuildManifest.plist` 和 `Image.trustcache`。主程序
会优先读取 `IPHONE_MIRROR_DDI_DIR`，其次读取完整的
`%LOCALAPPDATA%\iPhoneMirror\developer-image`；两个位置都不会随安装包提供。

发布构建（需要 Python、PyInstaller 和 .NET 10 SDK）：

```powershell
.\build.ps1
```

输出位于 `dist/iUsbBridge-Demo/`。`iUsbBridge.exe` 是可被其他程序复用的
控制进程，`iUsbBridgeDemo.exe` 是演示 UI。桥接器使用 onedir 运行时，发布或
复制时必须始终保留 `iUsbBridge.exe`、同目录的 `_internal/` 和
`iUsbBridge.runtime.json`；后者记录所有运行时文件的 SHA-256。也可以
直接运行：

```powershell
.\dist\iUsbBridge-Demo\iUsbBridgeDemo.exe
```

完整接口定义、生命周期事件和错误处理见 [docs/TECHNICAL.md](docs/TECHNICAL.md)；系统分层和工作原理见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

## 安全边界

本组件只使用用户态 userspace TCP 隧道，不安装内核驱动、不替换 Apple 的
`usbccgp` 驱动，也不上传设备数据。`ready` 前会验证 mainTouchscreen（Service ID
`257`）实际存在；媒体流认证被 `9021` 拒绝时，会在通过该验证后尝试 direct
Universal HID，而不是把未验证会话报告为可用。若检测到旧 DDI 缺少此 surface，
桥接器只会自动刷新一次 DDI 并重建隧道。
