# iUsbBridge 架构与工作原理

iUsbBridge 将宿主输入转换为 iOS Universal HID 报告，同时提供当前设备会话上的剪贴板与设备管理 RPC。当前 `main` 使用 Python/pymobiledevice3 实现，面向 Windows 与 iOS 18 及以上版本。IPC 详见 [TECHNICAL.md](TECHNICAL.md)。

## 1. 总体链路

```text
宿主程序（C# / Rust / Go / Python）
    │ stdin：uint32 LE 字节长度 + UTF-8 JSON
    │ stdout：UTF-8 JSON Lines；stderr：日志
    ▼
BridgeChannel → TouchSession
    ├─ USB：Apple usbmuxd / Lockdown
    │       └─ QuickTime 配置下可共享同一设备的用户态 USBMux
    └─ 无线：Network Lockdown / 已配对 RemotePairing mDNS
             ↓
       userspace TCP 隧道 → RSD / RemoteXPC
             ├─ Universal HID：触控与键盘
             ├─ Indigo HID：系统按键
             ├─ Pasteboard：剪贴板辅助通道
             ├─ DeviceHub：当前设备的管理服务
             └─ DisplayService：可选媒体认证与反馈
```

`--usb` 与 `--wireless` 不在控制传输之间静默回退。指定 UDID 后核对目标身份，允许连字符和大小写差异；不会采用唯一但序列号不匹配或不可读的手机。无线准备 DDI 必要时可借助同一设备的可信 USB 连接，随后仍建立无线控制。

## 2. 模块职责

| 模块 | 职责 |
| --- | --- |
| [usb_touch_bridge.py](../src/usb_touch_bridge.py) | CLI、IPC、触点状态、键盘/按键、会话建立、认证、健康检查和恢复。 |
| [ddi_support.py](../src/ddi_support.py) | 可取消的 Personalized/Cryptex 准备、校验、签名及连接清理。 |
| [mdns_discovery.py](../src/mdns_discovery.py) | 多网卡 RemotePairing mDNS 发现。 |
| [iostouch/qt](../src/iostouch/qt) | QuickTime 配置的用户态 USBMux、包组装、TCP 序列与本地兼容服务。 |
| [pasteboard_text.py](../src/pasteboard_text.py) / [pasteboard_recovery.py](../src/pasteboard_recovery.py) | 文本表示解码、转换恢复和辅助服务资源检查。 |
| [screen_orientation.py](../src/screen_orientation.py) | 只读方向观察及已有捕获 USBMux 的复用。 |
| [device_hub.py](../src/device_hub.py) | RPC 调度、取消、代次隔离与媒体 gate 反馈。 |
| [device_management.py](../src/device_management.py) / [document_file_service.py](../src/document_file_service.py) | 所选设备服务调用、文件路径与共享 Documents 边界。 |
| [device_hub_contract.json](../src/device_hub_contract.json) | 动作、读取/写入属性、暴露范围和超时的共同契约。 |
| [coredevice_media.py](../src/coredevice_media.py) | RTP/HEVC 组帧、RTCP 反馈、录制与可选解码/重放。 |
| [bridge_runtime_check.py](../src/bridge_runtime_check.py) | 冻结运行时中的离线依赖与功能验证。 |

普通 HID 和设备管理不需要外部媒体解码器。实际媒体解码/重放才使用可选的 `CoreDevice/iPhoneMirror.MediaDecoder.exe`；查找位置见调用文档。

## 3. 启动与恢复

1. 创建有界 IPC 输入队列，同时观察 stdin 关闭，允许取消尚未完成的启动。
2. 发现所选传输上的设备，建立可信 Lockdown，检查身份、开发者模式与 DDI。
3. 建立 userspace 隧道和 RSD，打开 HID，验证 mainTouchscreen（Service ID `257`）。
4. 根据设备能力使用媒体认证或经过验证的 direct HID，准备键盘并验证实际发送通道。
5. 发出 `ready`，包含 UDID、transport、capabilities 与递增的 `generation`。
6. 健康检查、空闲 HID 更新和故障恢复保持会话可用；恢复会取消旧设备管理与剪贴板操作。
7. 重新验证后发布新 `ready`；旧代次输入被丢弃。退出时有界释放所属资源。

仅发现服务名称或挂载成功不足以报告 `ready`。桥接器还会发送无害释放报告并等待同一 HID 连接的响应；媒体认证会话持续检查真实媒体流健康。键盘与触控持有状态受到生命周期锁保护，更新 HID 时不会在活动手势中替换健康连接。

## 4. 五点触控与 HID 编码

`FiveSlotStateMachine` 为逻辑 `pointerId` 分配最小空闲 slot 0–4，移动时保持 slot，释放后回收。IPC 批次表达本次变化；编码器合并保留的全部活动手指与本批释放手指，每批发送一份完整快照。省略其他活动手指的 HID 报告会使设备释放它们，因此不能用五份互相独立的单触点报告替代真正多点输入。

固定 58 字节报告布局由 `build_touchscreen_frame` 与本地测试锁定：

| 偏移 | 字节数 | 内容 |
| --- | ---: | --- |
| 0 | 1 | Report ID `0x09`。 |
| 1 | 1 | 本报告记录数，含本次释放，范围 1–5。 |
| 2 | 1 | 最大触点数 `0x05`。 |
| 3–27 | 25 | 最多五条 5 字节记录：状态/slot、X u16 LE、Y u16 LE。contact 为 `0xC0 \| slot`，release 为 `slot`。 |
| 28–39 | 12 | 滚动集合，全零。 |
| 40–44 | 5 | 按 slot 索引的 identity；所含 slot 写入 `slot + 1`。 |
| 45–50 | 6 | 48 位 little-endian 单调时间戳。 |
| 51–57 | 7 | 扩展手势标记，全零。 |

坐标按 `round(normalized * 65535)` 转换。按住期间可重复最后确认的完整快照，维持静止手指，不生成新的按下或重复键盘字符。`--rate-hz` 是提示值，实际输入频率还受宿主和设备处理能力影响。

## 5. 连接所有权与边界

QuickTime 镜像下，桥接器只管理同一设备的 USBMux 接口（子类 `0xFE`），在动态回环端口提供兼容 usbmuxd 服务；不读取视频接口 `0x2A`，不更改设备 USB 配置。接收路径组装 USB 分片、裁剪 TCP 重传、限制乱序窗口，并在缺口超时或传输失败时结束受影响连接。恢复保存的 USBMux 状态后仍须验证 Lockdown/HID，不能仅凭监听端口判活。

每项 DeviceHub 操作复用所选设备的 RSD，并拥有独立服务通道、超时和返回大小限制。文件服务限制到请求根目录，共享 Documents 先完成 House Arrest 协商再启动 AFC 读取。动作契约的 `public` 为集成元数据，宿主负责决定自身对外暴露哪些修改操作。

剪贴板是有界辅助通道，每次 PULL/SET 使用新连接并串行处理；设备恢复、前台写入与请求取消会淘汰旧结果。它不直接操作 Windows 剪贴板，宿主自行决定如何应用返回文本。

桥接器不安装内核驱动，也不依赖云端控制中转。DDI 自动准备可能访问上游下载与 Apple 个性化服务；设备管理只执行宿主明确请求的动作。断开后的释放为尽力清理，不能保证已经离线的设备收到报告。

## 6. 构建与验证

[build.ps1](../build.ps1) 使用固定 [requirements.txt](../requirements.txt) 和 [iUsbBridge.spec](../iUsbBridge.spec) 构建 Python onedir 运行时。交付时保留 EXE、`_internal` 与 schema 1 完整性清单；发布目录和自检命令见调用文档。

仓库根目录运行：

```powershell
python -m unittest discover -s tests -p '*_test.py'
```

[tests](../tests) 覆盖编码、传输重组、恢复代次、剪贴板、DDI 与设备管理，不需要连接手机。媒体二进制集成测试在缺少可选解码器时跳过。[five_finger_device_test.py](../tools/five_finger_device_test.py) 是另一个会向真机发送五指 down/move/up 的工具；运行前先检查其可执行文件路径和动作坐标，确认设备页面适合操作。
