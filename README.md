# iUsbBridge

iUsbBridge 是面向 Windows 的独立 iPhone/iPad 控制桥接器。设备无需越狱，无需安装或自签额外 App；桥接器通过 Apple 设备服务发送触控、键盘、系统按钮和剪贴板操作。

`main` 维护 iPhoneMirror 当前使用的 Python 实现；Rust 实现保存在 [`codex/rust-backend`](https://github.com/RayrenSX/iUsbBridge/tree/codex/rust-backend) 分支。两者的运行时布局不同，集成时应使用对应分支的构建脚本和清单。

当前 Python 实现包含：

- USB 有线控制、RemotePairing 无线发现，以及 QuickTime 投屏时的 USBMux 共存与恢复。
- 最多五点同时触控、独立释放、键盘输入、系统按钮和双向剪贴板。
- Personalized / Cryptex DDI 准备、下载校验、超时与连接清理。
- HID 会话健康检查、后台更新、输入与剪贴板恢复。
- 设备管理 IPC：应用、进程、共享文档、文件操作、日志和设备设置。
- [选定应用的 Metal HUD 重启请求](docs/METAL_HUD.md)，固定使用 `MTL_HUD_ENABLED=1`。

## 系统要求

- Windows 10/11 x64，Python 3.13 x64；构建演示程序另需 .NET 10 SDK。
- iOS/iPadOS 18 或更高版本，设备已解锁、信任此电脑，并开启开发者模式。
- Apple Devices 或 iTunes 提供的 Apple Mobile Device Support。
- 无线控制需要完成设备配对，并与电脑位于同一局域网。

## 开发运行与测试

```powershell
python -m venv .venv
.\.venv\Scripts\Activate.ps1
python -m pip install -r requirements.txt
python src\usb_touch_bridge.py --check-runtime
python -m unittest discover -s tests -p "*_test.py"
python src\usb_touch_bridge.py --usb --udid <UDID>
```

回归测试使用模拟设备服务，不操作真实手机。可选的 CoreDevice 解码器集成测试在未提供外部解码器时跳过，详见 [tests/README.md](tests/README.md)。

## 构建

Python 构建需要两个固定版本的 Windows x64 USB 后端。把下列文件放在 `native/`，或通过 `-NativeDirectory` 指定包含这两个文件的目录：

| 文件 | SHA-256 |
| --- | --- |
| `libusb0.dll` | `4F18B5D2C28AA66B648C8683C6D09B52B92CBBEE85984BBEFAD5F38A64BC2A14` |
| `libusb-1.0.dll` | `5072054CB3002AE071F382AD5C2C2B0092D9451C537D0C13444C2B6F968F7251` |

这与 iPhoneMirror 的固定载荷一致：分别来自 `src/DriverInstaller/Assets/libusb-win32-1.2.6.0/amd64/` 和 `third_party/libusb/bin/x64/`。DLL 不提交到本仓库。构建会校验哈希，并通过自定义 PyInstaller hook 将两个后端打包到 `_internal/`。

```powershell
.\build.ps1 -BridgeOnly -NativeDirectory C:\path\to\usb-dlls
# 包含 WinForms 演示程序：
.\build.ps1 -NativeDirectory C:\path\to\usb-dlls
```

默认构建环境位于 `work/usb-touch-bridge-python`；可用 `-EnvironmentPath` 指定其他环境。输出如下：

```text
dist/iUsbBridge/iUsbBridge.exe
dist/iUsbBridge/iUsbBridge.runtime.json
dist/iUsbBridge/_internal/
dist/iUsbBridge-Demo/                 # 未指定 -BridgeOnly 时生成
```

构建会执行冻结程序的 `--check-runtime`。清单使用 schema `1`，记录运行时文件的 SHA-256；发布时必须一起保留 EXE、清单和 `_internal/`。`ready` 事件的协议号为 `2`，与清单版本含义不同。

## 接入

stdin 接收四字节 little-endian 长度前缀的 UTF-8 JSON，stdout 输出 JSON Lines 事件，诊断日志写入 stderr。宿主应等待 `ready`，并持续读取事件。完整参数、消息格式、设备管理请求与错误处理见 [docs/TECHNICAL.md](docs/TECHNICAL.md)，实现分层见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

仓库不分发 Apple DDI 文件。桥接器可使用本地镜像、缓存或经校验的下载完成准备，Apple 个性化仍可能需要联网。可选 CoreDevice 媒体解码器由 iPhoneMirror 单独构建，普通 HID 控制不依赖它。

## 同步来源

本轮从 iPhoneMirror 的 `main`（提交 `cd7bdd5` 及 2026-11-18 工作区中的设备管理改动）同步 Python 运行时、打包配方及相关回归测试；包括 `restart_with_metal_hud` 契约和实现。独立仓库原有未提交修复已逐项核对，当前实现保留了对应功能。

## 许可

采用 [iUsbBridge 非商业使用许可](LICENSE)。版权所有 (c) 2026 RayrenSX；商业用途需事先联系作者取得许可：renxiang080104@gmail.com。第三方来源和许可证见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
