# fleetd × Code Agent：支持模型与未来模式（一次性整理）

> 2026-11-27 用户点名整理：①不同 code agent 的 hook 不一样；②不只 tmux，还有无
> tmux 模式；③调度器对接上游调用方（Hermes / openclaw / 聊天机器人）；
> ④两个进程（serve 调度器 + agent 执行环），不再细拆。

## 0. 两个进程（已拍板，不再细拆）

| 进程 | 职责 | 部署 |
|---|---|---|
| `fleetd serve` | 调度器：闸门/派单/事件摄入/台账/定时任务/队列 | flagship 必跑；ship 不跑 |
| `fleetd agent` | 执行环：心跳/领任务/驱动本机席位 | ship 必跑；flagship 默认也跑（可 `--no-agent` 关掉） |

同一个二进制；`service install` 注册 serve 为系统服务。

## 1. Code Agent 支持模型（核心抽象）

fleetd 不绑定某一个 agent。对 fleetd 来说，一个 code agent = 三件事：

```
1. 启动方式   怎么把任务交给它（spawn 参数/会话注入）
2. 状态介质   它把"我在干嘛"写到哪里（transcript/事件流/stdout）
3. 结束信号   它跑完了怎么告诉别人（hook 机制，各家不同）
```

这三件事用 **AgentDriver** trait 描述，每种 agent 一个实现：

| Agent | 启动 | 状态介质 | 结束信号（hook 差异） |
|---|---|---|---|
| **claude（interactive/tmux）** | tmux send-keys 注入任务文本 | `~/.claude/projects/<dir>/*.jsonl`（transcript 增量=活性） | **Stop hook**：settings.json 配 hooks.Stop，命中时回调 `fleetd event` |
| **claude（headless）** | `claude -p --output-format stream-json` 一次一个进程 | `stream.jsonl`（spawn 时重定向，事件即真相） | 进程退出码 + stream 里的 result 事件；**无需 hook** |
| **codex** | `codex exec --json "<task>"` | `stream.jsonl`（`--json` 全事件流） | 无 hook 体系；**进程退出 + JSONL 末尾 event** 即完成 |
| **opencode** | `opencode run --format json "<task>"` | `stream.jsonl` | 同上，无 hook |
| **自定义/未来 agent** | 实现新 Driver 即可 | 实现介质读取 | 实现结束信号 |

**关键设计：结束信号统一收口到同一入口。** 不管 hook 也好、进程退出也好、
JSONL 尾事件也好，最终都变成同一个 `SeatEvent{seat, head, open, dirty, kind}`
进 `fleetd event` → verdict 分类 → 台账。调度器只认这个统一面，不感知各家差异。

### 1.1 hook 差异的适配层

```
┌─────────────┐   各家原生机制    ┌──────────────────┐   统一事件    ┌──────────┐
│ claude Stop │ ───settings────▶ │                  │             │          │
│ codex exit  │ ───进程监控─────▶ │ fleetd agent 侧  │──SeatEvent─▶│ serve 事件│
│ opencode exit│──进程监控──────▶ │ hook-adapter     │             │ 端点/台账 │
│ 自定义       │──自选──────────▶ │ (每家一个小适配器)│             │          │
└─────────────┘                  └──────────────────┘             └──────────┘
```

- claude：**唯一有原生 hook 的**。安装席位时自动写 settings.json 的
  `hooks.Stop`，指向 `fleetd event --seat <seat>`。
- codex / opencode：没有 hook。fleetd **spawn 时就知道进程 pid 与输出文件**，
  由 agent 进程监视：退出码 + 流末尾标记 → 构造 SeatEvent 上报。
  （headless.rs 现状已实现这条：stream.jsonl 是 single source of truth）
- 兜底（对全部 agent 生效）：介质停更 N 秒判 stuck、进程消失且无事件判
  interrupted——**hook 丢了也能被对账捞回来**。

## 2. 两种执行模式（tmux 与 无 tmux 并存）

| | interactive（tmux） | headless（无 tmux） |
|---|---|---|
| 适用 | 人要看屏/要交互的长任务 | 无人值守批量任务 |
| 生命周期 | 常驻 REPL，任务注入后等 Stop hook | 一次一进程，结束即完 |
| 状态介质 | `~/.claude/projects/*.jsonl` | spawn 时指定的 `stream.jsonl` |
| 结束信号 | Stop hook（claude）/进程监视（其他家） | 进程退出码 + 流末事件 |
| 崩溃恢复 | 席位还在，补一次注入/续作令 | 任务重派（台账有 fired 记录） |
| fleetd 组件 | `fleetd agent` 的 interactive 面 | `scheduler::headless`（已实现：setsid+nohup，断连不死） |

同一任务两种模式跑的是同一份 `task.md`、走同一套闸门/台账/verdict。
**模式是派单参数，不是两套系统**：`fleetd dispatch --mode headless|interactive`。

## 3. 调度器的上游对接（谁来派单）

`fleetd serve` 是一个**服务**，任何上游都能派单。三个入口，一个契约：

```
上游 A: Hermes（本 agent 生态）      → MCP tools / CLI
上游 B: openclaw 等其他 agent 平台   → MCP tools / CLI
上游 C: 聊天机器人（微信等）         → 通过 Hermes 转接，或直接 HTTP
                ↓ 全部落到同一个契约
   fleetd dispatch task.md seat     （任务文件 + 目标席位）
   或 POST :3790 /v1/dispatch       （HTTP 面，--http 开启时）
                ↓
   闸门 → 台账 → 交付 → 事件回环
```

- **契约优先 CLI**：`fleetd dispatch <task.md> <seat>` 是唯一权威入口；HTTP/MCP
  都是它的投影。任何上游能 shell 到机器就能派单，不要求装 SDK。
- **MCP 已有**：`mcp::mcp_server` 把 dispatch/seat/status 暴露成 MCP tools——
  Hermes、openclaw 这类 MCP client 直接调用，不需要各自写适配。
- **聊天机器人**：不在 fleetd 内实现 IM 协议。机器人收到指令 → 生成 task.md →
  调 CLI 或 HTTP。IM 会话的回复走向也由上游负责；fleetd 只保证台账可查询
  （`fleetd status` / `/v1/fleet/summary`），上游拉取后转述给用户。
- **回调通道**：任务完成后上游要被通知的（如机器人回话），两种方式：
  ①上游轮询 `status`；②将来在 dispatch 时带 `--callback <url>`，事件摄入后
  serve 回调该 URL（规划项）。

## 4. 会话接入与共享（attach / 记忆 / session 复用）

### 4.1 任意位置 attach 一个会话

> 2026-11-27 用户实测否决：tmux attach 程序化透传 = 全乱码（ANSI 重绘序列），
> stream.jsonl 直接 cat 也不可读。**attach 只做接入编排，绝不透传原始终端流。**

任何能连到 serve 的地方都可以接入某个正在跑的会话。`fleetd attach <seat|run-id>`
根据台账里记的会话形态，给出**该形态下唯一不乱码的接法**：

| 会话形态 | 人要看（真终端） | 程序/远程要看（只读） |
|---|---|---|
| interactive（tmux） | fleetd 打印一条命令：`ssh <host> -t tmux attach -t <session>`——**人自己带真终端去接**（tmux attach 只在真 TTY 下正常，这是它的原生用法，fleetd 不代理） | `fleetd attach --snapshot <seat>`：`tmux capture-pane -p` 抓当前屏（干净单帧文本）；持续旁观用下面的 transcript 面 |
| headless claude（`--bg`） | 同上打印 `claude attach <session-id>` | **transcript 面**：agent 进程 tail `~/.claude/projects/*.jsonl`，解析出 assistant 文本块按时间输出（不是 cat 原始 JSONL——是解析后的可读流） |
| codex / opencode headless | 进程已结束/无交互面，无 attach 意义 | **stream 解析面**：tail stream.jsonl 并解析（同上，输出可读文本不是原 JSON）；结束后给最终结论 |

设计约束：
- **台账是路由表**：spawn 成功瞬间把 `{seat, pid, transcript_path, tmux_session, session_id}` 写进 ledger；attach 查台账选接法。
- **可读化责任在 fleetd**：JSONL/ANSI 流一律由 fleetd 解析成纯文本再输出（事件行、assistant 消息、结论），**绝不原样转发转义序列**。
- **权限分级**：`--read-only` 只旁观；交互注入仍走派单通道（task/续作令），不开裸键盘转发。
- 人不在场时同样可用：聊天机器人上游 attach → 拿到解析后的可读文本 → 流回给用户。

### 4.2 记忆与 session 共享

两个维度，分开设计：

**① 同一 agent 的跨会话记忆（project memory）**
- claude 的 CLAUDE.md / auto-memory、codex 的 instructions、opencode 的规则文件
  都按项目目录生效——fleetd 保证同一 seat 固定在同一 workdir 跑，
  项目记忆自然延续；**fleetd 不劫持不代管这些文件**。
- 派单时的上下文注入：dispatch 携带的 task.md 可引用台账里的历史结论
  （`fleetd status` / 事件记录），把"上次干到哪"喂给下一轮。

**② 跨席位/跨机器的 session 共享（fleet 级）**
- 会话转录是共享源：seat 的 transcript/stream.jsonl 路径进台账后，
  其他席位派单时可以挂引用（"参考 seat-A 的 run-42 结论"），
  serve 提供只读检索面（按 run-id / seat / 时间窗查事件与结论）。
- 全局记忆库（planned）：serve 侧一个 fleet-memory 约定面——上游（Hermes 等）
  写入的结构化笔记按 tag 存取，派单时自动随 task.md 附带相关条目。
  先做约定与存储位（ledger 加 memory 表），机制后续再扩。

### 4.3 与现有面的关系
- `fleetd status` / `fleet overview`：回答"有哪些会话、什么状态"——attach 的入口索引。
- presence lease（感知模型）：回答"会话此刻活不活跃"——attach 前的判活依据。
- MCP tools：把 attach/记忆查询暴露给 Hermes/openclaw（`fleet.attach`、
  `fleet.memory.query`，planned）。

## 5. 感知模型（hook 不够，三层合一）

| 层 | 来源 | 回答的问题 |
|---|---|---|
| 语义层 | Stop hook / 进程退出 / 流末事件 | 任务**完成没有**、结论是什么 |
| 活性层 | transcript/stream 文件 mtime+size 增量 | **此刻**在不在推进（running/idle/stuck） |
| 对账层 | 周期扫描：pid 存在性 + 台账 + 介质三方交叉 | hook 丢了/漏报了，兜底修正 |

会话 lease：agent 进程为每个席位登记 `{seat, pid, transcript, last_output_ts}`，
serve 侧 `GET /v1/fleet/summary` 直接回答"每个席位现在的状态"。
（这是当前最大缺口，接下来按此设计实现 presence crate。）

## 6. 落地顺序

1. ✅ 两进程模型 + crate 分层（本次重构收尾）
2. hook-adapter 统一 SeatEvent 入口（claude hook 已通；codex/opencode 走进程监视）
3. presence：lease 表 + 介质活性 + 周期对账（补感知缺口）
4. `--mode headless|interactive` 派单参数统一两种执行模式
5. `fleetd attach <seat|run-id>`：台账路由 + 只读/可交互分级（§4.1）
6. 记忆共享：台账挂引用检索 → fleet-memory 约定面（§4.2）
7. `--no-agent`：flagship 纯控制面
8. `--callback`：派单回调（聊天机器人回话用）
