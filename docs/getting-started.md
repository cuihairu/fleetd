# 快速上手

fleetd 把你的机器群组织成一支舰队:一台 **flagship**(主舰/控制中心)+ 若干 **ship**(成员舰)。

## 安装

```bash
# flagship(控制中心)——常驻 serve
fleetd service install      # 写入本机原生服务(systemd/schtasks/launchd)
fleetd serve --port 3790

# ship(成员舰)——心跳入队,只出站流量
fleetd agent --flagship http://<flagship-ip>:3790 --token <token> --seat <ship-name>
```

> `--hub` 是 `--flagship` 的历史别名,两者等价。

## 派单与观测

```bash
# 给某艘 ship 排任务(下一次心跳即送达)
fleetd relay queue <seat> <task.md> <repo>

# 任意一台机器上看全舰队
fleetd fleet overview http://<flagship-ip>:3790 --follow

# 看单任务数据流
fleetd watch <run-id>
```

## Windows 托盘

Windows 版 fleetd 带系统托盘(木色齿轮):

- **空闲**时慢速漂移;**在飞任务越多转得越快**(封顶 0.4 秒/帧,始终清晰可辨)
- 右键菜单:状态行(`fleetd <版本> — flagship :<端口>`)、打开面板、退出
- `fleetd service install` 同时注册开机自启(ONSTART)+ 登录托盘(ONLOGON)

### Windows 托盘部署（手动）

托盘 exe 由 CI 在每次改动 `crates/tray/**` 后于 windows-latest 原生 MSVC 构建并上传 artifact（见 `.github/workflows/tray-windows.yml`）；也可本机交叉构建：

```bash
cargo build --release -p tray --target x86_64-pc-windows-gnu
# 产物: target/x86_64-pc-windows-gnu/release/tray.exe（< 5MB）
```

部署到 Windows 机器（以 imac 为例，在本机执行）：

```powershell
# 1. 拷贝 exe 到用户目录
scp target/x86_64-pc-windows-gnu/release/tray.exe imac:C:/Users/cui/.fleetd/fleetd-tray.exe

# 2. 注册用户登录自启（登录会话内运行，无需管理员）
schtasks /Create /TN "fleetd-tray" /TR "C:\Users\cui\.fleetd\fleetd-tray.exe" /SC ONLOGON /RL LIMITED /F

# 3. 立即拉起（或等下次登录）
schtasks /Run /TN "fleetd-tray"
```

成功判据：右下角 systray 出现木色齿轮，右键菜单可见状态行（`fleetd <版本> — flagship :<端口> (<n> 在飞)`）。
