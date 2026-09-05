# iUsbBridge 架构与工作原理

## 1. 定位

iUsbBridge 是一个独立的 Windows 用户态输入桥接器。它把宿主程序产生的鼠标、触摸和键盘事件转换为 iOS Universal HID 报告，并通过 Apple 官方设备服务送入 iPhone/iPad。它不负责视频采集、解码、渲染，也不安装或替换内核驱动。

核心原则是：只有真实发现并验证了 `mainTouchscreen` 服务，才发送 `ready`；任何连接、认证或服务缺失都报告为错误，不伪造可用状态。

兼容性边界：项目面向 iOS 18 及以上。iOS 17 及更早版本不属于支持目标，不能据此推断其触控或键盘功能可用。

## 2. 总体拓扑

```text
宿主程序（C#/Rust/Go/Python）
        │ stdin: uint32 LE 长度 + UTF-8 JSON
        ▼
iUsbBridge.exe
  ├─ BridgeChannel：帧校验、生命周期事件、退出清理
  ├─ TouchSession：设备会话、重试、DDI 和 HID 初始化
  ├─ FiveSlotStateMachine：pointerId → slot 0..4
  └─ HID 报告编码器：坐标和状态 → 58 字节报告
        │
        ├─ USB：Apple usbmuxd / Lockdown
        ├─ CoreDeviceTunnelProxy（用户态 TCP 隧道）
        ├─ RSD / RemoteXPC
        └─ com.apple.coredevice.hid.universalhidservice
             ├─ Service 257 mainTouchscreen
             └─ Service 512 keyboard
```

`--usb` 只选择物理 USB 记录；`--wireless` 只选择 Network/RemotePairing 记录，不在两种传输之间静默回退。

## 3. 启动与会话生命周期

1. 解析命令行参数并创建 stdin/stdout IPC 通道。
2. 通过 usbmuxd 枚举设备；指定 `--udid` 时严格匹配目标设备。
3. 使用 Lockdown 检查配对、开发者模式和设备身份。
4. 检查 Personalized DDI。缺少时，从官方清单解析并下载 `Image.dmg`、`BuildManifest.plist`、`Image.trustcache`，校验大小、SHA-256 和 Git blob 身份后请求 Apple 个性化挂载。
5. 建立 CoreDevice 隧道，连接 RSD（Remote Service Discovery）。
6. 打开 Universal HID 服务，读取服务表并确认 Service ID `257`。
7. 某些系统会拒绝 `startmediastream`（错误 9021）。此时先验证 direct Universal HID；验证成功仍可进入 `authMode=direct`，验证失败才报告不支持。
8. 输出 `ready`，宿主开始发送输入帧。
9. stdin 关闭、会话断开或出现异常时，发送空键盘报告并释放 slot 0..4，然后关闭隧道。

## 4. IPC 协议

每一帧为：`4 字节 little-endian 无符号长度` + `长度字节的 UTF-8 JSON`。长度必须大于 0 且不超过 65536。stdout 只输出 JSON Lines 事件，诊断日志写入 stderr，宿主必须持续读取 stdout 以避免管道阻塞。

触控帧：

```json
{"schema":"iphoneMirror.touch.v2","kind":"touch_batch","seq":1,"timestampNs":0,"points":[{"pointerId":1,"action":"down","normalizedX":0.5,"normalizedY":0.5}]}
```

`points` 数量为 1..5；动作是 `down`、`move`、`up`；坐标必须在 `[0,1]`。键盘帧使用同一 schema，`kind` 为 `keyboard_batch`，`usages` 是当前完整按下集合而不是增量。

## 5. 五指触控原理

`pointerId` 是宿主侧逻辑触点 ID，iOS 报告使用固定 slot。状态机按最小空闲 slot 分配 0..4；同一 pointer 在移动期间保持原 slot；释放后 slot 回收并按顺序复用。超过五个同时触点会被拒绝，不会覆盖已有触点。

每个触点独立发送一个 58 字节 `mainTouchscreen` 报告：

- Report ID：`0x09`
- contact 状态：`0xC2 | slot`
- release 状态：`0x02 | slot`
- X/Y：归一化坐标乘以 `65535` 后四舍五入，按 little-endian `u16` 写入
- 固定设备字段位于报告偏移 40
- 48 位 little-endian 时间戳位于偏移 44

一帧中的多个触点按 slot 顺序逐个写入 HID 服务；120Hz 默认速率由事件循环控制。退出时所有仍占用的 slot 都发送 release，避免设备端残留按下状态。

## 6. DDI 与服务发现

iOS 18 及以后通常要求开发者模式和匹配的 Personalized DDI。桥接器不把 DDI 打进发布包：自动模式使用缓存和受校验的下载源，离线模式由 `--ddi-dir` 显式提供官方文件。DDI 挂载成功不等于触控可用，因此 RSD 建立后仍必须检查 Universal HID 的真实服务表。

## 7. 可靠性与安全边界

- usbmux、Lockdown、隧道和 HID 初始化均有超时与有限重试。
- 发现旧 DDI 缺少 `mainTouchscreen` 时最多自动刷新一次并重建隧道。
- 输入帧严格校验 schema、kind、长度、坐标、动作和触点数量。
- 仅使用用户态 Apple 服务和 userspace TCP，不安装 WinUSB/libusb 过滤驱动。
- 不上传屏幕、触控或设备内容；桥接器只发送宿主明确提供的输入。
- `ready` 是能力承诺，必须晚于 Service ID `257` 验证。

## 8. 调试与验证

开发运行：

```powershell
python src\usb_touch_bridge.py --usb --udid <UDID>
```

运行逻辑测试：

```powershell
python -m unittest discover -s ..\iphoneMirror\tests -p 'usb_touch_logic_test.py'
```

真机五指测试脚本：

```powershell
python tools\five_finger_device_test.py <UDID>
```

看到 `ready` 且 `transport=usb`、`gateOpen=true` 后，脚本会发送五指 down、move、up 序列。`touch_surface_unavailable` 表示 DDI 或设备没有发布 Service 257；`bad_frame` 表示宿主违反 IPC 格式。
