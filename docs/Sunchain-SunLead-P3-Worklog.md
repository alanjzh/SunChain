# SunChain SunLead P3 — Node 接入（出块 / 导入管线）工作日志

> 日期：2026-10-27
> 阶段：P3（节点侧把确定性 leader 接入出块与导入管线，按 `is_active` 与 BABE 并行；BABE 仍是默认出块）
> 前置：P1（crates/sunlead-primitives、sunlead-schedule、sunlead-verifier）、P2（pallet-sunlead + SunLeadApi + chain-spec）已完成并构建通过。

---

## 0. 目标

在节点侧把 SunLead 的"确定性 leader"能力接到真实出块与导入管线，但**默认仍是 BABE**，仅当 runtime 的 `is_active()` 为 `true` 时切换到 SunLead：

1. 新增节点模块 `node/src/sunlead_consensus/`（block_import / import_queue / worker），实现与 BABE 平行的 SunLead 出块 + 导入。
2. SunLead 的 block import **绕开 BABE**：内层是 `FrontierBlockImport<Block, FullGrandpa, FullClient>`（只跑 EVM frontier + GRANDPA 终局性），不消费 BABE pre-digest。
3. 导入队列用 `SunLeadVerifier`（复用 `sunlead-verifier` 的 `verify_pre_digest` / `verify_seal` / `verify_parent_hash`），从区块头派生 `SunLeadScheduleContext` 并查 runtime 的 `current_snapshot` / `configuration` 来校验。
4. `service.rs` 在启动时读 `is_active`（genesis block 0）选择导入队列；Authority 角色下按该标志在 `start_sunlead` 与 `start_babe` 之间二选一。

P3 不实现 slash 接入 `pallet-offences`、不做动态 handoff 的运行时切换——那是 P4/P5。

---

## 1. 落地的代码改动

### 1.1 新增 `node/src/sunlead_consensus/`
- `mod.rs`：导出 `SunLeadBlockImport`、`SunLeadParams`、`start_sunlead`、`sunlead_import_queue`。
- `block_import.rs`：`SunLeadBlockImport<B, I>`（`Clone`）。`import_block` 先 `find_sunlead_pre_digest(header)` 校验确实存在 SunLead pre-digest，再委托内层 `I`（Frontier→GRANDPA）以 `StateAction::Apply` 导入，并在 `aux` 里回传 `SunLeadAuxData`（供 worker 不需要但满足 trait）。
- `import_queue.rs`：`sunlead_import_queue(...)` 构造 `BasicQueue`，`Verifier = SunLeadVerifier`。`SunLeadVerifier::verify`：
  - 取 `parent_header`、从 runtime `api.current_snapshot(parent_hash)` / `api.configuration(parent_hash)` 取快照与参数；
  - `verify_pre_digest` → `verify_seal`（`genesis_hash`、`pre_seal_hash`、pre-digest、context、seal）→ `verify_parent_hash`；
  - 复用 `sunlead-verifier`，整条校验链路与导入路径完全一致。
- `worker.rs`：`SunLeadSlotWorker<B, C, E, I, SO, L, BS>` 实现 `sc_consensus_slots::SimpleSlotWorker`：
  - `claim_slot`：调 `sunlead_schedule::build_schedule(ScheduleInput{ authorities, chain_id, authority_set_id, epoch_index, epoch_seed, params })` 得到 `ComputedSchedule`，用 slot 取 leader；若本节点持有该 leader 的 `sunl`（SR25519）密钥 → 对 `SunLeadPreDigestBody` 签名生成 `SunLeadPreDigest` 并返回 `Some(claim)`，否则 `None`（不出块）。
  - `pre_digest_data`：把 `SunLeadPreDigest` 包成 `DigestItem::PreRuntime(SUNLEAD_ENGINE_ID, encoded)`。
  - `aux_data`：从 runtime 取 `current_snapshot` + `configuration` 组装 `SunLeadAuxData`。
  - `start_sunlead(...)`：与 `sc_consensus_babe::start_babe` 平行，spawn slot worker + backoff strategy。

### 1.2 `node/src/service.rs`
- 导入 `start_sunlead` / `sunlead_import_queue` / `SunLeadBlockImport` / `SunLeadParams`、`ProvideRuntimeApi`、`SunLeadApi`。
- 类型别名：`SunLeadEffectiveImport = FrontierBlockImport<Block, FullGrandpa, FullClient>`、`FullSunLeadBlockImport = SunLeadBlockImport<Block, SunLeadEffectiveImport>`。
- `new_partial`：
  - `sunlead_block_import = SunLeadBlockImport::new(frontier_block_import.clone())`；
  - 读 `genesis_hash = client.block_hash(0)`，经 `SunLeadApi::is_active(&*api, genesis_hash)` 得 `use_sunlead`；
  - 构造 `sunlead_import_queue(...)`（复用 `justification_import.clone()`）；
  - `let import_queue = if use_sunlead { sunlead_import_queue } else { import_queue };`（覆盖 BABE 导入队列）；
  - `other` 元组追加 `(use_sunlead, sunlead_block_import)`。
- `new_full_base`：Authority 角色下 `if use_sunlead { start_sunlead(...) } else { start_babe(...) }`，SunLead 分支用 `sunlead_block_import` 与同一个 `ExchangeProposerFactory`。

### 1.3 `node/src/main.rs`
- 增加 `mod sunlead_consensus;`（bin 与 lib 共享同一份源码，模块表必须镜像，否则 bin target 编译时 `crate::sunlead_consensus` 解析不到）。

---

## 2. 关键设计决策

- **绕开 BABE 的导入链**：SunLead 不生产 BABE pre-digest，因此 block import 内层必须是 `FrontierBlockImport<Block, FullGrandpa, FullClient>`（跳过 `FullBabe`）。这样 SunLead 出块仍走 frontier EVM 状态机 + GRANDPA 终局性，只把"谁有权出块 + 签名"换成 SunLead 的规则。
- **静态门控**：节点在启动时读 `is_active`（genesis block 0）一次性决定用哪套导入队列 / 哪个出块器。与 BABE 平行，互不影响既有 BABE/GRANDPA 网络。
- **签名与派生**：`claim_slot` 用 `sunl`（SR25519）对 `SunLeadPreDigestBody` 签名；leader 由 `build_schedule`（与 runtime `pallet-sunlead` 同款算法）在节点侧按 `current_snapshot` 本地派生，再与 runtime 验证一致。

---

## 3. 踩坑 / 修正（本轮编译期）

1. **`SimpleSlotWorker` 关联类型**：`BlockImport` 必须是 `SunLeadBlockImport<B, I>` 而不是裸 `I`；`Error = ConsensusError`。
2. **`Environment` / `Proposer` 的错误类型**：照搬 BABE 的泛型 `Error: std::error::Error + Send + From<ConsensusError> + From<I::Error>`，`CreateProposer` 固定为 `ConsensusError`，否则 `E::Proposer::Error`（= `sp_blockchain::Error`）与 `SimpleSlotWorker` 要求的 `ConsensusError` 对不上。
3. **runtime API 调用约定**：`at` 参数按**值**传（`Block::Hash`）；`ProvideRuntimeApi::runtime_api(client.as_ref())` 的类型推断需要显式 UFCS 形式（`ProvideRuntimeApi::runtime_api(client.as_ref())` + `SunLeadApi::is_active(&*api, genesis_hash)`），直接 `.runtime_api().is_active(...)` 会因 `Block` 推断失败报 E0282。
4. **导入别名**：`H256` 取自 `sp_core`（不是 `sp_runtime` 的私有 re-export）；codec crate 是 `scale_codec`（不是 `parity_scale_codec`）；`.encode()` 需要 `Encode` trait 在作用域。
5. **`env.init(block)` 是 Future**：对其 `map_err` 需要 `futures::future::TryFutureExt`（BABE 同款）。
6. **密钥比较的 `AsRef` 歧义**：`SunLeadId` 与 `sr25519::Public` 都有多个 `AsRef` 实现；改为 `sr25519::Public::try_from(leader_id.as_ref())` 后再与 `keystore.sr25519_public_keys(KEY_TYPE)` 比较，绕开歧义。
7. **`justification_import` 双 move**：BABE 导入队列与 SunLead 导入队列都要它，给 BABE 队列那一处 `.clone()`。
8. **模块声明镜像**：bin target 的 `main.rs` 必须和 `lib.rs` 一样声明 `mod sunlead_consensus;`。
9. **`BlockT` bound**：`SunLeadParams` 与 `SunLeadSlotWorker` 的 `B` 需 `BlockT`（因 `SunLeadBlockImport<B, I>` 要求）。
10. **`JustificationImport` dyn**：`BasicQueue::new` 要求 `Box<dyn JustificationImport<Block, Error = ConsensusError> + Send + Sync>`。

---

## 4. 验证结果

| 检查 | 结果 |
|------|------|
| `cargo check -p sunchain`（lib + bin `sunchain`） | 通过（exit 0，仅 pre-existing warnings） |
| `cargo check -p sunchain-runtime`（P2 已验证，含 WASM） | 通过 |
| `cargo check -p pallet-sunlead` / `crates/sunlead-*` | 通过（P1/P2 已验证） |

---

## 5. 已知问题 / 重要限制

- **激活门槛（影响 e2e 测试）**：节点侧门控在**启动时读 genesis block 0 的 `is_active`**。而 runtime 侧 `SunLeadActive` 是 `ValueQuery`（默认 `false`），且**只能经 Root 调用 `pallet_sunlead::set_sunlead_active(true)` 翻转**。因此：
  - 若只想在运行时用 sudo 翻转，**节点不会切换**——因为节点启动时就选定了 BABE 导入队列，且读的是 genesis（重启也仍是 `false`）。
  - 要真正以 SunLead 出块做 e2e 测试，必须让 **genesis 时 `SunLeadActive = true`**。当前 pallet 没有 `GenesisConfig` 注入该值，需在 `pallet-sunlead` 加 `GenesisConfig`/`build` 或在 dev chain-spec 预置；或把门控改成读 finalized head 的动态门控 + 重启。
  - `set_sunlead_active` 的 pallet 注释也写明"实际 handoff 到 SunLead 出块是 P5 迁移"。P3 仅把管线与门控接好；是否真正接管出块取决于 `is_active`。
- `runtime_integrity_tests` 失败是 pre-existing（staking `BondingDuration=1` 与 `SlashDeferDuration=1` 违反不变量），与 SunLead 无关。

---

## 6. 下一步建议

1. ~~给 `pallet-sunlead` 增加 `GenesisConfig`（或 dev chain-spec flag）预置 `SunLeadActive = true`~~ **已完成（Phase 0）**：`pallet-sunlead` 新增 `GenesisConfig { sunlead_active }`，`dev` chain-spec 置 `true` 并把权威集扩为 Alice+Bob；主网 preset 不动（默认 `false`）。同时 `build()` 里额外建了 epoch-0 快照，修复 genesis 区块不跑 `on_initialize` 导致的首块死锁。详见 `Sunchain-SunLead-P3-TestPlan.md` 的 Phase 0。
2. 实施 `Sunchain-SunLead-P3-TestPlan.md` 中的 2 节点验证流程，覆盖：runtime API 正确性、SunLead 出块 + 双节点同步 + GRANDPA 终局性、回退到 BABE。
3. P4：接 `pallet-offences` 实现 slash；P5：真正的共识 handoff（含动态切换）。
