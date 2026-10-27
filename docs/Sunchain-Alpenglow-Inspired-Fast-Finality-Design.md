# Sunchain：借鉴 Alpenglow 的快速最终性设计

> 状态：架构提案，未批准前不得替换 BABE/GRANDPA 的共识有效性。  
> 日期：2026-10-24  
> 目标：在不降低 Sunchain 当前安全边界的前提下，显著降低最终性延迟、降低验证人投票带宽与 CPU 开销，并为后续高性能交易执行域提供稳定的快速确认能力。

## 1. 决策摘要

不应把 Solana Alpenglow/Votor 的实现或阈值直接移植到 Sunchain。

Sunchain 当前采用 **BABE 出块 + GRANDPA 最终性**；Alpenglow 的 Votor 与其 leader window、slot/skip 规则、BLS 密钥体系、数据传播和惩罚/激励模型共同构成完整协议。只移植“80%/60% 阈值”或聚合签名会破坏其安全证明前提，也不能自动适配 NPoS 的会话、权重和 GRANDPA 锁定规则。

建议实施一个分阶段的混合协议，暂称 **Sunchain Fast Finality (SFF)**：

```text
                    现有、始终保留的安全路径
BABE block ──────────────────────────────► GRANDPA finality（权威）
   │
   │ 验证、导入并执行成功后
   ▼
SFF Direct Vote ─► BLS Prepare QC ─► BLS Commit QC ─► 快速确认 / 最终性候选
                                   │
                                   └── timeout / skip / repair
```

分阶段的含义至关重要：

1. **Shadow 阶段**，SFF 只产生并比较证书；所有用户、桥、交易所和 Runtime 仍只认 GRANDPA finalized。
2. **快速确认阶段**，SFF certificate 可作为 RPC 的 `fast_confirmed`，但不能触发提款、跨链证明或不可逆业务。
3. 只有在形式化验证、长期 shadow、故障演练和独立审计完成后，才通过 runtime/node 协调升级将 SFF Commit QC 设为新的最终性证明。
4. 任一阶段若 SFF 超时、证书错误、网络不健康或版本不一致，自动退回 GRANDPA；不能导致 BABE 出块或 GRANDPA 最终性停摆。

因此，在阶段 0--2 中，安全性不低于当前链：SFF 不是权威写者，也不是最终性判定者；它只能提供可撤销的低延迟信号。阶段 3 以后是否等价或更强，必须由正式安全证明和验收 gate 决定，而不能由性能测试推断。

## 2. 当前基线与瓶颈

### 2.1 当前实现

- Runtime 启用 `pallet-babe`、`pallet-grandpa`；
- node 侧由 `sc_consensus_babe::start_babe` 出块、`sc_consensus_grandpa::run_grandpa_voter` 最终化；
- `SLOT_DURATION` / `MILLISECS_PER_BLOCK` 当前为 **3 秒**；
- proposer 只拥有 slot 的 50% 时间用于构建 block；
- GRANDPA gossip duration 配置为 333ms。

这意味着现阶段端到端确认时间至少受到 3 秒 block 间隔、区块传播/执行、GRANDPA 多轮投票传播的共同限制。即使增加一个 150ms 的投票层，也不能让尚未被 BABE 生产的区块提前最终化。

### 2.2 可量化目标

在未改变 slot duration 的阶段，目标不是宣称 150ms finality，而是：

| 指标 | 当前基线 | SFF shadow / fast-confirmed 目标 |
|---|---:|---:|
| 已导入 block 到快速确认 p50/p95 | 先测量 | <= 1 个网络 RTT / <= 2 个 RTT |
| 已导入 block 到权威最终性 | 先测量 | GRANDPA 不退化；后续再比较 |
| 验证人投票字节与签名验证量 | 先测量 | certificate 路径显著低于逐票广播 |
| SFF 故障时 BABE/GRANDPA 可用性 | 100% 当前基线 | 不得退化 |

若要将 block 间隔降到 1 秒或以下，必须在独立升级中完成目标硬件、全球网络、执行 p99、PoV、传播、时钟漂移和网络分区测试。当前 BABE 的 slot duration 在已启动链上不能直接修改；这不是普通参数调节。

## 3. 从 Alpenglow 采用与明确不采用的内容

### 3.1 可采用的原则

本地 Firedancer `src/alpenglow` 实现体现了以下可借鉴的工程思想：

- 验证人之间直接传播 vote，而不是把 vote 当成普通用户交易；
- 对同一 epoch 的权重与验证人集合建立不可变 snapshot；
- 将 vote 聚合成带 signer bitmap 的证书，并独立验证阈值和聚合签名；
- 明确区分 notarize、finalize、skip/fallback 与 timeout；
- 对重复 vote、双投、skip 与 finalize 冲突维护可证明的违规证据；
- vote/certificate pool 使用有界状态、明确序列化格式、版本字段和网络域隔离；
- 数据可用性优先于投票：节点先完整导入、验证和执行 block，才可投票。

### 3.2 不直接采用的内容

- 不直接移植 Alpenglow 的 80% fast path / 60% fallback path；这些阈值依赖其完整锁定、skip、leader window 与故障模型，脱离后不安全。
- 不复用 Solana 的 shred、PoH、leader window、Rotor/Turbine 或 Bank 语义。
- 不复制 Firedancer C 实现进入 Rust node；应以协议规格为输入，使用 Sunchain 类型、会话和网络抽象重新实现，并保留测试向量互证。
- 不在 SFF 尚未证明正确前停止 GRANDPA，不将 SFF fast confirmation 用于桥、提款或 DEX 清算不可逆动作。
- 不把 BLS 聚合签名误认为共识算法；它只是降低证明大小和验证成本的密码学工具。

## 4. SFF 协议

### 4.1 安全模型

SFF 的最终安全目标沿用 GRANDPA/NPoS 的标准部分同步 BFT 假设：

- 每个 authority set 的 Byzantine stake 严格小于 1/3；
- session/epoch 权重、BLS 公钥及 key ownership proof 在投票开始前固定；
- 正确验证人对同一 `(set_id, height, round, phase)` 最多签署一个 vote；
- 验证人只对已获取、已执行且父链满足锁定规则的 block 投票；
- SFF 网络可被延迟、重排、重复、丢包或分区，但不能伪造签名。

两个冲突的 `>= 2/3` stake certificate 必然有至少 `> 1/3` stake 的交集；在上述假设下，该交集不可能全部双签。因此，所有会改变最终性的证书阈值都使用严格 `>2/3` 加权 stake，而非“约等于 60%”的阈值。

### 4.2 epoch authority snapshot 和 BLS 身份

每个 session 建立 `FinalityAuthoritySet`：

```text
set_id
activation_block
total_stake
[(validator_id, grandpa_id, bls12_381_public_key, proof_of_possession, stake)]
snapshot_root
```

- 引入新的 session key type，例如 `sffb`；
- BLS12-381 公钥注册必须验证 proof-of-possession，拒绝空、重复及无法解码的 key；
- `stake` 从 runtime 的已冻结 session snapshot 获取，不能从 node 本地 staking cache 推断；
- vote 和 certificate 必须包含 `genesis_hash`、`protocol_version`、`set_id`、`snapshot_root`，防止跨链、跨 session 和跨版本重放；
- key rotation 只在 session 边界生效；旧 set 的证书只能验证其自身 set_id。

### 4.3 消息与证书

```rust
Vote {
    version: u16,
    genesis_hash: H256,
    set_id: u64,
    snapshot_root: H256,
    height: BlockNumber,
    round: u32,
    phase: Prepare | Commit | Skip,
    block_hash: Option<H256>,
    parent_hash: H256,
    signature: BlsSignature,
}

QuorumCertificate {
    vote_payload_hash: H256,
    phase: Prepare | Commit | Skip,
    block_hash: Option<H256>,
    aggregate_signature: BlsSignature,
    signer_bitmap: BoundedBitVec,
    signed_stake: u128,
    set_id: u64,
    snapshot_root: H256,
}
```

`block_hash`、高度、round、phase、authority set 和域分隔符都必须被签名。certificate 验证必须先检查 bitmap 的长度、唯一 signer、stake 总和、阈值和 set，再验 aggregate signature；不能相信聚合者提供的 `signed_stake`。

### 4.4 两阶段锁定规则

为避免将“收集到很多赞成票”误当成最终性，SFF 使用两阶段 QC：

1. 验证人导入并执行候选 block 后，若其父块满足最高锁定 QC，签 `Prepare(height, hash, round)`；
2. 聚合到 `PrepareQC >2/3` 后广播；
3. 验证人仅在观察到该 block 的有效 PrepareQC 且仍满足锁定规则时签 `Commit(height, hash, round, prepare_qc_hash)`；
4. `CommitQC >2/3` 才是 SFF finality candidate；
5. 未在 deadline 前形成 PrepareQC 时，才允许对该 round 签 `Skip`；SkipQC 推进 round，不能覆盖已锁定冲突分支；
6. 每个 vote 的双签、Prepare/Skip 冲突、Commit/Skip 冲突均生成可验证 equivocation evidence，并沿用/扩展现有 slashing 流程。

这比 Alpenglow 的最快单轮路径慢，但其安全条件与 Sunchain 当前 <1/3 Byzantine stake 模型直接对齐。性能优化来自直连投票、批量聚合、证书传播和与 BABE 出块并行，而不是降低 quorum。

### 4.5 leader、聚合器与可用性

SFF 不引入新的 block leader，BABE 仍唯一负责生产 block。每个 `(set_id, height, round, phase)` 使用 authority set 计算确定性 aggregator 顺序：

```text
primary = H(genesis_hash || set_id || height || round || phase) mod validators
backup  = primary + 1, primary + 2, ...
```

- 所有验证人向 primary 发送直接 vote；未在 `aggregate_timeout` 内看到 QC 时向 backup fanout；
- 任一节点均可合法聚合并广播 certificate，primary 不是信任点；
- vote/certificate 可走专属 notification protocol，但必须保留有限 gossip/repair 路径；
- 接收方先执行快速、低成本的长度/version/set/bitmap 检查，再做 BLS 验证；
- 所有 pool、pending round、certificate、repair 请求和每 peer 队列有上限。

### 4.6 与 GRANDPA 的关系

| 阶段 | SFF CommitQC 含义 | 权威 finality | 故障行为 |
|---|---|---|---|
| F0/F1 shadow | 仅遥测和差分对象 | GRANDPA | 忽略 SFF |
| F2 fast-confirmed | 可撤销快速确认 | GRANDPA | RPC 降级为 GRANDPA 状态 |
| F3 dual-finality | 必须与 GRANDPA finalized 的祖先关系一致 | GRANDPA | 停止发 SFF confirmation |
| F4 hard fork 后 | 新 finality proof | SFF CommitQC | 仅在形式化/审计/迁移 gate 完成后启用 |

F0--F3 中，Runtime 不根据 SFF certificate 改变状态，不减少 GRANDPA vote，也不允许桥或 DEX 将其视为不可逆。

## 5. 代码落点

```text
crates/
  sff-primitives/             # SCALE 消息、certificate、错误码、域分隔
  sff-crypto/                 # BLS wrapper、PoP、聚合/批验，固定测试向量
  sff-verifier/               # no_std certificate/threshold/lock state machine

pallets/
  fast-finality/              # authority snapshot、BLS key/PoP、equivocation evidence

runtime/
  src/apis.rs                 # FastFinalityApi：authority snapshot、QC 查询
  src/configs/mod.rs          # session key、pallet 配置、升级 version

node/src/
  fast_finality_network.rs    # versioned direct-vote notification protocol
  fast_finality_pool.rs       # 有界 vote/QC/repair pool
  fast_finality_voter.rs      # import 后投票、锁定/round 状态机
  fast_finality_aggregator.rs # 聚合、backup、certificate 广播
  fast_finality_observer.rs   # shadow compare、指标、报警
```

现有接线点：

- `node/src/service.rs`：在 BABE import/网络创建后启动 SFF 网络、observer 和 voter；不替换 `start_babe` 或 `run_grandpa_voter`；
- `runtime/src/configs/mod.rs`：注册 BLS session key 与 authority snapshot 来源；
- `runtime/src/apis.rs`：提供固定高度的 authority set/snapshot 查询；
- `node/src/rpc`：新增 `sff_status`、`sff_certificate`、`sff_fastConfirmedHead`，明确标识其是否已 GRANDPA finalized。

## 6. 分阶段实施与 Gate

### F0：基线与可观测性

- 先增加 BABE proposal、block import、执行、GRANDPA prevote/precommit/finalized、网络 RTT 的 p50/p95/p99 指标；
- 采集正常、DEX 高峰、oracle stale、清算高峰、重组和网络降级场景；
- 固定目标硬件与 authority 地域矩阵。

**Gate：** 得到性能瓶颈证据；若主要瓶颈是 DEX 执行或 block propagation，而非 finality vote，则优先优化对应路径。

### F1：BLS authority snapshot 与 verifier

- 实现 key registration、PoP、session snapshot、SCALE 编码和 no_std certificate verifier；
- 只做离线 fixture、fuzz、属性和 cross-client 测试，不接入网络；
- 针对重复 key、bitmap 越界、stake 溢出、set rotation、恶意 aggregate 和域重放建立负面向量。

**Gate：** 独立实现得到完全相同的 certificate verdict；形式化状态机模型覆盖安全不变量。

### F2：Shadow vote / certificate

- node 在成功 import+execute 后发送 direct vote；
- 生成 PrepareQC/CommitQC，但不影响 fork choice、Runtime 或用户 finality；
- 对每个 CommitQC 比较其 block 与 GRANDPA finalized chain 的祖先关系；
- SFF 异常只能记录、限流、报警，不能中断 BABE/GRANDPA。

**Gate：** 连续至少 30 天和故障注入中零未解释冲突；p95 延迟、CPU、带宽满足目标；没有未界内存、连接或签名验证 DoS。

### F3：对外 fast-confirmed

- 开放带风险标识的 RPC/WS；
- DEX UI 可显示 fast-confirmed，但清算、提款、桥和资产会计仍等待 GRANDPA finalized；
- 为 indexer、钱包和 explorer 提供 reorg/降级处理接口。

**Gate：** 下游消费者完成回滚演练；没有把 fast-confirmed 当不可逆 finality 的路径。

### F4：替换权威 finality（单独治理升级）

- 仅在完整协议证明、外部审计、迁移机制、SFF/GRANDPA 双重运行结果和停机/分区演练通过后评估；
- 引入共识版本 digest、激活高度、双协议 handover certificate、明确 rollback 窗口；
- activation 前后的 authority set、key rotation、历史证明和 light client 验证必须可重放。

**Gate：** 证明任何两个冲突 block 不可能各自得到可接受 finality proof；安全、活性与资源界限均经独立审计。

## 7. 必测安全性质

- 任意高度至多一个 hash 可获得有效 CommitQC；
- 同一 validator 对同一 phase/height/round 的双签必可生成证据；
- `PrepareQC` 与 `CommitQC` 的 signer bitmap、stake、BLS aggregate、set_id 和 payload 完全绑定；
- session 切换前后 vote/certificate 不可重放；
- 无效 certificate、超大 bitmap、重复 signer、未知 set、错误 PoP 不造成 panic、无界分配或高代价 BLS 验证；
- SFF 超时、aggregator 拜占庭、网络分区和 vote flood 不阻断 BABE 出块或 GRANDPA 最终性；
- F2/F3 中 SFF CommitQC 不会触发不可逆 Runtime/桥/提款逻辑；
- F4 前必须有 model checking / TLA+ 或等价状态机证明，并结合随机长序列、Byzantine simulation 和多客户端差分测试。

## 8. 风险与边界

1. **不能承诺“直接达到 Alpenglow 150ms”。** 当前 3 秒 BABE slot 是硬下限的一部分；要达到亚秒端到端最终性需要独立处理 block interval、传播、执行和硬件。
2. **BLS 是高风险新增密码学依赖。** 必须做 PoP、DST/domain separation、subgroup 检查、常数时间实现审计、批验失败隔离和 key rotation 设计。
3. **证书收集不是数据可用性。** 节点只能在完整获得并执行 block 后投票；轻客户端/桥还需要 block/state proof 可得性策略。
4. **双协议会提高复杂度。** 因此 F0--F3 不让 SFF 影响 canonical finality；复杂性只换取可测量的低风险收益。
5. **DEX 不是替换共识的理由。** DEX 的吞吐瓶颈可能来自 Runtime/WASM 执行、状态读写、订单簿热键和 block 构建，应与 SFF 的 finality 优化并行测量、分别治理。

## 9. 推荐的近期行动

1. 批准 F0，先测量当前 BABE/GRANDPA 的端到端延迟与瓶颈；
2. 为 SFF 写独立 protocol spec 和状态机模型，不直接从 Firedancer C 代码翻译；
3. 启动 `sff-primitives` 与 `sff-verifier` 的纯 Rust、no_std 实现及测试向量；
4. 在不触碰 BABE/GRANDPA 权威性的条件下实现 F2 shadow；
5. 将“降低 BABE slot 至 1 秒”的可行性作为单独 RFC，依据 F0 的 p99 数据决定，而不与 SFF 绑定上线；
6. 在 F2 连续稳定前，不开始 F4，也不将 fast confirmation 用于资产不可逆路径。

