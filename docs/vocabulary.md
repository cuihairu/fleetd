# 舰队术语表(2026-11-27 钦定)

整个项目使用一套统一的海军模型词汇。代码标识符、文档、托盘菜单、commit 消息一律照此:

| 词 | 含义 | 说明 |
|---|---|---|
| **fleet** | 整个部署 | 项目名本身 |
| **flagship** | 主舰/控制中心(原 "hub") | `flagship serve`、`fleetd agent --flagship <url>`;`--hub` 保留为历史别名 |
| **ship** | 任何入列的机器,flagship 也在内 | `agent_seen` 表、`fleet overview` 的行 |
| **admiral** | 指挥舰队的人(用户) | 面向舰队的界面文案称 admiral,不称 user |
| **seat** | 舰上的工作席位,任务在其上执行 | 不变 |
| **dispatch** | 一次任务派发 | 不变 |

规则:

- 新代码和新文档**禁止再引入 "hub"**。仅允许两处历史兼容:CLI 的 `--hub` 别名,以及持久化 `agent.toml` 键上的 serde `alias = "hub"`。
- 旧 `agent.toml`(写着 `hub = "..."`)继续可读(serde alias);新保存一律写 `flagship = "..."`。
- 判定主舰用 `Topology::is_flagship()`(原 `is_hub`)。

## Tray ↔ flagship 通信通道(2026-11-27 钦定方向)

托盘(桌面指示面)与 flagship 之间走**本机 socket 通信**,不共享进程内存:

- **Windows(现)**:loopback TCP `GET /v1/status`——仅接受 127.0.0.1 对端,免 token;Windows 桌面生态没有 UNIX socket 惯例,named pipe 是等价物,留待后续切换。
- **macOS / Linux 桌面版(未来)**:一律 UNIX domain socket(如 `~/.fleetd/status.sock`),tray 定时拉取;新写桌面面时不得另开 HTTP 端口。
- 语义契约:`{"version","in_flight"}`,托盘据 `in_flight` 调齿轮转速、据 version/port 拼状态行;任何失败按空闲渲染,托盘永不因 flagship 故障而退出。
