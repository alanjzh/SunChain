# SunLead SFF — P2-8（leader/primary 聚合 + primary/backup fanout）可行性评估

- **日期**：2026-11-04
- **状态**：✅ 评估完成 → **结论：暂缓实现，先做阶段 1（多验证人部署 + 故障注入 + 测量）**
- **设计依据**：`Sunchain-Alpenglow-Inspired-Fast-Finality-Design.md` §4.5（leader、聚合器与可用性）、§13.2(c)（两阶段串行成因）、§13.3（P2 分阶段计划，标注"验证人数量上百后才必需"）
- **代码现状依据**：`node/src/fast_finality.rs`、`node/src/fast_finality_p2p.rs`、`scripts/sff-multi/`（start_multi.sh / measure_sff.py / fault_inject.sh）

---

## 1. 设计本意（§4.5 / §13.3）

用**确定性聚合者**替代"每节点 flood-fill 所有票并独立聚合"：

```text
primary = H(genesis_hash || set_id || height || round || phase) mod validators
backup  = primary + 1, primary + 2, ...
```

- 验证人把票**直接发给 primary**（而非广播给所有人）；
- primary 聚合后广播 QC；
- `aggregate_timeout` 内没看到 QC，再向 backup **扇出（fanout）**；
- 任一节点均可合法聚合并广播，primary 不是信任点（§4.5 "primary 不是信任点"）；
- vote/certificate 走专属 notification protocol，但必须保留有限 gossip/repair 路径（防 primary 失联后票丢失）。

---

## 2. 当前代码现状

| 关注点 | 现状 | 位置 |
|---|---|---|
| 聚合 | 每节点**独立**对脏键做 `aggregation_step`（`take_dirty` 事件驱动扫描），无 primary/backup 角色 | `fast_finality.rs:969` / `:1332` |
| 传播 | 投票走 **flood-fill 全量 gossip**：`validate` + 入池 + 未见过才中继，无定向 primary 寻址 | `fast_finality_p2p.rs:313-334` |
| 确定性聚合者 | **无**——grep 确认 `fast_finality.rs` 中没有任何 `primary` / `backup` / `fanout` / `H(...)` 逻辑 | P2-8 属**绿地**，未动过 |

即：当前是"每节点收全量票 → 本地各自聚合"，已经能在 n=2 下让两节点都拿到全部票并完成 QC。

---

## 3. 评估结论：能否现在做？

**技术上可以起步**，但**不建议现在做**。建议先完成阶段 1（多验证人实跑 + 故障注入 + 延迟测量），再决定是否上 P2-8。

理由见下。

---

## 4. 理由

1. **设计把 P2 明确标为"验证人数量上百后才必需"（§13.3）。**
   当前 dev 链 n=4（genesis 固定 4 验证人，见 `chain_spec.rs::DEV_GENESIS_VALIDATORS`），flood-fill 已让每个节点都拿到全部票，leader 聚合几乎零收益，反而增加复杂度。

2. **n=2 的延迟瓶颈不是聚合扇出。**
   实测（§12.5 F3 已知限制）dev 上 `fast-confirmed` 仍落后 GRANDPA 约 30 块，主因是**"两阶段串行 Prepare→Commit + 2/2 门槛 + 1s 兜底 tick"**（§13.2(c)），而非票的传播模型。P2-8 在 2–4 验证人下**不会让 SFF 更快**，只会把已工作的链路改复杂。

3. **P2-8 的核心价值（backup 在 primary 宕机时接管）无法在 2–4 验证人上验证。**
   "primary 挂了"同时意味着少了一个投票者，没有有意义的故障场景——必须 **3–5 验证人 + 可杀掉 primary 做故障注入**，才能证明 backup 接管后 QC 仍能形成。这恰是阶段 1 的环境。

4. **改传播模型有回归风险。**
   把 flood-fill 改成"票只发 primary"，若 backup 扇出不健壮，QC 形成会失败、重新引入空洞，反而**破坏当前已稳定的 2–4 验证人链**。

---

## 5. 建议的顺序

### 阶段 1（先）
实跑 **3–5 验证人链**，测量 GRANDPA 延迟随 n 变大后 SFF 是否显著领先，验证 set 旋转/自愈在多验证人下仍成立。这个测量**决定 P2-8 现在是否值得做**。

具体动作（已有脚本支撑）：
- `scripts/sff-multi/start_multi.sh --fresh` 起 4 验证人星型拓扑（genesis 现已确定性，见 §6）；
- `scripts/sff-multi/measure_sff.py --rpcs 9944,9945,9946,9947` 测 `fastConfirmedFrontier - grandpaFinalizedNumber` 稳态 gap、serving/degraded/conflicts、累积 STALL/Skip；
- `scripts/sff-multi/fault_inject.sh <idx> <outage>` 杀一个验证人（如 dave=3）做故障注入，观察 gap 与回归后的自愈/追平。

### 阶段 2（后，feature-gated）
若阶段 1 测量显示**聚合带宽 / 验签成为瓶颈**，再上 P2-8，且用开关保留 flood-fill 作为默认 / 兜底，避免回归 2–4 验证人。

---

## 6. 若仍要从 P2-8 起步：落地步骤

1. **辅助函数**：加 `primary_for(set_id, height, round, phase)` / `backup_for(...)`，用 §4.5 的 `H(...)` mod n（n 取自 authority set 规模）。
2. **投票传播**：每票直接发给其 `(set, height, round, phase)` 的 primary（及首位 backup）；保留 flood-fill 广播作为 **feature-flag 兜底**。
3. **primary 聚合并 gossip QC**；backup 在 `aggregate_timeout` 内未见 QC 则接管重聚合 + 广播。
4. **整体 feature-gate**，默认 2–4 验证人仍走 flood-fill，直到多验证人验证通过。
5. **验证环境**：3–5 验证人链 + 杀 primary 故障注入。

---

## 7. 补充：比 P2-8 更划算的"现在就让 SFF 更快"项

目标若是"现在就让 SFF 更快"，以下在 **n=2 就能见效，且不涉及传播模型重构**，优先级高于 P2-8：

- **pipeline 两阶段**（Prepare / Commit 重叠）：直接削掉 §13.2(c) 串行等待，收益最大；
- **收紧 1s 兜底 tick**：降低单轮最低延迟下限。

---

## 8. 阶段 1 执行计划（接"继续阶段 1 部分"）

> 详见 `../implementation/Sunchain-SunLead-P2-8-Phase1-Execution.md`（或下方运行记录）。

1. 起 4 验证人确定性 dev 链（genesis 已由 `DEV_GENESIS_VALIDATORS=4` 固定，杜绝此前"创世不一致→两两分区"）。
2. 稳态测量 60s，记录各节点 `max_gap(frontier-grandpa)`、STALL/Skip 累积。
3. 故障注入：杀 node3（dave）60s， outage 期间测量 gap；重启后观察追平。
4. 结论写入本评估的"阶段 1 结果"段，作为 P2-8 是否上马的数据依据。

---

*评估前提：genesis 确定性修复（前序 commit）已落地，4 验证人 dev 链可稳定同链运行，故阶段 1 现在可真实执行。*
