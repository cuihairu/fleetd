# fleetd 状态机对抗检查闸设计（GATE-001 定案，2026-11-28）

> 来源：2026-11-28 会话复盘「7 例报完成被推翻」，用户定案：
> 「那么这些是否可以设计到 fleet 中 分配的任务的状态机 完成之前有一次对抗检查」。
> 本文档是设计定案存档；施工任务书在 `~/.hermes/cache/scratch/tasks/FLEETD-GATE-001.md`。

## 1. 问题定案：不是能力问题，是流程缺闸

复盘 7 例「报完成被推翻」（presence 没跑 serve、关席被 guard 复活 39 席、beacon 文字
漏修对比度、kode 表格三修才净等），归纳三个结构性根因：

| 根因 | 实锤 |
|---|---|
| A. 动作完成 ≠ 结果完成 | 代码 commit 了就报落地，效果在目标环境没验证（serve 没跑/复活源没封/线上列宽照旧） |
| B. 修第一个因就收手 | beacon 文字=截断+对比度两因；kode 表格=断行+列宽+大纲栏三因，修三轮才净 |
| C. 状态变化无自动感知 | croupier 挂 3.5h、死席复活、3790 不通——全靠人肉想起去查 |

结论：编排方（Hermes）在「动作做完」与「报完成」之间缺一道强制闸。靠自觉必然复发，
**必须进 fleetd 状态机，用代码强制**。

## 2. 状态机定案

```
                 ┌──────────────────────────────────────┐
                 │  dispatch（创建, baseline_head 记账）  │
                 └──────────────┬───────────────────────┘
                                ▼
                    active ──(Stop hook, Verdict)──┐
                       ▲                           ▼
                       │              Verdict::Completed?
                       │                           ▼
                  [打回]                review（对抗检查态）
                       │                           │
                       │              ┌────────────┴────────────┐
                       │              ▼                         ▼
                       │        review_pass                  review_fail
                       │        （done, 落证据）      （打回 active + 原因入账）
                       │                                        │
                       └────────────────────────────────────────┘
     非 Completed verdict（Partial/Stalled/NoProgress）→ 不进 review，按既有语义处置
```

- dispatches.state 枚举扩展：`review` / `review_failed` 两态；既有 active/done 语义不变
- 非 Completed verdict 不进 review（Partial/Stalled/NoProgress 走既有处置）

## 3. 验收分级（acceptance 三档，任务书声明）

| 档 | 语义 | 流转 |
|---|---|---|
| `self-check` | 纯文档/勾账类，席位自检 | Completed → 直通 done |
| `adversarial`（默认） | 需对抗检查 | Completed → review 态，检查方复核 |
| `user-verify` | 用户装机亲验硬门（beacon 岛类） | review 态 + 通知引擎推「待用户亲验」 |

用户既有铁律（视觉类不自评、待亲验不许勾、虚勾打回）由此变成代码闸门。

## 4. 无证据不许 done（根治钩子）

- `fleetd review pass <dispatch-id>` **必须带 `--evidence <path>`**（测试输出/截图/实测数据）
- 无证据 pass 直接拒绝；evidence 落 dispatches.evidence_path，done 任务可审计
- `review fail --reason` 打回 active，reason 入账；打回 ≥2 次升级上报编排方

## 5. 检查面

- `fleetd review list`：全部 review 态任务（席位/任务/baseline→head/工作区状态）
- `fleetd review pass|fail`：过闸/打回
- MCP：`fleet.review.list` / `fleet.review.decide`（编排方程序化过闸）
- `/v1/fleet` 响应含 review 态任务数（看板可见）
- events 落账：进 review=kind `review_pending`；打回=kind `review_rejected`（含 reason）

## 6. gate 纯函数（core，可单测）

```rust
pub enum GateDecision { EnterReview, DirectDone, Reject }
pub fn gate(decision: Verdict, task: &TaskSpec) -> GateDecision
```
单测 ≥6 例（三档 acceptance × Completed/非 Completed）。分层纪律：纯函数 core /
SQLite store / CLI+MCP 在面。

## 7. 边界与后续

- 本单检查方=编排方（Hermes）人工/半自动过闸；**自动化检查席**（跑测试/看截图的
  review 席）是下一单
- review 态超时不设自动过闸——宁可积压不可虚过；超时告警进 events 由编排方处理
- 治本链路：本闸 + fleetd serve 上运行位（3790 事件流）+ presence 感知，三者齐了
  「对抗检查后才能说完成」= 机器强制，编排方自觉退居第二道防线

## 关联

- 施工任务书：`~/.hermes/cache/scratch/tasks/FLEETD-GATE-001.md`（已派 fleetd 席，插队最前）
- 复盘背景：本会话 7 例案例与三根因（见对话记录 2026-11-28）
- 既有铁律衔接：全局铁律⑥（对抗审查/虚勾打回）、beacon 视觉类用户亲验硬门
