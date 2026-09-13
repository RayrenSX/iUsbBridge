# iUsbBridge

iUsbBridge 是面向 Windows 的独立 iPhone/iPad 控制桥接器。设备无需越狱、无需安装或自签额外 App；桥接器通过 Apple usbmuxd、CoreDevice 隧道和 Universal HID 服务发送触控、键盘、系统按钮与剪贴板操作。

当前主实现已迁移到 Rust，支持：

- USB 有线控制与同一局域网内的无线控制
- 点击、拖动和最多五点触控
- 键盘按下/释放与 Home、音量等系统按钮
- Windows 到 iOS 粘贴、iOS 剪贴板读取与无线变化推送
- Personalized DDI 自动解析、下载、完整性校验和挂载
- QuickTime 投屏占用常规 usbmux 通道时的原始 USB 共存链路
- 4 字节 little-endian 长度前缀 JSON IPC

旧 Python 实现仍保留在 `src/usb_touch_bridge.py`，仅供历史兼容与协议对照；正式构建使用根目录 Rust 工程。

## 系统要求

- Windows 10/11 x64
- iOS/iPadOS 18 或更高版本
- Apple Devices 或 iTunes 提供的 Apple Mobile Device Support
- 设备已解锁并信任此电脑，且已开启开发者模式
- Rust stable MSVC toolchain；构建原始 USB 兼容层还需要 Visual Studio C++ Build Tools
- 无线模式要求设备已启用 Apple Wi-Fi 同步，并与电脑位于同一局域网

## 构建

```powershell
.\build.ps1 -BridgeOnly
```

首次构建会把 `jkcoxson/idevice` 固定到提交 `e98264c4194e6980173c576ac79a58adce95492b`，下载到被 Git 忽略的 `vendor/idevice`，随后应用 `patches/idevice-compat.patch`。该补丁增加 Indigo canceled 按钮状态以及旧版 Universal HID 服务标识兼容，不会静默跟随上游变更。

默认会先运行 Rust 单元测试。跳过测试可使用：

```powershell
.\build.ps1 -BridgeOnly -SkipTests
```

构建完整 WinForms 演示包：

```powershell
.\build.ps1
```

输出：

```text
dist/iUsbBridge.exe
dist/iUsbBridge.runtime.json
dist/iUsbBridge-Demo/
```

运行时清单使用 schema 2，并记录单文件 Rust bridge 的 SHA-256。发布或集成时应同时复制 EXE 与清单。

## 运行

USB：

```powershell
.\dist\iUsbBridge.exe --usb --udid <UDID> --rate-hz 120
```

无线：

```powershell
.\dist\iUsbBridge.exe --wireless --udid <UDID> --rate-hz 120
```

启用 Apple Wi-Fi 同步：

```powershell
.\dist\iUsbBridge.exe --enable-wifi-sync --udid <UDID>
```

指定本地 Personalized DDI：

```powershell
.\dist\iUsbBridge.exe --usb --udid <UDID> --ddi-dir C:\path\to\ddi
```

未指定 `--ddi-dir` 时，bridge 会从 `doronz88/DeveloperDiskImage` 的 `PersonalizedImages/Xcode_iOS_DDI_Personalized` 路径解析固定内容并下载，缓存位置为 `%LOCALAPPDATA%\iPhoneMirror\developer-image`。目录必须包含 `Image.dmg`、`BuildManifest.plist` 和 `Image.trustcache`。

## IPC

bridge 从 stdin 读取二进制帧：4 字节 little-endian JSON 长度，随后是 UTF-8 JSON。当前 schema 为 `iphoneMirror.touch.v2`，支持以下 `kind`：

- `touch_batch`
- `keyboard_batch`
- `button_event`
- `paste_text`
- `copy_selection`
- `read_clipboard`

stdout 每行输出一个 JSON 事件，包括 `status`、`ready`、`warning`、`error`、`clipboard_text` 和 `wifi_sync_result`。stderr 只用于诊断日志。集成方应等待 `ready` 后再发送控制帧，并在进程退出或收到不可恢复错误时重建会话。

## DDI 与数据边界

项目不分发 Apple 私有 DDI 文件。自动下载只接受指定 GitHub 仓库中的三个 Personalized DDI 文件，并校验 Git blob SHA-1、文件大小和本地 SHA-256 后原子发布到缓存。

控制数据在本机与设备之间传输。项目不安装或替换 Apple 驱动，不上传设备内容，也不创建云端中转。

## 许可

本项目采用仓库中的非商业使用许可。第三方组件及其许可证见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
