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
