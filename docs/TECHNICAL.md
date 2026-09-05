# USB Control 技术文档

## 1. 目标与边界

组件将桌面输入转换为 iOS Universal HID 报告。它只负责设备会话和输入发送，
不负责屏幕采集、视频解码、窗口渲染或驱动安装。宿主程序通过标准输入发送
帧，通过标准输出读取生命周期事件，因此 C#、Rust、Go 或 Python 均可集成。

兼容性限制：桥接器仅支持 iOS 18 及以上版本。iOS 17 及更早版本不在支持范围内，
即使设备能够建立部分 Apple 服务连接，也不保证 Universal HID 触控、键盘或 DDI 流程可用。

## 2. 会话链路

```text
宿主进程
  └─ 4 字节 LE 长度 + JSON ─► iUsbBridge
       └─ usbmuxd / Lockdown (USB)
          └─ CoreDeviceTunnelProxy
             └─ userspace TCP tunnel
                └─ RSD / RemoteXPC
                   ├─ UniversalHIDService (触控/键盘)
                   └─ DisplayService (认证 gate 探测)
```

`--usb` 严格选择物理 USB 设备；`--wireless` 严格选择 usbmux 的 Network 设备，
两者不会互相回退。UDID 未指定时使用该传输类型发现到的第一个设备。

## 3. 进程接口

启动：`iUsbBridge.exe [--usb|--wireless] [--udid UDID] [--rate-hz HZ] [--ddi-dir DIRECTORY]`。

stdin 是二进制流：`uint32 little-endian length` 后紧跟 UTF-8 JSON，单帧最大
65536 字节。stdout 为 UTF-8 JSON Lines，不要把日志写入 stdout；诊断日志写入
stderr。

### 触控消息

```json
{"schema":"iphoneMirror.touch.v2","kind":"touch_batch","seq":1,
 "timestampNs":0,"points":[{"pointerId":0,"action":"down",
 "normalizedX":0.5,"normalizedY":0.5}]}
```

`points` 为 1 至 5 项；坐标范围为 `[0,1]`；动作是 `down`、`move`、`up`。
每个逻辑 `pointerId` 会绑定到 0 至 4 的稳定 slot，释放后按序复用。坐标映射
为 `round(value * 65535)`。

### 键盘消息

```json
{"schema":"iphoneMirror.touch.v2","kind":"keyboard_batch","seq":2,
 "timestampNs":0,"usages":[4,225]}
```

`usages` 表示当前全部按住的 HID usage（不是增量），最多 30 个，范围 `[0,239]`。
退出清理阶段会发送空键盘报告并释放 5 个触控 slot。

## 4. 生命周期事件

典型顺序：`status/connecting_device` → `status/checking_developer_environment`
→ `status/mounting_developer_image` → `status/testing_developer_image_sources` →
`status/downloading_developer_image` → `status/initializing_touch` → `ready`。设备必须
开启开发者模式；若未挂载 DDI，桥接器会从 GitHub 官方 API 动态解析当前 commit 的 DDI
文件清单，校验 Git blob 身份和本地计算的 SHA-256，再请求 Apple 个性化挂载，首次使用需要联网。
`BuildManifest.plist` 的 build 必须与当前 `pymobiledevice3` 匹配。显式传入 `--ddi-dir` 时只接受本地目录中的
`Image.dmg`、`BuildManifest.plist`、`Image.trustcache`，挂载限制为 180 秒。它不在
安装目录查找或捆绑 DDI。已挂载但缺少 `mainTouchscreen` 时会发出
`remounting_developer_image`，仅自动重挂一次。

`ready` 包含 `protocol=2`、`capabilities`、`udid`、`transport`、`gateOpen` 和
`authMode`（`mediastream` 或 `direct`），且只会在已验证 `mainTouchscreen`（Service ID
`257`）后发出。`error` 表示协议、连接、DDI 或认证失败，随后退出。遇到 Apple 的
`9021` 时，桥接器先验证 direct Universal HID；只有该路径也不可用时才报告
`remote_control_unsupported_ios`。宿主必须持续读取 stdout，避免管道阻塞。

## 5. HID 报告

触控报告固定 58 字节：Report ID `0x09`，状态字节为 contact `0xC2 | slot` 或
release `0x02 | slot`，X/Y 为 little-endian u16；偏移 40 的固定字段为
`02 00 00 00`，偏移 44 为 48-bit little-endian 时间戳。该格式由
`build_touchscreen_report` 单元测试锁定。

## 6. 故障排查

| 现象 | 检查 |
| --- | --- |
| 找不到 USB 设备 | 数据线、解锁/信任提示、Apple Mobile Device Service、usbmuxd |
| `developer_mode_required` | 在设备的“设置 > 隐私与安全性 > 开发者模式”中开启并重启后重试 |
| `developer_image_download_failed` / `developer_image_download_timeout` | GitHub DDI 下载失败；检查网络后重试 |
| `developer_image_download_integrity_failed` / `developer_image_download_rate_limited` | 下载内容未通过校验或线路限流；稍后重试或显式传入官方 `--ddi-dir` |
| `developer_image_tss_failed` | DDI 已下载，但 Apple 个性化服务或设备挂载失败；检查 Apple 服务网络并保持设备解锁 |
| `touch_surface_unavailable` | 当前 DDI 未发布 mainTouchscreen（257）；桥会自动重挂一次，仍失败时重启设备后重试 |
| `remote_control_unsupported_ios` / `9021` | 媒体流被拒且 direct Universal HID 也无法使用；检查 DDI 后再试或改用蓝牙反控 |
| `bad_frame` | 长度前缀必须是 JSON 字节数，检查 UTF-8 和 65536 字节上限 |
| 键盘按键卡住 | 发送 `keyboard_batch` 且 `usages: []`，随后关闭 stdin |
| 无线模式失败 | 确认 Wi-Fi 同步已启用，设备以 Network 类型出现在 usbmux |

不要使用 Zadig 将 Apple 父设备替换为 WinUSB/libusb；本组件不需要也不会安装
内核过滤驱动。

## 7. 版本与测试

协议版本为 2，能力标识为 `iphoneMirror.usb_touch.v2` 与
`iphoneMirror.usb_keyboard.v1`。在仓库根目录运行：

```powershell
python -m unittest tests\usb_touch_logic_test.py
```

这些测试覆盖 slot 状态机、58 字节报告、坐标映射和消息校验，不需要连接真机。
