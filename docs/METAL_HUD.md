# 以 Metal 性能 HUD 重启应用

DeviceHub 动作 `restart_with_metal_hud` 在当前已验证设备上重启指定 App，并通过固定的 `MTL_HUD_ENABLED=1` 请求 Apple Metal 性能浮层。它复用当前会话的 RSD，不重新发现或选择另一台设备，也不接收任意环境变量。

设备需要可用的开发者模式、兼容 DDI 和现有设备会话；目标 App 必须支持 Metal HUD。浮层可显示 App 侧 FPS、帧间隔、GPU 耗时及内存等，具体内容由系统和应用决定。它不是全系统监控，也不提高帧率或降低投屏延迟。

## 请求与结果

宿主先等待当前设备的 `ready`，再按[stdin 帧格式](TECHNICAL.md#4-stdin-帧与基础输入)发送以下 JSON。示例代次和请求编号应替换为宿主当前值：

```json
{"schema":"iphoneMirror.touch.v2","kind":"hub_request","generation":1,"requestId":"0123456789abcdef0123456789abcdef","action":"restart_with_metal_hud","arguments":{"bundleId":"com.example.game"}}
```

一次成功响应的示例：

```json
{"event":"hub_result","requestId":"0123456789abcdef0123456789abcdef","generation":1,"success":true,"result":{"status":"launched","bundleId":"com.example.game","process":{"processToken":{"processIdentifier":1234}}}}
```

`process` 是系统返回信息的有界 JSON 表示，具体字段可能随系统版本变化。`success: true` 和 `status: "launched"` 只确认启动服务返回成功，**不保证 HUD 已显示**；需要查看手机画面，投屏是否包含 HUD 还需分别验证采集路径。

| 项目 | 固定行为 |
| --- | --- |
| 参数 | 只接受 `bundleId`；空值、非法 Bundle ID 或额外字段均在访问设备服务前拒绝，返回 `INVALID_REQUEST` |
| 启动参数 | `environment={"MTL_HUD_ENABLED":"1"}`、`kill_existing=True`、`start_suspended=False` |
| 动作契约 | `readOnly=false`、`public=true`、`timeout=35` 秒 |
| 普通 `launch` | 保持 `kill_existing=False`，不向新进程传入 HUD 环境变量 |
| 失败处理 | 失败不回退普通启动，不自动重试；超时或取消后不能假设设备端尚未重启 |

该操作会关闭目标 App，未保存内容可能丢失。宿主应在用户选择的设备与 App 上执行，避免重复提交；恢复或切换代次时丢弃旧请求。启动结果不确定时先检查设备状态，再决定后续操作。

## HUD 生命周期与宿主责任

环境变量在进程启动时生效，不能作为永久或全局开关。普通 `launch` 不结束已有进程，因此对仍运行的 HUD 进程执行普通打开不会移除浮层。要恢复普通运行，应先结束目标 App，再从手机图标或普通 `launch` 启动新的进程。

部分 App 可能拒绝开发者启动，或运行时不显示 HUD。桥接器不安装手机端辅助 App、不修改目标 App 签名，也不收集历史性能曲线、CSV 或测试报告。

`public` 是供集成方选择暴露范围的契约元数据。独立 stdin/stdout 桥本身没有 HTTP 控制租约或用户鉴权；宿主负责权限、操作所有权及必要的交互。iPhoneMirror 的鉴权 HTTP Hub 接口另行要求当前控制租约，不能将该宿主规则视为桥接协议自身的能力。

## 同步与验证范围

Python `main` 的基线提交 `d70a6d10275cc9d2e63c1e62bc571288a9d6acc9` 已包含此动作。本次复核确认，iPhoneMirror 的 12 个顶层桥接运行时／辅助模块映射到本仓库 `src/` 后字节一致，无需再次覆盖源码。HUD 相关映射为：

| iPhoneMirror | iUsbBridge |
| --- | --- |
| `tools/device_management.py` | `src/device_management.py` |
| `tools/device_hub_contract.json` | `src/device_hub_contract.json` |
| `tools/bridge_runtime_check.py` | `src/bridge_runtime_check.py` |
| `tests/device_hub_test.py`、`tests/device_management_test.py`、`tests/bridge_runtime_check_test.py` | 同名测试，仅将导入根目录 `tools/` 适配为 `src/` |

这些测试覆盖固定参数的服务请求序列化、选定 RSD、普通启动、非法参数、失败清理及动作契约预检。离线运行方式见[测试说明](../tests/README.md)。离线测试和打包自检不替代目标 iOS、App 及 USB／AirPlay 画面的实测。
