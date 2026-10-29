# SunChain SunLead 整改与后续开发计划

> 基线：`Sunchain-SunLead-Deterministic-Leader-Development-Plan.md`、P1--P3 Worklog、提交 `1f52424..ba5079b` 与当前实现。
>
> 状态：SunLead 已完成 P1--P3 的原型主路径和双节点 E2E（出块、同步、GRANDPA 终局、DEX 执行）；**尚未达到 P4 故障测试或 P5 测试网迁移门槛**。本文是后续整改的执行基线。

## 1. 目标与原则

整改目标不是增加功能，而是使下列协议不变量可以被代码、测试和运维观测共同证明：

1. 每个绝对 slot 只属于其所在 SunLead epoch，且只有该 epoch schedule 指定的 leader 可产块。
2. header 中的 SunLead slot、timestamp inherent 和 parent slot 一致且单调。
3. session/key/stake 变更只在明确的 epoch 边界生效；历史块可重放。
4. 双签可举报、可归属、可惩罚；作者归属、奖励与 liveness 逻辑不再依赖缺失的 BABE digest。
5. 共识切换由已最终化的链上状态驱动，不能由节点启动时的静态分支或任意时点布尔开关决定。

在完成 P0 项目前，不应把 `sunlead_active = true` 用于共享 staging/testnet，更不应作为生产共识。

## 2. 问题优先级

| 优先级 | 问题 | 风险 | 完成条件 |
|---|---|---|---|
| P0 | epoch/slot/timestamp 未绑定 | 可接受不属于当前 epoch 的 slot；时序规则失效 | verifier、worker、runtime API 使用同一 epoch 区间语义，恶意用例被拒绝 |
| P0 | 静态启动门控与可变 `SunLeadActive` 冲突 | 治理翻转后节点/Runtime 共识状态不一致，可能停链 | 以 finalized activation epoch 的状态机替换布尔切换 |
| P0 | 无界 schedule 重建与固定 weight | runtime 超重、PoV/存储增长、边界 block 风险 | 有 authority/epoch 上界、benchmark weight、失败可观测且 fail-safe |
| P0 | genesis 忽略 active schedule 构建错误 | 激活链可无有效 schedule 而无法出块 | active genesis 构建失败即拒绝生成 chain spec |
| P0 | `FindAuthor` 仍解析 BABE | SunLead block 的作者、奖励和 liveness 归属错误/缺失 | 从 SunLead digest + snapshot 解析作者并有经济行为测试 |
| P0 | 无 equivocation 报告/处罚 | 双签只有离线检测，没有经济约束 | proof、历史归属、offences/slashing 端到端可验证 |
| P1 | digest/header 规范性不足 | 重复 seal、parent 查询失败的 fail-open 行为 | 严格 canonical digest，所有本地错误 fail closed |
| P1 | RPC 与节点语义不一致 | 观测/调试会得到错误 leader | 统一 absolute-slot API，提供 epoch/status/next leader 指标 |
| P1 | 参数与配额语义未冻结 | 改参和极端 stake 下公平性不可解释 | params 写入 snapshot；公布并测试 quota 偏差边界 |
| P1 | 测试覆盖不足 | E2E 成功掩盖边界和攻击路径 | runtime/node/fault/performance 测试矩阵全绿 |

## 3. P0-A：统一 epoch、slot 与 timestamp 语义

### 3.1 协议定义

为每个 snapshot 明确保存：

```text
epoch_index
start_slot              // inclusive
epoch_slots
end_slot = start_slot + epoch_slots  // exclusive
params                  // 本 epoch 冻结
```

对任一 SunLead header，导入前必须满足：

```text
digest.epoch_index == parent_snapshot.epoch_index
start_slot <= digest.slot < end_slot
relative_slot = digest.slot - start_slot
leader/window == schedule[relative_slot]
digest.slot == slot_from_timestamp_inherent
parent.digest.slot < digest.slot
```

禁止使用 `absolute_slot % epoch_slots` 作为 epoch 映射。它不能表达 session 边界，也不能表达 activation 时刻。

### 3.2 实现任务

1. 在 `SunLeadAuthoritySnapshot` 中将 `activation_slot` 明确为 epoch `start_slot`，并加入冻结的 `SunLeadScheduleParams`；必要时增加 storage/version migration。
2. 定义一个唯一的纯函数 `relative_slot(snapshot, absolute_slot)`，返回 `Result<u64, SlotOutOfRange>`；worker、runtime API、verifier 全部复用。
3. 在 session/epoch 边界由链上 slot 而非 `session_index * epoch_slots` 决定下一个 epoch 的 `start_slot`。若继续复用 BABE session rotation，必须显式记录其对应的绝对 slot 边界。
4. 在 import verifier 中解析 block body 的 timestamp inherent，计算 timestamp slot，并要求与 pre-digest slot 相等；同时限制合理的 future drift。
5. 修复 `SunLeadApi::leader_at`、`schedule_proof`：输入 absolute slot，先校验所属 epoch，再映射至 relative slot。
6. 修复并保留 `sunlead-verifier::slot_out_of_range_is_rejected`；该测试目前失败，不能以错误类型差异忽略。

### 3.3 验收测试

- 当前 epoch 前/后各一个 slot、相邻 epoch 的首尾 slot、跨 session 边界。
- 正确签名但错误 epoch、错误绝对 slot、正确同余但不属于该 epoch 的 slot。
- 正确 SunLead digest 配错误 timestamp、future timestamp、重复 slot、倒退 slot。
- worker 在 epoch 边界前后只为实际 leader author；runtime API、worker、verifier 对同一 absolute slot 返回同一结果。

## 4. P0-B：安全 activation 与 BABE 依赖收敛

### 4.1 立即措施

- 将共享 `staging` 的 `sunleadActive: true` 降为专用实验 chain；使用独立 chain id、bootnode 和数据库，避免与既有 BABE 链数据混用。
- 禁止通过 `set_sunlead_active` 在运行链上开启或关闭共识；在迁移状态机完成前，移除该 call 或使其只能配置未来 activation proposal，不能直接改变 active engine。

### 4.2 P5 activation 状态机

以链上不可歧义状态取代启动期布尔分支：

```text
Disabled
  -> Prepared { activation_epoch, activation_parent_hash, required_node_version }
  -> Active { activation_epoch, activation_block_hash }
  -> EmergencyHaltAuthoring { reason }
```

- `Prepared` 只能在未来至少两个 epoch 生效，所有验证人先升级为双协议 verifier。
- activation 以 finalized parent/hash 和 epoch 边界锁定；切换前接受 BABE，切换后仅接受 SunLead，历史 BABE 仍可 replay。
- 节点 import queue 在迁移窗口根据 parent-state activation 状态选择 verifier，不能只在 genesis/read startup 时选择。
- emergency 仅停止 authoring，不应让节点接受另一套未定义的 header；恢复须由新 finalized epoch 决议。

### 4.3 BABE 的临时角色

在 P5 前，BABE 只能作为明确记录的 session rotation/randomness 依赖，不能同时充当隐式 author resolver。长期方案需单独决定独立 rotation provider 和抗偏置 randomness beacon；该变更不得与首次 SunLead activation 同批上线。

## 5. P0-C：runtime 资源、genesis 和作者归属

### 5.1 schedule 资源治理

- 为 authority 数、epoch slot 数、snapshot 编码长度、schedule 长度引入 `BoundedVec` 或 runtime 常量上限。
- `on_initialize` 重建逻辑采用真实 benchmark weight；不能使用固定 `reads_writes(4, 3)` 覆盖遍历 validators、读取 exposure、构建 Merkle tree 和写入完整 schedule 的成本。
- 若边界 schedule 构建失败，需发出明确事件/日志并进入定义好的 fail-safe 行为；不能静默沿用旧 schedule。
- 评估只存 root + 按需生成/证明与存完整数组的取舍；V1 如保留完整数组，须有明确 storage 预算。

### 5.2 genesis fail-fast

`sunlead_active = true` 时：缺 `sunl` key、authority 为空、参数非法、schedule 构建失败必须让 genesis build 失败。`sunlead_active = false` 时可不构建或记录非阻断告警。

### 5.3 作者、奖励与在线归属

实现 `SunLeadFindAuthor`：从 header 的唯一 SunLead pre-digest 取 `leader_index`，在 parent snapshot 的 canonical authority 集合中恢复 AccountId。替换当前的 `FindAccountFromAuthorIndex<Runtime, Babe>`。

必须增加：block author event、staking reward、im-online、session rotation、离线 validator 的 mock-runtime 与两节点 E2E 断言。

## 6. P0-D：equivocation 到 slashing 的闭环

1. 增加 SunLead equivocation report extrinsic 与 unsigned validation。
2. 证明验证必须同时验证：两份 seal、相同 `(set, epoch, slot, leader_index)`、不同 header、offender 等于该 schedule leader、当期/历史 snapshot 与 key ownership。
3. 通过 `pallet-offences` 记录 offence，并接入 staking slash、报告奖励、有效期和重复报告去重。
4. 节点收集同 slot 同 leader 的冲突 header，并暴露指标；自动提交策略须可配置且避免 spam。
5. 覆盖：有效双签、不同 slot/set/leader、伪造 offender、过期 key、同一证据重复提交、reorg 后证据。

## 7. P1：导入严格性、参数语义与可观测性

### 7.1 导入严格性

- SunLead pre-runtime digest 和 seal 都必须恰好一个；重复、无法解码、混入冲突 engine item 一律拒绝。
- 读取 parent header/snapshot 失败时拒绝 block，不能跳过 slot monotonicity。
- 抽取共享的 `verify_header`，供 remote import 与 local authoring 后的自检共用；本地块至少在提交 import 前执行同一套纯验证。
- 添加 node import queue 测试：错误签名、错误 root、错误 leader、重复 pre-digest/seal、错误 parent、错误 timestamp、无 snapshot、parent lookup error。

### 7.2 参数与公平性

- snapshot 存本 epoch 的参数，而不是 verifier 从可变 `ScheduleParams` 重新读取。
- `enforce_feasible` 会为了 run cap 改写 Hamilton quota。须在规格中定义 stake 偏差上限、极端 stake 的处理和治理可接受范围；若不能给出边界，应改算法或拒绝不可行参数组合。
- 为 1、2、最大 authority 数、零 stake、单一巨鲸、key rotation、stake reweight 建立 property/fuzz 测试和 golden vectors。

### 7.3 RPC、指标和告警

补充并固定语义：

- `sunlead_epoch`：snapshot、start/end slot、root、参数、authority set。
- `sunlead_nextLeader`：未来 N 个 absolute slot 的 leader/window。
- `sunlead_schedule` / proof：明确 slot 是否 absolute，越界应返回错误。
- `sunlead_status`：activation 状态、当前 verifier、last schedule build、authoring key readiness。
- Prometheus：空 slot 率、每 leader proposal/import 成功率、拒绝原因、schedule build 耗时、fork 数、GRANDPA finality lag、equivocation 数。

## 8. P4 测试与性能门槛

P4 必须输出可复现脚本、原始日志、指标与结论，不以人工观察替代。

| 类别 | 必测场景 | 通过条件 |
|---|---|---|
| 正确性 | epoch 边界、session/key/stake rotation、历史 replay | 所有节点得出相同 schedule，合法块可重放 |
| 可用性 | leader 停机/重启、window 第一/第二 slot 漏块 | 仅预定后继 slot 接手，无同 slot takeover |
| 安全性 | 双签、错误 slot/root/seal/timestamp、恶意重复 digest | runtime 执行前拒绝，双签可处罚 |
| 网络 | 10%/30%/40% 延迟与丢包、分区/恢复 | GRANDPA safety 不变，恢复后收敛 |
| 恢复 | snapshot/warp sync、DB 中断恢复、旧 BABE DB 隔离 | 不 panic，不接受错误协议 header |
| DEX | 高负载、oracle、清算、ADL | block/终局延迟、失败率和资源在预算内 |
| 性能 | 同硬件、同验证人、同负载下 BABE 对照 | 报告 p50/p95/p99 proposal、传播、finality、空 slot、fork、CPU/内存/网络 |

性能结论必须区分“确定性 leader 减少正常竞争”与“网络传播/执行/GRANDPA”瓶颈；在有 BABE 基线前不得宣称大幅性能提升。

## 9. 建议提交切分与 Gate

### R1：协议正确性（阻断后续共享链测试）

1. epoch/slot/timestamp 统一语义与 verifier 修复。
2. active genesis fail-fast、strict digest、parent fail-closed。
3. absolute-slot RPC 修复及 mock/node tests。

**Gate：** 全部纯 crate、pallet、node 测试通过；恶意 header 在 runtime 前拒绝；epoch 边界双节点 E2E 通过。

### R2：runtime 可运营性与经济归属

1. bounds、benchmark weights、schedule failure policy。
2. SunLeadFindAuthor、奖励/im-online/session 断言。
3. 参数冻结和公平性规格/测试。

**Gate：** try-runtime、runtime integrity、资源 benchmark、最大 authority 配置测试通过。

### R3：处罚和故障验证

1. equivocation reporting/slashing。
2. P4 网络、宕机、分区、恢复、DEX 负载矩阵。
3. 指标、告警和 E2E 自动化。

**Gate：** P4 完整报告；相对 BABE 不降低可用性/GRANDPA finality；无未解释 panic 或 import failure。

### R4：P5 迁移设计与测试网

1. 双协议 verifier 和 finalized activation 状态机。
2. 新旧节点混跑、错误 activation、紧急停产、回滚演练。
3. 至少 30 天多地域测试网观察。

**Gate：** 独立安全审计完成，治理批准后方可激活生产 SunLead。

## 10. 当前结论

SunLead 当前适合作为隔离 devnet 的功能原型：其确定性排程、window authoring、签名验证、双节点同步及 GRANDPA 终局已经具备验证价值。下一阶段应优先修复协议边界与运行时安全性，而非调整 window 长度或缩短 slot；后两者必须在 R3 性能数据证明安全余量后再评估。
