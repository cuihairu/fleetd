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
