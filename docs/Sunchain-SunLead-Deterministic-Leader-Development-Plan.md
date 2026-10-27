# Sunchain SunLead：确定性 Leader / Window 出块开发计划

> 状态：开发计划提案。未经协议规格审查、测试网迁移演练与治理批准，不得替换现有 BABE 出块。  
> 日期：2026-10-25  
> 目标：以确定性、stake 加权的 epoch leader schedule 取代 BABE 的每 slot VRF 竞争；支持同一 leader 连续负责有限个 slot（leader window）；第一阶段保持 GRANDPA 为唯一权威最终性协议。

## 1. 决策与非目标

### 1.1 决策

新增独立共识出块引擎 **SunLead**，而不是修改 `pallet-babe` / `sc-consensus-babe` 的内部逻辑。

理由：BABE 的 VRF primary/secondary slot、epoch digest、block import、authoring worker、fork-choice 和 session rotation 相互耦合。移除 slot 内竞争、改为预定 leader/window 后，协议语义已不再是 BABE；在 BABE 上打补丁会使升级、审计和上游维护不可控。

第一期目标架构：

```text
Session / Staking snapshot
          │
          ▼
SunLead epoch schedule ──► SunLead block production / import
          │                         │
          │                         └── 必须由预定 leader 签名
          ▼
    GRANDPA prevote/precommit/finality
          │
          ▼
  唯一权威 finalized chain
```

SunLead 仅替换“谁可以在何时提议 block”的规则；不在第一期替换 GRANDPA、Runtime 执行、交易格式、DEX 执行或普通 block sync。

### 1.2 非目标

- 不承诺网络永远不会出现 candidate/fork；拜占庭 leader 双签、父块延迟和网络分区仍可能产生临时分叉；
- 不复用 Alpenglow 的 60%/80% Votor 阈值；
- 不在第一期实施同 slot backup 抢占；
- 不在第一期缩短 3 秒 slot；
- 不将 SunLead 的“块已出”作为 bridge、提款、清算或资产会计的最终性；
- 不把 leader window 当作提高 Runtime/WASM 撮合吞吐的替代方案。

## 2. 当前基线

当前链：

- 使用 `pallet-babe` + `sc-consensus-babe` 出块；
- 使用 `pallet-grandpa` + `sc-consensus-grandpa` 最终化；
- `SLOT_DURATION = 3s`；
- `PRIMARY_PROBABILITY = (1, 4)`，允许 `PrimaryAndSecondaryPlainSlots`；
- session rotation 当前由 BABE 驱动；
- node 的 Exchange proposer 是普通 proposer 的包装器，可复用，不应与 SunLead 耦合。

当前 BABE 模型：

```text
每个 slot：每个 authority 对 epoch randomness 做 VRF
      │
      ├─ 命中 primary threshold：可出块
      └─ 无 primary 时：secondary plain slot author 可出块
```

SunLead 要替换为：

```text
每个 epoch：冻结 authority set、stake、SunLead key 与 epoch seed
      │
      ▼
canonical stake-weighted schedule
      │
      ▼
slot 0..1 → A；slot 2..3 → B；slot 4..5 → C
      │
      ▼
每 slot 仅 schedule 指定的 leader 可产块
```

## 3. 安全模型与协议边界

### 3.1 安全假设

- GRANDPA 所需的 `< 1/3` Byzantine stake 假设继续成立；
- authority set、stake、SunLead key 与 epoch seed 都从已最终化的链状态取得；
- 正确节点只接受预定 leader 对该 slot 产生的 block；
- 正确 leader 每 slot 至多签一个 block；
- block 先完成共识 pre-digest 校验，再执行 Runtime 并验证 state root；
- GRANDPA 继续处理分区或 equivocation 导致的临时 fork 的最终收敛。

### 3.2 “没有 candidate”的准确含义

对于正常、诚实网络，SunLead 使每个 slot 只有一个合法 author，因此消除 BABE primary slot 多 winner 的正常竞争。

但不能把此表述为“协议不存在 candidate”：恶意 leader 可双签，或不同节点在父块传播延迟时在不同父块上继续出块。SunLead 的安全要求是：

```text
非 schedule leader 的 block 必须被拒绝；
同一 leader 同 slot 的冲突 block 可生成 equivocation evidence；
临时分叉不得越过 GRANDPA finalized 边界。
```

### 3.3 Leader 宕机规则

V1 使用安全优先的空 slot 规则：

```text
A 负责 slots 10-11
A 宕机或失联 → slots 10-11 空过
B 从 slot 12 正常接手
```

禁止 V1 中由 backup 在同一个 slot 仅凭本地 timeout 抢占。否则网络延迟会使 A 与 B 同时认为自己可出块，重新引入竞争。V2 以后仅在有 `>2/3` stake 的 SkipQC 时才能提前转交剩余 window。

## 4. 协议设计

### 4.1 authority snapshot

每 epoch 固化：

```rust
SunLeadAuthoritySnapshot {
    chain_id: u64,
    protocol_version: u16,
    authority_set_id: u64,
    epoch_index: u64,
    activation_slot: Slot,
    authorities: BoundedVec<(AccountId, SunLeadId, Stake)>,
    epoch_seed: H256,
    schedule_root: H256,
}
```

要求：

- `SunLeadId` 为新的 session key type，例如 `sunl`，不复用 BABE key type；
- authority 按 canonical `(AccountId, SunLeadId)` 排序；
- stake 来自 session 边界的 runtime snapshot，而非 node 本地缓存；
- epoch seed 必须来自已经 finalized 的历史随机性；
- key rotation 和 stake 变更仅在下一个 epoch 生效；
- snapshot、seed 与 schedule root 通过 Runtime API 在 parent state 查询。

初期可读取 BABE 已有、已最终化 epoch 的 randomness 作为 seed；完全迁离 BABE 前，必须另行设计抗偏置的 randomness beacon / commit-reveal。

### 4.2 schedule 算法

输入：`epoch_seed`、authority set、stake、epoch slot 数、window 参数。

算法：

1. 对 authority 做 canonical 排序；
2. 从 `H(chain_id || authority_set_id || epoch || epoch_seed)` 得到确定性随机排列；
3. 根据 stake 用有界的 weighted round-robin / apportionment 分配 epoch 内 slot 配额；
4. 将相邻配额合并为 window，但不超过 `MaxWindowSlots`；
5. 对每个 `(slot, leader_index, window_id)` 编码并构建 Merkle `schedule_root`；
6. Runtime 保存 root，node 可经 Runtime API 获得完整 schedule 或按需证明。

必须满足：

- epoch 内每一个 slot 恰有一个 leader；
- 相同 snapshot 在所有节点、所有架构上产生相同 schedule；
- 每个 authority 获得的 slot 数与其 stake 权重误差有明确上界；
- 单一 authority 连续 slots 不超过 `MaxWindowSlots`；
- 不能依赖 `HashMap` 迭代顺序、浮点数、本地时间或 node 配置。

### 4.3 初始参数

| 参数 | V1 初值 | 说明 |
|---|---:|---|
| Slot duration | 3 秒 | 与当前链一致，隔离选主变更风险 |
| Epoch length | 600 slots | 约 30 分钟；最终以压测确定 |
| Window length | 2 slots | leader 最多连续约 6 秒 |
| `MaxWindowSlots` | 4 | 防止大 stake leader 长时间垄断 |
| takeover | 仅下一个 scheduled slot | V1 不做同 slot backup |
| finality | GRANDPA | 保持当前权威 finality |
| fork choice | 先保留 LongestChain | 后续仅在数据证明需要时替换 |

### 4.4 block header digest

每个 SunLead block 必须包含：

```rust
SunLeadPreDigest {
    version: u16,
    authority_set_id: u64,
    epoch_index: u64,
    slot: Slot,
    window_id: u32,
    leader_index: u32,
    parent_hash: H256,
    schedule_root: H256,
    signature: SunLeadSignature,
}
```

签名 payload 必须含 chain genesis hash、engine id、全部 digest 字段及 header hash 的固定部分，防止跨链、跨 epoch、跨版本和跨父块重放。

import verifier 必须依次检查：

```text
版本 / engine id
→ parent state 中 SunLead 是否已激活
→ timestamp 推导的 slot
→ authority set / epoch / schedule root
→ slot 对应的唯一 leader_index
→ leader key 与签名
→ parent / slot 单调性
→ Runtime 执行、state root、普通 block validity
```

### 4.5 equivocation

若同一 `(authority_set_id, slot, leader_index)` 存在两个不同的有效 SunLead header，任何节点可提交：

```rust
SunLeadEquivocationProof {
    first_header,
    second_header,
    authority_set_id,
    slot,
}
```

Runtime 验证后接入现有 `pallet-offences` / staking slashing 机制。双签 evidence 的处理、报告奖励、历史 key ownership proof 和报告有效期必须在 P1 规格中冻结。

## 5. 代码结构与接线

```text
crates/
  sunlead-primitives/      # digest、snapshot、schedule、evidence、Runtime API types
  sunlead-schedule/        # no_std deterministic schedule / Merkle root
  sunlead-verifier/        # no_std header、slot、signature verifier

pallets/
  sunlead/                 # config、snapshot、epoch seed、schedule root、equivocation

runtime/
  src/configs/mod.rs       # sunl session key，session rotation provider，pallet config
  src/apis.rs              # SunLeadApi

node/src/sunlead/
  authoring.rs             # scheduled leader worker
  import_queue.rs          # digest verification before Runtime execution
  block_import.rs          # BlockImport wrapper
  worker.rs                # slot / window lifecycle
  equivocation.rs          # evidence collection and report submission
  rpc.rs                   # schedule and leader observability
```

### 5.1 Runtime 变化

- 增加 `sunl` 到 `SessionKeys`，chain spec/genesis 必须为每个 validator 配置该 key；
- 添加 `pallet-sunlead`，但在迁移前不改变 BABE 出块；
- 新增 `SunLeadApi`：`configuration(at)`、`authority_snapshot(at)`、`leader_at(slot)`、`schedule_proof(slot)`；
- 替换当前由 BABE 实现的 `ShouldEndSession` 与 `NextSessionRotation`，或引入独立固定 slot/block 高度 rotation provider；
- 第一版保留 `pallet-grandpa`、GRANDPA authority key 和 GRANDPA runtime API；
- 移除 BABE 前，`pallet-authorship::FindAuthor` 迁移为从 SunLead digest 恢复 author。

### 5.2 Node 变化

当前 `service.rs` 的 `sc_consensus_babe::import_queue` 与 `start_babe` 是替换点；现有 GRANDPA block import 与 `run_grandpa_voter` 第一阶段保持不变。

`ExchangeProposerFactory` 继续包装普通 `Environment/Proposer`，只需作为 SunLead scheduled authoring 的 proposer 来源，不应知道 leader schedule 的细节。

### 5.3 网络与传播

V1 复用 Substrate normal block announce、sync、request-response 和 authority discovery；不在同一阶段引入 Rotor 类新 block dissemination。

只有在基线数据证明 block propagation 是主要瓶颈后，才单独设计 relay/erasure-code 协议。将“换出块算法”与“换传播层”分开，避免无法归因的性能或安全回归。

## 6. 分阶段开发计划

### P0：基线、规格与模型（2--3 周）

工作项：

- 记录 BABE authoring、proposal、block propagation、Runtime execute、GRANDPA finality 的 p50/p95/p99；
- 在 DEX 高峰、oracle stale、清算高峰、重组、网络延迟场景复测；
- 冻结 SunLead digest、schedule、slot/window、空 slot、equivocation、activation 规格；
- 建立 TLA+ / 状态机模型，至少覆盖唯一合法 leader、空 slot、双签、epoch 切换和分区；
- 确认目标硬件、区域分布与时钟漂移预算。

**Gate：** 有数据证明 leader handoff/选主是可优化瓶颈；所有开放安全问题有 owner 与日期。

### P1：纯逻辑与密码学验证（3--4 周）

工作项：

- 创建 `sunlead-primitives`、`sunlead-schedule`、`sunlead-verifier`；
- 实现 canonical weighted schedule、schedule root、digest 编码、签名验证；
- 实现 equivocation proof verifier；
- 制作 Rust/WASM/独立 reference implementation 共用的 golden vectors；
- 实施 property/fuzz 测试。

必测：

- 同输入跨平台 schedule 恒等；
- schedule 覆盖 epoch 中每个 slot 且 leader 唯一；
- stake 极端不均、零 stake、单 validator、最大 validator 数；
- seed/domain/chain/epoch 重放；
- digest 截断、bitmap/索引越界、签名错误和整数溢出；
- 同 leader 同 slot 双签 evidence；
- 不依赖浮点数或无序 map 迭代。

**Gate：** verifier 可独立复现所有 verdict；模型检查不存在两个不同的“合法 scheduled leader”。

### P2：Runtime/session 接线（3--5 周）

工作项：

- 实现 `pallet-sunlead`；
- 注册 `sunl` opaque session key，完成 key generation、chain spec、keystore 与 key rotation；
- 实现 authority snapshot、epoch seed、schedule root 和 Runtime API；
- 设计独立 session rotation provider；
- 先在 BABE 仍运行时仅计算并暴露 SunLead schedule；
- 增加 runtime migration、try-runtime pre/post checks 和 storage version。

**Gate：** session 切换、validator 加入/退出、slashing、升级、历史快照均可重放；BABE 现有运行不受影响。

### P3：Node block production/import（5--8 周）

工作项：

- 实现 `SunLeadBlockImport`、`SunLeadImportQueue` 和 `SunLeadWorker`；
- 只在本节点为 scheduled leader 时启动 authoring；
- 在 proposal 前预构建后续 window 的 transaction/DEX batch template；
- 接入 block header digest、slot/timestamp 检查、equivocation 收集；
- 保留 GRANDPA block import、finality、warp sync 与 RPC；
- 增加 `sunlead_epoch`、`sunlead_nextLeader`、`sunlead_schedule`、`sunlead_status` RPC/指标。

**Gate：** 单节点和多节点 devnet 能稳定产块；非 leader、错误 slot、错误 schedule、错误签名均在 Runtime 执行前被拒绝。

### P4：devnet 故障与性能测试（4--6 周）

固定 V1 参数：3 秒 slot、2-slot window、GRANDPA finality。

场景：

- leader 宕机 / 重启 / 双签；
- window 内第一个或第二个 slot 漏块；
- 10% / 30% / 40% 网络延迟与随机丢包；
- 网络分区和恢复；
- epoch 边界和 authority/key rotation；
- snapshot sync、warp sync、DB 恢复与 replay；
- DEX 高负载、oracle、清算、ADL；
- GRANDPA 仍可在 SunLead 临时 fork 后正常最终化。

**Gate：** 相对 BABE 基线，不能降低出块可用性或 GRANDPA finality；内存、CPU、网络和 PoV 不超过预算；所有错误均可观测。

### P5：测试网迁移（至少 30 天）

迁移前提：所有验证人先升级到同时理解 BABE 与 SunLead header 的 node release。由于 block import 在 Runtime 执行前判断 block 有效性，不能只依赖一次 Runtime upgrade。

迁移流程：

```text
T-2 epochs：注册 sunl key、生成 schedule；仍由 BABE 出块
T-1 epoch ：检查 key、snapshot、schedule root 与节点版本 readiness
Activation：以 finalized activation block hash 锁定切换，下一 epoch 起 SunLead 出块
Post      ：历史 BABE block 仍可验证；activation 后 block 仅接受 SunLead digest
Finality  ：GRANDPA 全程保持运行
```

必须演练：旧节点拒绝/隔离、新旧 node 混跑策略、错误 activation height、回滚窗口、紧急停止 authoring、GRANDPA finality 恢复。

**Gate：** 连续 30 天无未解释 safety/liveness 差异；真实多地域验证人下性能优于 BABE 基线；独立安全审计通过。

### P6：优化迭代（独立治理决策）

仅在 P5 后，按风险从低到高逐项评估：

1. window 从 2 slots 调至 3--4 slots；
2. 3 秒 slot 降至 2 秒；
3. 评估 1 秒 slot；
4. 增加 SFF SkipQC，支持安全的提前 handoff；
5. 增加 BLS PrepareQC/CommitQC fast-confirmed；
6. 仅在 formal proof、长期 shadow 与审计完成后评估替换 GRANDPA finality。

任何一步都必须单独 benchmark、测试网运行和治理批准；slot duration 不能在已启动链上作为普通参数直接修改。

## 7. 验收指标

### 正确性

- [ ] 每 slot 的合法 leader 唯一、可由 parent state 独立推导；
- [ ] 非 leader block 在 Runtime 执行前被拒绝；
- [ ] 同 leader/slot 双签能形成并验证 slashing evidence；
- [ ] epoch、seed、set_id、schedule root、slot 和 parent 均签名绑定；
- [ ] 历史 BABE 与 activation 后 SunLead block 都可 replay；
- [ ] SunLead 分区/超时不会破坏 GRANDPA safety 或 liveness。

### 性能

- [ ] proposal p95/p99 相对 BABE 基线不退化；
- [ ] leader handoff 的 p95 开销降低；
- [ ] 空 slot 率、import failure、fork 数和 GRANDPA finality p95 可量化；
- [ ] DEX batch inclusion latency 和吞吐单独报告；
- [ ] CPU、内存、网络、block size、PoV 均在目标硬件预算内。

### 运营

- [ ] schedule/next leader/window、空 slot、equivocation、epoch activation 有 RPC 和指标；
- [ ] validator key provisioning、rotation、恢复与灾备有可复现 runbook；
- [ ] migration、rollback、snapshot、reorg、分区和节点升级演练通过；
- [ ] 外部审计覆盖 schedule、digest、import、session transition 与 slashing。

## 8. 预期收益与限制

预期收益：

- 消除诚实网络中的 BABE primary slot 多 winner 竞争；
- 减少 leader handoff、proposal warm-up、交易路由目标变化和 DEX batch 重打包；
- leader 可提前预热后续 1--3 个 slot 的交易模板与网络连接；
- 为后续直连投票、BLS QC 和 fast finality 提供稳定 leader 基础；
- 在相同 slot duration 下改善 proposal/传播尾延迟和可预测性。

限制：

- 3 秒 slot 下，端到端用户确认仍主要受等待下一个 slot 限制；
- window 不会自动提高 Runtime/WASM、订单簿或数据库吞吐；
- window 越长，leader 宕机、带宽瓶颈和审查暴露越大；
- 如不引入 SkipQC，故障 leader 至多造成一个 window 的空 slot；
- GRANDPA finality 不变时，强最终性延迟不会仅因确定性出块而数量级下降。

第一期成功标准不是宣称达到 Alpenglow 的 150ms finality，而是证明：在维持 GRANDPA 安全边界下，确定性 2-slot window 能够稳定降低 leader handoff 与 block proposal 的尾延迟，并且不降低可用性与最终性安全。

