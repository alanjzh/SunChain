# SFF 快速终局（投票 Gossip）投递 — 分析、整改与实测

> 日期：2026-11-04
> 代码：`node/src/fast_finality_p2p.rs`、`node/src/fast_finality.rs`、`node/src/fast_finality_pool.rs`
> 环境：4 验证人 dev 链（`--chain dev`，固定 Alice/Bob/Charlie/Dave），单机共置 + loopback，自定义通知协议 `/sun/sff/2`。

---

## 1. 背景与问题

SFF 对区块做快速终局确认，目标是让 `fastConfirmedHeight`（fch）**领先于** GRANDPA 的
`grandpaFinalizedNumber`（g）—— 即"比 grandpa 快"。每个高度需 4 验证人中的 2/3 投出
Prepare/Commit 票并聚合出 QC，fch 才能推进。

现象：出块正常、内置区块 gossip 完美同步，但 fch 长期卡在 `null`/0–5，frontier 长期
`null`，全链只形成 4–5 个 PrepareQC。

---

## 2. 分析过程（假设 → 验证 → 否证/确认）

测试环境：4 节点星型启动（均连 bootnode node0），临时 `-l sff=debug` 加诊断日志
（`gossip-rcv` / `gossip-drop` / `gossip-loop` 心跳 / `gossip-send` / `gossip-validate` 计时）。

逐项排查（各指标均为 0 或明确）：

- **子流关闭**：只有 `opened`、无 `closed`，`peers` 始终满 3 → 否证。
- **`send_async_notification` 失败/超时**：0（已加 500ms 超时包裹）→ 否证发送端问题。
- **`next_event()==None` 退出循环**：0，循环常空闲待收 → 否证提前退出。
- **`validate` 丢弃 / panic**：0 → 否证。
- **重发风暴**（`MAX_REPAIR_WINDOW=32` 限制后）：重发量受限，但投递率仍 ~5% → 否证负载/背压。
- **CPU/BLS 阻塞 worker**：把 `validate` 用 `spawn_blocking` 移出热路径后，投递率仍 ~5% → 否证。
- **`select` 每轮重建 `next_event` future 丢在途通知**：substrate 官方范式也每轮重建且正常；且
  与 `relay` 争夺 `&mut service` 借用冲突编译不过 → 已回退，否证。

**关键数值证据**：gossip 循环 `last_proc` 高达 **1226ms**，`validate`（BLS）单票高达 **590ms**
（4 节点共置抢 CPU，BLS 比常态慢 ~100×）。接收循环被 BLS 拖死 → 通知缓冲溢出 → ~95% 投票静默
丢弃 → 无法凑齐 2/3 → frontier 卡死。

---

## 3. 整改策略（两层根因）

### 根因 A — 自己建的 QC 进不了自己的 pool → frontier 恒为 null（fch 推不动的主因）
`aggregation_step_for` 之前只广播 QC，靠 gossip 回显进 pool；但自己的证书不会被回显给自己。
**修复**（`fast_finality.rs`）：本地组出 QC 后
`self.pool.lock().unwrap().add_qc(qc.clone())` 自注册到 pool。

### 根因 B — 接收路径逐票 BLS 验签重复且阻塞
每票既在 gossip 接收时验一次，又在组 QC 时由 `aggregate_votes` 验一次（重复）；且 4 节点抢 CPU
时把接收循环拖死（见 §2）。
**修复**（`fast_finality_p2p.rs`）：接收路径改用 `gossip_validate`（**仅结构性校验**：set 存在 /
signer 越界 / phase 合法；**证书仍 `verify_qc`**，单次聚合配对），逐票签名/PoP 验签完全推迟到
`aggregate_votes`（组 QC 时本就会验）。移除 `pop_cache` / `validate_with_pop_cache` /
`MAX_POP_CACHE_ENTRIES` / `spawn_blocking`。保留 `validate`（全量 BLS，仅供测试）。

### 其余整改（均已保留）
- 协议 `/sun/sff/1` → `/sun/sff/2`。
- 可靠投递层：`Ack([u8;32])` + outbox 重试 + 校验流水线（semaphore `MAX_CONCURRENT_BLS_VALIDATIONS=4`）
  + `SeenKey::Ack`；补偿底层通知偶发丢包。
- 引擎侧 PoP 缓存 `checked_pop_sets`（`current_set` 去重 PoP 验证）。
- `prune_backlog` 与重发 floor 锚定 `frontier`（避免未成 QC 的票被过早剪枝的死锁）。
- `maintain_votes` 改为按高度排序只重发最老 `MAX_REPAIR_WINDOW=4` 个未解高度，`REPAIR_INTERVAL_BLOCKS=4`。

---

## 4. 实测结果

`--fresh` 重启，长稳运行（链到 #1749）：

| 节点 | best | fch | grandpa(g) | PrepareQC |
|---|---|---|---|---|
| 0 | 1749 | 1730 | 1746 | 1689 |
| 1 | 1749 | 1728 | 1746 | 1700 |
| 2 | 1749 | 1728 | 1746 | 1692 |
| 3 | 1749 | 1726 | 1747 | 1690 |

- fch 从长期 `null`/0–5 变为跟随链尖（#1749 时 fch≈1728），4 节点一致，PrepareQC 持续形成。
- 接收循环 `last_proc` 从 ~1226ms 降到 **0ms**（BLS 已移出接收路径）。

---

## 5. 是否比 grandpa 快？

**否。** 实测 `fch`（1726–1730）仍**落后于** `grandpa`（1746–1747），稳态差距约 **16–21 块**
（早期低高度时仅 3–5 块，随链长放大为稳态滞后）。即 SFF 快速终局目前仍**慢于 GRANDPA**，
尚未达成"比 grandpa 快"的最终目标。

可能成因（待进一步排查）：fch 推进需每高度收齐 2/3 Prepare 票 → 组 PrepareQC → 收齐 2/3 Commit
票 → 组 CommitQC → 且 QC 需经 gossip 送达各节点；这是两轮 2/3 收集 + 传播，天然比 GRANDPA 单轮
终局滞后。稳态 ~18 块差距说明 SFF 确认吞吐略低于出块速率 / GRANDPA 速率，存在可压缩的残余延迟。

---

## 6. 信任前提与后续

- 接收路径不验票签名依赖**可信验证人集合**；敌对环境下一票伪造可抢占 signer 槽位致 QC 无法形成。
  生产加固应改 `try_build_qc` / `aggregate_votes` 为"过滤无效票"而非整组 `abort`，或恢复接收路径验签。
- 缩小 fch↔grandpa 差距的候选见 §8（已排定 1→2→3 顺序实施）。
- 测试脚本 `scripts/sff-multi/start_multi.sh` 仍带 `-l sff=debug`，测试后可撤。

---

## 7. 为何暂不做 Alpenglow 三项规模化手段（当前 4 节点）

Alpenglow 快的"三大来源"常被归纳为：① 单轮终局一批块；② leader 单点聚合；③ 纠删码抽样中继（Rotor）。
基于设计文档 §4.4 / §13.2(e) / §13.3，这三条在**当前 4 验证人阶段都不该做**：

- **① 单轮终局一批块**：
  - "批量确认一整段前缀"其实已在 P1-4 完成（`fastConfirmedBase` / `fastConfirmedFrontier`，§13.6），消费方可整批拿，不必逐高度查。
  - 但"把 Prepare→Commit 两阶段**合并成单轮**"正是 Alpenglow fast path 的快法，而设计 §4.4 **有意不选**它——"这比 Alpenglow 的最快单轮路径慢，但其安全条件与 Sunchain 当前 <1/3 Byzantine stake 模型直接对齐"。F1 的 TLA+ 规格 + `model_check`（§10）安全证明建立在**两阶段锁定**之上（quorum-intersection 引理、`LockInv`、`QuorumInv`）。合并单轮 = 重写安全论证，必须等 F4 形式化重证。**现在动它 = 推翻已证明的安全不变量。**
- **② leader 单点聚合**：
  - 设计 §4.5 **已有确定性 aggregator**：`primary = H(genesis||set_id||height||round||phase) mod validators`，`backup = primary+1…`（主聚+备扇出），并非完全没有。
  - 真正"leader 单点产出证书"属 §13.3 **P2 item 8**，针对的是"**上百验证人**时每节点独立全量聚合的发散成本"。
  - 但当前 4 节点的瓶颈**不是聚合发散**——P0 已把聚合从 O(积压) 降到 O(dirty key)（§13.5）。现在拖慢的是：收齐 **3/4 票**+ 重传延迟，以及组 QC 时的逐票 BLS。leader 聚合既不减少要收的票、也不减少 BLS，治不了；反而引入设计有意避免的 leader 概念与额外信任点。
- **③ 纠删码抽样中继（Rotor）**：
  - Rotor 解决的是**大区块数据可用性 + 上百验证人扇出**，而 SFF 的 vote/QC 是极小消息，纠删毫无必要。
  - 当前丢包根因是**自定义通知协议底层投递不可靠（~5–11%）+ 重传延迟**（本档前文实测），不是带宽/扇出瓶颈，纠删码治不了病根。
  - 同样属 §13.3 **P2 item 9**，上百节点才必需。

结论：这三条是设计文档明确的**规模化（P2）手段**，且都不针对当前 4 节点的真实瓶颈；其中①还会伤安全。先把 §8 的 1→2→3 做完，看 4 节点差距能否收敛到 2 块以内（对齐 §13.5 两节点的水平），再谈规模化。

## 8. 收敛 fch↔grandpa 差距的下一步计划（1→2→3）

当前 ~18 块差距由两类成本主导：**收齐 3/4 票 + 重传延迟**，以及**组 QC 时的逐票 BLS**。按下顺序实施：

### 1. 池内"已验证票"缓存（✅ 本次已实施）
- 改动：`sff_crypto` 新增 `aggregate_votes_cached(votes, set, pre_verified: &[bool])`；原 `aggregate_votes` 改为委托它（`&[]` 即全验，兼容 fixture/测试）。
- 池（`fast_finality_pool.rs`）新增 `verified: HashSet<(set_id, phase, height, round, signer_index)>`；`try_build_qc` 先按 `verified` 给每票打 `pre_verified` 标志，未缓存者在本处 `verify_vote` 一次并记入 `verified`，已缓存者跳过。`prune_below` 同步按高度裁剪 `verified`，避免无限增长。
- 安全性：票以 `(set_id, phase, height, round, signer_index)` 为唯一键，池中同键只存一次（后续重复被 `seen_vote` 丢弃），故票字节不可变，记忆化结果可靠；与早前"接收路径不验签依赖可信集合"的信任前提一致。
- 效果：`verify_vote`（PoP+签名配对）对**每张票只跑一次**，消除 `try_build_qc` 每次尝试对全部已收票的重验。
- 实测（--fresh，4 节点，本档 §4 同脚本）：`fch` 已**持续领先** `grandpa` 约 2 块——height 31（`fch=31`/`g=29`）、52（`fch=51`/`g=49`）、106（`fch=106`/`g=104`）均一致领先；接收循环 `last_proc=0ms`。对照此前未加缓存的同脚本：height 45 时 `fch=38–40` **落后** `g=43` 达 3–5 块，height 1749 时落后 18 块。可见此前"落后 grandpa"的主要部分正来自组 QC 时对已收票的重复 BLS 验签，缓存后已反超。

### 2. 两阶段流水线重叠（✅ 本次已实施）
- 根因（细化）：`on_built_qc` → `maybe_commit` 早已在**本地组出** PrepareQC 时同步触发 Commit 投票；但**经 gossip 收到的** PrepareQC 仅 `add_qc` 入池、未触发 `on_built_qc`，需等 `aggregation_step` 定时器（1s）兜底才补发 Commit 票——这正是 §13.2(c) "收到 PrepareQC 也要等 tick" 的残留延迟。
- 改动：
  - 池 `add_qc` 对所有已接受证书调用 `notify()`（原仅 Commit 阶段），使收到 Prepare/Skip QC 立即唤醒引擎。
  - 引擎新增 `reacted_qcs: Mutex<HashSet<(set_id, phase, height, round)>>`；`on_built_qc` 入口按 QC 身份去重（本地组出或收到都只反应一次）；新增 `react_to_received_qcs()` 在 `aggregation_step` 开头扫描池内 Prepare/Skip QC、对未反应的立即 `on_built_qc`（→ 发 Commit 票 / 推进 round）；`prune_backlog` 按高度同步裁剪 `reacted_qcs`。
  - 安全性：gossip 收到的 QC 已在 `gossip_validate` 经 `verify_qc` 全量验签后才入池；Commit 票仍引用同一 PrepareQC 哈希，不破坏两阶段锁定（`LockInv`）。
- 实测（--fresh，4 节点）：height 21（fch=21/g=19）、86（fch=85/g=83）均**持续领先 grandpa ~2 块**，`last_proc=0ms`，4 节点一致。item 2 将"收到 PrepareQC 等定时器"的 ~1s 兜底延迟改为事件驱动；稳态差距与 item 1 持平（即已反超 2 块），并增强"仅经 gossip 学到 PrepareQC 也能立即 commit"的健壮性。

### 3. 压缩重传延迟（✅ 本次已实施）
- 根因：可靠投递层（Ack + outbox 重传）首重传等待 `DELIVERY_RETRY_BASE=500ms`、每 100ms tick 仅重传 `MAX_RETRIES_PER_TICK=4` 条、上限 `DELIVERY_RETRY_MAX=8s`。共置验证人 RTT 为亚毫秒级，500ms 首重传对"任一张票在 gossip 中丢失"的恢复过慢，直接拉长"收齐 3/4 票"的尾部时延。
- 改动（`fast_finality_p2p.rs` 常量）：
  - `DELIVERY_RETRY_BASE` 500ms → **150ms**（首重传 ~3.3x 更快；仍是对跨域链路的合理下限，退避上限不变）。
  - `DELIVERY_RETRY_MAX` 8s → **4s**（基数为 150ms 时退避序列 150/300/600/1200/2400/封顶 4s）。
  - `MAX_RETRIES_PER_TICK` 4 → **16**（修复突发丢票时不再串行 4 条/100ms，而是每 tick 最多 16 条）。
  - 不改协议语义、不改安全不变量、不改两阶段锁定；仅加快"未确认消息"的重发节奏。
- 实测（--fresh，4 节点）：稳态 `fch` 仍**持续领先 grandpa ~2 块**（h=46/106 一致），`last_proc=0ms`，`validating=0`；gossip 循环 `pending=0–12`，健康。重传频率 ~7.5 次/s/节点，且**全部在 `attempts=2` 即恢复**（单点丢失→快速补发，未升级到更高 attempt），说明 150ms 首重传已让丢票即时自愈，无重传风暴。稳态差距由两阶段固有成本决定（~2 块），item 3 主要压低"丢包恢复尾部时延"、增强抖动/丢包下的健壮性。

完成 1→3 后，若 4 节点仍系统性落后于 grandpa，再回头评估是否需触及协议语义（如 §7 的 P2 项）。

## 9. 收敛进展小结（截至本档更新）

- **① 票缓存（✅）**：`aggregate_votes` 逐票 BLS 改为 `aggregate_votes_cached` + 池内 `verified` 记忆化，每张票仅验一次。立竿见影：`fch` 由落后 grandpa 18 块反转为**领先 ~2 块**（h=31/52/106/...），`last_proc=0ms`。
- **② 两阶段流水线重叠（✅）**：收到 PrepareQC 经 gossip 也立即触发本节点 Commit 投票（原仅本地组出时触发，收到需等 1s 定时器兜底）。实测稳态与 ① 持平（领先 ~2 块），并消除该兜底延迟、增强健壮性。
- **③ 压缩重传延迟（✅）**：Ack/重传层首重传 500ms→150ms、上限 8s→4s、每 tick 重传 4→16。实测重传 ~7.5 次/s/节点且全部 `attempts=2` 自愈，无风暴；稳态 `fch` 仍稳定领先 grandpa ~2 块，但丢包恢复尾部时延显著下降、抖动下更健壮。当前 4 节点 `fch` 已稳定领先 grandpa ~2 块（对齐设计 §13.5 两节点水平），收敛计划 1/2/3 全部完成。

