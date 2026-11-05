# P2-8 阶段 1 执行记录 — 多验证人部署 + 故障注入 + 测量

- **日期**：2026-11-04
- **关联评估**：`../design/Sunchain-SunLead-P2-8-Feasibility.md`（结论：暂缓 P2-8，先做本阶段）
- **目的**：实跑 4 验证人链，测量 GRANDPA/SFF 行为；故障注入验证 set 旋转/自愈；为"P2-8 现在是否值得做"提供数据。

---

## 1. 环境与前置

- 4 验证人 dev 链，**genesis 已确定性固定**（`chain_spec.rs::DEV_GENESIS_VALIDATORS=4`，见前序修复）：共享 genesis `0x093439b43571a1689ceda1dca1a6092029a6869962afbb5282e85da5bb299ff9`，杜绝"创世不一致→两两分区"。
- 启动：`scripts/sff-multi/start_multi.sh --fresh`（星型拓扑，node0 为 bootstrap，chainType 强制 Local）。
- 工具：`measure_sff.py`（SFF-vs-GRANDPA gap）、`fault_inject.sh`（杀节点）。

> **测试卫生教训**：`pkill -f "chain dev"` 匹配不到 `--chain /tmp/.../spec_raw.json` 的节点；必须 `pkill -f "target/debug/sunchain"` 清干净，否则会残留旧进程与新进程争抢同一 `base-path` 的 DB 锁，表现为 `sff_status.fastConfirmedFrontier=null`、duplicate 进程。本记录所有数据均来自**干净的单实例 4 节点**。

---

## 2. 稳态测量（无故障）

| 指标 | 结果 |
|---|---|
| GRANDPA 终局高度 | 持续上升（观测 #91 → #186，无停滞） |
| 各节点 peer | 3–4 peers，同 genesis，单一链 |
| `sff_status.serving` | `true`，`degraded=false`，`conflicts=0` |
| `grandpaFinalizedNumber` | 正常推进（如 #44、#103、#186） |
| `fastConfirmedFrontier` | **`null`** |
| `fastConfirmedHeight` | 卡在 ~1（个别样本到 3） |
| SFF 投票日志 | **94× `signed Prepare vote`，仅 1× `signed Commit vote`，无 formed `PrepareQC`** |

**解读**：SFF 的 fast-confirmation（F3）在 4 验证人下**完全未推进**——卡在 Prepare→Commit 边界，PrepareQC 未形成。这与设计文档已知限制一致（`§12.5`："dev 上 fast-confirmed 仍落后 GRANDPA…两阶段（Prepare→Commit）串行 + 2/2 门槛 + 200ms 聚合节拍是主因"；`§13.2(c)`）。

**关键结论**：当前延迟/吞吐瓶颈在 **F2/F3 的两阶段串行 + 聚合节拍**，而**不是** flood-fill 传播模型。把票改发 primary（P2-8）不会触动这个瓶颈。

---

## 3. 故障注入（杀 node3 / dave）

1. **杀 node3**（确认进程真正退出，sunchain 进程数 4→3）。
2. **3/4 窗口内 node0 GRANDPA 持续终局**：`#156 → #159 → #160 → #163`（~32s）。
   - 4 验证人 quorum = 2/3×4 = 3；剩 3 节点恰好满足 → **终局不中断**。
3. **重启 node3**（从其 `.cmd` 文件 standalone 启动，从 peer 重同步）。
4. **追平**：~45s 内 node3 `finalized #186` == node0 `finalized #186`，`Idle (3 peers)`。

**解读**：set 旋转/自愈在多验证人下成立——丢 1/4 验证人不致停链，回归节点能从存活节点追平。这是 P2-8 的 backup 接管场景的**前置环境条件已具备**（≥3 验证人、可杀 primary 做注入），但目前 backup 接管的价值仍无法在 F3 已卡住的前提下被度量。

---

## 4. 阶段 1 结论 → 对 P2-8 决策的影响

- ✅ 4 验证人链稳定、不分区（genesis 修复落地）。
- ✅ GRANDPA 终局 + 自愈在 3/4 下成立（故障注入通过）。
- ⚠️ **SFF fast-confirmation 当前被 F2/F3 两阶段串行 + 聚合节拍卡死**，非传播模型问题。
- ⇒ **维持评估结论：P2-8 暂缓**。在 n=4 下它既不能加速 SFF（瓶颈不在 fanout），也无法被有效度量（F3 未推进）。
- ⇒ **更划算的加速项优先**：pipeline 两阶段（Prepare/Commit 重叠）、收紧 1s 兜底 tick——二者在 n=2–4 即可见效，且不涉及传播模型重构（见评估文档 §7）。
- ⇒ 当且仅当阶段 1 测量显示**聚合带宽/验签成为瓶颈**时，再上 P2-8，且 feature-gate 保留 flood-fill 兜底。

---

## 5. 复现命令

```bash
# 干净起链
pkill -9 -f "target/debug/sunchain"; rm -rf /tmp/sff-multi
bash scripts/sff-multi/start_multi.sh --fresh

# 稳态测量
python3 scripts/sff-multi/measure_sff.py --rpcs 9944,9945,9946,9947 --logs-dir /tmp/sff-multi --duration 90

# 故障注入（杀 node3 60s）
bash scripts/sff-multi/fault_inject.sh 3 60
# 另开窗口测 outage 期 gap：
python3 scripts/sff-multi/measure_sff.py --rpcs 9944,9945,9946,9947 --logs-dir /tmp/sff-multi --duration 60
```
