# fleetd 整体设计总结（边缘任务控制平面）

> 版本 0.2（完整抽象版，拿去与其他 AI 讨论）
> 状态：待评审
> 日期：2026-11-09

---

## 0. 一句话本质

**这是一个自托管边缘任务控制平面**：一个轻量控制面（controller）把"AI agent 任务"按约束下发到多台异构节点（本地机 / Windows 真机 / 云 VPS），节点只做执行，控制面管调度、同步、清理、授权。

类比：KubeEdge/OpenYurt 的边缘模型，但只为"跑 coding agent"这一件事，单二进制、无 K8s、无 Docker。

---

## 1. 问题定义（为什么需要抽象层）

现状是**散乱的五件事混在一起**，各自手写、各自为政：

| 现状问题 | 表现 |
|---|---|
| 调度 | 任务全压本机，load 75+ 超载 5-7 倍，imac 20核/128G 全空 |
| 同步 | scp/ssh 硬编码，换后端要改代码 |
| 清理 | 无统一清理，scratch 越堆越多 |
| 授权 | claude 的模型配置/token/SSH 凭据散在各机 settings.json，改一处要 ssh 上去改 |
| 台账 | 派发记录散在 shell 日志，无对账 |

**核心矛盾**：任务在哪跑（调度）、文件怎么过去（同步）、文件怎么消失（清理）、凭据怎么给（授权）——四件事本该是四个正交抽象，现在耦合在派发函数里。

---

## 2. 整体架构（四层抽象 + 控制面）

```
┌─────────────────────────────────────────────────────┐
│  CONTROL PLANE (控制面 — fleetd 单二进制)            │
│                                                     │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────┐ │
│  │ Scheduler│  │ FileFs   │  │ Authz    │  │Ledger│ │
│  │ 调度     │  │ 文件同步 │  │ 授权     │  │台账  │ │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └──────┘ │
│       │  place(t)   │ push/pull   │ grant/revoke     │
└───────┼─────────────┼─────────────┼──────────────────┘
        │             │             │
        ▼             ▼             ▼
┌─────────────────────────────────────────────────────┐
│  DATA PLANE (数据面 — 各节点)                        │
│  Node: {id, os, arch, caps[], load, state}          │
│  ├─ 执行器: claude -p "$(task)"                     │
│  ├─ 文件区: scratch/ (fleetd-* 前缀)                │
│  └─ 凭据区: auth/ (临时, 用完即删)                  │
└─────────────────────────────────────────────────────┘
```

**正交原则**：调度不知道文件怎么传，文件层不知道授权怎么给，授权层不知道调度策略。四个抽象各自独立演进，靠一个 `Task` 结构体串联。

---

## 3. 各抽象设计

### 3.1 节点抽象 `Node`

```rust
Node {
    id,            // "imac"
    ssh,           // 连接别名（免密）
    os, arch,      // linux/windows/macos + x86_64/arm64
    caps[],        // 能力声明: ["claude","cargo","dotnet","qt","vcpkg"]
    max_concurrent,// 并发上限（容量）
    load,          // 实时负载 = running/max_concurrent（探测所得）
    state,         // online/offline/unreachable（心跳/探测）
    workdir,       // 任务工作目录
    scratch,       // 文件区
    auth_dir,      // 凭据区
}
```

**关键点**：`os/arch/caps` 让调度能按"任务需求"过滤节点（任务声明 `requires_os: windows`，只选 Windows 节点）——这就是边缘框架里 node affinity/taint 的雏形。

### 3.2 任务抽象 `Task`

```rust
Task {
    id, repo, prompt,     // 任务书内容
    requires: {           // 需求声明（调度约束）
        os?, arch?, caps[]?, // 要什么机器
        max_workers?,       // 并行度
        priority?,          // 优先级
    },
    files: { input[], output[] },  // 要同步的产物
    auth: [credential_name],       // 要授权的凭据
}
```

### 3.3 调度抽象 `Scheduler`（核心）

```rust
place(task) -> node
```

**决策 = f(task.requires, node.caps, node.load)**，优先级：

1. 过滤：节点满足 `requires`（os/arch/caps）
2. 容量：`load < max_concurrent`
3. 评分：`score = load`，取最小（least-loaded）
4. 可扩展：weighted（节点权重）、affinity（偏好某机）、history-based（按历史性能，借鉴 dispatch 项目）

**边缘计算的调度本质**：这是 bin-packing / first-fit-decreasing 家族问题，任务=箱体，节点=容器。成熟参照：KubeEdge 的 device model、Nomad 的 bin-packing。

### 3.4 文件抽象 `FileFs`（同步+清理统一层）

```rust
trait FileFs {
    fn ensure_dir(host, dir) -> Result<()>;
    fn push(host, local, remote) -> Result<()>;    // 任务书/产物上行
    fn pull(host, remote, local) -> Result<()>;    // 产物回收
    fn list_managed(host, scratch) -> Vec<ManagedFile>;  // 只列 fleetd-* 前缀
    fn remove(host, path) -> Result<()>;
    fn cleanup(host, scratch, older_than, active_ids) -> CleanReport;  // 见§3.5
}
```

**实现**：`ScpFs`（默认，跨平台）· `RsyncFs`（Linux 备选，增量快）· 可加 `GitFs`（若任务书要版本化）。后端由 `fleet.toml: sync_backend` 切换，调度/清理代码零改动。

### 3.5 清理语义（怎么保证干净）

**三条铁律**：

1. **只删自己的**：统一命名 `fleetd-<repo>-<taskid>.*`，`list_managed` 只匹配前缀，**绝不 `find -mmin +N -delete` 一刀切**（会删别人的文件）
2. **活跃任务不删**：对照台账，`state ∈ {dispatched, running}` 的文件跳过（claude 正在写的日志不删）
3. **幂等+对账**：清理幂等；每次输出「删了哪些/跳过哪些活跃」，台账记 `cleaned_at` 可对账

### 3.6 授权抽象 `Authz`（凭据生命周期）

现在 claude 的模型配置/token/SSH 凭据散在各机 `~/.claude/settings.json`，**改一台要 ssh 上去改一台**，且无法按任务隔离。抽象成凭据生命周期：

```rust
trait Authz {
    fn grant(node, credential) -> Result<Handle>;   // 注入一次性凭据
    fn revoke(node, handle) -> Result<()>;          // 用完即删
    fn verify(node, credential) -> Result<bool>;    // 校验（fail-closed）
}
```

**三类凭据，三种生命周期**：

| 凭据 | 生命周期 | 说明 |
|---|---|---|
| 环境凭据（SSH key / 网关 token） | 长效，注册时注入 | 节点身份，`grant` 一次 |
| 任务凭据（API key / 一次性 token） | 任务级 | 派发时 grant，任务完成即 revoke |
| 结果凭据（产物签名） | 短时 | 回收时校验，防伪造 |

**关键设计点**：凭据与任务**绑定**（task.id → credential），不共享；`fail-closed`——缺凭据的节点不接需要该凭据的任务。这正是 cockpit 设计文档里 `CapabilityAccess`（readonly/writable）想解决的，但这里做成独立抽象。

### 3.7 台账抽象 `Ledger`

```rust
Task { id, node, repo, state, created_at, finished_at, output_ref }
```

SQLite 持久化。调度、清理、授权都读它——它是四层抽象的**共享状态**（唯一耦合点，刻意保留）。

---

## 4. 为什么是"边缘计算"

四个特征与边缘框架同构，所以可直接借用其成熟模型：

1. **节点异构 + 无中心存储**：任务产在控制面，执行在边缘 → 控制面/数据面分离
2. **控制面轻量**：只做决策，不碰计算 → 单二进制，无 K8s/Docker
3. **NAT 后节点**：节点可 outbound 注册（WebSocket），无需入站端口 → 借 cockpit 的 agent 注册模型
4. **离线韧性**：节点断连 → 控制面把任务重派给别的节点 → 借 KubeEdge 的 device twin 概念

**可参考的成熟开源模型**（拿去讨论）：KubeEdge（device/edge node）、Nomad（bin-packing + 任务驱动）、cockpit（自有 agent 注册+心跳+job ledger）、clawd-coordinator（跨机 claude + least-loaded，已分析）。

---

## 5. 开放问题（拿去问其他 AI 的点）

1. **调度策略**：least-loaded 够吗？要不要加 weighted/affinity/history-based（按历史性能）？bin-packing 更优但复杂，值不值？
2. **任务粒度**：按任务书派，还是按文件/目录派（增量同步才有意义）？大任务（cargo build 半小时）如何避免抢节点？
3. **结果回收**：v0.1 不回收（输出留各机日志）。要不要 pull 回控制面？增量（只拉 diff）还是全量？
4. **凭据模型**：长效 vs 任务级，哪个够用？任务级凭据的注入/回收在 Windows 上怎么安全做（环境变量 vs 文件 vs DPAPI）？
5. **离线韧性**：节点掉线时任务状态机怎么走（重派/重试/死信）？这是边缘框架最难的部分，值得单独讨论。
6. **控制面高可用**：单控制面挂 = 全挂。要不要 HA（leader 选举）？还是 v0.1 接受单点？
7. **抽象层粒度**：四层抽象会不会过度设计？还是像你说的"像边缘计算"就该这么分？

---

## 6. 建议的落地顺序

```
P0（已验证）: Node + Scheduler(least-loaded) + FileFs(scp) + Ledger(SQLite) + cleanup
P1: Authz(凭据注入) + RsyncFs + ledger 子命令 + dispatch 后验证
P2: 结果回收(pull) + 离线重派 + 发布开源
```

---

## 附：已实现进度（代码现状）

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 配置加载 + 主机注册 | ✅ |
| P0 | 负载探测（local pgrep / windows tasklist） | ✅ |
| P0 | least-loaded 选机 | ✅（6 测试过） |
| P0 | dispatch 派发（scp + ssh 后台跑 claude） | ✅（已派到 imac 成功） |
| P0 | SQLite 台账 | ✅ |
| P0 | UNREACHABLE 跳过 | ✅ |
| P1 | cleanup 定时清理 | ✅（cleanup 子命令） |
| P1 | ledger 子命令 | ⬜ |
| P1 | dispatch 后验证送达 | ⬜ |
| P1 | 按 OS 过滤任务（requires_os） | ⬜（可选） |
| P2 | README + LICENSE + 发布 | ⬜ |
| P2 | 端到端演示 | ⬜ |

**实测状态**（2026-11-09）：

```
local  63/4  load 15.75   ← 超载
imac    1/4  load 0.25
dev     0/2  load 0.00   ← LEAST LOADED -> dev
```

**已验证**：imac 的 claude 通过本地 LiteLLM 4000 网关真实出结果（`"Test received — everything's working"`）——跨机派发的前提已解决。

## v0.3 完成事件面（2026-11-26，用户令：搭全局调度器，agent 完成要有钩子）

问题背景：席位「做完一个就不继续」/「卡死不自愈」的根因是**缺完成钩子**——会话结束（无论完成/卡死/限流）没有任何信号给调度器。补上事件回传面：

- `src/ledger.rs` — 抽出共享状态：tasks/events/dispatches 三表 + `classify()` 纯函数判定（completed/partial/stalled/no_progress/untracked；HEAD 前进+clean=completed、前进+dirty=partial、原地+dirty=stalled、HEAD 未知=untracked）。9 单测含大小写敏感（Git 短头是 hex，"AAA"≠"aaa"）。
- `src/events.rs` — 事件接收面：`POST /v1/events` 极简 HTTP（std-only，无框架）+ `fleetd event <seat>` CLI 回退。hook 只报告 {seat,head,open,dirty}，判定全在 fleetd。
- CLI：`fleetd serve --port 3790` 常驻监听；`fleetd status` 面板（1d 各席 fired/done/inflight/stall/partial 计数）。
- Stop hook 薄化：`~/.claude/settings.json` 注册 `hooks.Stop`（async，exit 0 永不阻塞，`stop_hook_active` 防循环）；hook 只 curl localhost，daemon 挂时回退 CLI。SQLite WAL 持久，重启不丢。
- 真机验证：serve 起 → curl POST 事件 → events/dispatches 落库 → status 面板显示 ✓；HTTP 404 ✓；19 单测绿。

---

## 核心观点

你不是在做"派任务"，是在做一个**极简边缘控制平面**。抽象成 Node/Task/Scheduler/FileFs/Authz/Ledger 六件套后，每个都能独立演进、独立替换、独立测——这正是能分享给别人的价值。
