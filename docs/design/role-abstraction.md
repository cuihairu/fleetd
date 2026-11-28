# fleetd 角色抽象设计备忘（2026-11-28 讨论，用户定案方向）

> 用户令：「一个任务领取分配执行审核 也有对应的角色吧」+「这个其实也可以抽象」。
> 本文档是设计分析定案；施工任务书待用户确认后落。

## 1. 任务生命周期的四个动作 → 四个角色

任务全链路 = **领取 → 分配 → 执行 → 审核**，每个动作是一个角色（Role），
角色是接口不是具体进程——同一台机器可以承载多个角色，一个角色可以有多个实现：

| 角色 | 职责 | 现有对应物 | 实现形态 |
|---|---|---|---|
| **Claimer**（领取） | 发现任务源、判断可做性（blocked 排除后有无 actionable）、生成 Job 声明 | seat_patrol / fleet-idle-watch / webhook 事件 | trait ClaimSource：todo 扫描器 / SCM webhook / 用户消息 / cron |
| **Dispatcher**（分配） | 挑 Machine、投递 mailbox、校验三件套、路由 runtime | fleetd dispatch 路由 + MACHINE-001 的 select 循环 | 调度器本体（单实现，策略可插拔） |
| **Worker**（执行） | 在 runtime 里跑任务、回报告示/产物 | 席位（tmux claude）/ 容器任务 / AHK 真机 | Machine trait + Executor impl（已有抽象） |
| **Reviewer**（审核） | 对抗检查、验证据、pass/fail、打回 | GATE-001 的 review 闸（目前=编排方人肉） | **trait Reviewer：本单要抽象的核心** |

## 2. Reviewer 抽象（目前最弱的一环，GATE-001 只做了闸没做检查者）

```rust
#[async_trait]
pub trait Reviewer: Send + Sync {
    fn id(&self) -> &str;                       // checker 名（审计链用）
    fn scopes(&self) -> &[ReviewScope];         // 能审哪类：code/test/docs/ui/e2e
    async fn review(&self, job: &Job, evidence: &Evidence) -> ReviewVerdict;
}

pub enum ReviewVerdict {
    Pass { notes: String },                     // 附检查记录
    Fail { reason: String, severity: Severity },// 打回 + 原因
    Escalate { to: EscalationTarget },          // 升级：user-verify / 编排方裁决
}
```

**第一批实现（开源优先，复用现成能力）：**
| Reviewer | 实现 | 审什么 |
|---|---|---|
| TestGate | 跑仓内测试套件（cargo test/go test/vitest），绿=过 | code/test 类 |
| CodexReview | codex exec --profile luna 只读审计（舰队既有通道） | 代码质量/架构 |
| VisualCheck | 真机截图 + 像素门脚本（beacon AH5d 类） | UI/视觉类 |
| BuildGate | CI run 状态查询（gh run view） | 构建产物类 |
| HumanReviewer | 编排方/用户（user-verify 档必经） | 终审兜底 |

任务书声明 `review-by: test,codex`（默认按任务类型自动选），Reviewer 链式执行
（全过才 pass，任一 fail 即打回）。

## 3. 角色即服务（与 Machine 抽象的关系）

- Claimer/Dispatcher/Reviewer 都是**逻辑角色**，Worker 的 Machine 是**执行资源**——
  角色跑在哪个 Machine 上是部署问题，不是模型问题
- 本机 flagship：Dispatcher + 全部 Reviewer 常驻（token 成本低的检查类可本地跑）
- 远程机（work-runner）：只承载 Worker 角色跑重活；Reviewer 轻量面（TestGate）
  可下放远程跑测试、回传结论，避免测试产物流量回传
- 同机多角色隔离：Reviewer 绝不与被审 Worker 同席（自审无效——GATE-001 既有红线）

## 4. 与既有单的收拢关系（不新增并行概念）

- GATE-001 的 review 闸 = 本抽象的**触发点**（review 态→选 Reviewer→执行→verdict）
- MACHINE-001 的 mailbox = Worker 角色的接入面；Reviewer 审查请求同样走
  mailbox（review 也是一个 Job，runtime 多为 native 短进程）
- JOB-PAYLOAD-001 的三件套 = Claimer 产出的 Job 声明；Reviewer 审的就是
  "outputs 声明 vs 实际产出 + evidence 真实性"

## 5. 施工切分（待用户拍板）

- **ROLE-001（下一单）**：Reviewer trait + TestGate + HumanReviewer（最小闭环，
  GATE-001 的 review 态从"等编排方"变成"TestGate 自动审 + 编排方只处理 Escalate"）
- **ROLE-002**：CodexReview/VisualCheck/BuildGate 接入 + review-by 链式策略
- **ROLE-003**：ClaimSource trait 收编（todo 扫描/webhook/cron 四源统一）
