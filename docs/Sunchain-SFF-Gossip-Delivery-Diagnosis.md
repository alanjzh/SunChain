# SFF 快速终局（投票 Gossip）投递 — 分析、整改与实测

> 日期：2026-11-04
> 代码：`node/src/fast_finality_p2p.rs`、`node/src/fast_finality.rs`、`node/src/fast_finality_pool.rs`
> 环境：4 验证人 dev 链（`--chain dev`，固定 Alice/Bob/Charlie/Dave），单机共置 + loopback，自定义通知协议 `/sun/sff/2`。

---

## 1. 背景与问题

SFF 对区块做快速终局确认，目标是让 `fastConfirmedHeight`（fch）**领先于** GRANDPA 的
`grandpaFinalizedNumber`（g）—— 即“比 grandpa 快”。每个高度需 4 验证人中的 2/3 投出
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
尚未达成“比 grandpa 快”的最终目标。

可能成因（待进一步排查）：fch 推进需每高度收齐 2/3 Prepare 票 → 组 PrepareQC → 收齐 2/3 Commit
票 → 组 CommitQC → 且 QC 需经 gossip 送达各节点；这是两轮 2/3 收集 + 传播，天然比 GRANDPA 单轮
终局滞后。稳态 ~18 块差距说明 SFF 确认吞吐略低于出块速率 / GRANDPA 速率，存在可压缩的残余延迟。

---

## 6. 信任前提与后续

- 接收路径不验票签名依赖**可信验证人集合**；敌对环境下一票伪造可抢占 signer 槽位致 QC 无法形成。
  生产加固应改 `try_build_qc` / `aggregate_votes` 为“过滤无效票”而非整组 `abort`，或恢复接收路径验签。
- 缩小 fch↔grandpa 差距的候选：
  ① 缩短聚合/确认轮次（如 Prepare 与 Commit 并行或重叠）；
  ② 观测 QC gossip 是否仍有残余漏传进一步提速；
  ③ 确认 GRANDPA finality gap 配置是否被压得过小。
- 测试脚本 `scripts/sff-multi/start_multi.sh` 仍带 `-l sff=debug`，测试后可撤。
