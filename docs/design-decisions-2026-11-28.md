# fleetd 设计决策记录（2026-11-28 与用户讨论定案）

> 本文档沉淀 2026-11-28 会话中的架构讨论与决策。状态标注：✅已落地 / 🟡进行中 / ⏳排队 / 💤远期预案 / ❌明确否决。

## D1. tmux 原生机制的定位与边界 ✅/🟡

**讨论起点**：用户问「tmux 难道没有后续之类的机制吗」。

**结论**：tmux 原生四层机制，fleetd 此前只用了一层。补齐后形成三层感知模型（agent-support.md §5 的实现化）：

| 层 | tmux 机制 | 感知能力 | 状态 |
|---|---|---|---|
| 语义层 | Claude Stop hook → /v1/events | 任务完成/结论 | ✅ 既有 |
| 活性层 | transcript 尾部分析（时间戳+pending tool_use） | 在跑/空闲/长命令 | ✅ presence 落地（2049793） |
| 死亡层 | `set-hook -g pane-died` + `remain-on-exit` | 席位崩溃秒级感知+退出码 | ✅ 落地（e065bb4） |
| （参考） | Format 变量 `#{pane_pid}` `#{pane_dead}` | 进程名/死活/attach 数 | ✅ dispatch_io 既有 |

**边界（重要）**：tmux 全部机制只看「终端层」（会话/pane/进程），看不到 pane 内 claude
在干什么——那层信息只在 claude hook 与 transcript。Control Mode（`tmux -C` 结构化事件流）
**暂缓**：仅当出现「席位输出实时流」硬需求再上，当前 transcript+hook+tmux-hook 三层已覆盖
状态判断全部场景（2026-11-28 决策）。

**已落地细节**（e065bb4）：
- `remain-on-exit` 全局开（serve 启动 ensure，幂等）：席位崩溃留尸体+退出码
- `set-hook -g pane-died` → POST /v1/events（kind=pane_died，observational 不裁决）
- presence 状态枚举扩展 `dead`（区别于 gone：尸体在，退出码可查）

## D2. VFS/容器化构思：用户提出「虚拟文件系统更彻底」→ 收敛为 Docker Executor 💤

**讨论链**：用户先提「构建一层虚拟文件系统更彻底」（解决任务文件记账+退出清理+越跑越慢），
后自我修正「这个构思就是 Docker 了」，再确认「不需要对实体机做操作的任务，Docker 是不错的方案」。

**决策过程记录**：
1. 自研 VFS 被双方共同否决：FUSE 仅 Linux，Windows 侧（imac/openclaw 真机席位）无成熟
   用户态方案；且 DESIGN.md 边界（单二进制、无 Docker、不做 K8s）本就排除自研运行时。
2. 用户正确指出 Docker 持久化成熟（named volume / bind mount / commit / restart 策略），
   「容器=无状态=丢缓存」的反对论点不成立——CLAUDE.md、target/、node_modules 全可 volume
   挂载，增量缓存延续。
3. 收敛结论：**任务画像分类决定席位形态**——
   - 容器友好（约 7 成）：croupier 系/boundless/kode 系/fishpond/sandtable/rillane 系/seep
     （纯代码栈、CI 在云端、无真机依赖）
   - 必须裸机（约 3 成）：beacon（WinUI 真机走查）/custos（OpenWrt+Windows 验收）/
     persona（Edge 真机）/chirp（部分）
4. **架构落点**：worker-design.md 的 Executor trait 第二实现——
   `TmuxExecutor`（现有，真机/长记忆席位）+ `ContainerExecutor`（新，纯代码席位）。
   派单路由按任务标签选择；presence/hook/记账全复用（容器内 claude 照样有 Stop hook，
   transcript volume 挂出）；文件记账用 `docker diff` 白送。

**状态**：💤 远期预案（用户令「先把目前的问题解决」）。触发条件：presence/attach/memory
队列清空后，选一个任务独立的仓（seep 或 fishpond）做单席位容器试点，验证镜像矩阵
（Go/TS/Rust/Node 基础镜像含 claude CLI+凭据挂载）→ 跑通再扩。Windows 侧席位维持 tmux。

**关联修正**：讨论中确认「越跑越慢」的量化现状——workspaces 合计 486G，其中 Rust target
约 210G（seep 118G/kode 42G/persona 24G…）。既有 `fleetd cleanup` 只清 scratch_dir 不管
workspaces 构建产物。DESIGN.md §3.4 FileFs/§3.5 三铁律（只删自己的/活跃不删/幂等对账）
仍为未实现的正确设计，作为容器化之前的过渡方案排队（写记账用 dispatches.baseline_head
+ Stop hook 时 git status 快照进台账）。

## D3. fleet 功能进度基线（2026-11-28 盘点）✅

规划 17 项功能面，当日盘点 14 项已实现、3 项（attach/memory/--no-agent/--callback）未实现。
当日队列消化后预期 16/17（attach ✅、memory 🟡、--no-agent/--callback 并入后续）。

已完成关键件：
- presence 层（2049793）：presence 表 + transcript 判定（ACTIVE/IDLE/STUCK/GONE，
  后加 dead）+ `/v1/fleet/summary` 端点 + `fleetd fleet` ANSI 表
- TMUXHOOK（e065bb4）：见 D1
- 阻塞维度（当日）：SeatEvent.blocked + decide() actionable 口径 + `fleetd blocked` 列表
  + stop-hook 上报——全阻塞席位自动 Noop 不再空烧
- 自然语言 dispatch：`fleetd dispatch "人话"` + workspaces 模糊匹配 + 送达校验

## D4. 派单/席位管理约定（当日新增）✅

- **fleetd 仓单据在 fleetd 专属会话做**（用户令：不在借来的 courier 席做）：席位=workspaces
  子目录约定对 fleetd 仓同样成立。fleetd 会话 cwd=/home/cui/workspaces/fleetd。
- **队列自动推进**（用户令「派完不需要问，直到全部完成」）：前单 commit 落地+席位 idle →
  立即派下一单；停止条件仅三种（验收做不到/需拍板安全面/连续派发失败）。
- **courier 额度耗尽**教训：单席位连续大队列任务会烧完额度，大队列应分摊到同仓专属席位。
- `fire_seat.sh` 第二参数是 tag（tasks/ 下文件名去 .md），传全路径会拼出坏路径。

## 关联队列
`~/.hermes/cache/scratch/tasks/QUEUE-2026-11-28.md`（5 单滚动状态）。

## D5. tmux 库生态调研与集成决策（2026-11-28 用户令「集成 libtmux」「测试验证 tmux-mcp」）🟡

**调研结论**（生态盘点）：
| 库 | 语言 | 形态 | 版本要求 | 评估 |
|---|---|---|---|---|
| libtmux（Rust，libtmux-rs） | Rust | 异步 typed 对象模型+ControlMode+subscribe | Rust 1.85+，tmux >= 3.3a（基础）；control-mode 需 daemon >= 3.7 | **选用**（pin =0.1.2，API 未稳） |
| tmux_interface | Rust | 同步命令 builder | 老 | 备选，无 control mode |
| libtmux（Python） | Python | ORM 式对象模型，tmuxp 同门 | tmux >= 3.2a | pyhost 侧可选 |
| tmux-mcp（fr1sk/nickgnd 等） | MCP | agent 直控 tmux | node | **已实测，见下** |

**版本硬事实（本机）**：tmux 3.6a（ubuntu 26.04 源顶）→ libtmux Rust 基础对象模型可用，
**control-mode/subscribe/pane-stream 不可用**（需 daemon ≥ 3.7 final）。升级路径=源码编译
tmux 3.7+ 至 /usr/local/bin（与系统 3.6a 并存），暂不做——control-mode 等有真实流需求再升。

**集成策略**（避免大翻面）：libtmux 进 fleetd-core 依赖，新代码（presence 对账 pane 查询等）
用 typed API；现有 tmux_run 14 处调用点不翻，新旧并存渐进迁移。

**tmux-mcp 实测（2026-11-28 编排方验证，fr1sk/tmux-mcp）**：
- 依赖干净（仅 @modelcontextprotocol/sdk + zod），npm install+build 通过
- stdio JSON-RPC 握手 ok；tools/list 14 个工具：list_sessions/windows/panes、capture_pane、
  send_keys、new_session/window、split_pane、kill_*、rename_*、get_config
- 实调 list_sessions（返回全部席位 JSON）与 capture_pane（读 fleetd 席实时屏）均正确
- 评估：**能力面=席位管理的只读+操作基本盘，缺 hook/presence 集成**。对 fleetd 的价值=
  参考其工具面设计（fleetd MCP 补 seat_capture/seat_send 时对齐）；或作为 Hermes 侧独立
  通道（不经 fleetd 直控 tmux 的旁路，调试用）。不建议直接上生产主力通道。

## D6. 席位环境类型登记（2026-11-28 用户令）⏳→FLEETD-EXECUTOR-001

用户令原文：「席位记录的时候需要记录环境类型 是tmux 还是docker 还有native 还是各种vm」。
落点：seats 表加 `executor` 列（枚举 tmux/container/native/vm，默认 tmux），dispatch 路由
按 executor 分派（container/vm/native 先占位明确报错，容器试点单来填），/v1/fleet 输出加
executor 列。与 libtmux 集成合并为 FLEETD-EXECUTOR-001（队列 #7）。

## D7. 任务状态机加对抗检查闸（2026-11-28 用户令，复盘 7 例「报完成被推翻」）✅

用户令原文：「那么这些是否可以设计到 fleet 中 分配的任务的状态机 完成之前有一次对抗检查」。
把「对抗检查后才能说完成」从编排方自觉升级为 fleetd 状态机强制闸——席位报 Completed
不直接落 done，先入 `review` 态，检查方复核效果过闸才 done，不过闸打回并记录原因。

**状态机**：dispatch（记 baseline）→ active → Stop hook 报 Completed →（gate 纯函数）
→ `review`（对抗检查态）→ review_pass（带证据）→ done；review_fail → `review_failed`
（打回原因入账，行重开，席位下轮 Stop hook 重新走 verdict）；打回 ≥2 次升级上报编排方。
非 Completed verdict（Partial/Stalled/NoProgress）不进 review，按既有语义处置。

**验收档位**（TaskSpec.acceptance，任务书里 `acceptance: <mode>` 声明）：
- `self-check`：纯文档/勾账类，Completed 直接 done；
- `adversarial`（**默认**，未声明或读不到一律按此）：入 review，编排方或检查席复核；
- `user-verify`：入 review + 事件链推「待用户亲验」（beacon 岛类任务硬门）。

**落地件**（FLEETD-GATE-001）：
- core/gate.rs 纯函数 `gate(verdict, &TaskSpec) -> GateDecision`（EnterReview/DirectDone/Reject）；
- store 迁移：dispatches 加 `state/acceptance/evidence_path/review_note/reject_count` 列
  （ALTER 容错+存量回填 done），events 加 `note` 列；ingest 在 Completed 处走闸；
- 检查面：`fleetd review list|pass|fail` CLI + MCP `fleet_review_list`/`fleet_review_decide`；
- **证据强制**：pass 无 `--evidence` 拒绝（没有证据就没有 done），evidence 落列可审计；
- 事件链：入 review 落 `review_pending`，打回落 `review_rejected`，升级落 `review_escalated`，
  滞留 >24h 落一次 `review_stale`（去重）；/v1/fleet 每席位带 `review` 计数，status 面板加
  review/bounced 列。

**边界**：检查方本单仍是编排方（Hermes）人工/半自动过闸；自动化检查器（跑测试/看截图的
review 席）是下一单。review 态超时不设自动过闸——宁可积压不可虚过。

## D8. 机器抽象 CSP 化（2026-11-28 派单 FLEETD-SCHED-MACHINE-001）✅

把 fleetd 执行链抽象成 Go CSP 模型：machine=processor、task=message、dispatch=send、
receipt=receive，channel 是唯一通信面。落地件 `core/machine.rs`（~1500 行，10 单测全绿）：
- **MailboxMachine**：每机一个 mpsc 信箱（cap=max_concurrent=N），N 个消费者任务各持一个
  Semaphore 许可；**许可先于 recv** 是要害——否则消息进等待队列，背压永不显形（真机
  演示 run1 踩过：投递 3.5s 占着槽，j2 看似"负载改道"实为信箱未满）。全满 → `MailboxFull`
  错误，Pool::route 顺次改道，全满时报错并列出背压机清单。
- **多提交竞态**：Queued 记账+jobs 表插入必须先于 try_send，Err 回滚——drainer 在另一线程，
  消息入道瞬间就可能被消费。
- **取消走专用无界通道+专职任务**：绝不排在作业后面；状态先迁移再 stop，未知作业 let-else。
- **Scheduler::run**：tokio::select! 轮 pending_rx/event_rx/ticker；ticker 需 `if 源仍开` 守卫，
  否则 `else` 臂永不可达、循环退不出（实测踩过）。
- **Executor 迁移**：`Machine` trait（id/caps/submit/status/cancel/load）+ `Executor` trait
  （kind/deliver/stop），TmuxExecutor 承接原 agent 面（stop=L1 kill-session+L2 pkill，L3
  台账闸仍由调用方组合），HeadlessExecutor 承接 headless 面逐字迁移；AgentMachine 占位
  （pull 模式，relay F2 范围外，submit 即拒）。
- **配置兼容**：hosts 配置原样可用（按 executors 派生 channel），fleet.toml `[[workers]]`
  显式声明则替换派生值。
- **接线**：`fleetd machines` CLI（活表走 /v1/machines UDS 面，`[live]`/`[static]` 标注，
  PROCS 列用 Claude 进程探针）；serve 起真池（build_machines→publish_machines→Scheduler
  30s 对账）；dispatch CLI 先走机器池、Err 回落 legacy 面逐字保底（零回归）。
- **GATE-001 挂接**：机器池只做投递 reservation（Completed→Stop hook→台账 ingest 的 review
  闸不动，复用既有链）。

**真机验收**（`tests/csp_dual_machine_demo.rs`，#[ignore] 手动跑）：LocalMachine 本席 tmux +
SshMachine work-runner 双机各 1 槽，j1 本地、j2 满载改道远端、j3/j4 入队、j5 双满背压报错、
双 pane 见任务书原文、cancel 真杀 tmux 会话——2026-11-28 四机位全绿 15.3s。
**陷阱实录**：全新目录的信任框默认选「No, exit」，投递的 Enter 会确认退出（headless
`-p` 预信任不落盘，run2/run3 连败）；解法=目录放已信任的 workspaces 根下+等待循环里
见「Yes, I trust this folder」就 Down+Enter（run4 实证）。
