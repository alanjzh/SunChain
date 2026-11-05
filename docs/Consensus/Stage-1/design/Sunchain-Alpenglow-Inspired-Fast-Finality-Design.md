# Sunchain：借鉴 Alpenglow 的快速最终性设计

> 状态：架构提案，未批准前不得替换 BABE/GRANDPA 的共识有效性。〔该约束已于 2026-11-05 撤销，见下方策略更新〕
> 日期：2026-10-24  
> 目标：在不降低 Sunchain 当前安全边界的前提下，显著降低最终性延迟、降低验证人投票带宽与 CPU 开销，并为后续高性能交易执行域提供稳定的快速确认能力。

> **【2026-11-05 策略更新｜上位文档：`../Stage-2/design/Sunchain-Consensus-Genesis-Composition-Strategy.md`】**
> 项目方已决定：共识在 **genesis 时决策**、链生期不动态切换；**没有在运行的链需要升级**，按新链设计；最终形态为出块轴（BABE / SunLead）与终局轴（GRANDPA / SFF）在 genesis 任意组合、四种组合各自组网。
> 据此重定级（详见上位文档 §3 / §8）：
> - **取消**（§3.1）：头部"未批准前不得替换"约束；§4.6 / §6 中 F4 的**切换机制**——共识版本 digest、激活高度、双协议 handover certificate、rollback 窗口、SFF/GRANDPA 双重运行、迁移演练、"单独治理升级"式 gate；§7 的"多客户端差分测试"；F2 Gate / F3 Gate 中与迁移、下游回滚演练相关的时长与流程约束；§13 `- [ ] 10` 下游 fast-confirmed 接入（权威终局下用 `finalizedHead` 即可）。
> - **降级**（§3.2）：§13 P2 `- [ ] 8/9`（leader/primary 聚合 + fanout、Rotor 纠删码）维持"验证人上百后才必需"，不作为 4 组合组网前置；§9 第 5 项降 slot 仍为独立 RFC。
> - **保留为硬门槛**（§3.3）：F4 中 "**CommitQC 成为 new finality proof**" 的能力实现（justification 管线、`apply_finality`、fork choice 锁定、独立 set id、warp/light client）、§4 安全模型、§7 其余必测性质、**完整协议形式化证明 + 外部安全审计 + 故障注入**；§13 `- [ ] 11` 生产 `register_bls_key`。
> - **F4 重新定义**：从"在运行链上 hard fork 替换 GRANDPA"改为"**genesis 选定 SFF 作为该链唯一权威终局**"（上位文档 §1.3 / Stage 3）。不需要 handover，但安全证明不可省。
> 本文与上位文档冲突时，以上位文档为准。

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
| F4 genesis 选定 SFF 后 | 新 finality proof | SFF CommitQC | 仅在形式化证明/外部审计 gate 完成后启用 |

F0--F3 中，Runtime 不根据 SFF certificate 改变状态，不减少 GRANDPA vote，也不允许桥或 DEX 将其视为不可逆。

> **【2026-11-05 策略更新】** F4 由"在已运行链上 hard fork 替换 GRANDPA"改为"**在 genesis 时选定 SFF 作为该链唯一权威终局**"：不同组合即不同 genesis、不同链，不需要 handover / 激活高度 / 双协议并行 / rollback 窗口（上位文档 §1.3、§3.1）。表中"迁移 gate"相应改为"形式化证明 + 外部审计"这一安全 gate。F0--F3 的影子语义在**终局轴选 GRANDPA** 的组合中继续成立；终局轴选 SFF 的组合中 SFF 不再是影子。

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

### F4：genesis 选定 SFF 为权威 finality〔2026-11-05 重定义〕

> **【策略更新】** 原定义为"在已运行链上以单独治理升级替换权威 finality"，含迁移机制、双重运行、共识版本 digest、激活高度、handover certificate、rollback 窗口——这些**切换机制整体取消**（上位文档 §3.1：genesis 决策、无存量链、不动态切）。以下按新定义重写。

- **能力实现（必需，Stage 3）**：SFF voter 升为 essential；CommitQC 编码为 justification 并经 `JustificationImport` 验证后 `apply_finality` 写入 `finalized_number`；fork choice 按 SFF 锁定规则；按终局轴分派 justification import / voter / warp provider，取代 `--no-grandpa` 的半残状态。
- **配套前置（Stage 2）**：独立 authority set id 与 `sffb` 轮换（脱离 `pallet_grandpa::CurrentSetId`）、生产 `register_bls_key`、RFC 9380 hash-to-curve、接收层验票加固、SFF equivocation→slashing、SFF warp / light-client 证明（历史证明可重放）。
- **安全 gate（不可省）**：完整协议形式化证明（活性、round/Skip/timeout、set rotation、Byzantine 调度、分区）、外部独立审计、故障注入与长稳零未解释冲突。
- **不再需要**：handover、双协议并行、激活高度、rollback 窗口、新旧节点混跑、治理批准式切换流程、多客户端差分测试。

**Gate：** 证明任何两个冲突 block 不可能各自得到可接受 finality proof；安全、活性与资源界限均经独立审计；C3（BABE+SFF）与 C4（SunLead+SFF）各自多节点组网并通过上位文档 §6 组合矩阵验收。

## 7. 必测安全性质

- 任意高度至多一个 hash 可获得有效 CommitQC；
- 同一 validator 对同一 phase/height/round 的双签必可生成证据；
- `PrepareQC` 与 `CommitQC` 的 signer bitmap、stake、BLS aggregate、set_id 和 payload 完全绑定；
- session 切换前后 vote/certificate 不可重放；
- 无效 certificate、超大 bitmap、重复 signer、未知 set、错误 PoP 不造成 panic、无界分配或高代价 BLS 验证；
- SFF 超时、aggregator 拜占庭、网络分区和 vote flood 不阻断 BABE 出块或 GRANDPA 最终性；
- F2/F3 中 SFF CommitQC 不会触发不可逆 Runtime/桥/提款逻辑；
- F4 前必须有 model checking / TLA+ 或等价状态机证明，并结合随机长序列、Byzantine simulation 和多客户端差分测试。〔**2026-11-05 更正**：前三项（model checking / 随机长序列 / Byzantine simulation）**保留为硬门槛**；"多客户端差分测试"按上位文档 §3.1 降级为可选（当前仅单一客户端实现）。证明范围须从 F1 静态锁不变量扩展到活性、round/Skip/timeout、set rotation 与网络分区。〕

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

## 10. F1 实现状态（2026-10-30）

F1（BLS authority snapshot 与 verifier）已实现为三个纯逻辑 / 密码学 crate，**完全离线**，不接入 BABE/GRANDPA、不接入网络、不影响任何既有共识路径。

### 落地代码

| 设计落点 | 实现 |
|---|---|
| `crates/sff-primitives` | SCALE 编码的 `Vote` / `QuorumCertificate` / `FinalityAuthoritySet` / `FinalityAuthority`；域分隔标签 `DOMAIN_VOTE` / `DOMAIN_POP`；`canonical_message_bytes`（防跨链/会话/版本重放）；`SignerBitmap`（有界、越界检查）；`signed_stake_of`（独立重算，256 位精确 `> 2/3` 阈值 `exceeds_two_thirds`）；共享 `SffError`。`no_std`。 |
| `crates/sff-crypto` | BLS12-381 **min-sig**（签名 G1/48B，公钥 G2/96B）封装：keygen、sign/verify、聚合签名/公钥、`verify_aggregate`、PoP（sign/verify）、`verify_qc`（组合 verifier 逻辑 + BLS 校验）、`aggregate_votes`（离线 fixture 构造器）、`authority_set_from_keypairs`。std-only（节点侧）。 |
| `crates/sff-verifier` | `no_std` + `alloc`：`verify_certificate_logic`（set/snapshot 绑定、bitmap 边界、独立 stake 重算、`> 2/3` 阈值、payload-hash、phase 不变量）；`LockTracker` 两阶段锁状态机（Prepare→Commit 门控、冲突检测）；`detect_vote_equivocation` / `detect_qc_equivocation` 可证明违规证据。`rust` |

### 测试向量与负向覆盖

全部 22 个单测通过（`sff-primitives` 4、`sff-verifier` 8、`sff-crypto` 10），覆盖设计 §7 的安全性质：重复 key/bitmap 越界、stake 溢出、set rotation（snapshot 重放拒绝）、恶意 aggregate（低于阈值、错误 set_id、篡改 bitmap/signature、跨 snapshot 重放）、双签与 Prepare/Commit 冲突的违规证据。

固定可复现向量（交叉实现锚点）：`BlsKeypair::from_seed([0x01;32])` 的公钥已钉入 `sff-crypto` 的 `FIXED_VECTOR_PK` 常量，任何按文档流程独立重实现的实现必须产出相同字节。

### 已知限制（F1 明确范围内）

1. **hash-to-curve 为 try-and-increment**（`y^2 = x^3 + 4` 上计数查找），非 RFC 9380 最优 SWU。它确定、可逆、可复现，足以支撑 F1 离线 fixture 与交叉实现比对，但**生产前须替换为经审计的 RFC 9380 映射**（设计 §8.2）。
2. 子群检查用 `r·P == 0`（r 为 BLS12-381 标量域素数），cofactor clearing 用协议常数 `z = -0xd201000000010000` 推导的 `h1 = (z-1)^2/3`，无硬编码魔数。
3. **F1 不接网络**：无 notification protocol、无 pool、无 aggregator 扇出、无 observer。这些属于 F2（shadow vote/certificate）。
4. F1 验收时 `FinalityAuthoritySet` 仍是纯内存结构；runtime session snapshot、`sffb` 和 `FastFinalityApi` 已在 F2 接线，具体实现见 §11.1。

### F1 Gate 评估

- [x] 独立实现得到完全相同的 certificate verdict（primitives 阈值/锁逻辑 + crypto BLS 校验在单 crate 内一致实现）。
- [x] 固定可复现测试向量（公钥已钉入常量）。
- [x] 负向向量覆盖重复 key、bitmap 越界、stake 溢出、set rotation、恶意 aggregate、域/会话重放、双签/冲突证据。
- [x] 形式化状态机覆盖安全不变量（F1 gate 收尾项）：
  - `sff-f1-formal/SFFFinality.tla` + `SFFFinality.cfg`：TLA+ 规格，形式化陈述三条安全不变量 —— `SafetyInv`（同 `(height,round,phase)` 两冲突 commit 必共享 `> 1/3` stake，即任何安全破坏都需 `> 1/3` 双签，在 `< 1/3` Byzantine 假设下不可能）、`LockInv`（commit 必引用同 `(height,round,block)` 的 prepare）、`QuorumInv`（证书严格 `> 2/3`）。可用 TLC 在 `Nodes={n1..n4}`、4 节点均匀 stake、2 区块/2 高度/1 轮次下模型检验。
  - `crates/sff-verifier/src/lib.rs` 的 `mod model_check`：可执行的有界穷举模型检验，作为 TLA+ 不变量在真实代码上的 CI 可验证对应物。它对每个 1..5 委员会 + 多个加权 stake 配置穷举所有 `> 2/3` 法定证书与每个可达 `LockTracker` 状态（DFS 深度 4），断言：(1) **quorum-intersection 引理**——两冲突证书签名者交集 stake 严格 `> 1/3`（即 `3·inter > total`）；(2) **equivocation 完备性**——每个冲突对都被 `detect_qc_equivocation` / `detect_vote_equivocation` 捕获；(3) **锁安全**——`LockTracker` 任一层高不会记录两个不同 commit 块，且冲突 commit 必被拒。覆盖 `n∈{2..5}` 的全部投票双签组合（含 block/parent/prepare_qc 差异）。
  - 运行：`cargo test -p sff-verifier model_check`（`model_check_certificate_safety` + `model_check_vote_equivocation` 两个测试，约 0.1s 通过）。

## 11. F2 实现状态（2026-10-30）

F2（shadow vote / certificate 网络接线）已实现：**节点在成功 import+execute 后发送 direct vote；本地聚合生成 PrepareQC / CommitQC，但不影响 fork choice、Runtime 或用户最终性**；observer 仅将每个 CommitQC 的区块与 GRANDPA finalized 链做对照，日志/metrics 告警，且任何 SFF 异常都不得中断 BABE/GRANDPA。F2 任务作为非核心任务（non-essential task）启动，`panic` 或 stall 不会影响既有共识。

### 代码落点

**Runtime 侧（设计 §5 的 `pallets/fast-finality`、`runtime/src/apis.rs`）**

| 落点 | 实现 |
|---|---|
| `pallets/fast-finality` | 新增 pallet：以 `AccountId` 为键存储每个验证人的 BLS12-381 公钥（G2/96B）+ proof-of-possession；`register_bls_key(pk, pop)` 外因（链下节点验 PoP，runtime `no_std` 无法做 BLS）。仅存储，不参与 BABE/GRANDPA 记账。 |
| `runtime/src/apis.rs` | 新增 `FastFinalityApi`：`fast_finality_current_set_id()`（复用 GRANDPA `CurrentSetId`）+ `fast_finality_authorities(set_id)` —— 从 `pallet_session::Validators`、`NextKeys.sffb`、`pallet_staking` exposure（stake）和 `pallet_fast_finality::BlsKeys` PoP 组装 `FinalityAuthoritySet`。注册 key 必须与当前 session key 相同；validator_id 由 AccountId 左补零为 H256，grandpa_id 取自 session key。 |

**Node 侧（设计 §5 的 `fast_finality_network.rs` / `pool.rs` / `voter.rs` / `aggregator.rs` / `observer.rs`）**

| 落点 | 实现 |
|---|---|
| `node/src/fast_finality_p2p.rs` | 版本化专属 notification protocol（`/sun/sff/1`），gossip 循环镜像 `exchange_p2p` 的 DoS 加固：尺寸检查 → 去重（`SeenKey`）→ 按高度窗口限速 → **BLS 验签后才中继**（无效流量绝不放大）；`Repair` 请求仅回给询问方。 |
| `node/src/fast_finality_pool.rs` | 有界池：按 `(set_id,phase,height,round)` 收集 vote/QC、两阶段 `LockTracker`、CommitQC 快照；聚合达 `> 2/3` 才产出 QC，CommitQC 必须引用同集合、同高度、同轮次、同区块的 PrepareQC。 |
| `node/src/fast_finality.rs` | `FastFinalityEngine`：仅对新 best import 签 Prepare vote，并在每个 `(set_id,height,round)` 至多签一个 Prepare 目标；收到/生成 PrepareQC 后签 Commit vote；周期性聚合与 observer。Authority snapshot root 改变时重新解析 signer index。 |
| `node/src/service.rs` | 注册 SFF notification protocol；spawn SFF gossip 任务（validate+relay）与 SFF engine 任务（import 驱动投票 + 聚合 + observer），均为 non-essential。 |

### F2.1 `sffb` session key 与 keystore signer（方案 B）

F2 signer 已从 dev seed 派生切换为 `KeystoreSffSigner`。选择方案 B：保留 Sunchain 当前 `sff-crypto` 的 BLS12-381 min-sig 编码和域分离，不直接复用 SDK experimental BLS key type；SDK key type 的公钥/签名格式与 Sunchain 的 G2/96-byte 公钥、G1/48-byte 签名不兼容。为此增加应用专用 keystore 操作，不把 `BlsKeypair` 或私钥传进共识引擎。

落地路径：

1. `runtime/src/configs/mod.rs` 定义 `SffSessionPublic` / `SffSessionKey` 并将 `sffb` 加入 `SessionKeys`。`SffSessionPublic` 实现 `RuntimeAppPublic`：生成、公钥枚举、签名和 PoP 生成调用自定义 host function；验签和 PoP 验证使用 `sff-crypto`。
2. `runtime/src/sff_keystore.rs` 定义 `SffKeystore` runtime interface，提供生成密钥、公钥枚举、签名和 PoP 操作。节点在 `service::HostFunctions` 中注册该接口。生成操作在 keystore 后端内完成，runtime 只接收公钥；签名操作只把消息和公钥交给后端并取回签名。
3. `sp_keystore::Keystore` 增加 SFF 专用扩展操作。`sc-keystore::LocalKeystore` 实现密钥生成、签名和 PoP：根据 key type 和公钥定位 keystore 条目，在本地重建密钥对、校验公钥与条目匹配，然后只返回签名结果。配置了 keystore password 时，生成与签名使用相同的 password 派生路径（`Blake2(seed || password)`）。seed 按 LocalKeystore 的 SURI 条目格式持久化；此实现没有另加静态加密层，keystore 文件访问权限和 password 管理由节点部署负责，不能把它视作 HSM 或加密文件 keystore。
4. 节点 `KeystoreSffSigner` 持有 `KeystorePtr`，用 `keys(sffb)` 查找本机公钥、用 `sff_sign(sffb, public, payload)` 签名。F2 引擎只获得公钥和签名，不接收私钥。自定义/HSM keystore 可实现相同的 `Keystore` SFF 操作；未实现这些操作的后端会返回 `KeyNotSupported`。
5. 新链现有 staging genesis 没有 SFF 私钥材料，因此 `sffb` 初值是全零占位，**不从公开 BABE/GRANDPA 公钥推导 SFF 密钥**。`FastFinalityApi` 在 key 为零、PoP 未注册或注册公钥与 `NextKeys.sffb` 不一致时不提供 authority set。验证人轮换出真实 `sffb` 并注册其公钥与 PoP 后才加入 SFF set。

操作流程：在允许 unsafe RPC 的本机管理接口调用 `sff_rotateKeys`。该方法与 `author_rotateKeys` 一样生成并保存完整 session key set，同时返回 `sessionKeys`（可供 `session.set_keys` 使用）、`sffPublicKey` 和 `proofOfPossession`。validator 使用 `sessionKeys` 提交 `session.set_keys`，再用公钥与 PoP 调用 `pallet_fast_finality::register_bls_key`。RPC 不自动提交链上交易；validator 仍需使用自己的账户签名并等待 session key 生效。此 RPC 会新生成一组 session keys，不要把它当作只查询命令。

自定义 keystore 后端也必须实现 `sff_generate_new`、`sff_sign`、`sff_generate_proof_of_possession` 三项操作才能参与 SFF；当前 LocalKeystore 的 SURI seed 沿用标准 keystore 文件存储方式，且没有额外静态加密层，部署者须保护 keystore 目录与管理 RPC。

### 自检

- `cargo check -p sunchain-runtime` 通过（运行时已用 `SKIP_WASM_BUILD=1` 跳过 WASM 重编）。
- `cargo check -p sunchain`（lib + bin）通过。
- keystore signer / `RuntimeAppPublic` / host bridge 接线后再次运行 `RUSTC_WRAPPER= SKIP_WASM_BUILD=1 cargo check -p sunchain --quiet`，通过；该检查跳过 WASM 重编，未替代两节点签名与 PoP 注册流程验证。
- `cargo test -p sc-keystore sff_` 3 项通过：除原有 `sff_key_generation_signing_pop_and_password_round_trip`（SFF key 生成/枚举、正确签名与 PoP、篡改签名、错误/缺失公钥、keystore password、不支持 SFF 的 backend）外，新增 `sff_operations_are_forwarded_through_a_shared_keystore_ptr` 与 `sff_signing_works_on_an_in_memory_keystore`（见 §11.2）。
- F1 三个 crate 单测仍全绿（`sff-primitives` 10 / `sff-verifier` 10 / `sff-crypto` 10），`sff-primitives` 仅新增 `Phase: Hash` 派生与统一 `alloc`/`vec!` 前置（为 runtime `std` 编译需要），未改动协议逻辑。

### F2 复核修正（2026-10-30）

- 修复 `BlsSignature` / `BlsPublicKey` 重复 `MaxEncodedLen` 实现导致的编译失败。
- 按 authority `set_id` 隔离 pool vote/QC/CommitQC 与锁状态；CommitQC 可有界暂存应对网络乱序，并在 PrepareQC 到达后验证关联与锁规则。
- 只对新 best 区块签 Prepare，并用 `(set_id,height,round)` 本地防双签；observer 正确检查 SFF block 是否为 GRANDPA finalized head 的祖先。
- P2P 只在 BLS 与 genesis 校验通过后占用 seen key，并对 Repair 请求限速、拒绝 SCALE 尾随垃圾字节。
- 验证：F1 crate 测试通过；F2 node 定向测试 5 项通过；runtime 与 node check（lib + bin）通过（跳过 WASM build）。两节点链上流程见 §11.1。

### 11.1 dev 两节点端到端验证（2026-11-02）

在 dev 链上真实跑通了 vote → PrepareQC → CommitQC → observer 对照 GRANDPA 的完整影子流程。

**接线方式（仅 dev）**

1. `node/src/chain_spec.rs::dev_sffb_for` 用 `BlsKeypair::from_seed(blake2_256(b"//Alice"))` / `b"//Bob"` 确定性派生 genesis 的 `sffb` session key（非 dev 账户仍为零值）；节点侧 `inject_dev_session_keys` 用**同一 seed** 调 `Keystore::sff_generate_new`，两者是同一条 `blake2_256(seed)` 派生路径，故链上公告的公钥就是节点实际持有私钥的那个公钥。
2. dev 的 PoP 由 `RuntimeAuthorityProvider` 的 dev 列表按公钥回填。**F1 的 `pop_verify` 校验没有被放宽**——零 PoP 仍会被 `verify_vote` 以 `InvalidPop` 拒绝（早期正是因此出现 `self Prepare vote failed verification; skipping`）。
3. `FastFinalityApi::fast_finality_authorities` 允许在 `BlsKeys` 未登记时回落到 `NextKeys.sffb`（PoP 记零），但保留"零 key"与"登记公钥 ≠ session key"两个拒绝条件。

**测量结果**（`--chain dev --alice/--bob --validator`，两节点互联，`--fresh`，观察窗口 11:24–11:38，约 14 分钟）

| 指标 | 节点 A（`//Alice`） | 节点 B（`//Bob`） |
|---|---|---|
| Prepare vote 签名 | 191 | 197 |
| Commit vote 签名 | 70 | 110 |
| PrepareQC | 28 | 71 |
| CommitQC | 1 | 2 |
| vote 验签失败 | 0 | 0 |
| panic | 0 | 0 |
| best / finalized | #271 / #269 | #271 / #269 |

**核心安全结论（shadow-only）**：两节点 GRANDPA 最终化**完全一致且持续推进**（均 finalized #269、best #271），SFF 全程没有改变 fork choice、runtime 状态或用户最终性，0 panic / 0 stall。这与 §11 的 non-essential 约束一致：SFF 只观测、不干预。

**两阶段与 set 轮换均真实生效**：`set_id` 从 0 推进到 4，说明 authority snapshot 随 session 轮换更新，节点在轮换后仍能解析新的 signer index 并继续签名/聚合。

**样例日志**

```text
built PrepareQC set_id=0 height=23 round=0 block=Some(0xc82d266f7b7e332abca570fd0701dadf5ad240147ec3603972c58605eefc3a42)
built PrepareQC set_id=4 height=155 round=0 block=Some(0x0de8c19dd990578a8b0d3932a90b665549fa7c8f222e913c374923e42535e6ca)
SFF CommitQC height 13 block Some(0x5956ae32bddaaa6edd4c3685d9440eaa881993ba3fb2f0f581dfffc0ee203689)
  is NOT on the GRANDPA finalized chain (finalized 0xbc7363251906c808314148d9fc513a49a5d8815e85bc001835f9c8857efcb0f8)
```

最后一条是 observer 的**预期告警**（SFF 领先于 GRANDPA），不是错误；反向（SFF 落后/分叉）时才需要排查。

**本次验证暴露并已处置的缺口**

- `Arc<dyn Keystore>` 未转发 SFF 操作，生产 `KeystoreSffSigner` 当时全部签名返回 `KeyNotSupported`，dev 期间临时改用 `DevSeedSigner` 绕过。**该缺陷已在 §11.2 修复**，dev 与生产现在走同一条 keystore 签名路径。
- dev 链是 `EthereumSignature`（`AccountId = H160`，secp256k1 + keccak），现有工具链没有可用签名器，因此**生产 `register_bls_key` 仍未在链上真实注册过**；dev 走的是"未登记回落"分支。生产部署必须注册真实公钥 + PoP，否则零 PoP 会被 F1 verifier 拒绝。

### 11.2 `sff_sign` 修复：`Arc<dyn Keystore>` 未转发 SFF 操作（2026-11-02）

**现象**：`KeystoreSffSigner` 每次签名都失败——

```text
SFF keystore signing failed: Key not supported: KeyTypeId([115, 102, 102, 98])
```

**根因（不在 `local.rs` 的 `sff_sign`/`sff_pair` 里）**：`KeystorePtr = Arc<dyn Keystore>`，而
`substrate/primitives/keystore/src/lib.rs` 中的 `impl<T: Keystore + ?Sized> Keystore for Arc<T>`
**没有转发** `sff_sign` / `sff_generate_proof_of_possession` / `sff_generate_new`。Rust 的方法解析会先命中
`Arc<T>` 的这个转发实现，于是调用落到 trait 的**默认实现**（对每个 key type 一律返回 `KeyNotSupported`），
根本没有进入 `LocalKeystore`。任何持有 `KeystorePtr` 的调用方（节点引擎、`sff_rotateKeys` RPC）都会静默
降级成"后端不支持 SFF"——这也是为什么 `LocalKeystore` 自身单测全绿、线上却全量失败。

**修复**

1. `substrate/primitives/keystore/src/lib.rs`：在 `Arc<T>` 转发实现里补全三个 SFF 方法（`(**self).sff_*`）。
2. `substrate/client/keystore/src/local.rs`：`sff_generate_new` 在**内存型** keystore（没有 key 文件可读）
   下把 seed 记入 ephemeral map（新增 `KeystoreInner::insert_ephemeral_seed`），否则刚生成的 key 能被
   `keys()` 列出却永远签不出名。
3. `node/src/service.rs`：删除 `DevSeedSigner` 绕过，统一使用 `KeystoreSffSigner`；`DevSeedSigner` 降级为
   单测内的最小 signer 实现。

**回归验证**

- `cargo test -p sc-keystore sff_` 3 项通过。新增的两个测试直指缺陷：`sff_operations_are_forwarded_through_a_shared_keystore_ptr`
  走 `KeystorePtr`（去掉转发后精确复现 `KeyNotSupported(KeyTypeId([115,102,102,98]))`），
  `sff_signing_works_on_an_in_memory_keystore` 覆盖内存型 keystore。
- dev 两节点重跑（11:56–12:03，**keystore 签名路径**）：A 92 Prepare / 9 Commit / 1 PrepareQC + 1 CommitQC；
  B 78 Prepare / 54 Commit / 46 PrepareQC。双方 `SFF keystore signing failed` **0 次**、验签失败 0 次、
  panic 0 次，finalized #115 / #116、best #118 一致。节点日志可见
  `injected dev sffb session key (seed //Alice) pubkey 0xa4eff6ea…`（`//Bob` 为 `0xad15496e…`），
  与 dev genesis 公告的公钥逐字节一致。

**遗留观察**：两节点的 QC 数量不对称（A 明显少于 B，此前一轮是 28 vs 71），怀疑是 gossip 限速/聚合时机
差异而非方向性故障——A 仍能收到 B 的票（至少产出过 QC），且后续轮次比例会变化。shadow 模式下无安全影响，待查。

### 已知偏离 / 限制（F2 明确范围内）

1. **新链 genesis 尚未配置有效 `sffb`**：session key 类型和 LocalKeystore signer 已实现，但 staging genesis 只能提供零值占位。验证人需通过 `sff_rotateKeys` 获取新的完整 session key set 和对应 PoP，再调用 session 与 fast-finality pallet 的注册接口。
2. **PoP 链下验证**：runtime `no_std` 不能做 BLS，故注册时不验 PoP；节点取 authority set 后链下 `pop_verify`，非法 key 仅告警、**保留其在集合中的位置**以维持 vote 的 `signer_index` 对齐。
3. **仅 round 0，无 Skip / 轮次推进**：F2 只做 Prepare→Commit 两阶段；超时/重排仅驱动 Commit，不推进 round（设计 §4 的 round 推进留待 F3/F4）。
4. **`activation_block` 未记账**：runtime 端未跟踪 session 激活块，填 `0`（仅影响集合标识 tuple，节点重导）。
5. **端到端验证范围**：dev 两节点已跑通 vote→PrepareQC→CommitQC→observer 对照 GRANDPA（§11.1，0 验签失败 / 0 panic，GRANDPA 未受影响）。但**生产路径仍未验证**：`register_bls_key` 尚未在链上真实注册（dev 走的是未登记回落分支），且 `KeystoreSffSigner` 曾因 `Arc<dyn Keystore>` 未转发而全量失败（§11.2）。设计 §9.4 的完整影子流程需在带真实 BLS key 注册 + keystore signer 的两验证人链上复验。
6. **启用条件**：每个当选验证人的当前 `NextKeys.sffb` 必须非零，并且 `pallet_fast_finality::BlsKeys` 中登记的公钥及 PoP 与该 session key 匹配；否则 authority set 不完整，`FastFinalityApi` 返回 `None`，SFF 静默 dormant（observer 仍运行但无 QC）。
7. **observer 对照已升级为四分类（F3）**：原实现只有"在/不在 finalized 链上"两态，把正常的"SFF 领先"当告警。现按 §12 分为 `onFinalizedChain` / `aheadOfFinality`（正常快速确认）/ `conflicting`（分叉，停发并告警）/ `unknown`；只有 `conflicting` 记 `warn`。Dormant（runtime 给不出 authority set）也有限速日志，便于与"坏了"区分。

## 12. F3 实现状态（2026-11-02）

F3 把 F2 的 CommitQC 变成对外可读的 **fast confirmation**：`node/src/fast_finality_view.rs`
（状态机）+ `node/src/rpc/sff.rs`（RPC/WS）+ 引擎/observer 接线。GRANDPA 仍是权威最终性；
所有对外字段都显式标注"可撤销"，清算/提款/桥/资产会计必须继续等 GRANDPA（F3 gate）。

### 12.1 一致性规则（设计 §4.6 F3 行）

每个 CommitQC 按它与 GRANDPA finalized 链的祖先关系分类，只有前两类会被发布：

| 分类 | 含义 | 行为 |
|---|---|---|
| `onFinalizedChain` | QC 区块就是 finalized head 或其祖先 | 发布，`grandpaFinalized=true`、`revocable=false` |
| `aheadOfFinality` | finalized head 是 QC 区块的祖先（SFF 领先） | 发布，`revocable=true`（这才是 fast-confirmed 的正常态） |
| `conflicting` | QC 区块在 GRANDPA 没终局的分支上 | **停发**，`degraded=true`，记录 `lastConflict` 供下游回滚 |
| `unknown` | 区块本地还没导入 | 不发布 |

关键点：
- **发布前每次查询都会重新校验**（`revalidate`）。已经发布的确认若被重组掉（不再是 finalized 链祖先/后代），会立刻降级并停发 —— 这是给 indexer/钱包/explorer 的 reorg 接口。
- 一致后自动恢复服务，但 `conflicts` 计数与 `lastConflict` **保留**，下游仍可据此回滚。
- `revision` 单调递增，WS 订阅只在它变化时推送，消费者不必轮询比对 payload。

### 12.2 对外接口（`node/src/rpc/sff.rs`）

| 方法 | 返回 |
|---|---|
| `sff_status` | `phase / serving / degraded / degradeReason / conflicts / lastConflict / fastConfirmedHeight / grandpaFinalizedNumber / revision / warning` |
| `sff_fastConfirmedHead` | 当前快速确认头（含 `consistency`、`grandpaFinalized`、`revocable`、`warning`），停发或无证书时为 `null` |
| `sff_certificate(height)` | 该高度的 CommitQC（有界历史 128 条），无则 `null` |
| `sff_subscribeFastConfirmed` (WS) | 状态变化推送（新确认 / GRANDPA 追上 / 降级 / 恢复） |

`warning` 字段恒非空：`"fast confirmation is revocable: GRANDPA remains the authoritative
finality; clearing, withdrawals, bridges and asset accounting must wait for GRANDPA finalized"`。

### 12.3 本轮顺带修掉的吞吐问题

先前 SFF 的确认高度会**完全卡死**（停在 #47 不再前进）。根因：vote 绑定 `(set_id, snapshot_root)`，
而 runtime 只能构造**当前** set（session 轮换后 session key 变了），因此：

- gossip `validate` 对旧 set 的 vote 一律 `authorities(old) = None` → 丢弃；
- 聚合只处理当前 set 的 key → 轮换时在途的 Prepare/Commit 全部作废。

dev 链 session 极短（约 30–48 块一轮换），于是大多数高度来不及完成两阶段。

修复：`RuntimeAuthorityProvider` 增加**有界快照缓存**（`SetCache`，上限 8 个 set，按插入顺序淘汰），
聚合遍历所有仍可解析的 set（含刚轮换出的）。缓存只在 set 仍为当前时从 runtime 读取并写入，
旧 set 只读缓存；RPC/gossip/引擎三条路径共用。有界，长轮换历史不会让它增长。

效果（dev 两节点，同一脚本）：修复前 sff 高度停在 #47 且 revision 冻结；修复后持续推进
（sff #71→#78，revision 38→44），两节点的投票/QC 数量也从 92:68、1:46 的不对称收敛到
82:82、23:14。

### 12.4 验证

**单测**：`cargo test -p sunchain --bin sunchain fast_finality` 14 项通过，其中 6 项是 F3 新增
（`consistency_classifies_finalized_ahead_and_conflicting`、`ahead_of_finality_is_served_but_revocable`、
`conflicting_commit_degrades_and_stops_serving`、`a_served_head_that_is_reorged_away_degrades`、
`serving_resumes_after_a_divergence_but_keeps_the_audit_trail`、`history_is_bounded…`）
+ 2 项 `SetCache` 边界测试。

**dev 两节点实跑**（`--chain dev --alice/--bob --validator`）：

```jsonc
// sff_status
{ "phase": "f3-fast-confirmed", "serving": true, "degraded": false, "conflicts": 0,
  "fastConfirmedHeight": 76, "grandpaFinalizedNumber": 107, "revision": 40,
  "warning": "fast confirmation is revocable: GRANDPA remains the authoritative finality; ..." }

// sff_fastConfirmedHead
{ "height": 70, "setId": 2, "consistency": "onFinalizedChain",
  "grandpaFinalized": true, "revocable": false,
  "warning": "finalized by GRANDPA: irreversible" }
```

- `sff_certificate(42)` 返回该高度 CommitQC；`sff_certificate(9999)` → `null`。
- WS 订阅实测收到推送（`subscription id` + `rev=21 serving=True height=70 finalized=#178`）。
- 全程 `SFF keystore signing failed` 0 次、验签失败 0 次、panic 0 次、conflicts 0，GRANDPA 正常终局。

### 12.5 F3 已知限制

1. **dev 上 fast-confirmed 仍落后 GRANDPA 约 30 块**（sff #78 vs finalized #119），即现在还**没有**体现延迟优势；两阶段（Prepare→Commit）串行 + 2/2 门槛 + 200ms 聚合节拍是主因。shadow 模式下无安全影响，但 F3 的价值要等 F2 的延迟 gate（primary/backup fanout、聚合节拍）做完才成立。
2. **下游尚未接入**：设计里"DEX UI 显示 fast-confirmed、清算/提款仍等 GRANDPA"属于消费方工作，未做；本轮只提供接口与风险标识。
3. 生产 `register_bls_key` 仍未真实注册（dev 走未登记回落分支），见 §11 已知偏离 5。
4. 快照缓存让节点在 set 轮换后仍可用**旧快照**完成/验证证书；上限 8 个 set，超过即淘汰。这是可用性与"runtime 只认当前 set"之间的权衡，生产 session 很长时不触发。

## 13. F3 延迟分析：为什么 fast-confirmed 落后 GRANDPA（2026-11-02）

F3 的接口与安全语义已完成，但**它现在比 GRANDPA 慢**，因此还没有兑现"低延迟确认"的价值。
本章记录成因、量化数据与待办，作为 F2 延迟 gate（设计 §6）的工作清单。

### 13.1 实测数据（dev 两节点，debug 构建）

| 指标 | 实测 |
|---|---|
| 出块间隔 | ≈ 2.9 s / 块 |
| SFF 单高度完成耗时 | ≈ 8.6 s / 高度（71→78 用了一分钟） |
| 落后量 | 跑 6 分钟后 fast-confirmed #78 vs GRANDPA #119（落后 41 块，且持续扩大） |
| 投票/成证率 | 114 个块中签 82 张 Prepare 票（72%），最终 23 个 QC（≈20%） |
| 修复前 | fast-confirmed **冻结**在 #47 不动（GRANDPA 到 #160） |

### 13.2 成因

**(a) 结构性：逐高度确认 vs 一轮终局一段链。** GRANDPA 一轮 precommit 终局"最优链上尽可能高的
祖先"，一次吃掉几十个块，并有 catch-up 消化积压。我们的 `fast_confirmed_height` 是"已完成的最高
高度"，每个高度各自跑一遍 Prepare→Commit，且**没有追赶机制**——一旦落后只能以同样速率追，永远追不回。

**(b) 主因：全量轮询聚合，成本随积压增长（正反馈）。** `run_fast_finality` 每 200 ms 遍历池里
**所有**未完成的 `(set, phase, height, round)` 调 `try_build_qc`；而 `sff_crypto::aggregate_votes`
对**每一张票**重新做一次 BLS 验签（`verify_vote`）。于是

```text
单次聚合成本 ≈ 积压高度数 × 每张票的 BLS 配对验签名
```

名义 200 ms 的周期变成 `200ms + 全量扫描`，积压越长越慢 → 越慢积压越长 → **正反馈**；极端表现就是
"冻结在 #47"。debug 构建的 BLS 配对比 release 慢一个量级，放大了系数，但 O(积压) 的结构不变。

**(c) 两阶段串行，且每次都要等一个聚合周期 + 一跳 gossip。** 高度 h 的时间线是
Prepare 票 →（等 tick）→ PrepareQC → Commit 票 →（等 tick）→ CommitQC，中间没有流水线。
GRANDPA 的 prevote/precommit 在同一轮内由消息事件直接驱动，不等定时器。

**(d) 空洞不补。** `on_block_imported` 只在 `is_new_best` 时投票，`claim_prepare` 对
`(set,height,round)` 只认一次、永不重投；两节点链上乱序到达（块 100 先到、99 后到）会让高度永久
缺票。再加**没有轮次推进 / Skip 票**（`round` 恒为 0）、**Repair 从不发送**（`repair_for` 是
`#[allow(dead_code)]`），空洞既不推进也不修复。这解释了 82→23 的乘法衰减。

**(e) 没有 leader / Rotor / 批量。** Alpenglow 快的两大来源缺失，对比：

| Alpenglow | 当前实现 |
|---|---|
| Votor：leader 聚合，单点产出证书 | 每个节点独立对全部积压做全量聚合 |
| Rotor：纠删码分片 + 抽样委员会中继，单跳扇出 | flood-fill 广播给所有 peer，且**中继前先做 BLS 验签**（每跳付费） |
| primary + backup 冗余发送，抗慢/丢包 | 单路径，丢包即空洞（见 d） |
| 一轮终局一批块 | 逐高度；每块每节点 2 条消息（4 条/块） |

### 13.3 待办（按优先级）

**P0 — 先把单高度成本从 O(积压) 降到 O(1)（切断正反馈）**

- [x] 1. **事件驱动聚合**：`FastFinalityPool::take_dirty()` 只返回"票集合自上次尝试后变化过的 key"；`run_fast_finality` 用 `tokio::Notify` 在每次插入时唤醒引擎，200 ms 定时器降级为裁剪/兜底的安全网。
- [x] 2. **增量聚合**：`aggregate_votes` 对每张票的重验只发生在该 key 的 dirty 周期内；`add_vote` 写入即 `dirty.insert` + `notify`，`take_dirty` 抽取后清空，未变化的 key 不再进入 BLS 验签路径。
- [x] 3. **积压上限**：`MAX_BACKLOG_HEIGHTS = 64` 作为安全网；`prune_backlog()` 每节拍按 `finalized.min(best - 64)` 设 floor 并 `prune_below`（裁剪到 GRANDPA 终界，而非固定尾部，避免提前丢弃未完成高度），旧高度的票/证书/去重键一并丢弃；`add_vote` 直接拒收 `height < floor` 的票。

**P1 — 协议完整性（决定"会不会卡住"）**

- [x] 4. 批量确认：一次确认到某个高度（对齐 GRANDPA 一次终局一批）。
- [x] 5. 轮次推进与 Skip 票（`round` 现在恒为 0，高度拿不到法定人数就永远不动）。
- [x] 6. 主动 Repair：原 `repair_for` 死代码已移除（§13.2(d) 描述的历史状态）；现由 §13.7.3 的 `maintain_votes`（重发未达 2/3 的 Prepare/Commit 票）+ `request_repairs`（向 peer 发 `RepairRequest` 补证书）实现，两函数均会**主动发送**。
- [x] 7. 非 `new-best` 区块补投 / catch-up，消除 (d) 的永久空洞：由 §13.7.3 把 healing 窗口下界锚定到 `fastConfirmedHeight`（前沿）实现——`[frontier, best]` 内任何缺口都被重发 / 补证，前沿推进后窗口自然收窄；`advance_rounds` 对低于其窗口的高度直接忽略（不 Skip），安全。

**P2 — 规模化（验证人数量上百后才必需）**

- [ ] 8. Leader/primary 聚合 + primary/backup fanout。
- [ ] 9. Rotor 式纠删码分发、票打包压缩。

**F3 侧遗留**

- [ ] 10. 下游接入：DEX UI / 钱包显示 fast-confirmed，清算提款仍等 GRANDPA。
- [ ] 11. 生产 `register_bls_key` 真实注册（dev 走未登记回落分支）。

> **【2026-11-05 待办重定级｜上位文档 §3】** 复选框保留原始记录；实际排期按下列定级：
> - **8 / 9（P2 规模化）→ 降级**：维持"验证人上百后才必需"的判断（`Sunchain-SFF-Gossip-Delivery-Diagnosis.md` §7），**不作为四种 genesis 组合可组网的前置**。
> - **10（下游 fast-confirmed 接入）→ 取消**：终局轴选定 SFF 后其为权威终局，下游直接跟随 `finalizedHead`；仅当仍需对外提供 shadow 组合的 fast-confirmed 读数时保留为可选项。
> - **11（生产 `register_bls_key`）→ 保留为硬门槛**（上位文档 §3.3 B2 / Stage 2）：连同 `sffb` 真实轮换、PoP 验证、零值占位与链外 PoP 注入的移除一并完成。
> - **新增（本文未列，见上位文档 §3.3 / §5 Stage 3）**：SFF `JustificationImport` + `apply_finality`、fork choice 锁定、独立 set id、SFF warp/light-client、F4 级形式化证明与外部审计——这些是 C3/C4 能组网的核心缺口，优先级高于上述全部 P2 项。

### 13.4 验收标准

| 项 | 验收 |
|---|---|
| 1+2+3 | dev 两节点上 `fastConfirmedHeight` 与 `grandpaFinalizedNumber` 的差距**收敛且不再扩大**；聚合 CPU 不再随积压增长；不再出现高度冻结 |
| 5/6/7 | 连续运行 30 分钟无永久空洞（每个高度最终都有 PrepareQC 或被显式放弃） |
| 4/8/9 | p95 确认延迟优于 GRANDPA（设计 §6 F2 延迟 gate） |

以上全部处于 shadow 模式：不影响 BABE 出块、不影响 GRANDPA 终局、无安全后果——只是 F3 承诺的
低延迟尚未兑现。

### 13.5 P0 实测结果（2026-11-02）

重新构建并跑通 1+2+3 后，dev 两节点（`--chain dev --alice/--bob`）表现：

| 指标 | 修复前（§13.1） | 修复后（P0） |
|---|---|---|
| `fastConfirmedHeight` vs `grandpaFinalizedNumber` | 落后 41 块且持续扩大 | **领先 1 块**（`aheadOfFinality`，revocable） |
| 单高度完成耗时 | ≈ 8.6 s / 高度 | 追平出块节拍（128 个近期高度连续完成） |
| 高度冻结 | 有（卡在 #47） | 无 |
| 近期窗口完成密度 | 约 20% | 126/128（仅 2 个空洞，源于丢票） |
| 签名失败 / 验签失败 / panic | 0 / 0 / 0 | 0 / 0 / 0 |
| 一致性分类 | — | `aheadOfFinality` 实测出现（`sff_fastConfirmedHead` 返回 `revocable=true`） |

**结论**：§13.2(b) 的正反馈被切断——聚合成本从 O(积压) 回到 O(每 tick 的脏 key)，确认前沿随出块持续推进并反超 GRANDPA。这同时满足 design §6 F2 延迟 gate 的"收敛且不再扩大"验收项：F3 的快速确认现在**真实成立**（实测 SFF #140 vs GRANDPA finalized #138，SFF 领先 2 块）。

**空洞消除（P0 收尾）**：首轮修复后仍有 2/128 空洞，根因不是丢票，而是 `floor = best - 64` 的**固定尾部裁剪**会在某高度凑齐 CommitQC 之前就把它 prune 掉（被裁的高度早已被 GRANDPA 终局，却从未形成过证书）。把裁剪策略改为 **floor = GRANDPA 终界**（仅在 GRANDPA 滞后超过 `MAX_BACKLOG_HEIGHTS` 时才退化为固定窗口）后，再次实测：活跃窗口 77..140 **64/64 连续、0 空洞**，仅 height 5（启动 warmup 残留，已远超终界、无害）一个早期空洞。 backlog 仍被终界差有界，O(积压) 不会复发。

**仍明确的限制**

- dev（2 验证人）下 GRANDPA 本身极快（2/2 即终局），所以 SFF 只领先 1–2 块；在更大网络里 GRANDPA
  延迟随规模上升而 SFF 两阶段近似恒定，领先量会更大——但这需要用多验证人网络实测才能量化。
- 主动 Repair（P1-6）的**接收/应答侧**已在 `fast_finality_p2p.rs` 就绪（`RepairRequest` + `answer_repair`），但引擎从不发送；由于裁剪策略已消除绝大多数空洞，其优先级下降，仅在出现真实丢票时才需要补齐发起侧。
- P1（轮次推进 / Skip / 批量 / leader / Rotor）尚未做，决定规模化后的延迟与吞吐上限。

### 13.6 P1-4 / P1-5 实测（2026-11-02）

**P1-4 批量确认（视图层）**：`SffStatus` 新增 `fastConfirmedBase` 与 `fastConfirmedFrontier`。
`fastConfirmedFrontier` 是满足"**[base, H] 内每个高度都有 CommitQC**"的最高 `H`，其中
`base = max(GRANDPA finalized + 1, 历史窗口下界)`；它与原有的 `fastConfirmedHeight`（最大完成高度，
可能落在空洞之上）区分开。消费方据此可一次性确认 `[base, frontier]` 整批，而非逐高度查询。
单测 `batch_frontier_is_the_contiguous_prefix_above_finalized` / `batch_frontier_stops_at_a_hole`
覆盖连续与遇洞即止两种情形。

**P1-5 轮次推进与 Skip 票（引擎层）**：`round` 不再恒为 0。每个 `(set_id, height)` 维护
`RoundState { current_round, prepare_done, block_hash, parent_hash }`；`on_block_imported` 不再硬编码
round 0，而是prepare当前 round；新增 `advance_rounds`（每聚合节拍检查：某高度 `best - height >
ROUND_TIMEOUT_BLOCKS(=8)` 且未形成 PrepareQC 时，对该 round 发 `Skip` 并把 `current_round` 推进到
`round+1` 重新 prepare），以及 `on_skip_qc`（收到 `SkipQC` 后推进本地 round 并重投新 round 的 Prepare）。
`Skip` 票的 `block_hash=None`、`parent_hash=zero`，保证任意两张同 `(height, round)` 的 Skip 可聚合。
`maybe_commit` 改为复用统一的 `build_vote`/`emit_vote` 辅助，Prepare/Commit/Skip 三态统一经
`on_built_qc` 分发。

实测（dev 两节点，同 P0 环境）：活跃窗口 101..164 **64/64 连续、0 空洞**，`fastConfirmedBase=163` /
`fastConfirmedFrontier=164`（连续故等于 head），`fastConfirmedHeight=164` vs `grandpaFinalizedNumber=162`
（SFF 领先 2 块）；日志 `signed Skip vote = 0`（健康路径下 PrepareQC 在 1–2 块内形成，不误触发轮次推进）、
签名失败/验签失败/panic/conflict 全为 0。轮次推进逻辑已接好，健康流无回归；其在"丢票/停滞高度"下的表现
属于 (d) 的活性兜底，需构造丢票场景专项验证（见 P1-7 catch-up 一并处理）。

### 13.7 真链部署与永久空洞自愈（2026-11-03）

本轮把 SFF 从 dev 两节点搬到真链 `--chain sunchain`（固定 master/slave 两验证人），修复三处部署 / 活性缺陷，最终真链与 dev 一致：`fastConfirmedFrontier` 连续、紧贴 GRANDPA（gap=2）、0 STALL、无 Skip/conflict。

#### 13.7.1 genesis `DuplicatedKey`（initial_config.rs::session_keys_json）

- 症状：`--chain sunchain --fresh` 起链 panic `Client(DuplicatedKey)`（session genesis 的 `inner_set_keys` 发现同一把 raw 公钥被两个不同 `who` 共用）；复用旧 DB 则表现为 wasm trap。
- 根因：`session_keys_json` 给 master/slave 都写**全零** `sffb`（`0x0000…00`），两验证人共用一把 BLS 公钥 → 触发重复密钥检查。
- 修复：每个验证人的 `sffb` 由其**自身 babe 公钥确定性派生** `BlsKeypair::from_seed(blake2_256(babe_bytes))`，天然不同且非零。与节点侧 sffb 私钥注入同源派生，公钥一致。

#### 13.7.2 真链 SFF 激活：sffb 私钥注入 + PoP 自举（service.rs + initial_config.rs::staging_sffb_pops）

- 症状：真链 keystore 没有 `sffb` 私钥，且未链上 `register_bls_key` → authority set 的 `sffb` 公钥非零但其 PoP 为零 → `verify_vote` 因 `InvalidPop` 拒票 → SFF dormant（不投票、不组 QC）。
- 修复（镜像 dev 的 `dev_pops` 兜底）：
  - `service.rs`：非 dev 的 authority 节点启动时用自身 babe 公钥 `sff_generate_new(KeyTypeId(*b"sffb"), Some(babe_bytes))` 注入 `sffb` 私钥（两侧各注入自己那份，不造成 equivocation）。
  - `initial_config.rs::staging_sffb_pops()`：用 master/slave 各自 babe 公钥派生双方 `(sffb 公钥, PoP)`，注入到 `RuntimeAuthorityProvider` 的 `dev_pops`；两节点都持双方 PoP 用于验票，但仅持自身签名私钥。PoP 与 genesis 广告的 `sffb` 公钥同源，通过 `verify_vote`。

> ⚠️ **临时偏差（生产前须移除）**：本 off-chain `staging_sffb_pops` 注入**不是**设计要求的生产密钥注册路径。设计（§11.1 已知偏离 5、§13.3 item 11）要求验证人通过 `sff_rotateKeys` 生成真实 `sffb` 并向 `pallet_fast_finality::register_bls_key` 提交公钥 + PoP（链下 `pop_verify`）。固定 2 验证人的 staging 为快速打通 SFF 才走此捷径；真实多验证人部署必须改用 `register_bls_key`，否则密钥自举不可扩展且不安全。

#### 13.7.3 自愈窗口锚定前沿，消除永久空洞（fast_finality.rs + fast_finality_view.rs）

- 症状：P0 收尾后 dev 已连续，但真链上 `fastConfirmedFrontier` 卡在某高度、`frontier=null`、落后 GRANDPA 数百块，且落在 `maintain_votes`/`request_repairs` 之外无法自愈。
- 根因：两函数的 healing 窗口下界 `floor = finalized.min(best - MAX_BACKLOG_HEIGHTS)`。某高度一旦在凑齐 CommitQC 前落到窗口下界之下，就**永不重发投票**，形成永久空洞；前沿推进遇首个空洞即停。真链因 set 旋转 / gossip 冷启动偶发丢票，空洞落在窗口下方后无法自愈。
- 旁支排查：`current_set_id()` 曾加 10s TTL 缓存，旋转后瞬间用旧 set_id 签名 / 验票被拒，造成一部分早期空洞。因 `current_set_id` 仅是单次存储读（廉价），改为**每次实时查询**，避免旋转后陈旧 set_id 投票；重活（`fast_finality_authorities` 重建 authority vector）仍按 set_id 缓存于 `authorities()`。
- 修复：把 `floor` 锚定到 SFF 前沿 `floor = finalized.min(self.view.fast_confirmed_height().unwrap_or(0))`（新增 `FastFinalityView::fast_confirmed_height()` getter）。`[frontier, best]` 内任何缺口都被重发 / 补证，前沿推进后窗口自然收窄，有界且自愈；高度低于前沿者已提交，重发无害。`advance_rounds` 对低于其窗口的高度直接忽略（不 Skip），安全。

#### 13.7.4 验证结果（2026-11-03）

- dev 两节点（1858 块）：`advance_rounds: STALL=0`，`gap` 稳定 = 2，`fastConfirmedFrontier` 连续非 None，`signed Skip vote=0`，`conflicts=0`，`serving=true/degraded=false`。
- 真链 `--chain sunchain --fresh`（master RPC 9944 / slave 9945，复用 keystore）跑过 ~80+ 块（含 set 旋转）：master 与 slave 一致 `frontier=80`、`grandpa=78` → **gap=2**，`serving=true`、`degraded=false`、`conflicts=0`、0 STALL；仅 slave 出现 1 次 Skip（单瞬时空洞被 Skip，但前沿仍推进到 80，已自愈）。SFF 在真链上连续跟踪 GRANDPA。

#### 13.7.5 运维注意

- `session_keys_json` 的 sffb 派生方式变化属于 genesis 变更：现有 sunchain DB 必须 `--fresh` 重建（`start_chain.sh --fresh` 保留 keystore、仅清 db/frontier）。
- 仅注入本节点 sffb 私钥（按自身 babe），master/slave 不共享签名私钥，避免 equivocation。

### 13.7.6 实现侧补记（早前会话，归档）

以下两处修复已在代码中落地，此前未写入设计文档，在此归档以免与 §13.2/§13.3 的描述脱节：

- **gossip 去重改为"始终入池、仅未见过才中继"**（`fast_finality_p2p.rs`）：原实现在 `seen` 命中时直接丢弃消息，会丢失"已 seen 但池插入失败"的重传，进而造成空洞。现改为 `validate`（BLS 验签 / set 校验）通过后即 `accept_into_pool`（池内 `add_vote`/`add_qc` 自行去重，安全），仅在 `!already_seen` 时 `relay`（防环）。`Repair` 的去重键也从常量 `Cert(0,0,0,255,..)` 改为按 `(set,height,round,phase)` 实际字段（旧键导致仅首个 repair 被应答、其余被静默丢弃，pull-based repair 无法自愈多个空洞）。
- **run-loop 用 `biased select!` 优先 block-import**（`fast_finality.rs::run`）：原公平 `select!` 会被持续的 vote 通知流饿死 `import_stream`，导致 SFF 在区块导入后很多块才 prepare，前沿落后 GRANDPA。改为 `biased` 把 `import_stream` 放在首位，确保区块导入优先 prepare（前沿不再因导入饿死而滞后）。聚合本身是事件驱动（`take_dirty` 脏键扫描），定时器仅作空闲兜底。
