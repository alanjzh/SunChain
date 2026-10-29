# SunChain SunLead P1 — 整改工作日志（导入严格性 / 参数语义 / 可观测性）

> 基线：`Sunchain-SunLead-Remediation-Plan.md` §7
> 状态（2026-10-28）：P1-7.1A/B 已完成并编译通过；发现并修复了"SunLead 链重启 panic"这一阻断性前置 bug（见 §0）；
> 7.1C/D、7.2、7.3 待续。
> 决策：用户要求先放一放 §4.2（P5 激活状态机）与 §4.3（BABE 依赖收敛），优先做 P1。

## 〇、前置修复：SunLead 链"复用既有 DB 重启 panic"（链级阻断 bug）

### 0.1 现象
- 节点以既有 SunLead 链 DB（非 `--fresh`）重启时，启动即 panic：
  `finalized header must be valid; valid blocks have a pre-digest; qed: NoPreRuntimeDigest`
  （`babe/src/lib.rs:1713`，BABE 终局层读 finalized head 找不到 babe 摘要）。
- 这意味着**任何 validator 重启都会让链停摆**——与 Remediation 的"运行期不能切换共识"同源但独立。

### 0.2 根因（两层）
1. **`use_sunlead` 在重启时被误判为 false**：`new_partial` 用
   `SunLeadApi::is_active(client, genesis_hash)` 判断激活引擎；但长链重启后**创世状态会被
   状态 pruning 丢弃**，runtime API 在 `block_hash(0)` 上返回
   `Err(UnknownBlock("State already discarded for <genesis>"))`，被 `unwrap_or(false)` 静默转成
   false → 误以为 BABE 是激活引擎。
2. **BABE `block_import` 构造即 eager-finalize 当前 finalized head**：`sc_consensus_babe::block_import`
   构造时会立刻 finalize 现有 finalized head；若它是 SunLead 块（无 babe pre-digest）就直接 panic。

### 0.3 修复（`node/src/service.rs`）
- **BABE 链上不构造其 block import / link**：把 `babe_block_import`/`babe_link` 改为
  `Option`，仅在 `!use_sunlead`（BABE 激活）时构造；SunLead 链恒为 `None`，且 `slot_duration` 改由
  `sc_consensus_babe::configuration(&*client)?.slot_duration()`（仅读运行时配置，不构造）取得。
  `PartialComponents` 的 `Other` 元组类型相应改为 `Option<...>`。
- **`use_sunlead` 改为读 finalized head（不被 pruning 影响）**：
  `SunLeadActive` 是创世恒定值，任意块读到的值相同；改为在
  `client.chain_info().finalized_hash` 上调用 `is_active`（近期状态不会被 pruning 丢弃）；
  **兜底**：若 API 仍失败，则检查 finalized head 的 digest 是否含 `SUNLEAD_ENGINE_ID`，
  保证 validator 重启永远不会因为状态 pruning 被错判成 BABE。

### 0.4 验收（关键）
- 两节点以**既有 SunLead DB 直接重启**（不清库）：
  - master 日志 `SunLead activation flag (is_active) = true` → `starting SunLead authoring`，
    从既有链续跑（`best #961 / finalized #959`，不是从 0 开始）；**无 panic**。
  - slave 同步上线，`peers:1`，`Imported #962…` 持续出块、`finalized #962` 推进。
  - RPC `system_health` 两节点 `peers:1 / isSyncing:false`，`state_call SunLeadApi_is_active = 0x01`。
  - 错误扫描（master+slave）为空：无 `Verification failed` / `banned` / `SlotOutOfRange` /
    `panic` / `NoPreRuntime` / `equivocation`。
- 推论：今后 SunLead 链**重启不必 `--fresh`**（之前记忆里"复用 DB 必 panic"已不成立，根因已修）。

## 一、P1-7.1 导入严格性（Remediation §7.1）

### 1.1 目标
- SunLead pre-runtime digest 与 seal 都必须恰好一个；重复 / 不可解码 / 混入冲突 engine item 一律拒绝。
- 读取 parent header / snapshot 失败时拒绝 block，不能跳过 slot monotonicity。
- 抽出共享 `verify_header`，供 remote import 与 local authoring 自检共用。
- 补 node import-queue 测试：错误签名 / root / leader、重复 digest/seal、错误 parent、错误
  timestamp、无 snapshot、parent lookup error。

### 1.2 已完成（`node/src/sunlead_consensus/import_queue.rs`）
- `find_sunlead_pre_digest`：除拒绝重复、缺失外，新增"声明 SUNLEAD 引擎但解码失败即拒绝"
  （不可解码不再被静默当作缺失，避免畸形 header 绕过验证）。
- `find_sunlead_seal`：改为 `Result`，拒绝重复 seal、缺失 seal、声明 SUNLEAD 但不可解码的 seal。
- 新增 `check_no_conflicting_engine_items`：header 中任何 `PreRuntime` / `Seal` 若属于非
  `SUNLEAD` 引擎（如 BABE）直接拒绝，防止混合共识协议 / 走私 fallback 出块路径。
- `verify` 在取得 pre-digest 后立即调用 `check_no_conflicting_engine_items`。
- **fail-closed（§7.1B）**：parent header 读取失败（`Err` 或缺失）现在直接拒绝块；仅当 parent
  本身不是 SunLead 块（如 genesis，无 SunLead pre-digest）才跳过 slot monotonicity 校验，
  避免误伤首块。

### 1.3 7.1C：抽出共享 `verify_sunlead_header`（remote import + 本地出块自检共用）
- 在 `import_queue.rs` 新增**纯函数** `verify_sunlead_header<B: BlockT>(header, genesis_hash, &SunLeadHeaderInputs)`
  与配套 `SunLeadHeaderInputs { context, params, parent_sunlead_slot }` / `VerifiedSunLeadHeader`
  （均为 `pub`）。该函数承载全部导入前校验：恰好一个 pre-digest / 一个 seal（拒绝重复与不可解码）、
  拒绝冲突 engine item、再依次跑 `verify_pre_digest` / `verify_seal` / `verify_parent_hash` /
  `verify_slot_monotonicity`（parent 非 SunLead 块时 `parent_sunlead_slot = None` 跳过单调性）。
- `SunLeadVerifier::verify`（remote import 适配器）现在只负责从 client 取 snapshot / config /
  parent header，组装 `SunLeadHeaderInputs`，然后 **delegate 到 `verify_sunlead_header`**；并把 seal
  从 digest 迁到 `post_digests`。
- **本地出块自检**（`worker.rs::block_import_params`）：出块前用 `aux_data` 里已有的 `context` /
  `params` 拼出同一套 `SunLeadHeaderInputs`，把 seal 补回 pre-seal header 后调用同一个
  `verify_sunlead_header::<B>`。即"本地块在提交 import / 广播前，先按 peer 对 gossip 块完全相同的规则
  校验一遍"——produce/keystore 失误（错 leader、旧 epoch、坏签名）会在本地暴露，而不是等 peer 拒块。
  需给 worker impl 加 `B::Header: Clone` 约束以构造"pre-seal + seal"联合 header。

### 1.4 7.1D：node import-queue 测试矩阵（15 个测试全过）
位置：`node/src/sunlead_consensus/import_queue.rs` 的 `#[cfg(test)] mod tests`（cargo test 通过）。
分两组：
1. **纯验证核心（11 个）**：直接构造合法 SunLead header（按 keys/stakes/params 派生 schedule 得到
   leader/window_id/schedule_root 并签名），覆盖——合法块通过、错误签名、错误 root、错误 leader_index、
   错误 parent_hash、slot monotonicity 违例、重复 pre-digest、重复 seal、不可解码 pre-digest、不可解码 seal、
   混入 BABE 冲突 engine item。每个篡改只隔离一个验收准则（其余保持合法），断言返回具体错误串。
2. **适配器（4 个）**：`mock_impl_runtime_apis!` 实现 `SunLeadApi<opaque::Block>` + 手写 `HeaderBackend`，
   经 `tokio` 当前线程 runtime 驱动 `SunLeadVerifier::verify`：
   - `verifier_happy_path_through_adapter`：合法块 + 合法 snapshot/config/parent 通过；
   - `verifier_no_snapshot_errors`：`current_snapshot` 返回 `None` → `schedule snapshot not available`；
   - `verifier_parent_lookup_error`：parent header 缺失 → `parent header not found`；
   - `verifier_fails_on_bad_signature_through_adapter`：用非 leader 密钥签名 → `seal verification failed`。
   注意 mock 必须用 `sunchain_runtime::opaque::Block`（而非 `sunchain_runtime::Block` 别名，后者是
   `UncheckedExtrinsic` 版本，与 verifier 的 `opaque::Block` 不是同一类型，否则 `ProvideRuntimeApi` /
   `HeaderBackend` bound 不满足）。

### 1.5 验收
- `cargo test -p sunchain --bin sunchain sunlead_consensus::import_queue::tests`：**15 passed**。
- `cargo check -p sunchain` / `cargo build -p sunchain` 通过。
- **链上回归（复用既有 DB，不再 `--fresh`）**：两节点重启后 `is_active=true` → `starting SunLead
  authoring`，从 `best #2015` 续跑；slave 正常 `Prepared block for proposing` + `Imported`（即本地自检
  通过），master/slave 持续出块、`finalized #2024→#2026` 推进；错误扫描为空（无 `local self-check
  failed` / `Verification failed` / `banned` / `panic` / `equivocation`）。

## 二、P1-7.2 参数与公平性（7.2.1 / 7.2.2 / 7.2.3 已完成）
- snapshot 冻结本 epoch params（verifier 当前从可变 `ScheduleParams` 读取，应改为读 snapshot
  冻结值，避免运行期参数变更影响已签名块）。
- `enforce_feasible` 偏差上限 / 极端 stake 处理与治理可接受范围；无法给出边界应改算法或拒参。
- 1、2、最大 authority 数、零 stake、单一巨鲸、key rotation、stake reweight 的 property/fuzz
  测试与 golden vectors。

### 2.1 已完成：snapshot 冻结本 epoch params（7.2.1）
- 在 `SunLeadAuthoritySnapshot<AccountId>`（`crates/sunlead-primitives/src/lib.rs`）新增
  `params: SunLeadScheduleParams` 字段；`pallet_sunlead::build_current_snapshot` 在 epoch 边界把当时的
  `ScheduleParams` 写入 snapshot（genesis 经 `BuildGenesisConfig::build` 调用同一函数，自动携带）。
- `SunLeadApi::leader_at` / `schedule_proof` 改用 `snapshot.params`（冻结值）而非可变 `ScheduleParams`，
  使运行期诊断与节点行为一致。
- 验证路径彻底改用冻结值：`node/.../import_queue.rs::SunLeadVerifier::verify` 与 `worker.rs::aux_data`
  均从 `current_snapshot()` 取 `snapshot.params`，**不再调用 `configuration()`**。因此运行期 `ScheduleParams`
  治理变更不会影响已签名块的验证，也无法借参数变更伪造。
- 测试：新增 `verifier_uses_snapshot_params_not_configuration`（mock 让 `configuration()` 返回与 snapshot
  不同的 params，块仍以 snapshot.params 验证通过，证明冻结）；既有 15 个 import-queue 测试仍过。
- 运行时存储布局变更（snapshot 新增字段）→ **必须 `--fresh`**（清 db+frontier，保留 keystore+network）。
  链上验证：两节点复用 keystore/network 重启，fresh genesis 正常，`is_active=true`、`starting SunLead
  authoring`、从 #0 续跑；slave 正常 `Prepared block for proposing` + `Imported`（本地自检通过），
  master/slave 持续出块、`finalized` 推进（#11→#13），错误扫描为空（无 `local self-check failed` /
  `Verification failed` / `SlotOutOfRange` / `banned` / `panic` / `equivocation`）。
- 验收：`cargo test -p sunchain --bin sunchain sunlead_consensus::import_queue::tests` **16 passed**；
  `cargo test -p pallet-sunlead -p sunlead-verifier -p sunlead-primitives` 全过。

### 2.2 已完成：enforce_feasible 偏差上限 / 极端 stake 治理边界（7.2.2）
- **偏差上限（显式定义）**：`crates/sunlead-schedule/src/lib.rs::enforce_feasible` 的封顶条件是经典
  task-scheduler 可行性条件 `q_max <= max_window_slots * (others + 1)`。文档明确：
  * 最大 authority 的实际 slot 数相对"按 stake 比例（Hamilton/最大余额法）分配"的份额，**最多偏差
    `max_window_slots * (others + 1)` 个 slot**；这部分超量只会再分配给——不会从——其他 authority
    拿走（或回退到等分）。
  * 任何单一 authority 的**连续** run 长度永远不超过 `max_window_slots`。
  * 因为 `L = max_window_slots` 本身被治理上限 `MAX_WINDOW_SLOTS`（=64，见 primitives）约束，所以
    上述**偏差**与**单 authority 连续 run** 都有限、且处于治理可接受范围。若未来需要更大偏差，必须
    换一种显式有界算法或拒绝该参数，而不是放任无界参数。
- **治理可接受范围（`SunLeadScheduleParams::validate()`，`crates/sunlead-primitives/src/lib.rs`）**：
  新增 `MAX_WINDOW_SLOTS` / `MAX_EPOCH_SLOTS` 常量与 `ScheduleParamsError` 枚举，校验
  `epoch_slots >= 1`、`epoch_slots <= MAX_EPOCH_SLOTS`、`window_len >= 1`、
  `max_window_slots >= window_len`、`max_window_slots <= epoch_slots`、
  `max_window_slots <= MAX_WINDOW_SLOTS`。`v1_defaults()`（600/2/4）满足。
- **拒绝不可行参数（两处落地）**：
  * `pallet_sunlead::build_current_snapshot` 在取参后立即 `params.validate()`，不合法返回
    `InvalidScheduleParams`（新增 pallet error），从而 fail-fast，避免不可行输入深入到出块路径才 panic。
  * **修复历史 bug**：`ScheduleParams` 此前**从未被写入**（只有 Default=v1_defaults 被读取），
    `GenesisConfig.params` 字段被静默忽略。现在 `BuildGenesisConfig::build` 会先 `validate()` 再
    `ScheduleParams::put(self.params)`（字段以 `#[serde(skip)]` 默认 `v1_defaults`，不破坏既有
    chain-spec），使 genesis 真正落盘并校验参数；未来若加 `set_schedule_params`（root）治理 extrinsic
    可直接复用 `validate()`。
- **测试**：
  * `sunlead-primitives`：新增 `schedule_params_validate_accepts_defaults_and_bounds` 与
    `schedule_params_validate_rejects_each_bound`（6 条边界，含治理上限本身可取）。
  * `sunlead-schedule`：新增 `extreme_whale_is_capped_and_feasible`（2 验证人、99.9999% 巨鲸，
    验证按偏差上限封顶且 run 不超 `max_window_slots`）、`many_authorities_with_whale_is_feasible`
    （8 验证人巨鲸，同不变式）、`enforce_feasible_respects_cap_invariant`（直接验证封顶后
    `q_max <= hard*(others+1)` 且总额守恒）。既有零 stake / n=1 / 多 authority 测试已覆盖
    "零 stake → 等分、n=1 → 全归单一、多 authority 不 Infeasible"。
  * 验收：`cargo test -p sunlead-primitives`（**4 passed**）、
    `cargo test -p sunlead-schedule`（**13 passed**，含 3 新增）、pallet 编译通过。
- 运行时变更（新增 `InvalidScheduleParams` error + genesis 落盘 `ScheduleParams`）→ **必须 `--fresh`**。
  链上验证：两节点复用 keystore/network、清 db+frontier 重启，fresh genesis 正常（`is_active=true`、
  `starting SunLead authoring`）、`build()` 写入并校验 `ScheduleParams` 无 panic；slave 正常
  `Prepared block for proposing` + `Imported`（本地自检通过），master/slave 持续出块且 GRANDPA
  终局推进（#5+），错误扫描为空（无 `local self-check failed` / `Verification failed` /
  `SlotOutOfRange` / `banned` / `panic` / `equivocation` / `Infeasible`）。

### 2.3 已完成：property / fuzz 测试 + golden vectors（7.2.3）
- **Golden vectors**（`crates/sunlead-schedule/src/golden.rs`，首次运行自举并落盘
  `golden/sunlead.golden.json`，后续运行做一致性校验）。覆盖工作日志要求的 **1 / 2 / 最大
  authority 数** 等边界，共 10 个场景：
  `single`(1 验证人)、`two-equal` / `two-skewed`(2 验证人)、`equal-4` / `skewed` /
  `zero-stake`(4 验证人)、`many-16`(16)、`max-64`(**64 验证人压力，逼近
  `MAX_WINDOW_SLOTS` 量级的最大 authority 数)、`tiny-epoch`(epoch_slots=8 小周期) /
  `long-epoch`(epoch_slots=600 自定义参数)。每个场景固化 `root` + 完整 `leaders`/`windows`
  列表，供独立/ WASM 参考实现对齐。
- **Property 测试**（`crates/sunlead-schedule/src/lib.rs`）：抽出共享 `assert_schedule_invariants`
  统一校验全部不变量（coverage、leader 索引合法、`counts == quota`、`root` 可重算、run 上限、
  偏差上限），新增：
  * `property_leader_at_consistency`：`leader_at(slot)` 与 `(leaders[slot], windows[slot])`
    全 slot 一致，越界返回 `None`。
  * `property_proof_roundtrip_full_small_epoch`：小周期（16 slot）逐 slot 验证 Merkle 证明
    正确、篡改 leader 必失败。
  * `property_balanced_two_validator_always_builds`：**生产配置**（两验证人**等 stake**）在多种
    量级/seed 下恒能建表且全部不变量成立 —— 这是链上实际部署必须满足的保证。
- **Broadened fuzz**（`fuzz_schedule_invariants_broad`）：对 `1..64` 验证人数、随机 stake、以及
  **随机 schedule 参数**（epoch_slots / window_len / max_window_slots 均在治理边界内）跑 3000 组，
  断言全部不变量 + 确定性 + 抽样 Merkle 证明往返。既有 `fuzz_schedule_invariants`（v1_defaults、
  n 1..12）保留。
- **已修复：greedy 构建器死锁（原 7.2.3 期间发现的局限，现已修复）**：fuzz 暴露 `build_schedule` 在
  **极端倾斜 stake 的 2 验证人（或更一般 n）** 配置下会返回 `Infeasible`，即使
  `enforce_feasible` 已判定可行（理论可行性条件 `q_max <= hard*(others+1)` 成立）。根因：`build_sequence`
  的放置贪心在少数派 authority 被耗尽后，多数派需要连续超过 `max_window_slots` 个 slot，而贪心在
  `at_soft`（连续长度已达 `window_len`）偏好下提前严格交替、把多数派"堆积"到最后，导致最后连续段
  超过硬上限而哑火。**修复方式**：从选择键中**移除 `at_soft` 偏好**，改为经典"带冷却的任务调度"贪心——
  每一步在"未达硬上限 `hard`"的 authority 中，选**剩余配额最多**者（并列按确定性排列 rank 打破），
  仅当该 authority 连续已达 `hard` 才跳过它。该贪心在可行性条件成立时保证存在合法排布、绝不死锁；运行
  段长度只会在必要时延伸到 `hard`（治理有界的连续槽硬上限），始终合法。删除 `build_sequence` 的 `soft`
  形参（及其调用处传参）。**生产无感**：两验证人等 stake 时排布本就接近交替，行为基本一致。
- **死锁回归测试**：新增 `property_greedy_no_deadlock_skewed`（含 fuzz 当时命中的精确配置
  `(404,893,563,1,5)` 等多组极端倾斜），并**把 `fuzz_schedule_invariants_broad` 改为对每组合
  `.expect("...must succeed")`**（不再 `continue` 跳过），即所有经 `enforce_feasible` 的配置都必须建表成功。
- 验收：`cargo test -p sunlead-schedule` **18 passed**（含 golden 自举/校验、3 个 property、
  broad fuzz、死锁回归）；`cargo check -p sunchain` 通过。
  **注意：本次改动了 `sunlead-schedule` 运行期逻辑（贪心算法），canonical schedule 输出随之改变 →
  golden vectors 已同步重置；因 schedule crate 是 runtime 依赖，runtime WASM 变化 → 部署时需 `--fresh`
  （清 db+frontier，保留 keystore+network）重建链。**
  - 链上验证（`--fresh` 双节点，复用既有 keystore/network）：fresh genesis 正常（`is_active=true`、
    `starting SunLead authoring`）；两节点 peer 成功、`Prepared block` + `Imported` 持续、GRANDPA
    终局推进（master 终局 #17 / best #20，slave #17 / best #19）；四 RPC 均返回正确数据（epoch
    显示 `epoch_slots=600, window_len=2, max_window_slots=4`、2 等 stake authority、absolute 起止
    slot 推导正确；`nextLeader`/`schedule` 正确；平衡 2 验证人排布仍为 0,1,0,1… 交替，生产行为一致）；
    `:9615/:9616/metrics` 可见 `sunlead_blocks_verified_total{chain="SUN"}=8/10`、
    `sunlead_equivocations_total=0`；错误扫描为空（无 `panic`/`BadProof`/`SlotOutOfRange`/
    `Infeasible`/`equivocation`/`banned`/`Verification failed`）—— 死锁修复在运行期无回归。

## 三、P1-7.3 RPC / 指标（已完成 RPC 四方法 + 验证器侧验证/equivocation 指标）
- **RPC 四方法**（`node/src/rpc/sunlead.rs`，注册于 `node/src/rpc/mod.rs::create_full`，约束
  `C::Api: SunLeadApi`；全部 `slot` 参数为 **absolute slot**，与 verifier/leader 推导一致）：
  * `sunlead_status`：`is_active` / `has_snapshot` / `epoch_index` / `authority_set_id` /
    `schedule_root` / `best_slot`（best block 的 absolute slot，未出 SunLead 块时为 `null`）。
  * `sunlead_epoch`：快照冻结的 `epoch_slots` / `window_len` / `max_window_slots`，按 `best_slot`
    推出的当前 epoch **absolute 起止 slot**（`start = floor(best/epoch_slots)*epoch_slots`）、
    `schedule_root` / `authority_set_id`，以及完整 `(id, stake)` authority 列表。
  * `sunlead_nextLeader(from, count)`：`from` 默认 `best_slot+1`，返回 `count` 个 absolute slot
    的 `(slot, leader_index, window_id)`；无快照时返回错误。
  * `sunlead_schedule(slot)`：返回该 absolute slot 的 Merkle 证明（`root` / `leaf_index` /
    `number_of_leaves` / `proof`）及提交的 `(leader, window_id)`；越界/无快照返回错误。
- **验证器侧指标**（`node/src/sunlead_consensus/metrics.rs`，镜像 `ExchangeMetrics`，注册到节点
  prometheus registry，无需 `--fresh`）：
  * `sunlead_blocks_verified_total`：通过 pre-import 验证的 SunLead 块数。
  * `sunlead_blocks_rejected_total{reason=...}`：被拒块数，按稳定 `reason` 标签分类
    （`no_snapshot` / `snapshot_fetch` / `bad_pre_digest` / `bad_pre_digest_decode` / `bad_seal` /
    `bad_seal_decode` / `parent_hash` / `slot_monotonic` / `parent_fetch` / `no_parent` /
    `missing_pre_digest` / `dup_pre_digest` / `missing_seal` / `dup_seal` / `foreign_engine` / `other`）；
    reason 由 `reason_from_error()` 从验证错误信息折叠而成，保持低基数。
  * `sunlead_equivocations_total`：检测到的 equivocation（同一 slot 出现不同 seal → 不同冲突块）。
    验证器维护 `slot -> seal` 映射，re-import 同 seal 不重复计数，超 8192 清空。
  - 埋点位置：`SunLeadVerifier::verify`（成功 `record_verified`，失败 `record_rejected(reason)`，
    成功后按 slot 比对 seal 检测 `record_equivocation`）；`sunlead_import_queue` 用
    `config.prometheus_registry()` 构造 `SunLeadMetrics` 并传入验证器。
- **【本期未实现】剩余指标（已决定推迟，下一期再做）**：需 runtime / GRANDPA / worker 侧埋点，
  超出本期 node 侧范围：schedule build 耗时（在 `on_initialize` 内，需 runtime 侧计时）、GRANDPA
  finality lag（grandpa 已有现成指标）、空 slot 率、每 leader proposal/import 成功率
  （需 worker 侧埋点）。这些**不在 7.3 本期交付内**，仅作记录。
- 验收：`cargo check -p sunchain` 通过；`cargo test -p sunchain` **72 passed**（含新增
  `reason_from_error` / `SunLeadMetrics` 单测）。链上（运行时未变，原地重启两节点，启用
  `--prometheus-port`）：四 RPC 均返回正确数据（status/epoch/nextLeader/schedule）；
  `curl :9615/metrics` 可见 `sunlead_blocks_verified_total{chain="SUN"}=10`、
  `sunlead_equivocations_total=0`，错误扫描为空，GRANDPA 终局持续推进（#1866+）。
