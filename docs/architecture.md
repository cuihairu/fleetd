# fleetd 架构：目的 · 功能 · 模块分层 · 进程关系

> 2026-11-27 用户拍板：按功能划分；本机部署 = agent + 独立调度器；
> flagship 是功能组合（agent + 调度器），**可关闭 agent 功能**。

## 1. 目的

一台机器（flagship 角色）控制多台机器（ship 角色）上的编码 agent
（claude / codex / opencode）：

1. **派单**：任务文件 → 席位 → agent 执行
2. **感知**：知道每个 agent 活着/推进/空闲/卡死
3. **止损**：可停席、禁席、查台账
4. **留痕**：每次派单的结果可追溯（谁、什么时候、什么结论、有没有推送）
5. **无人值守**：定时任务 + headless 派发，不依赖人的屏幕

## 2. 角色与功能组合

| 角色 | 功能组合 | 进程形态 |
|---|---|---|
| **flagship（默认本机部署）** | 调度器 + agent | 两个进程：`fleetd serve`（调度器）+ `fleetd agent`（本机席位心跳/执行环） |
| **flagship（纯控制面）** | 只跑调度器，**agent 功能关闭** | 只有 `fleetd serve --no-agent`（规划），不跑本机 agent 环 |
| **ship** | 只跑 agent | 只有 `fleetd agent --flagship <url>` |

- flagship/ship 是**角色名**，不是 crate 名；同一二进制跑不同子命令。
- 关闭 agent 功能（规划）：`fleetd serve --no-agent` 或配置
  `[flagship] agent_enabled = false`；默认本机部署两者都开。
- tray 是 Windows 桌面附加物（`fleetd tray`），登录会话里独立进程。

## 3. 功能域（按用户能感知的能力划分）

| 功能域 | 一句话 | 主要子命令 |
|---|---|---|
| 派单 | 把任务交给某台机器的某个席位 | `dispatch`、`relay queue` |
| 调度 | cron 式定时任务（替代 crontab/systemd-timer） | `job add/list/run/enable/remove/stats`、`serve` |
| 事件 | agent 结束后回报结论（Stop hook 入口） | `event`、`serve` |
| 台账 | 席位/派单/事件/任务的持久记录 | `status`、`seat list`、`fleet overview` |
| 注册与心跳 | 节点向旗舰报到、领任务 | `agent`（ship 侧心跳环） |
| 转发 | 任务队列的投递与取件 | `relay queue`、`relay alive` |
| 探测 | 机器上装了哪些工具、有什么设备 | `hosts`、`cleanup`、`tools list/install` |
| 汇总 | 一眼看全舰队在干什么 | `fleet`（表）、`GET /v1/fleet/summary` |
| 服务化 | 装成系统服务，开机自启 | `service install/uninstall/start/stop/status` |
| 托盘 | Windows 桌面上能看到它在跑 | `tray` |
| MCP | 让 agent 自己调用舰队能力 | MCP server |
| 脚本宿主 | 让钩子用真 Python 写，不用 shell 拼 | 内嵌 CPython |

## 4. 进程关系图

```
                flagship 机器
┌───────────────────────────────────────────────────────┐
│  fleetd serve（调度器进程，SYSTEM 服务/开机自启）        │
│  ├─ 调度循环（cron 式 job tick）                        │
│  ├─ 事件摄入（:3790 HTTP + ~/.fleetd/flagship.sock）    │
│  ├─ headless 派发（每任务一个 agent 子进程）             │
│  │    └─ codex exec / claude -p / opencode run          │
│  ├─ 心跳登记簿（register：ship agent_seen）             │
│  ├─ 任务队列（relay/forward：ship 来领）                │
│  ├─ 内嵌 Python（pyhost：钩子脚本）                      │
│  └─ 台账 ledger.db（seats/dispatches/events/jobs）      │
│                                                         │
│  fleetd agent（agent 进程；--no-agent 时不跑）           │
│  └─ 心跳环：向 serve 登记 + 领任务 + 驱动本机席位          │
│       └─ tmux/claude 席位（interactive 模式）            │
│                                                         │
│  fleetd tray（Windows 桌面托盘，登录会话）                │
│  └─ 拉状态：~/.fleetd/flagship.sock（AF_UNIX）           │
└───────────────────────────────────────────────────────┘
                    ▲ 心跳+领任务        ▲ 事件上报
                    │                    │（Stop hook → event 端点）
┌───────────────────┴──────┐   ┌─────────┴──────────────┐
│ ship 机器                 │   │ ship 机器（同左，省略）  │
│  fleetd agent             │   │                          │
│  └─ 席位（tmux/claude）   │   │                          │
└──────────────────────────┘   └──────────────────────────┘
```

**通信面**：
- serve ↔ tray：loopback TCP HTTP `127.0.0.1:3790`（`GET /v1/status`；2026-11-28 起 tray 不依赖 AF_UNIX，Windows 全版本可用）
- serve ↔ 外部/跨机：`:3790` HTTP（`--http` 显式开启；跨机 relay 用）
- ship agent ↔ flagship serve：心跳 + 领任务（relay 协议）
- Stop hook → serve：`fleetd event`（CLI 回退）或 curl 到事件端点

## 5. 模块分层（crate 按功能命名，无前缀）

依赖单向：上→下，禁止反向。

```
L6 接口      mcp / tray / agent
L5 生命周期  service（systemd/launchd/sc 定义 + SCM 握手）
L4 业务输出  report（summary 聚合 + fleet_view 视图）
L3 服务面    scheduler（调度循环/jobs/events/headless/pyhost）
             relay（队列/取件） + register（心跳登记）
L2 探测      probe（probe + resources）
L1 持久化    store（ledger SQLite）
L0 领域模型  core（config/topology/agent/schedule/dispatcher/verdict）
```

### 每个 crate 的功能边界

| crate | 内容 | 职能 | 不许做的事 |
|---|---|---|---|
| `core` | config、topology、agent 抽象、schedule 解析、dispatcher 决策、verdict 分类 | 纯逻辑零 IO | 不碰 SQLite/网络/进程 |
| `store` | ledger（SQLite 台账）、dispatch_io（派单副作用） | 持久化 + 写副作用 | 不做网络/调度 |
| `probe` | probe、resources | 机器事实采集 | 不写台账 |
| `register` | registry | ship 心跳登记/存活 | — |
| `relay` | forward、relay | 任务队列投递/取件 | — |
| `scheduler` | scheduler 循环、jobs、events、headless、pyhost | 调度执行面 | 不装服务（归 service） |
| `report` | summary、fleet_view | 只读聚合/视图 | 不写任何状态 |
| `service` | service 定义生成、scm_service | 服务安装/SCM 握手 | 不做调度（serve 入口用回调注入） |
| `mcp` | mcp_server、tools | agent 调用舰队能力 | — |
| `tray` | tray、tray_frames | Windows 托盘 | — |
| `agent` | agent_loop | ship 心跳/领任务/驱动席位 | — |

### 依赖边（目标态）

```
main(fleetd) → 全部
mcp        → core, register, store
tray       → (独立，socket 拉 serve 状态)
agent      → core, relay
service    → (无内部依赖；SCM 用回调)
report     → core, store
scheduler  → core, store, report, relay
relay      → core, register
register   → core
probe      → (独立)
store      → core
core       → (无)
```

### 依赖规则

- 只允许 L6→L5→L4→L3→L2→L1→L0 单向
- L0 不 use 任何上层
- L4（report）只读 L1（store），不许写
- L5（service）不许 use L3（scheduler）——serve 入口用回调注入
- 二进制 `main.rs` 只做路由，不含业务逻辑

## 6. 数据流（一次派单的完整路径）

```
用户/CLI → fleetd dispatch task.md seat
  → 调度闸门：gates 检查（noauto/disabled/agent busy/cooldown）
  → store::dispatch_io: 台账记 fired
  → 交付：headless spawn（无人值守）或 tmux 投递（interactive）
  → agent 跑完 → Stop hook → fleetd event / curl 事件端点
  → scheduler::events: 采集 HEAD/dirty → core::verdict::classify
  → store::ledger: 事件入账 + 派单收口
  → core::dispatcher::decide: 下一步（派下一个/提醒继续/续作/不动）
```

## 7. 已知缺口（诚实登记）

| 缺口 | 现状 | 补齐方向 |
|---|---|---|
| agent 运行时状态感知 | 只有 Stop hook（结束后报一次）+ ship 级心跳（机器级） | 会话级 lease + transcript 增量监听（三层模型：hook 语义 + 介质活性 + 周期对账，另立文档） |
| 会话级 lease | 无 | 无法回答"这个席位现在在不在推进" |
| transcript 增量监听 | 无 | 无法用介质活性判断 running/idle/stuck |
| 周期对账 | 只有 headless 模式读 stream.jsonl | interactive 席位无兜底 |
| flagship 关 agent | 规划：`--no-agent` / 配置开关 | serve 路由分支 + 文档 |
| tray 回退 | 已走 loopback TCP HTTP（无 AF_UNIX 依赖，Windows 全版本） | 无需回退（2026-11-28 tray 改造收口） |
