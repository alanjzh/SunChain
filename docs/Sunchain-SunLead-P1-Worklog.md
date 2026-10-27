# SunChain SunLead 确定性 Leader 选举 — P1 实现工作日志

- **日期**：2026-10-26
- **阶段**：P1（确定性 Leader 选举核心逻辑 — 算法 + 密码学 + 验证）
- **参考计划**：`docs/Sunchain-SunLead-Deterministic-Leader-Development-Plan.md`
- **状态**：P1 核心编码完成，三个 crate 全部编译通过、单元测试全绿。P0（仓库改名 / `--fresh` 清库）按用户要求**未执行**。

---

## 一、确认的设计决策

1. **签名算法：SR25519（已落地）**
   - 原先采用 ED25519（对齐 GRANDPA、签名确定、便于批量验证）。
   - 用户确认改为 **SR25519**，以与 BABE 会话密钥保持一致。
   - 注意：SR25519 签名本身**非确定性**（签名带随机 nonce），但签名不参与确定性 schedule 的计算，因此不影响 schedule 的 WASM/native 一致性（确定性 GATE 仍然满足）。验证侧只做 `Pair::verify`，逻辑不变。
   - `sunl` 会话密钥 ID 为 `b"sunl"`（4 字节 ASCII）。

2. **排程语义：维持原方案（已确认）**
   - `max_window_slots = 4` 为**硬上限**（单一 authority 连续 slot 数不超过 4）。
   - `window_len = 2` 为**软目标**：stake 均衡时运行长度≈2，极端偏斜时才逼近硬上限 4。
   - 与"相邻配额合并为 window，不超过 MaxWindowSlots"完全兼容。

---

## 二、实现内容

新增三个纯逻辑 / 密码学 crate（不碰共识接线，不影响现有链）：

### 1. `crates/sunlead-primitives`
- `sunl` 会话密钥 crypto 封装：`app_crypto!(sr25519, KEY_TYPE)` → `SunLeadPair / SunLeadPublic / SunLeadSignature / SunLeadId`。
- `SunLeadPreDigestBody` / `SunLeadPreDigest`：前置摘要（版本、authority_set_id、epoch_index、slot、window_id、leader_index、parent_hash、schedule_root、signature）。
- `SunLeadAuthoritySnapshot`、`SunLeadAuthority<AccountId>`、`canonical_authorities()`（按 account 再按 id 排序，`AccountId: Ord`）。
- `SunLeadEquivocationProof`（含 `offender` 公钥以便自验证）。
- `SunLeadScheduleParams`（`max_window_slots / window_len / epoch_slots`），默认 `v1_defaults()`。
- `signing_message()`：绑定 `b"sunl"` + genesis + header_hash + **全部** digest 字段，防跨链 / 跨块重放。
- 测试：`signing_message` 重放防护、跨 header / 跨 key 不通过、raw 签名结构兼容。

### 2. `crates/sunlead-schedule`
- `apportionment()`：**最大余数法**（Hamilton）把 epoch_slots 按 stake 分配为整数配额，余数按 `w*slots % total` 降序补 1。
- `deterministic_permutation()`：用 `BlakeTwo256(seed || i)` 生成确定性排列（无浮点、无 HashMap，WASM/native 一致）。
- `enforce_feasible()`：极端 stake 偏斜时把主导者配额封顶到 `max_window_slots*(others+1)`，多余权重轮转再分配给其他验证者；封顶后仍不可行则回退均分。
- `build_sequence()`：**运行受限贪心排程**——每步选"剩余最多且未达软/硬阈"的验证者，运行长度硬上限 `max_window_slots`；单验证者（n==1）特例：直接全量归一个 leader（运行上限在 n>1 才有意义）。
- `compute_root()` / `leader_at()` / `schedule_proof()` / `verify_schedule_entry()`：用 vendored `binary-merkle-tree` 对 `(slot, leader, window)` 做 Merkle 树，root 存 digest；支持单 slot 的 Merkle proof 验证。
- `build_schedule()` → `ComputedSchedule { root, leaders, windows, quota, params }`（暴露**实际使用的、经可行性调整后的配额**）。
- golden 向量：测试内 `golden.rs` 把 5 个固定场景（含 stake 偏斜、单验证者、4 验证者、16 验证者）序列化为 `crates/sunlead-schedule/golden/sunlead.golden.json`（~88KB），CI 比对"bootstrap 生成 == 落盘"，防止算法悄悄变化。
- 测试（10 个）：配额精确 / 总和 / 下界、运行上限、确定性、单验证者、覆盖每个 slot、golden bootstrap & 重验证、400 组 fuzz 不变量（覆盖率 / 配额精确 / 运行上限 / 确定性）。

### 3. `crates/sunlead-verifier`（`no_std`）
- `verify_pre_digest()`：版本号、authority_set_id / epoch_index / slot 一致性、leader==`leader_at(slot)`、ScheduleInput 重建与 `schedule_root` 一致、SR25519 签名验证（`sp_core::sr25519::Pair::verify`）。
- `verify_slot_monotonicity()`：同 epoch 内 slot 严格递增。
- `verify_parent_hash()`：d1.parent == d2.hash。
- `verify_equivocation_proof()`：两块 header 不同、digest 必填字段一致、两签名均合法、offender 自洽。
- 测试（8 个）：合法摘要通过、tamper leader / root / 版本 / set-id / slot / parent 均拒、双签 equivocation、坏签名 key 拒、offender 自验证。

---

## 三、过程与踩坑（关键修复）

1. **`H256::repeat_byte` 不存在** → 统一改用 `H256::from_slice(&[b; 32])`（各 crate 内加 `h256_repeat()` 测试辅助）。
2. **`BlakeTwo256` 路径** → 来自 `sp_runtime::traits::BlakeTwo256`（不在 `sp_core`）。
3. **`binary-merkle-tree` 的 `Leaf` 约束** → 需 `AsRef<[u8]>`，`verify_proof` 传 `&leaf`（`&Vec<u8>`），`merkle_proof` 传 `leaves.iter().cloned()`。
4. **`Pair::verify` 是 `sp_core::crypto::Pair` trait 的静态方法** → `Public` 没有实例 `verify`，需 `use sp_core::crypto::Pair as _;` 后调 `sp_core::sr25519::Pair::verify(&sig, &msg, &pub)`；并修正 `Slot` 用 `u64::from(slot)`。
5. **`canonical_authorities` 需 `AccountId: Ord`**，且 `SunLeadId` 比较用 `<SunLeadId as AsRef<[u8]>>::as_ref` 全限定写法。
6. **`serde_json` 在 workspace 中 `default-features=false`** → dev-dependency 显式加 `features = ["std"]`，否则 `to_string_pretty` 报 `Display` 缺失。
7. **`apportionment` 测试期望值算错**（100/200·600=300 而非 334）→ 改为正确的 `[300,150,90,60]`；并把断言改为 `u64` 字面量。
8. **`n==1` 死锁**：单验证者无法满足运行上限 → 在 `build_sequence` 开头特例直接返回全 0 leaders/windows。
9. **配额比较基准错误**：测试原比对"原始 apportionment"，但 schedule 用的是**可行性调整后**配额 → `ComputedSchedule` 暴露 `quota` 字段，测试改比对 `s.quota`。
10. **SR25519 切换**：`app_crypto!(sr25519, …)` + 全部 `ed25519` 引用改为 `sr25519`（Public/Signature/Pair），测试用真实签名（无定长数组），64 字节签名天然满足。

所有修复后：`primitives 2 passed`、`schedule 10 passed`、`verifier 8 passed`，且三个 crate 在 `cargo build` 下无自身 warning。

---

## 四、未做 / 下一步（P2+）

- **P0 未执行**：仓库改名、`--fresh` 清库仅保留在计划中（按用户要求）。
- **P2（Runtime wiring）**：在 runtime 加 `SunLeadApi`、epoch seed 来源、`SunLeadAuthoritySnapshot` 存储；把 `sunl` 会话密钥接入 chain-spec 与 `BabeApi`/会话管理；区块生产侧注入 digest、导入侧接 `verify_pre_digest` + 单调性 + parent 检查；equivocation 证据收集与 `pallet-offences` 接入。
- **P3**：端到端联调 + 性能（epoch 很大时 schedule 计算 / proof 大小）。
- **注意（历史教训）**：任何 runtime 改动（含签名算法 / 密钥类型）后，本地起链务必 `--fresh` 清库，否则旧 WASM 与节点原生 runtime 不一致（曾表现为喂价投票被 BadProof 全拒、价格陈旧）。
