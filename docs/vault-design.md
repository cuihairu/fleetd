# fleetd vault 设计：机器间密钥/账号/额度统一管理

状态：设计定稿（2026-11-27，用户确认范围），待实现。
前置事实：**persona vault 已存在且在役**（gateway-fleet 的 24 个 api_key
权威源，unix socket + pconn token，litellm/hermes/bifrost 全在消费）。
fleetd **不自建密钥库**，做 persona 的 fleet 分发适配层。

## 范围（用户令）

1. 机器间密钥统一管理 + 加密传输分发
2. 订阅账号管理 + 额度查询
3. litellm / gateway-fleet 类网关工具纳管
4. gateway-fleet 纳入 fleet 管理

## 三层架构

```
1. 权威源  persona vault（已存在）
   ├─ 24 api_key items（zai/kimi/sensenova/xiaomi/qiniu/ark/bifrost/glm…）
   ├─ unix socket + pconn token（scoped read-only）
   └─ 新增条目类型：订阅账号（provider/plan/expiry/额度查询端点）
2. fleetd 分发适配层（新实现）
   ├─ fleetd tools config sync：persona 读 → 公钥封装 → 分发
   ├─ ship 端落盘 ~/.fleetd/secrets/<name>（0600）
   ├─ 通道复用 relay F2 心跳（出站拉取，公网零明文）
   └─ agent spawn 注入环境变量（FLEETD_SECRET_*）
3. 消费面（已有，不动）
   ├─ gateway-fleet fleet_apply 继续管 litellm env.systemd
   └─ hermes .env / bifrost / magpie 只读校验链不变
```

## 分工裁决

| 面 | 归属 | 理由 |
|---|---|---|
| 密钥权威存储 | persona vault | 已在役、有审计、有自动锁策略 |
| litellm 配置渲染 | gateway-fleet fleet_apply | 已成熟，secret refs 机制完备 |
| 机器间分发 | fleetd（新） | persona 是单机 unix socket，无跨机面 |
| 额度查询 | fleetd（新） | 聚合各 provider 用量端点 → fleet overview 加列 |
| agent 生命周期 | fleetd tools 面（同日落地） | install/inventory/config sync |

## 实现顺序

1. F2 心跳通道（ship 出站到 flagship）——vault 分发的载体
2. vault 分发：`fleetd tools config sync`（flagship 拉 persona → 封装下发）
3. 额度查询：`fleetd quota`（persona 条目里的 provider 用量端点聚合）
4. gateway-fleet 纳管：`fleetd status` 加 litellm/gateway 服务健康行

## 安全边界（用户硬要求：数据外发默认关）

- 分发默认关：显式 `fleetd tools config sync --scope <ship>` 才下发
- payload 端到端加密（ship 公钥封装），中继/公网零明文
- flagship 不落任何明文密钥到磁盘（persona 是唯一权威源）
- 审计：flagship 记录谁在何时拉了哪个条目
