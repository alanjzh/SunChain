# SunChain SunLead P0 — 整改工作日志

> 基线：`Sunchain-SunLead-Remediation-Plan.md`
> 状态（2026-10-28）：P0-A 完成并验证；P0-B 已完成（立即措施）；P0-C、P0-D 已完成。
> 决策：用户要求先放一放 P0-B 的 P5 状态机（4.2）与 BABE 依赖收敛（4.3），优先推进 P1（见
> `Sunchain-SunLead-P1-Remediation-Worklog.md`）。4.2 / 4.3 暂不实现，待 P1 之后或单独排期。

## 一、P0-A：统一 epoch / slot / timestamp 语义（已完成并验证）

### 1.1 问题（致命回归）
早期一次 P0 修复把 verifier 的 epoch 绑定为 `slot - snapshot.activation_slot`，其中
`activation_slot = session * epoch_slots`（**session 计数**）。但链上真实出块 slot 是
**Unix 时间锚定的巨大数**，与 session 完全脱节，导致窗口检查

```text
slot - session*epoch_slots ∈ [0, epoch_slots)
```

永远不成立 → 每块 `SlotOutOfRange` → 两验证节点互相 ban、GRANDPA 卡在 `finalized #0`、网络分裂。

### 1.2 修复（epoch 必须由绝对 slot 推导，session 与 time 解耦）
- `crates/sunlead-verifier/src/lib.rs::verify_pre_digest`
  - `expected_epoch = slot / epoch_slots; rel = slot % epoch_slots`
  - 校验 `digest.epoch_index == expected_epoch`（不符即 `EpochMismatch`）
  - 删除 `relative_slot(context.epoch_index, …)` 的 session 窗口依赖（该函数已移除）
- `node/src/sunlead_consensus/worker.rs::aux_data`：同样由 slot 推导 `epoch_index` 构建
  schedule，保证产块侧 `schedule_root` 与验证侧一致。
- `pallets/sunlead/src/lib.rs::leader_at` / `schedule_proof`：诊断 API 同步改为 slot 派生，
  与链上验证一致。
- 明确 `Snapshot.epoch_index / activation_slot` 仍只是 session 记账值，验证路径不再读取它们。

### 1.3 验收
- `cargo test -p sunlead-verifier`：**10/10 通过**（含新增 epoch 匹配验收；原 cross-epoch /
  cross-slot 用例改为断言 `EpochMismatch`）。
- 全工作区 `cargo check`：通过（既有 `pricefeed` 等 warning 与本改动无关）。
- **fresh 链双节点回归**（master 9944 / slave 9945，清 `db`+`frontier`、保留 `keystore`+
  `network`）：
  - 两节点 `peers:1`、`isSyncing:false`，`state_call SunLeadApi_is_active = 0x01`。
  - 区块持续产出，GRANDPA 终局越过 `#0`（slave best #16 / finalized #14）。
  - 两节点日志**无** `Verification failed` / `banned` / `SlotOutOfRange` / `panic` /
    `equivocation`；仅剩良性 `vote rejected: Already imported` 去重噪声。

## 二、P0-B：安全 activation 与 BABE 依赖收敛（进行中）

### 2.1 立即措施（Remediation 4.1）
- **[已完成] 移除 `pallet-sunlead::set_sunlead_active` 运行时节点的治理翻转 call**
  （`pallets/sunlead/src/lib.rs`）：删除 `#[pallet::call] set_sunlead_active`，`SunLeadActive`
  现在只能由 genesis（`GenesisConfig.sunlead_active`）决定，运行时节点的布尔开关被删除，避免治理
  翻转导致节点 / Runtime 共识状态不一致、停链。对应 Remediation 4.1 第二条
  “禁止通过 `set_sunlead_active` 在运行链上开启或关闭共识”。
  - 验收：`cargo check -p sunchain-runtime` 通过；fresh 双节点（清 `db`+`frontier`、保留
    `keystore`+`network`）启动，两节点 `peers:1`、`is_active=0x01`、GRANDPA 终局越过 `#0`
    （master best #21+），日志无 `Verification failed` / `banned` / `SlotOutOfRange` / `panic`。
- **[已满足（运维实践）] 独立实验链**：`sunchain` 规格已显式 `sunleadActive: true`（chain id
  `SUN`、独立 bootnode 与数据库 `chain_data/master`+`slave`），不与既有 BABE 链数据混用
  （Remediation 4.1 第一条）。

### 2.2 P5 状态机（Remediation 4.2，待后续）
- `Disabled → Prepared{activation_epoch, activation_parent_hash, required_node_version} →
  Active{activation_epoch, activation_block_hash} → EmergencyHaltAuthoring{reason}`。
- 以 finalized activation epoch 取代启动期布尔分支；迁移窗口根据 parent-state activation 状态选
  择 verifier，而不是只在 genesis/read startup 时选择。

### 2.3 BABE 临时角色（Remediation 4.3，待后续）
- BABE 仅作为显式记录的 session rotation / randomness 依赖，不再充当隐式 author resolver
  （已由 P0-D `SunLeadFindAuthor` 完成；`SunLeadAwareOnTimestampSet` 仍从 timestamp 同步
  `Babe::CurrentSlot`）。

## 三、P0-C / P0-D（已完成）
- **P0-C**：`on_initialize` 在 `SunLeadActive && Snapshot` 缺失时 fail-fast（`panic!`）；
  schedule 资源 / genesis 构建错误路径已在 pallet 内。
- **P0-D**：`SunLeadFindAuthor` 从 SunLead digest + parent snapshot 解析作者，已接入
  `pallet_authorship`（替换 `FindAccountFromAuthorIndex<Babe>`）；EVM 侧仍用
  `FindAuthorTruncated<Babe>`。
