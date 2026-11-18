# iUsbBridge 调用与技术文档

iUsbBridge 是独立的 Windows 用户态设备桥接进程，支持 iOS 18 及以上版本的触控、键盘、系统按键、剪贴板和设备管理。宿主通过 stdin/stdout 通信，无需依赖 iPhoneMirror 主程序。协议中保留的 `iphoneMirror.*` 名称用于兼容现有客户端。

源码入口为 [usb_touch_bridge.py](../src/usb_touch_bridge.py)，架构见 [ARCHITECTURE.md](ARCHITECTURE.md)。

## 1. 运行与部署

安装 [requirements.txt](../requirements.txt) 后可直接运行源码：

```powershell
python src\usb_touch_bridge.py --usb --udid "目标设备UDID"
```

通过 [build.ps1](../build.ps1) 构建后，独立桥接器位于 `dist/iUsbBridge/iUsbBridge.exe`。PyInstaller 使用 onedir 布局，复制或发布时须一起保留 `iUsbBridge.exe`、`iUsbBridge.runtime.json` 和 `_internal/`。运行时清单 schema 为 `1`，记录路径与 SHA-256；它与应用 IPC 的协议号 `2` 含义不同。

```powershell
.\dist\iUsbBridge\iUsbBridge.exe --help
.\dist\iUsbBridge\iUsbBridge.exe --check-runtime
```

离线自检不会连接手机。成功返回 `runtime: "ready"` 和 `functional_checks`；其中 `simultaneous_contacts_independent_release` 验证完整多触点报告及独立释放。自检通过不表示已经与手机建立会话。

## 2. 参数与设备选择

| 参数 | 行为 |
| --- | --- |
| `--usb` | 默认模式，仅使用 USB 控制路径。 |
| `--wireless` | 使用 usbmux `Network` 记录或已配对的 RemotePairing mDNS 隧道，不回退为 USB 控制。 |
| `--udid UDID` | 指定设备；省略时按所选传输的发现结果选择。多设备宿主应始终指定。 |
| `--rate-hz HZ` | 输入速率提示值，默认 `120`；不自动生成固定频率输入。 |
| `--ddi-dir DIRECTORY` | 显式提供 Personalized DDI 三文件目录。 |
| `--hide-native-keyboard` | 请求连接外接键盘并隐藏设备原生键盘；默认保留原生键盘。实际能力以 `ready` 为准。 |
| `--check-runtime` | 离线依赖与功能自检后退出。 |
| `--check-setup --udid UDID` | 只读检查已有信任、开发者模式和无线控制设置后退出。 |
| `--observe-orientation --usb --udid UDID` | 只读观察设备界面方向；该模式输出方向记录，不使用通常的输入会话事件流。 |
| `--enable-wifi-sync --udid UDID` | 通过可信 USB 启用 Apple Wi-Fi 同步后退出，会修改设备设置。 |
| `--device-action restart\|shutdown --usb --udid UDID` | 对指定可信设备执行一次重启或关机，不因结果不确定而重发。 |
| `--replay-media FILE --media-pipe PIPE` | 无设备媒体重放；另需可选解码器，见第 8 节。 |

`--usb` 与 `--wireless` 互斥。USB 首次使用需要数据线、解锁设备并信任电脑，通常还需 Apple Devices 或 iTunes 提供的 Apple Mobile Device Support。无线首次使用应先完成可信 USB 初始化与配对，启用 Wi-Fi 同步，并允许局域网 Bonjour/mDNS（UDP 5353）。缺少配对记录时返回 `wireless_remote_pairing_required`。

指定 UDID 后会核对设备身份，允许连字符和大小写差异；不会仅因为只有一台手机，就接管序列号不匹配或不可读的 USB 设备。QuickTime 镜像配置下可以使用同一设备的用户态 USBMux 接口，但不改变 USB 配置、不接管视频接口。

## 3. 生命周期与恢复

通常输入会话的 stdout 是 UTF-8 JSON Lines，stderr 为诊断日志，宿主应持续读取两者。启动状态可能包含 DDI 准备，不保证固定顺序或每项都出现。

```json
{"event":"ready","protocol":2,"capabilities":["iphoneMirror.usb_touch.v2","iphoneMirror.usb_keyboard.v1","iphoneMirror.clipboard.rpc.v1","iphoneMirror.hub.rpc.v1","iphoneMirror.native_keyboard.v1"],"udid":"目标设备UDID","rateHz":120,"gateOpen":true,"authMode":"direct","transport":"usb","generation":1,"nativeKeyboardSupported":false,"nativeKeyboardVisible":false}
```

`ready` 只在验证 mainTouchscreen（Service ID `257`）、发送无害释放报告并收到同一 HID 连接的响应后发出。`authMode` 为 `mediastream` 或 `direct`；媒体认证返回 `9021` 时，可以使用实际验证通过的 direct HID，不会仅凭服务名称报告就绪。

| 事件 | 含义 |
| --- | --- |
| `status` / `warning` | 准备进度或非致命诊断；按 `code` 处理，不解析自然语言 `message`。 |
| `ready` | 当前代次可发送输入。恢复后再次发出，并递增 `generation`。 |
| `user_action_required` | 需要用户在手机上完成操作，例如开启开发者模式。 |
| `error` | 启动、协议或会话失败；结合后续状态和进程退出判断。 |
| `clipboard_text` | 后台或无请求编号的主动读取返回文本，可带 `readId`。 |
| `clipboard_read_started` / `clipboard_read_finished` | 剪贴板读取的生命周期标识，用于判断结果是否过时。 |
| `clipboard_result` / `hub_result` / `native_keyboard_result` | 与请求编号对应的结果，含 `generation`、`success`。 |

新宿主应给每个会话请求附上最新 `generation`。旧代次消息会被丢弃；没有该字段的旧客户端仍兼容，但无法依靠桥接器过滤自己的过期队列。恢复时暂停输入、丢弃旧队列，重新验证 `ready` 的 UDID、传输、协议与代次后恢复，不重放旧按键或粘贴。

关闭 stdin 可取消启动或结束会话。桥接器会有界释放所属触点、键盘、系统按键与服务连接；设备已经断开时无法保证释放报告送达。

## 4. stdin 帧与基础输入

每帧为 `uint32 little-endian JSON字节数`，后接 UTF-8 JSON 对象。长度大于 `0`，最大 `4 MiB`，不包含前缀。输入队列有界，宿主应合并高频移动，避免积压。

```python
import json
import struct

def send(process, message):
    raw = json.dumps(message, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    process.stdin.write(struct.pack("<I", len(raw)) + raw)
    process.stdin.flush()
```

```json
{"schema":"iphoneMirror.touch.v2","kind":"touch_batch","generation":1,"seq":1,"points":[{"pointerId":0,"action":"down","normalizedX":0.5,"normalizedY":0.5}]}
{"schema":"iphoneMirror.touch.v2","kind":"keyboard_batch","generation":1,"seq":2,"usages":[4,225]}
{"schema":"iphoneMirror.touch.v2","kind":"keyboard_batch","generation":1,"seq":3,"usages":[]}
```

触控 `seq` 与 `pointerId` 为非负整数；每批 `points` 为 1–5 项，ID 不重复；动作是 `down`、`move`、`up`；坐标为 `[0,1]` 内有限数值，映射为 `round(value * 65535)`。可选 `timestampNs` 为整数；省略时采用单调时钟生成 HID 时间戳。

同一 pointer 在移动期间保持 slot 0–4。输入批次可以只包含变化的触点，桥接器保留其余活动状态，并编码为包含全部活动触点及本批释放触点的单份 HID 报告。一个手指的 `up` 不会释放其他仍按下的手指。

键盘 `usages` 是当前全部按住的键，最多 30 项，范围 `0–239`；`[]` 释放全部。`4` 为 A、`40` 为 Enter、`41` 为 Esc、`224` 为左 Ctrl、`225` 为左 Shift。键盘所有权交接可发送空集合并加 `releaseAll: true`，同时取消未完成的键盘粘贴。

系统按键使用同一 schema，`kind` 为 `button_event`，包含 `seq`、`usagePage`、`usageCode` 和 `state`。page/code 范围为 `0–65535`，state 为 `down`、`up`、`canceled`。Home 的 `usagePage=12`、`usageCode=64`，还支持单次 `double_press` 请求。按下与释放应配对，不以重复重试模拟不确定的按键结果。

## 5. 剪贴板与原生键盘

| kind | 字段与返回 |
| --- | --- |
| `paste_text` | `text` 字符串；写入设备并发送粘贴快捷键。 |
| `read_clipboard` | 无编号时通过 `clipboard_text` 发布；带 `requestId` 时返回专用 `clipboard_result`，其中有 `text`。 |
| `write_clipboard` | `requestId`、`text`；只写剪贴板，不发送粘贴按键。 |
| `cancel_clipboard` | `requestId`；取消相应进行中请求。 |
| `native_keyboard_visibility` | `requestId`、布尔 `visible`；返回 `native_keyboard_result`，以实际 `visible` 和 `success` 为准。 |

带编号的剪贴板请求与原生键盘请求使用 1–64 位 ASCII 字母数字 `requestId`，并携带输入 schema 和最新 `generation`。带编号的剪贴板写入/粘贴文本最多 64 KiB。原生键盘切换还需 `ready.nativeKeyboardSupported`；键盘回退通道并不代表支持切换。

USB 与无线会话均后台轮询剪贴板，每次 PULL/SET 使用独立连接并串行访问。失败会退避；恢复、前台写入和取消会使旧读取结果失效。剪贴板请求有独立期限，失败不等于 HID 已断开。写入超时的 SET 可有界重试，粘贴按键不会自动重复。宿主负责把返回文本写入自己的系统剪贴板，并处理本地新复制与旧设备结果的竞争。

## 6. DeviceHub RPC

设备管理复用当前已验证的所选设备 RSD，不创建另一台设备的隐式会话。动作及 `readOnly`、`public`、`timeout` 元数据以 [device_hub_contract.json](../src/device_hub_contract.json) 为准，当前为 contract `1`。`public` 是供集成方选择暴露范围的元数据；桥接层仍可处理标为 false 的内部动作，宿主应决定允许调用的操作和所需用户交互。

```json
{"schema":"iphoneMirror.touch.v2","kind":"hub_request","generation":1,"requestId":"0123456789abcdef0123456789abcdef","action":"apps","arguments":{}}
{"event":"hub_result","requestId":"0123456789abcdef0123456789abcdef","generation":1,"success":true,"result":{"apps":[]}}
```

`requestId` 必须是 32 位小写十六进制且当前未使用。失败返回稳定 `code`；用同一编号的 `cancel_hub` 取消操作。动作包括设备信息、应用与进程、文件/共享 Documents、描述文件、设置、位置、日志和电源命令。`restart_with_metal_hud` 只接受指定应用 bundle ID，使用固定 `MTL_HUD_ENABLED=1` 环境重启，不透传任意环境变量。

请求排队、结果大小和期限均有界，恢复会取消旧操作。超时或取消不保证设备端写入尚未发生，客户端不应盲目重发安装、删除、启动或电源命令。字段定义见 [device_management.py](../src/device_management.py)。

## 7. DDI 准备

正常控制需要设备开发者模式及可用的开发者服务。USB 会话可请求显示手机上的开发者模式开关，成功后发出 `developer_mode_revealed`，用户仍需手动开启、重启并确认。只读配置检查和无线控制不会通过该请求开启设置。

Personalized 流程接受 `--ddi-dir` 中的 `Image.dmg`、`BuildManifest.plist` 和 `Image.trustcache`，也兼容 `IPHONE_MIRROR_DDI_DIR` 与已有缓存。自动候选使用固定版本身份及受校验下载，核对大小、Git blob/SHA-256 和 manifest，再请求 Apple 个性化签名。

iOS 27 优先使用 [ddi_support.py](../src/ddi_support.py) 的 Cryptex 准备流程，检查 `ddi/Xcode_iOS_DDI_Cryptex`、缓存与固定上游下载。独立构建不自动提供 DDI 文件；`--ddi-dir` 仍指 Personalized 三文件目录。Cryptex 镜像拒绝或兼容性失败可在同一期限内回退 Personalized，设备信任、网络或超时等失败不触发该兼容回退。无线缺少 HID 时可通过同一 UDID 的可信 USB 连接准备 DDI，再重建无线会话。挂载成功不能替代实际 HID 验证。

## 8. 可选媒体功能

[coredevice_media.py](../src/coredevice_media.py) 提供 RTP/HEVC 组帧、反馈、录制与重放。正常触控、键盘及 DeviceHub 无需媒体解码器。实际媒体解码或重放另需兼容的 `iPhoneMirror.MediaDecoder.exe`：源码运行从 `dist/CoreDevice/` 查找，冻结程序从 EXE 同级的 `CoreDevice/` 查找。本仓库不包含该外部解码器的构建工程。

`--replay-media` 配合 `--media-pipe` 输出到宿主创建的 Windows 命名管道，管道名须匹配 `iPhoneMirror.CoreDevice.` 加 32 位小写十六进制；可用 `--replay-speed` 设置速度。缺少解码器时媒体操作明确失败，不影响普通 HID 使用。

## 9. 排障与测试

| 错误/现象 | 检查 |
| --- | --- |
| `apple_device_not_found` / `apple_device_locked` | UDID、数据线、解锁、信任和 Apple Mobile Device Service。 |
| `wireless_remote_pairing_required` / `wireless_device_not_discoverable` | 可信 USB 初始化、局域网与 mDNS 防火墙。 |
| `developer_image_download_*` | 上游可达性与完整性校验；也可提供可信本地 Personalized 镜像。 |
| `developer_image_tss_failed` / `developer_image_tss_rejected` | 设备解锁、Apple 个性化服务及镜像兼容性。 |
| `touch_surface_unavailable` | DDI 没有 mainTouchscreen（257）；查看准备及回退诊断。 |
| `bad_frame` / `bridge_input_overflow` | UTF-8 JSON 字节长度、4 MiB 上限及宿主输入积压。 |
| 恢复后旧输入无效 | 使用新 `ready.generation`，丢弃旧队列并重新发送当前需要的状态。 |

在仓库根目录、已安装固定依赖的 Python 环境运行本地离线测试：

```powershell
python -m unittest discover -s tests -p '*_test.py'
```

[tests](../tests) 覆盖多点报告、剪贴板、DDI、USBMux、恢复与设备管理，不依赖兄弟仓库。可选解码器未提供时，对应二进制集成测试跳过。
