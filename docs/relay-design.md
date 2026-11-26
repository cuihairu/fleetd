# fleetd 公网中继角色（dev 机）设计

状态：设计定稿，待实现。日期：2026-11-26。
背景：舰队连通性 100% 依赖 ZeroTier；ZT 断线即全舰队失联。
用户令：公网角色放 dev（唯一公网机），舰主动出站连接，SSH 不作为依赖
（部分 Windows 不开 SSH）。

## 角色定义

dev 是 **fleetd 公网接入点（Public Relay / Rendezvous）**，不是第二个 hub：

1. **通道中继**：hub 与舰都只做出站连接到 dev；dev 把两边的流量对上号。
2. **保底路径**：ZT 内网直连为主（快路径），dev 公网为备（慢路径），
   断线自动切换、恢复自动回内网。
3. **不做业务**：dev 不存台账、不派单、不跑 job——台账永远只在 hub。
   dev 宕机的后果 = 退化回"只有 ZT 在线时可用"，不丢数据。

## 需要的功能清单（按实现顺序）

### F1. hub 公网暴露（dev 反代）
- dev 上 nginx/caddy 反代 `https://dev.xxx/fleetd/` → `hub:3790`（经 ZT 或内网）。
- TLS 由 dev 终结；hub 侧 fleetd 只监听（serve_on 已支持绑定地址）。
- 鉴权：共享 token（`FLEETD_RELAY_TOKEN`），agent 心跳/拉单必带，dev 只转发不解析。

### F2. 舰端 agent 模式（fleetd agent，出站 only）
- `fleetd agent --hub <url> [--hub <url2> ...] --name <seat> --workdir <dir>`
- 多 hub URL 按序试：主 ZT 内网 → 备 dev 公网；失败自动切换、恢复自动回主。
- 心跳（默认 30s）：POST `/v1/agent/heartbeat` 带 {seat, head, dirty, ver}。
- 断线重连：循环重试，失败仅记日志，进程不死。
- 任务下发：心跳响应内嵌待派任务（pull 模式）；执行完结果随下次心跳回执。
- Windows 无 SSH 也成立：只要出站 HTTP 通就能被管。

### F3. hub 端 agent 接入点
- `POST /v1/agent/heartbeat`：登记舰存活/状态（写 events 台账）+ 返回待派任务。
- 台账加 `agent_seen` 字段：区分"SSH 可达"与"agent 心跳可达"两种活性。
- 派单路由升级：优先 SSH push（在线且快），不可达且 agent 心跳活 → 排入
  pull 队列，等舰下次心跳带走。

### F4. dev 侧守护
- 反代配置落 systemd + cloudflare-tunnel-watchdog 同款看门狗（已有 skill）。
- dev 只需 nginx + 一条 location 块；无 fleetd 组件、无状态。

### F5. 安全边界（用户安全默认：数据外发默认关）
- 心跳只带 seat/git 元数据，不带代码/日志正文；任务文本经 dev 中转属必要
  最小面。
- token 校验失败一律 403；dev 日志不落请求体。
- 后续可加：hub↔舰 payload 端到端加密（dev 零知识），一期不做。

### F6. 可观测
- `fleetd status` 增列：每舰 `via: zt|relay|down`（当前走的路径）。
- 心跳超时 3 轮 → events 记 seat down；恢复自动记 up。

## 非目标（明确不做）
- dev 不做第二 hub（不持有台账、不决策）。
- 不做 P2P 打洞（WebRTC 等）——出站中继已满足需求，打洞复杂度不值。
- 不替换 SSH push 主路径——内网直连优先级永远最高。

## 验收口径
1. 断 ZT（hub 或任一舰）→ 该舰心跳自动经 dev 公网继续，`status` 显示 relay。
2. 恢复 ZT → 自动切回内网，`status` 显示 zt。
3. Windows 舰关掉 SSH 服务 → 派单/巡检/任务回执全链路仍可用（纯 agent 模式）。
4. dev 宕机 → ZT 在线时无影响；ZT 也断时该舰标记 down（不丢台账数据）。
