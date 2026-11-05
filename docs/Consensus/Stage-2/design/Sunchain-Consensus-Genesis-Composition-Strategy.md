# Sunchain 共识终局形态：genesis 组合策略

> 状态：**已决策，后续推进的指导性上位文档**。与前序设计/整改文档冲突时，以本文为准。
> 日期：2026-11-05
> 决策来源：2026-11-05 项目方对《共识落地情况 review》的三条修正意见。
> 依据：前一轮 review（设计文档 + 代码交叉核实）、`Sunchain-SunLead-*.md` 系列、`../Stage-1/design/Sunchain-Alpenglow-Inspired-Fast-Finality-Design.md`、`../Stage-1/implementation/Sunchain-SFF-Gossip-Delivery-Diagnosis.md`、当前 `node/`、`runtime/`、`pallets/` 实现。

---

## 0. 决策摘要

前一轮 review 按"**要在运行中的链上把 BABE/GRANDPA 平滑迁移到 SunLead/SFF**"这一隐含前提评估，因此把 activation 状态机、双协议 verifier、新旧节点混跑、回滚窗口、30 天测试网迁移、治理批准列为硬门槛，并据此判定"不具备整体替换能力"。

项目方明确了三条前提，**该隐含前提不成立**：

1. 出块共识在 **genesis 时决策**，**不需要动态切换**；目标是 SunLead 尽量与 BABE 解耦，但**不是要完全去掉 BABE**。
2. **没有在运行的链需要升级**，可按一条新链设计，不必投入兼容/迁移。
3. SFF 与 GRANDPA 同理，genesis 时决策，不需要动态切。

由此，**迁移类工作整体取消**，前一版 review 的"不具备整体替换能力"结论需要重新表述为：

> 迁移能力确实不具备，但本策略也不需要它。真正未完成的、且必须完成的，是 **「终局轴」上 SFF 作为权威最终性的实现**，以及**让两个轴在 genesis 时独立可选、四种组合都能各自组网**的工程能力。

**最终形态定义**：出块轴（BABE / SunLead）与终局轴（GRANDPA / SFF）在 genesis 时各选其一，四个组合都是一条自洽的新链，都能多节点组网、出块、终局连续推进、重启恢复、跑通业务闭环。不要求不同组合的节点在同一网络混跑——不同组合即不同 genesis、不同链。

**本策略最核心的纠偏**：`pallets/sunlead/src/lib.rs:108-116` 的 `SunLeadActive` 已经是 "genesis-only…fixed for the lifetime of the chain，无 runtime dispatch 可翻转"，**这正是目标语义，是特性不是缺陷**。把"静态门控"记为已知问题、并计划用 P5 activation 状态机替换它的既有文档与注释，在新策略下应当**废止**而非实施。Stage 0 的工作量因此远小于原计划。

---

## 1. 策略前提与边界

### 1.1 出块轴：genesis 决策、尽量解耦、不清除 BABE

- 一条链在 genesis 时确定用 BABE 还是 SunLead 出块，链生期内不变。
- "尽量与 BABE 解耦"指**运行语义解耦**：被选中 SunLead 的链，其出块决策、导入验证、slot/epoch/timestamp、session 轮换、作者归属、fork choice 不得依赖"BABE 正在出块"这一前提。
- **不是**要把 `pallet_babe` / `BabeApi` / `babe` session key / `sc_consensus_babe` 从代码或 runtime 类型中删净。类型级、工具库级依赖允许保留（§4.4）。
- 判据：SunLead 链上不启动 BABE 的 `block_import` / import queue / `start_babe`，链仍应长期正确运行。

### 1.2 无存量链：按新链设计

- 不存在需要升级的在线网络；每次换组合 = 换 genesis = 换链。
- 因此不需要：向后兼容旧块/旧 DB、旧节点隔离、混跑、双协议 handover certificate、激活高度、回滚窗口、测试网迁移演练、治理批准式切换流程。
- 仍需要：**安全性验证**。"新链"降低的是迁移成本，不降低"把最终性交给 SFF"这一安全变更的证明与审计要求。

### 1.3 终局轴：同理，genesis 决策

- 一条链在 genesis 时确定用 GRANDPA 还是 SFF 作为**唯一权威最终性**，链生期内不变。
- 选 SFF 时，GRANDPA voter 不必启动，但 SFF 必须能真正产生并写入 finality（不是影子读数）。
- 不需要 F4 原设计里的双协议并行、handover、rollback、共识版本 digest 等**切换机制**；但 F4 里"CommitQC 成为 new finality proof"的**能力实现**依然必需。

### 1.4 最终形态：2×2 任意组合

| # | 出块 | 终局 | 定位 | 当前状态 |
|---|---|---|---|---|
| C1 | BABE | GRANDPA | 原共识基线 / 对照组 | ✅ 可组网 |
| C2 | SunLead | GRANDPA | 出块轴替换 | ✅ 可组网（dev/staging 默认） |
| C3 | BABE | SFF | 终局轴替换 | ❌ 无法终局 |
| C4 | SunLead | SFF | 全替换（目标形态） | ❌ 无法终局 |

> "任意组合"取两个轴**独立选择**的直接含义，故以 4 格为验收上界。若后续决定只保留 C1/C4 两格，Stage 0 的分支数可相应裁剪，但 §3.3 的硬门槛不变。

### 1.5 明确非目标（不投工作量）

- 任何形式的运行时/链上动态共识切换、activation 状态机、`set_sunlead_active` 之类的 dispatch。
- 与存量链/旧 DB/旧节点的兼容、混跑、回滚、迁移演练。
- 跨客户端多实现兼容与多客户端差分测试（当前只有单一客户端实现；单实现自洽与故障注入测试仍必须做）。
- 协议多版本共存与协商（不同组合天然由不同 genesis 隔离）。
- 把 BABE/GRANDPA 从 runtime 类型与依赖清单中彻底删除。

---

## 2. 现状基线（2026-11-05 代码实测）

### 2.1 已落地（可直接复用，不必重做）

**出块轴**
- `crates/sunlead-primitives` / `sunlead-schedule` / `sunlead-verifier`：确定性 stake 加权 schedule（Hamilton 配额 + Merkle root/proof + golden 向量）、pre-digest/seal/slot/parent/equivocation 校验。
- `pallets/sunlead`：epoch snapshot、schedule、`SunLeadApi`（5 方法）、`SunLeadActive` genesis-only 开关。
- `node/src/sunlead_consensus/`：`SunLeadBlockImport`、`sunlead_import_queue`、`SunLeadVerifier`、`Worker`/`start_sunlead`、4 个只读 RPC、3 个 Prometheus 计数器。
- 2 节点 E2E 已跑通出块、同步、GRANDPA 终局、DEX 执行。

**终局轴**
- SFF F1 三 crate（BLS12-381 / PoP / 聚合 / `LockTracker` / `model_check`）+ `../Stage-1/design/sff-f1-formal/SFFFinality.tla`。
- F2 shadow：`/sun/sff/2` 专属协议、Ack + outbox 重传、限流、有界内存池、Prepare/Commit/Skip 两阶段投票与成证、轮次推进、Repair、catch-up。
- F3 只读 `FastFinalityView` + `sff_status` 等 4 个 RPC。
- 设计 `../Stage-1/design/Sunchain-Alpenglow-Inspired-Fast-Finality-Design.md:602-611` 的 P0/P1 全部勾选；`../Stage-1/implementation/Sunchain-SFF-Gossip-Delivery-Diagnosis.md` §8 的票缓存 / 流水线重叠 / 重传压缩三项已完成，4 节点 `fch` 稳定领先 GRANDPA ~2 块、`last_proc=0ms`。

> 注意：该 ~2 块领先是**影子层读数相对 GRANDPA 的领先量**，证明影子协议跑得快，**不等于** SFF 更接近替换 GRANDPA。避免把性能指标误当替换能力。

### 2.2 2×2 矩阵现状的接线事实

| 接线点 | 现状 | 证据 |
|---|---|---|
| 出块选择 | genesis 布尔 `sunLead.sunleadActive` → `SunLeadApi::is_active(finalized_hash)`，启动时读一次；另有 digest 兜底 | `node/src/chain_spec.rs:334-336`、`node/src/chain_spec/initial_config.rs:377-379`、`node/src/service.rs:350-374` |
| 激活时跳过 BABE | `(babe_block_import, babe_link)=(None,None)`；走 `sunlead_import_queue`；`start_sunlead` 而 `else` 才 `start_babe` | `node/src/service.rs:382-391,396-434,1173-1253` |
| **终局选择** | **不存在**。`justification_import` 恒为 `grandpa_block_import`，两条 import queue 都传入 | `node/src/service.rs:298,400,414` |
| GRANDPA voter | `enable_grandpa = !config.disable_grandpa`，`if enable_grandpa` **不判断出块组合**；`spawn_essential` | `node/src/service.rs:550,1312-1339` |
| `--no-grandpa` | 只能**关闭** voter，无替代终局；import queue 仍挂 GRANDPA justification import → 链无终局 | 同上 + `service.rs:400,414` |
| warp sync | 恒为 `sc_consensus_grandpa::warp_proof::NetworkProvider` | `node/src/service.rs:618-633` |
| SFF 引擎 | `spawn()`（**non-essential**），注释 "a panic or stall here must never stop BABE authoring or GRANDPA finality" | `node/src/service.rs:1068-1070,1109,1136` |
| SFF 写 finality | **否**。`finalized_number` 只读；`fastConfirmedHeight` 是内存 `Mutex<Inner>` 字段，写方 shadow 引擎、读方仅 RPC；产出 `QuorumCertificate` 而非 `GrandpaJustification` | `node/src/fast_finality.rs:1-10,1369`、`fast_finality_view.rs:159-168`、`rpc/sff.rs:93-106` |
| 一键切换 | 无 consensus feature / CLI 参数 / 热切换；`spec_version` 单值 | `node/Cargo.toml:190-235`、`node/src/cli.rs:1-81` |

**结论**：C1、C2 已可组网；C3、C4 因终局轴没有 SFF 实现而无法组网。差距**集中在终局轴**，不是出块轴。

### 2.3 出块轴仍存在的 BABE 运行语义耦合（Stage 1 对象）

| 耦合点 | 证据 |
|---|---|
| slot duration 恒取自 `sc_consensus_babe::configuration(...)` | `node/src/service.rs:295-296,731,1171` |
| slot inherent 恒用 `sp_consensus_babe::inherents::InherentDataProvider` | `node/src/service.rs:420-424,1186` |
| `SunLeadAwareOnTimestampSet` 用 `Babe::slot_duration()` + `sp_consensus_babe::Slot`，且 SunLead 链上要手动写 `pallet_babe::CurrentSlot` 以免 BABE 断言 panic | `runtime/src/configs/mod.rs:174-199` |
| epoch seed 取自 `pallet_babe::Randomness` | `pallets/sunlead/src/lib.rs:251-253` |
| `ShouldEndSession` / `NextSessionRotation` = `Babe`（ImOnline 同） | `runtime/src/configs/mod.rs:770-771,966` |
| EVM `FindAuthorTruncated<Babe>`；authorship 回退 `FindAccountFromAuthorIndex<Self, Babe>` | `runtime/src/configs/mod.rs:1113,593` |
| 以太坊 pending 块恒用 `BabeConsensusDataProvider` 写 `BABE_ENGINE_ID` pre-digest | `node/src/service.rs:817-820` |
| `pallet_sunlead::Config` 强制 `type Babe` + `type Grandpa` | `pallets/sunlead/src/lib.rs:69-72` |

### 2.4 SFF 作为权威终局所缺的接线（Stage 3 对象）

- `FastFinalityApi` 复用 `pallet_grandpa::CurrentSetId`（`runtime/src/apis.rs:794-804`），且读 `keys.grandpa`（`:832`）；GRANDPA 不启动时 SFF 没有独立 set id / 轮换来源。
- `sffb` 仍是零值占位 + 链外 PoP 注入（`node/src/chain_spec/initial_config.rs:444`、`runtime/src/apis.rs:830`、`node/src/service.rs:1076-1095`），设计 `../Stage-1/design/Sunchain-Alpenglow-Inspired-Fast-Finality-Design.md:694-699` 标为"⚠️ 临时偏差（生产前须移除）"。
- 无 `JustificationImport` / `apply_finality` / fork choice 锁定 / SFF warp proof。
- TLA+ 仅覆盖 F1 静态不变量（`SFFFinality.cfg`：4 节点 / 2 高度 / 1 轮 / 2 块），`model_check` 限 `n≤5`、DFS 深度 4。

---

## 3. 缺口重定级（本策略的核心）

前一版 review 列出的缺口，按新前提重新分级。**本节是后续排期的直接依据**：3.1 不排期、3.2 降优先级、3.3 是必须完成的硬门槛、3.4 是本策略新增的工作。

### 3.1 取消 —— 不再作为工作项 / 前置 gate

| 原缺口 | 取消理由 |
|---|---|
| SunLead P5 activation 状态机 `Disabled→Prepared→Active→EmergencyHaltAuthoring` | 用户明确"不需要动态切换"。`SunLeadActive` genesis-only 语义已达标（`pallets/sunlead/src/lib.rs:108-116`），改为**保留并固化**，废止 `../Stage-1/implementation/Sunchain-SunLead-Remediation-Plan.md:77-102,198-204` 与代码注释 `:113-114` 的替换计划 |
| import queue 在迁移窗口按 parent-state activation 选 verifier、双协议 verifier | 无迁移窗口；每条链只有一种 verifier |
| 新旧节点隔离 / 混跑 / 回滚演练 / 紧急停止 authoring | 无存量链，换组合即换 genesis |
| SFF F4 的共识版本 digest、激活高度、双协议 handover certificate、rollback 窗口、双重运行 | 同上；`../Stage-1/design/Sunchain-Alpenglow-Inspired-Fast-Finality-Design.md:250-252` 中属于"切换机制"的部分取消。**注意：同段中 "CommitQC 成为 new finality proof" 的能力保留（§3.3）** |
| P5 ≥30 天测试网迁移、治理批准式切换流程 | 无在线网络、无迁移；`../Stage-1/design/Sunchain-SunLead-Deterministic-Leader-Development-Plan.md:347-363`、`Remediation-Plan:204` 相关 gate 取消 |
| 跨客户端兼容 / 多客户端差分测试 | 单一客户端实现；`Alpenglow-...Design.md:265` 该项降级为可选 |
| 协议多版本共存、握手与版本协商 | 不同组合由不同 genesis 天然隔离 |
| BABE/GRANDPA 代码与类型依赖清零 | 用户明确"不是要完全去掉 Babe"；见 §4.4 |
| F3 遗留 `- [ ] 10` 下游 DEX UI / 钱包显示 fast-confirmed | SFF 作为权威终局时，下游直接跟随 `finalizedHead` 即可；仅当 shadow 组合仍需对外读数时保留 |

> **取消的是前置条件，不是能力本身**：例如"治理批准"取消后，`../Stage-1/design/Sunchain-SunLead-Deterministic-Leader-Development-Plan.md:3` 与 `Alpenglow-...Design.md:3` 头部"未批准前不得替换"的措辞应按本策略修订，否则文档会与决策长期冲突（§8）。

### 3.2 降级 —— 从硬门槛降为优化/规模触发项

| 原缺口 | 新定位 |
|---|---|
| 独立 session rotation provider + 抗偏置 randomness beacon | 不再要求新建 VRF/beacon。收敛为"**中立 slot/epoch 参数源**"（chain spec 常量或独立轻量 pallet），BABE pallet 可继续作为兼容读取者；需验证 SunLead 链上 BABE randomness 在无 BABE 出块时是否仍推进（§7） |
| `SFF §13` `- [ ] 8` leader/primary 聚合 + fanout、`- [ ] 9` Rotor 纠删码 | 维持 `../Stage-1/implementation/Sunchain-SFF-Gossip-Delivery-Diagnosis.md` §7 结论：P2 规模化项，验证人数量上百后才必需，**不作为 4 组合组网前置** |
| 缩短 slot 到 1 秒、backup takeover、SkipQC 提前 handoff、window batch template 预构建、P2-8 聚合 fanout | 独立性能优化项，与本策略解绑；3 秒 slot 仍是 `fch` 延迟硬下限 |
| F2 Gate "连续 30 天零未解释冲突"、F4 Gate 中的"停机/分区演练" | 保留**故障注入测试**，但去掉"针对迁移/双协议"的时长与流程约束，改为"目标组合下长稳 + 故障注入全绿"（§6.3） |
| 4 类推迟的 Prometheus 指标、mock runtime 单测 | 随 Stage 1 顺带补齐，不单独设 gate |

### 3.3 保留为硬门槛 —— 与本策略无关，必须完成

这些缺口源于"**SFF 要成为权威最终性**"和"**四种组合要各自正确组网**"，与是否动态切换、是否有存量链无关。

**A. SFF 权威终局能力（差距最大，决定 C3/C4 能否存在）**

| # | 缺口 | 证据 |
|---|---|---|
| A1 | `JustificationImport` 管线：CommitQC → 可验证 justification → `apply_finality` 写入 `finalized_number` | 当前 SFF 只写内存 view（`node/src/fast_finality.rs:1-10`、`fast_finality_view.rs:226-313`） |
| A2 | fork choice 锁定 / 分叉选择规则；F3 dual-finality 的一致性检查目前仅 `log::warn!` + 停发 | `node/src/fast_finality.rs:1216-1275` |
| A3 | 独立 authority set id / 轮换来源（不依赖 `pallet_grandpa::CurrentSetId` 与 `keys.grandpa`） | `runtime/src/apis.rs:794-804,832` |
| A4 | justification / voter / `justification_import` / warp provider 按终局轴分支；SFF voter 升为 essential | `node/src/service.rs:298,400,414,550,1312-1339,618-633,1068-1070` |
| A5 | SFF warp sync / light-client 证明（或在新链前提下明确替代方案） | `service.rs:618-633`；设计 `Alpenglow-...Design.md:271` "证书≠数据可用性" |

**B. 生产级密码学与密钥（安全前置，不可因"新链"而省）**

| # | 缺口 | 证据 |
|---|---|---|
| B1 | hash-to-curve 换成经审计的 RFC 9380 实现 | `crates/sff-crypto/src/lib.rs:12-16` 注释 "MUST be replaced…before any production use"；设计 `:304` |
| B2 | 生产 `register_bls_key` 真实注册 + `sffb` 真实轮换 + PoP 验证路径；移除零值占位与链外 PoP 注入 | 设计 `:351,413,459,694-699`、`Sunchain-...Design.md §13:621` `- [ ] 11`；`node/src/service.rs:1076-1095` |
| B3 | 接收路径不验逐票签名（依赖可信验证人集合）→ 改为过滤无效票而非整组 abort，或恢复验签 | `fast_finality_p2p.rs:7-10`；`../Stage-1/implementation/Sunchain-SFF-Gossip-Delivery-Diagnosis.md:97-98` |
| B4 | equivocation → `pallet-offences`/slashing 闭环：SunLead 与 SFF **两侧**都缺 | SunLead：`crates/sunlead-verifier/src/lib.rs:183` 仅链下 proof、`node/src/sunlead_consensus/metrics.rs:44-46` 仅计数；SFF：`crates/sff-verifier/src/lib.rs:248-339` 在 node/runtime/pallet 无调用。另 **GRANDPA 自身** `runtime/src/configs/mod.rs:997-999` 把 `EquivocationReportSystem` 设为 `()` |
| B5 | F4 级形式化安全证明：把 TLA+ / `model_check` 从 F1 静态锁不变量扩展到活性、round/Skip/timeout、set rotation、Byzantine 调度、网络分区；外部安全审计 | `../Stage-1/design/sff-f1-formal/` 仅 2 文件；设计 `:238,254,265` |

> "新链"降低迁移成本，**不降低**把不可逆最终性交给新协议所需的证明与审计强度。B5 是 C3/C4 可否上线的唯一安全 gate。

**C. 出块轴运行语义解耦（保证 C2/C4 离开 BABE 出块仍正确）**

| # | 缺口 | 证据 |
|---|---|---|
| C-1 | slot/epoch/timestamp 收敛到中立参数源；去除对 `Babe::slot_duration` / `sp_consensus_babe::Slot` / `pallet_babe::CurrentSlot` 的运行依赖 | `runtime/src/configs/mod.rs:174-199`、`node/src/service.rs:295-296,420-424` |
| C-2 | epoch/slot/timestamp 一致性校验：现用 `slot / epoch_slots` 绝对槽（`crates/sunlead-verifier/src/lib.rs:93-97`），与 `Remediation-Plan:59` "禁止使用 absolute_slot % epoch_slots 作为 epoch 映射" 冲突；timestamp inherent ↔ pre-digest slot 相等 + future drift 校验缺失 | `node/src/sunlead_consensus/import_queue.rs` 全文无 `timestamp` |
| C-3 | `FindAuthor` / EVM pending provider 按出块轴选择 | `runtime/src/configs/mod.rs:593,1113`、`node/src/service.rs:817-820` |
| C-4 | runtime 资源治理：`#[pallet::unbounded] Schedule`、固定 `reads_writes(4,3)`、genesis 吞错 | `pallets/sunlead/src/lib.rs:101,162-166,220-221`；`Remediation-Plan:108-115` |
| C-5 | 严格 canonical digest（恰好一个 pre-digest/seal、本地错误 fail closed） | `Remediation-Plan:29,131-154` |

### 3.4 新增 —— "任意组合"本身带来的工作

| # | 工作项 | 说明 |
|---|---|---|
| N1 | 统一 `ConsensusProfile`（两轴各一枚举）作为 genesis 配置，取代分散布尔 | 见 §4.1 |
| N2 | node 启动按 profile 分派：import queue / authoring engine / justification import / voter / warp provider / 网络协议名 / RPC 扩展 / keystore 注入 | 当前只有出块轴分派，终局轴无分派（§2.2） |
| N3 | 未实现组合显式 fail-fast | C3/C4 当前会静默退化成"GRANDPA 关掉→无终局"；应给出明确启动诊断 |
| N4 | 4 组合各自的 chain spec 基线与测试脚本 | 现有 `scripts/sff-multi/start_multi.sh` 只覆盖 C2 的影子观测 |
| N5 | 组合矩阵验收（§6）与文档状态统一（§8） | |

---

## 4. 目标架构

### 4.1 统一的 genesis ConsensusProfile

```text
ConsensusProfile {
    authoring: Babe | SunLead,     // 出块轴
    finality:  Grandpa | Sff,      // 终局轴
}
```

- **单一事实来源**：写入 genesis（chain spec patch 或独立 `pallet_consensus_profile` 的 `GenesisConfig`），node 在 `new_partial` 启动时读一次，链生期不变；runtime 与 node 读同一字段，不各自维护布尔。
- 现有 `SunLeadActive`（`pallets/sunlead/src/lib.rs:108-118`）语义已正确，可保留为该 profile 出块轴的存储表示，也可上提为新 pallet；**不需要**新增状态机、激活高度或 dispatch。
- 读取点沿用 `SunLeadApi::is_active(finalized_hash)` 的做法（`node/src/service.rs:350-374`）：读 recent finalized head 而非 block 0，避免 state 被裁剪后重启失败。这与"动态切换"无关，是长跑链的工程稳健性，保留；digest 兜底在新策略下可简化但非必须移除。
- 不同 profile → 不同 genesis → 不同 `genesis_hash` → 网络协议名天然隔离，无需版本协商。

### 4.2 出块轴：解耦分级

**必须按 profile 分派**（否则组合无法独立组网）：`block_import` / import queue / verifier / authoring worker / fork choice 策略 / 以太坊 pending consensus data provider / RPC 扩展。

**应尽量改为中立源**（"尽量解耦"的落点）：slot duration、slot inherent、timestamp `OnTimestampSet`、epoch 参数与 seed、session rotation、FindAuthor。目标是"SunLead 链不因 BABE 不出块而语义失效"，而非"删掉 BABE 类型"。

**允许保留**：`pallet_babe`、`BabeApi`、`babe` session key、`Config::Babe`、`sc_consensus_babe` 作为类型/工具库，以及 C1 组合中 BABE 作为正常出块者。

### 4.3 终局轴：两个对等实现

- **Grandpa 路径**（现状）：`grandpa_block_import` 作为 `justification_import` → `run_grandpa_voter` 产生 justification → `apply_finality` → warp `warp_proof::NetworkProvider`。
- **SFF 路径**（待建）：SFF voter（essential）产生 CommitQC → 编码为 justification 并经 `JustificationImport` 验证 → `apply_finality` 写入 → fork choice 按 SFF 锁定规则 → SFF warp proof / light-client 可重放 → 独立 set id 与 `sffb` 轮换驱动 authority set 变更。
- 选 SFF 时 GRANDPA voter 不启动，但 GRANDPA pallet / 类型可保留（§4.4）；`--no-grandpa` 不应再是"关掉但挂 GRANDPA justification import"的半残状态，SFF 分支须接管完整 justification 路径。

### 4.4 依赖处理原则

> **类型依赖可留，运行语义必须解耦；权威写入必须唯一。**

- 允许 `pallet_sunlead::Config` 继续要求 `type Babe` / `type Grandpa`，允许 `SessionKeys` 保留六把 key——这些是编译期/数据兼容，不是共识参与。
- 不允许：被选中的共识路径在**运行时**依赖另一套共识正在产出的数据（BABE `CurrentSlot` / randomness 是否推进、GRANDPA `CurrentSetId` 是否轮换）。这类依赖必须改为中立源或按 profile 明确分支。
- 任一组合中，`finalized_number` 的写入方**有且仅有一个**：GRANDPA 或 SFF，不得双写、不得影子写。

---

## 5. 分阶段推进

依赖顺序：`Stage 0 → Stage 1 / Stage 2（可并行）→ Stage 3`。

### Stage 0：固化 profile 与组合矩阵骨架

**目标**：把"genesis 决策"从隐含约定变成显式、可测、可诊断的一等配置。

- N1 引入 `ConsensusProfile`，把 `SunLeadActive` 纳入其中；4 组合在 chain spec 层显式可选。
- N2 node 启动按 profile 分派出块轴 + 终局轴的所有接线点。
- N3 C3/C4 未实现前给出明确启动诊断，禁止静默退化。
- N4 为 C1/C2 建立基线脚本与回归；N5 统一文档状态（§8）。
- **废止** P5 activation 状态机计划，修订 `pallets/sunlead/src/lib.rs:113-114` 注释与相关文档措辞。

**Gate**：C1、C2 在全新 genesis 下多节点出块 + 终局连续推进 + validator 重启 + DEX 闭环全绿（回归，不改行为）；C3/C4 有明确诊断与路线；profile 在 node 与 runtime 读数一致。

### Stage 1：出块轴运行语义解耦

**目标**：C2/C4 在"不启动 BABE 出块"下长期正确，消除 §2.3 的运行依赖。

- C-1 中立 slot/epoch 参数源；C-2 epoch/slot/timestamp 一致性与 drift 校验；C-3 FindAuthor / EVM provider 分派；C-4 资源治理；C-5 严格 digest。
- B4 的 SunLead 侧：equivocation → `pallet-offences`/staking slash 闭环（含 key ownership、去重、报告奖励）。
- 顺带补 4 类推迟指标、mock runtime 单测。

**Gate**：C2 长稳（含 session/epoch 边界、validator 重启、key 轮换）；双签可举报可罚；EVM 作者归属与奖励正确；SunLead 链上 BABE 相关存储不再影响正确性（§7 开放项闭环）。

### Stage 2：SFF 生产化（可与 Stage 1 并行）

**目标**：把 SFF 从"dev 影子原型"提升到"可承载不可逆最终性"，但不改其接线。

- B1 RFC 9380；B2 生产 `register_bls_key` + `sffb` 轮换 + PoP 验证，移除零值占位与链外注入；B3 接收层验票加固；B4 的 SFF 侧 equivocation 上报/slashing。
- A3 独立 set id / authority set 来源，脱离 `pallet_grandpa::CurrentSetId` 与 `keys.grandpa`。
- B5 F4 级形式化证明（活性、rotation、Byzantine、分区）+ 外部安全审计。

**Gate**：安全 gate 通过；目标组合下长稳零未解释冲突；`sffb` 轮换可在多节点上重放验证。

### Stage 3：SFF 成为权威终局 + 4 格全绿

**目标**：达成 §1.4 最终形态。

- A1 `JustificationImport` + `apply_finality`；A2 fork choice 锁定；A4 按 profile 分派 justification import / voter / warp，SFF voter 升 essential；A5 SFF warp / light-client 证明。
- 把 `--no-grandpa` 语义收编进 profile，消除"半残"状态。
- 完成 §6 组合矩阵全部验收。

**Gate**：C1–C4 每一格独立起链、多节点一致、终局连续推进、重组/set 变更/宕机重启/DEX 闭环全绿；任一格故障不影响其他格。

---

## 6. 验收标准

### 6.1 组合矩阵

| 组合 | 必过项 | 目标阶段 |
|---|---|---|
| C1 BABE+GRANDPA | 基线对照、性能基线（p50/p95/p99） | Stage 0 |
| C2 SunLead+GRANDPA | 无 BABE 出块下长稳、schedule 正确性、双签惩罚、作者归属 | Stage 1 |
| C3 BABE+SFF | SFF 独立终局、set 轮换、justification 可重放 | Stage 3 |
| C4 SunLead+SFF | 全替换端到端、两条轴同时非默认 | Stage 3 |

### 6.2 每格必测场景

出块连续性与空 slot 恢复；终局连续推进且不回退；validator 宕机/重启/追块；session 与 key 轮换；网络分区与恢复、丢包注入；双签/冲突证书检测与惩罚；重组与 fork choice；DEX 下单→撮合→清算闭环；warp/light-client 同步（终局轴为 SFF 时按 A5 的替代方案验收）；EVM 作者归属与奖励。

### 6.3 全局 gate

- 每格 `finalized_number` 写入方唯一，无双写、无影子写。
- 任一组合的故障（SFF 超时、证书错误、网络不健康）不得影响其他组合的链；本组合内按设计退回行为可验证。
- B5 安全 gate 通过（C3/C4 上线的唯一硬性安全前置）。
- 各组合的性能数据独立记录，**不以影子层 `fch` 领先量代替终局能力验收**。

---

## 7. 风险与开放问题

| # | 项 | 说明与处置 |
|---|---|---|
| R1 | **文档与代码状态冲突**：`Remediation-Plan:17` 禁止 staging 激活，而 `chain_spec.rs:334-336`、`initial_config.rs:377-379` 已置 `sunleadActive: true`；`P3-TestPlan:27` 仍称主网 preset 未激活 | 新策略下这**不再是"违规"**，而是"文档未同步"。需确认这些 spec 就是新链基线，并统一文档（§8）；否则容易被后续 review 再次误判 |
| R2 | **SunLead 链上 BABE randomness 是否推进**：epoch seed 取 `pallet_babe::Randomness`（`pallets/sunlead/src/lib.rs:251-253`），但 BABE 不出块时其 epoch 是否照常轮换未验证 | Stage 1 必须验证；若否，改为中立 epoch seed 源（§3.2 降级项） |
| R3 | **SFF 独立 set id 来源**：现复用 GRANDPA `CurrentSetId`（`apis.rs:794-804`），GRANDPA 不启动时无轮换驱动 | Stage 2 (A3) 解决；需给出 set 变更时旧 set 在途证书的处理规则（现有 `SetCache` 上限 8 可作起点） |
| R4 | **3 秒 slot 是 `fch` 延迟硬下限** | 缩短 slot 作为独立优化项，不纳入本策略验收 |
| R5 | **既有文档状态标注自相矛盾**：`P1-Remediation-Worklog:5` 头部"待续"但正文全"已完成"；`P0-Worklog:43` 标题"进行中"但正文"已完成"，且 P0-D 编号与 `Remediation-Plan:123-129` 不是同一件事 | §8 统一回填，避免按错误状态排期 |
| R6 | **"静态门控"被记为已知问题**：`P3-Worklog:86-91`、`Remediation-Plan:24` | 按本策略明确改判为**目标特性**，`SunLeadActive` 注释中的 "P5 will replace this boolean" 应删除 |
| R7 | **GRANDPA 自身 equivocation 上报被关闭**：`configs/mod.rs:997-999` `= ()` | C1/C2 组合也应补齐，否则旧共识组合本身缺惩罚闭环 |
| R8 | **安全审计不可省** | 用户取消的是迁移/治理流程，不是安全性验证；B5 保留为 C3/C4 上线 gate |

---

## 8. 与既有文档的关系

### 8.1 本策略覆盖 / 废止的结论

| 文档 | 被覆盖的结论 |
|---|---|
| `../Stage-1/design/Sunchain-SunLead-Deterministic-Leader-Development-Plan.md` | `:3` "未经审查/演练/治理批准不得替换"；`:347-363` P5 ≥30 天迁移与 activation/回滚/紧急停产；`:252` 独立 rotation 作为替换前置。性能与正确性验收项仍有效 |
| `../Stage-1/implementation/Sunchain-SunLead-Remediation-Plan.md` | `:5` 的"未达 P4/P5 门槛"定性；`:17` 禁止 staging 激活；`:77-102` activation 状态机与"按 parent-state 选 verifier"；`:198-204` R4 治理批准。**P0 正确性项（epoch/slot/timestamp、资源治理、FindAuthor、equivocation）与 P4 故障矩阵仍然有效**，见 §3.3 |
| `../Stage-1/implementation/Sunchain-SunLead-P3-Worklog.md` / `../Stage-1/implementation/Sunchain-SunLead-P3-TestPlan.md` | 把静态门控列为"已知问题"的定性（`:86-91`）；`P3-TestPlan:27` 主网 preset 状态；`:139-143` 动态 handoff 属 P5 的待办 |
| `../Stage-1/implementation/Sunchain-SunLead-../Stage-1/implementation/Sunchain-SunLead-P0-Worklog.md` | `:58,64` P5 状态机 / BABE 临时角色"待后续"改为取消；`:5-6` 头部状态标注统一 |
| `../Stage-1/design/Sunchain-Alpenglow-Inspired-Fast-Finality-Design.md` | `:3` 头部措辞；`:250-252` F4 中属**切换机制**的部分（digest/激活高度/handover/rollback/双协议运行）；`:265` 多客户端差分测试；`:238` F2 30 天约束的迁移相关部分。**F4 的 "CommitQC = new finality proof" 能力、§4 安全模型、§7 必测性质、§13 P2 8/9 与 F3 遗留 11 仍有效** |
| `../Stage-1/design/Sunchain-Alpenglow-Inspired-Fast-Finality-Design.md:620` / §13 `- [ ] 10` | 下游 fast-confirmed 接入降级（权威终局下用 `finalizedHead`） |
| `pallets/sunlead/src/lib.rs:113-114` 注释、"P5 will replace this boolean" | 改为保留 genesis-only 语义的注释 |
| `../Stage-1/implementation/Sunchain-SFF-Gossip-Delivery-Diagnosis.md` | §7/§8/§9 的分析与三项性能收敛**全部有效**，继续作为 SFF 性能基线；其"暂不做 Alpenglow 三项"结论仍适用 |

### 8.2 需回填修订的文档〔✅ 2026-11-05 已全部回填〕

> **回填完成记录**：以下 1–5 项已在同一批次完成，涉及 **7 份既有文档 + 8 个 Rust 模块**（另有本策略文档自身的 §8.2 状态更新），共 16 个文件；均为注释/文档改动，无逻辑变更，`cargo build --bin sunchain` 通过。各文档统一以 `【2026-11-05 策略更新 / 状态回填】` 引用块指向本文，明确"取消 / 降级 / 保留"三类重定级；代码注释统一改为按 genesis profile 条件表述。
>
> **行号说明**：本文 §2、§3、§7、§8.1 中引用的既有文档/代码行号是**回填前快照**；回填后因插入策略说明块而整体下移，请按**章节标题与关键字**定位，勿按旧行号索引。代码行号（`node/src/service.rs` 等）本次未改动逻辑，仅注释行数变化。

1. `../Stage-1/design/Sunchain-SunLead-Deterministic-Leader-Development-Plan.md`、`../Stage-1/implementation/Sunchain-SunLead-Remediation-Plan.md`：标注"迁移/治理类 gate 已由本策略取消"，更新状态行。〔✅ 状态行 + P5/P6/§7、§4 P0-B/R4/§10 均已加策略块与标记〕
2. `../Stage-1/implementation/Sunchain-SunLead-P3-Worklog.md` / `../Stage-1/implementation/Sunchain-SunLead-P3-TestPlan.md` / `../Stage-1/implementation/Sunchain-SunLead-P0-Worklog.md` / `../Stage-1/implementation/Sunchain-SunLead-P1-Remediation-Worklog.md`：统一 R5/R6 的状态标注与主网 preset 描述。〔✅ 静态门控改判目标特性；P5 取消；主网 preset 过时描述更正；P1 头部状态冲突以正文为准；P0 的 P0-D 编号歧义澄清〕
3. `../Stage-1/design/Sunchain-Alpenglow-Inspired-Fast-Finality-Design.md`：头部状态行、§6 F4 定义按"genesis 决策、无 handover"重写；§13 待办按 §3 重定级。〔✅ 含 §4.6 阶段表 F4 行、§7 多客户端差分测试定级〕
4. `runtime/src/lib.rs:461-472`、`runtime/src/apis.rs:63-64,78`、`pallets/sunlead/src/lib.rs:5,14,113-114`、`pallets/fast-finality/src/lib.rs:3-6` 的阶段注释（多处停留在 P2/P3 描述，与现状及本策略不符）。〔✅ 另删除 `pallets/sunlead` 中 "P5 will replace this boolean" 的过时表述，改为 genesis-only 目标语义〕
5. `node/src/fast_finality.rs:1-10` 与 `node/src/service.rs:1068-1070` 的 "strictly shadow / non-essential" 注释：在 SFF 作为终局的组合下不再成立，Stage 3 按 profile 分条件表述。〔✅ 另同步 `fast_finality.rs` 的 `run_fast_finality` 文档注释，以及 `fast_finality_p2p.rs` / `fast_finality_pool.rs` 模块头的无条件 shadow 表述，避免同仓库注释自相矛盾〕

---

## 9. 一句话策略

> **不做迁移，只做选择；不清旧码，只解运行耦合；不比影子读数，只认权威写入。**
> 以 genesis `ConsensusProfile` 固化两个轴，Stage 0 立骨架、Stage 1 解出块轴、Stage 2 把 SFF 生产化并通过安全 gate、Stage 3 补齐 SFF 权威终局，最终让 C1–C4 四种组合各自成链、各自组网。
