# Worker 节点抽象设计

状态：设计定稿（2026-11-27，用户批准方向）。日期：2026-11-27。
背景：fleetd 的 `hosts` 概念目前绑死了 coding agent（`max_concurrent`
= claude 并发数、`workdir` = repo checkout 根）。实际舰队里每台机器
的能力不同：dev 是 flagship+网关、work-runner 是纯执行机、imac/openclaw
是 Windows 桌面机（还有 Android 设备、GPU 机器这类非主机资源）。
需要把「节点」抽象成能力声明式的 worker，派单按能力路由。

## 核心模型

```
Worker = 节点 + 能力(Executor × N) + 信道(ssh|agent-pull) + 资源标签
```

```toml
# fleet.toml 演进形态（向后兼容：旧 hosts 字段映射为 executor=claude）
[[workers]]
name = "dev"
channel = "local"                # local | ssh:<alias> | agent (pull)
executors = [
  { kind = "claude",    max_concurrent = 4, workdir = "/home/cui/workspaces" },
  { kind = "opencode",  max_concurrent = 2 },
  { kind = "shell",     max_concurrent = 8 },   # 通用命令执行面
]
tags = ["flagship", "gateway", "linux"]

[[workers]]
name = "imac"
channel = "ssh:imac"             # 过渡；relay F2 落地后切 "agent"
os = "windows"
executors = [
  { kind = "claude", max_concurrent = 2 },
  { kind = "ahk-ui", max_concurrent = 1 },  # UI 自动化面（ahk-mcp）
]
tags = ["desktop", "vscode"]
```

## Executor 抽象（代码面）

```rust
trait Executor {
    fn kind(&self) -> &'static str;          // claude/codex/opencode/shell/ahk-ui/…
    fn probe(&self, w: &Worker) -> Load;     // 并发占用探测
    fn dispatch(&self, w: &Worker, job: &Job) -> Delivery;   // 派发
    fn state(&self, w: &Worker, job_id: &str) -> JobState;   // 回执
}
```

- 现有 `agent.rs` 的 `Agent` trait（claude/codex/opencode/pi 四个 TUI
  agent）收编为 `Executor` 的第一批实现——**接口合并，不推翻**。
- `shell` executor：通用命令执行（jobs 面已有，挂进同一抽象）。
- `ahk-ui` executor：openclaw 的 ahk-mcp（UIA 自动化）——设备类任务。
- 未来 worker 类型（编译农场、GPU 推理、浏览器 farm）= 新 Executor
  实现 + 声明到对应节点的 executors 列表，flagship 零改动。

## 派单路由演进

现状：`dispatch` 在 hosts 里挑「claude 进程数最少」的机器。
目标：`dispatch --requires <capability>` 按声明匹配：

```
fleetd dispatch task.md repo                    # 默认：任意有 claude 能力的 worker
fleetd dispatch --requires windows,vscode …     # 必须带这些 tag/能力
fleetd dispatch --executor shell "cmd"          # 通用命令
```

路由权重：内网直连 > agent 心跳活 > SSH-only；同信道内按
`max_concurrent - running` 余量选。

## 与现有面的关系

| 现有 | 演进 |
|---|---|
| `hosts`（fleet.toml） | 变成 `workers`（旧字段自动映射，兼容读） |
| `agent.rs Agent trait` | 收编为 `Executor` 首批实现 |
| `resources.rs`（host/tool/device） | 保持资源台账；worker 的 tags 对接它做能力查询 |
| headless.rs SPECS 表 | 成为 claude/codex/opencode Executor 的 launch 层 |
| relay F2 心跳 | channel = "agent" 的 worker 的信令面 |

## 实现顺序（每步独立可验收）

1. **模型落地**：fleet.toml 支持 `[[workers]]`（旧 hosts 兼容映射），
   `fleetd fleet ships` 列出 worker 与能力清单。
2. **Executor trait 合并**：agent.rs 收编 + shell executor 第一版；
   dispatch 路由按 executor 余量选。
3. **能力路由**：`--requires` 标签匹配 + `--executor` 直选。
4. **relay F2**：channel=agent 的 worker 心跳上线（vault 分发同通道）。

## 明确不做

- 不做通用调度器（K8s 替代品）——单 fleet 单 flagship 的简单路由足够。
- 不做动态节点发现（mDNS 等）——节点列表显式声明。
- 不做跨 fleet 联邦。
