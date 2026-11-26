# fleetd 席位控制权（seat control）设计

用户令（2026-11-26 深夜）：claude 席位必须完全受 fleetd 控制，杜绝
「用户说停了席位还在跑/会话被自动重建」这类失控。falcon 事件已定性：
席位会话脱离控制自主 commit+push，停止令没有执行闭环。

## 目标：一个控制面，两种权力

fleetd 是**唯一**有资格启动/停止 claude 席位的组件。人对 fleetd 下令，
fleetd 对 tmux/claude 执行。其他一切路径（手工 fire_seat、散脚本、
schtasks 残留）全部收编或废除。

### 权力 1：绝对停止（stop = 三层斩）
`fleetd seat stop <name>` 一次执行：
1. **杀会话**：`tmux kill-session -t <name>`
2. **杀进程**：`pkill -f "workspaces/<name>"`（pane 内 claude 及子进程）
3. **写闸**：台账 seats 表 `status='disabled'`——闸门状态的席位，
   一切自动启动路径（idle-watch / fire / agent 心跳 / 巡检续作）见到
   disabled 必须跳过，这是硬规则不是惯例。

「退出会话」= stop。没有只杀会话不杀进程、或只杀进程不写闸的中间态。
三层缺一即失控（falcon 复活事故的根因就是只做了第 1 层）。

### 权力 2：受控启动（start 唯一入口）
`fleetd seat start <name> [--task <file>]`：
- 闸门检查：disabled → 拒绝并提示 `seat enable <name>` 先解闸。
- 建会话、起 claude、登记台账，一切启动动作走这一个函数。
- `fleetd seat enable <name>` 才能把 disabled 翻回 idle——显式、留痕。

### 自动化面的收编
- idle-watch / 巡检 / 内容巡逻：只**观测**（capture/对账），对 disabled
  席位零动作；对 idle 席位的自动点火必须先查闸。
- fleet-arch-diagrams / fleet-i18n-all 这类批处理脚本：从 cron 体系中
  移除（无 cron 即无触发）；要跑必须显式经 fleetd job 并逐仓点名。
- schtasks / 计划任务残留（Session1Test、FleetdAhkMcp 等）：tools 面
  落地时统一登记，fleetd `tools list` 可见、`tools stop` 可停。

### 台账字段（seats 表升级）
```
status: idle | running | disabled      ← disabled = 闸门
stopped_by TEXT                        ← 'user' | 'agent' | 'crash'
stopped_at INTEGER
```

### 命令面（fleetd seat 子命令）
```
fleetd seat stop <name>      # 三层斩 + 写闸
fleetd seat start <name>     # 检闸 + 建会话 + 登记
fleetd seat enable <name>    # 解闸
fleetd seat disable <name>   # 只写闸（不杀当前，防复发）
fleetd seat list             # 全席位 + 闸门状态
```

## falcon 事故复盘（作为验收反例）
1. 用户令停 → 只 kill 了 tmux 会话（第 1 层），pane 内 claude 进程没死
   → 它继续在后台跑完手头任务并 commit+push。
2. 会话 22:53 被某个保留名单机制重建 → 没有闸门概念，名单里有就重建。
3. 巡检对 falcon 的引用散在 4 个脚本 + session-priority.txt 名单，
   无统一闸门可查。

验收：`seat stop falcon` 后 —— 无 tmux 会话、无进程、台账 disabled、
任何自动路径不点火、`seat list` 显示 disabled。复测 24h 无复活。

## 实现顺序
1. seats 表迁移 + 三层斩 stop（今日）
2. start/enable/disable/list 命令面（今日）
3. 巡检/idle-watch/名单元数据清理 falcon 引用（今日）
4. 自动化面统一查闸（跟 agent pull 模式一起）
