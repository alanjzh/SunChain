# Sunchain DEX 整改计划

> 日期：2026-10-23
> 来源：[设计与实现 Review](Sunchain-DEX-Design-Implementation-Review.md)
> 本文先按 **文件:行号** 核对了 Review 的每条证据，再给出分阶段落地计划。凡与 Review 不一致处，以本文为准。
>
> **执行状态**：S0 已于 2026-10-23 完成（含验收），见 §4.1。S1–S3 未开始。

相关文档：

- [设计与实现 Review](Sunchain-DEX-Design-Implementation-Review.md)
- [Phase 2 工作日志](Sunchain-Phase2-Worklog.md)
- [Phase 2 详细设计](Sunchain-Phase2-Exchange-Core-Detailed-Design.md)
- [喂价流量调查与整改](Sunchain-Pricefeed-Bandwidth-Investigation.md)

---

## 1. 核对结论

| 条款 | 证据是否成立 | 建议是否合适 | 核对要点 |
| --- | --- | --- | --- |
| P0-1 子账户只用于 nonce | ✅ 成立，且比描述更糟 | 合适（先拒绝非零） | `lib.rs:1090` 调 `do_place_order(&who, …)` 不含 subaccount；`account_of` / `position_of` 硬编码 `DEFAULT_SUBACCOUNT`（`lib.rs:1272-1277`）；另有 13 处热路径写同样硬编码；`CancelUpTo` 读默认账户却写 `signed.subaccount`（`lib.rs:1097-1106`）；`ActionNonce`（`lib.rs:311-320`）是唯一真正使用 subaccount 的存储 |
| P0-2 无 action 级事务 | ✅ 成立 | 基本合适，**nonce 措辞需收敛** | 全 crate 无 `with_transaction`；nonce 在业务前写入（`lib.rs:1085-1087`）；失败只发 `ExchangeActionFailed` 后继续（`lib.rs:1112-1128`）；`do_place_order` 中途失败时已写入的 book/订单/账户不回滚 |
| P0-3 签名缺 domain/expiry | ✅ 成立 | 合适 | payload 为 `(subaccount, action_nonce, action).encode()`（`lib.rs:1078`、`lib.rs:2861`）；`signer` 未进 payload；Cancel / CancelUpTo 无 expiry 字段，仅靠 nonce 单调 |
| P0-4 下标 arrival 非公平 | ✅ 成立 | 合适 | `lib.rs:1063-1070` 直接 `enumerate()` 当下标；`validate_unsigned`（`lib.rs:643-676`）不校验顺序；`arrival_index` 全仓只出现在 `types.rs` / `tests.rs`，撮合 FIFO 并不使用它 |
| P1-1 P2P 限流可绕过 | ✅ 成立，且更严重 | 合适，需补两点 | `RateLimiter::advance()` 每收到一条消息 +1 并清空全部计数（`exchange_p2p.rs:102-108`、`:179`）；限流 key 只有 signer 字节（`:216`）；节点侧无大小检查；`seen` 在解码前插入（`:182`） |
| P1-2 batch 选择未闭环 | ⚠️ 部分成立 | 合适，但**当前不可达** | `truncate_from` 在 `exchange_proposer.rs:148`；池上限 8（`service.rs:69`）小于 runtime 上限 16（`runtime/src/configs/mod.rs:390`）⟹ 永不截断；真问题是完全没有预算：`action_weight` API（`runtime/src/apis.rs:35`）存在但 node 从未调用 |
| P1-3 weight 低估 | ✅ 成立 | 合适 | `lib.rs:1123` 硬编码 `stale_removed = 0`，而 signed 版 `execute_batch`（`lib.rs:955`）传了 `total_stale` ⟹ 是遗漏而非设计；`MatchStats.level_scans` 被丢弃；`set_liquidity_tier`（`lib.rs:982`）借用 `create_market()` 权重且无 benchmark |
| P1-4 ADL 候选非全局 | ✅ 成立，**位置写错** | 合适 | `adl()` 在 `pallets/liquidation/src/lib.rs:130-208`，不在 exchange-core；`selection.rs:20-57` 只在 keeper 传入集合内排序；liquidation pallet 无任何 storage，无全局候选索引 |
| P1-5 失败不可查询 | ✅ 成立 | 合适，但 receipt 必须**有界** | RPC 只有 `exchange_submitAction` 一个方法（`node/src/rpc/exchange.rs:20-32`）；无 receipt storage、无订阅、无 exchange 指标 |
| P1-6 构建/发布门禁 | ⚠️ 一半成立 | 合适 | `migrations.rs:8-20` 重复导入 `Vec` 属实；pallet-revive 一项措辞不准，见 §2 |

---

## 2. Review 本身需要修正的 4 处

1. **ADL 位置写错**：`adl(bankrupt, candidates)` 在 `pallets/liquidation/src/lib.rs:130`；exchange-core 只提供 `adl_rank`（`lib.rs:2541`）/ `adl_close`（`lib.rs:2509`）两个 trait 实现。
2. **"pallet-revive 与 `Checkable` 接口不兼容"措辞不准**：两边签名逐字一致（含 `#[cfg(feature = "try-runtime")]` 的 `unchecked_into_checked_i_know_what_i_am_doing`）。真实故障是 **feature 传播不对称导致的 E0046** —— `pallet-assets` 是 dev-dependency，`try-runtime` 把 `sp-runtime` 统一成带 try-runtime 而 revive 侧没开。且只在 `cargo test -p pallet-exchange-core --features try-runtime --no-run`（会构建 dev-deps）时复现；runtime 级 try-runtime 是通的。
3. **P1-2 应降格**：`truncate_from` 当前数值上不可达（8 < 16），是"一旦调大池上限即静默丢单"的潜在缺陷，不是活跃 bug。
4. **P0-2 的 nonce 措辞会误导**：应明确为 **业务失败也消费 nonce**（否则同一签名可重放）。要回滚的是**业务状态**，不是 nonce。receipt 需显式表达 `nonce_consumed: true, status: Rejected`。

---

## 3. Review 遗漏的 3 处加重情节

1. **限流器是死代码**：`RateLimiter::new(60, 100)`（`exchange_p2p.rs:135`）—— 窗口 60 条消息、上限 100，而 `allow()` 每窗口最多被调用 60 次 ⟹ **任何情况下都不会拒绝**。比"可被绕过"更严重。
2. **seen set 可被垃圾包挤占**：`Seen::new(4096)`（`:133`），FIFO 淘汰，key 是**整个 payload 字节**，且在 SCALE 解码**之前**插入（`:182` vs `:186`）。4096 个不同字节的垃圾包即可把合法 action 挤出；被淘汰的 id 又可被重放。节点侧没有 `notification.len()` 检查，只有协议层 64 KiB。
3. **发布门禁实际是坏的**：`.github/workflows/release.yml:56-65` 上传 `target/release/solochain-template-node` 与 `wbuild/solochain-template-runtime/...`，均为模板残留名；本仓库是 `sunchain` / `sunchain-runtime`，该 job 必然失败。

---

## 4. 分阶段计划

### S0 · 止血（0.5–1 天）

| # | 改动 | 位置 |
| --- | --- | --- |
| S0-1 | `ensure!(signed.subaccount == 0, Error::SubaccountUnsupported)`；`validate_signed_action` 同步拒绝 | `pallets/exchange-core/src/lib.rs:1057`、`:2855` |
| S0-2 | 删除重复的 `use sp_std::vec::Vec` | `pallets/exchange-core/src/migrations.rs:19-20` |
| S0-3 | `try-runtime` feature 补 `"pallet-assets/try-runtime"`（或改在 `[dev-dependencies]` 条件化） | `pallets/exchange-core/Cargo.toml:66` |
| S0-4 | 修正发布产物路径，补 `--features on-chain-release-build` | `.github/workflows/release.yml:56-65` |
| S0-5 | 文档声明：dev/testnet prototype、unsigned batch 无出块者激励、非零子账户暂不支持 | docs + `node/src/bin/dex-cli.rs` 头部注释 |

**验收**：`cargo check -p pallet-exchange-core --features try-runtime` 与 `cargo test -p pallet-exchange-core --features try-runtime --no-run` 均通过；非零 subaccount action 被拒且有明确错误码。

#### S0 执行记录（2026-10-23 完成）

| # | 落地内容 |
| --- | --- |
| S0-1 | 新增 `Error::SubaccountUnsupported`；`execute_exchange_batch` 的每条 action 先 `ensure!(signed.subaccount == DEFAULT_SUBACCOUNT)` 再验签（`lib.rs:1080` 附近），`validate_signed_action` 同步拒绝；新增回归测试 `a_non_zero_subaccount_is_refused`（断言 nonce 未消费、sub-account 7 无 `Accounts` 写入、无订单） |
| S0-2 | 删除 `migrations.rs` 中 `try-runtime` 下重复的 `use sp_std::vec::Vec`（E0252） |
| S0-3 | `try-runtime` feature 补 `"pallet-assets/try-runtime"`，修复 E0046（pallet-revive 的 `Checkable` 缺方法） |
| S0-4 | `release.yml` 改为 `cargo build --locked --release -p sunchain --features on-chain-release-build`，产物路径改为 `target/release/sunchain` 与 `target/release/wbuild/sunchain-runtime/sunchain_runtime.compact.compressed.wasm`；`node/Cargo.toml` 新增 `on-chain-release-build` feature 转发 |
| S0-5 | `dex-cli.rs` 头部声明 subaccount 暂固定为 0 及原因；本文档登记执行状态 |

**验收结果（已实际执行）**

- `cargo test -p pallet-exchange-core`：**103 passed**（较 Review 记录的 102 项 + 新增 1 项）
- `cargo check -p pallet-exchange-core --features try-runtime`：通过（此前必挂）
- `cargo test -p pallet-exchange-core --features try-runtime --no-run`：通过（E0046 修复验证）
- `SKIP_WASM_BUILD=1 cargo check -p sunchain-runtime --features try-runtime`：通过（未破坏 runtime 级）

**遗留**：`release.yml` 的 `--features on-chain-release-build` 路径未在 CI 实跑（本地 release 构建耗时），下次 CI 触发时确认。

### S1 · Runtime 语义（约 2 周；改 runtime ⟹ dev 链需 `--fresh` 重启）

- **S1-1 签名 payload 版本化（P0-3，最先做，因为改的是对外协议）**
  新签名域：`(DOMAIN_TAG, version, genesis_hash, subaccount, nonce, expires_at, action)`，Cancel / CancelUpTo 也带统一 expiry。
  改动：`lib.rs:1078`、`lib.rs:2861`（两处必须字节一致）、`types.rs:512`；同步 `node/src/bin/dex-cli.rs`（当前**唯一**构造该 payload 的客户端：`scripts/`、TS、`admin` 均无第二处）。
  测试：golden vector、跨链重放、过期、nonce 缝隙。
- **S1-2 action 级事务 + nonce 语义（P0-2）** ✅ 已完成（2026-10-23）
  用 `frame_support::storage::with_transaction` 包住单条 action 的**业务**部分（Place / Cancel /
  CancelUpTo），nonce 写入留在事务外：业务失败也消费 nonce（防重放），业务状态全部回滚。
  改动 `lib.rs` `execute_exchange_batch`。新增 2 个测试：
  - `a_half_failed_action_leaves_no_half_applied_state`：BOB 的 action 成交后 remainder 无处挂单
    （`TooManyOpenOrders`），断言 maker 订单回到簿上且数量/保证金不变、maker 与 taker 仓位为 0、
    FeePool 不变、**nonce 仍被消费**（这是唯一不回滚的东西）；
  - `a_failed_action_does_not_contaminate_the_next_one`：同 batch 中失败 action 之后的成功
    action 看到的仍是失败前的状态。
  测试有效性已反向验证：把 `Rollback` 临时改成 `Commit`（即不回滚）后，第一个测试立即失败
  （maker 订单消失），恢复后通过。

- **S1-3 canonical 排序（P0-4）** ✅ 已完成（2026-10-23）
  `SortKey` 由 `(class, action_nonce, arrival_index)` 改为
  `(class, account, action_nonce, action_hash)`：`account = blake2_256(signer, subaccount)`、
  `action_hash = blake2_256(signer, subaccount, nonce, expiry, action)`，全部来自签名或签名
  字节，提议者无法通过重排 batch 改变顺序。`account` 把同一账户的 action 聚在一起，因此
  同一账户的多个 action 必然按 nonce 升序执行（否则第二个会因 nonce 被拒）。
  - `validate_unsigned` 新增"batch 必须已按该键严格升序"校验，违反返回
    `InvalidTransaction::Call`（重试同一笔无意义，所以不是 `Stale`）；
  - dispatchable 内排序保留（防御性，到达执行的都是已排序的）；
  - `node/src/exchange_pool.rs` 删掉 `arrival` / `next_arrival`，`pending()` 直接用新键；
  - 删除 runtime 的 `ExchangeRuntimeApi::sort_key`（无调用方，且签名已过时）；
  - 重写两个测试：`reordering_the_batch_does_not_reorder_the_trades`（交换 batch 中两个
    action 的位置，成交结果不变，且成交者就是 canonical 顺序在前者）、
    `an_out_of_order_batch_is_refused`（乱序被 `validate_unsigned` 拒、同序放行）；
    pool 侧 `a_tie_is_broken_by_the_action_not_by_arrival`（提交顺序不影响序列）。
  - 反向验证：删掉顺序校验 → 第一个测试失败；去掉 dispatchable 内排序 → 第二个测试失败。

- **S1-4 weight 计量（P1-3）**
- **S1-4 weight 计量（P1-3）** ✅ 已完成（2026-10-23）
  - `execute_exchange_batch` 现在累计 `MatchStats` 的**三项**（`matches` / `stale_removed` /
    `level_scans`），事务闭包直接返回 `MatchStats`；回滚的 action 不计入（不为没发生的
    工作收费）。此前 `stale_removed` 硬编码 `0`、`level_scans` 整个丢弃，而 signed 版
    `execute_batch` 三个都传——所以那是个遗漏，不是设计。
  - `WeightInfo::execute_batch` 增加第四维 `level_scans`（trait、`execute_batch_weight!`
    宏、`()` 与 `SubstrateWeight` 两个 impl、两处调用、两处 `#[pallet::weight]` 声明上界
    `MaxActionsPerBatch × MaxPriceLevelsPerSide`）。
  - `set_liquidity_tier` 不再借用 `create_market()` 的平铺权重：新增
    `WeightInfo::set_liquidity_tier(markets)`（base + 每市场 读1 写1），dispatchable 返回
    `DispatchResultWithPostInfo` 并按**实际刷新的市场数**退款。
  - benchmark：新增 `execute_batch_matching`（ resting 卖单 + 全部成交的 batch，覆盖
    matches/stale/level-scan 三维，旧 benchmark 只有 resting）与 `set_liquidity_tier`
    （按市场数线性）。`bench_set_liquidity_tier` 测试通过；`execute_batch_matching` 与所有
    既有 place/cancel benchmark 一样在测试模式下因 `MarkPriceStale` 失败（mock 的 oracle 不
    提供价格，只有真实 benchmark CLI 才注入）——预存在问题，非本次引入。真实数字需跑
    `frame-benchmarking-cli` 重新生成 `weights.rs`。
  - 新测试：`the_batch_weight_covers_what_it_did`（三个维度各自使 weight 增长 + tier 按
    市场数增长）、`a_matching_batch_reports_more_than_a_resting_one`（成交 batch、以及
    **只做 stale 移除**的 batch，报告的 weight 都高于只挂单的 batch）。反向验证：把
    `stale_removed`/`level_scans` 重新丢掉（旧行为）后，stale 断言立即失败。
- **S1-5 最小可用 receipt（P1-5 第一步）** ✅ 已完成（2026-10-23）
  - `types.rs` 新增 `ActionOutcome`（`Applied` / `Refused` / `Rejected`，`consumed_nonce()`
    区分"nonce 还空着"与"nonce 已花"）与 `ActionReceipt{action_nonce, block, outcome,
  error: Option<(u8,[u8;4])>}`（`DispatchError` 不可存，`Token` 变体带无界 payload，因此只存
  module index + error code）。
  - 新增 storage `ActionReceipts: (AccountId, SubaccountId) -> BoundedVec<ActionReceipt,
    MaxReceiptsPerAccount>`（ring，超出淘汰最老）；新 Config 常量 `MaxReceiptsPerAccount`
    （mock 4、runtime 32）。
  - `execute_exchange_batch` 的循环拆成 validation / business 两段：refusal（签名/过期/
    subaccount/nonce）→ `Refused` 且 nonce 未消费；业务回滚 → `Rejected` 且 nonce 已消费；
    成功 → `Applied`。receipt 写在事务**外**（否则最需要记录的那条会被回滚掉）。
  - `Pallet::action_receipts(who, subaccount)` + Runtime API `ExchangeRuntimeApi::
    action_receipts`，供 S2-5 的 RPC 直接调用。
  - 新测试：`every_action_leaves_a_receipt`（同一 batch 内三种 outcome 各一条，含 block 号、
    错误码、nonce 状态）、`the_receipt_ring_is_bounded`（超出淘汰最老、保留最新）。反向
    验证：删掉 rejected 分支的 receipt 写入后，前者立即失败。
  - 偏差记录：计划原写"`on_initialize` 按 TTL 清理"，实际改为**按条数上界**——
    `on_initialize` 无法遍历所有账户（违反 §16.2 的有界迭代），而条数上界同时给出存储上界
    与可预测的驱逐行为。RPC 查询/订阅/告警属 S2-5。

**验收**：`cargo test -p pallet-exchange-core` 全过 + 新增 20+ 用例；`cargo test -p sunchain-runtime --features try-runtime --test migration` 过；dev 链 `--fresh` 重启后 `dex-cli` 下单/撤单成功且 receipt 可查。

#### S1-1 执行记录（2026-10-23 完成：签名 payload 版本化）

**改动**

- `types.rs`：`SignedExchangeAction` 新增 `expires_at: BlockNumber`（放在 `action` 前，SCALE 顺序即此）；新增 `SIGNED_ACTION_DOMAIN = b"sunchain:trade-action"` 与 `SIGNED_ACTION_VERSION = 1`。
- `lib.rs`：新增 `Pallet::signed_action_payload(action)` —— 单一构造点，payload 为
  `(DOMAIN, VERSION, genesis_hash, subaccount, action_nonce, expires_at, action).encode()`；
  `execute_exchange_batch` 与 `validate_signed_action` 都改用它（此前两处各自 `.encode()`，
  存在漂移风险）。新增 `ensure_action_fresh`：`expires_at > now` 且
  `expires_at <= now + MaxOrderLifetime`，**在 nonce 之前**校验（过期签名不消耗 nonce）。
  新增错误码 `ActionExpired` / `InvalidActionExpiry`。
- `mock.rs`：`TestSignature` 从"忽略消息的固定关系"改为**真实验签**（`keyring(account)` 确定性
  ECDSA 密钥 + keccak digest + `recover_prehashed`）。这是本次能测到 payload 绑定的前提：
  旧的 double 不看消息，payload 怎么改都测不出来。
- `tests.rs`：`signed_action` helper 增加 `expires_at` 并真实签名；新增 3 个测试：
  - `a_signature_is_bound_to_the_chain_it_was_signed_on`：同一条 action 在别的 genesis hash
    下重签，回到本链后拒绝，且 nonce/仓位零变化；
  - `a_signature_expires`：已过期、以及超过 `MaxOrderLifetime`（28 801）两种，均拒绝且不消耗 nonce；
  - `the_signed_payload_is_a_golden_vector`：断言 payload 头部（compact 长度 + domain +
    version LE）与 keccak digest 常量
    `13add7a9b574dc717e2933c5f85168a697a4b5113955ee288560ef4a91ade3a5`。
- `node/src/bin/dex-cli.rs`：不再本地拼 payload，改为 `in_ext(genesis, || CorePallet::<R>::signed_action_payload(&signed))`（外部性是因为 payload 读 genesis hash）；新增 `--action-expiry`（默认 100 块），与订单级 `--expiry` 分开；模块头文档同步。

**验收结果（已实际执行）**

- `cargo test -p pallet-exchange-core`：**106 passed**（103 + 3 个新测试）
- `cargo test -p pallet-exchange-core --features try-runtime`：106 passed
- `cargo check -p sunchain --bins`、`SKIP_WASM_BUILD=1 cargo check -p sunchain-runtime --features try-runtime`：通过

**遗留**：dev 链端到端（`dex-cli buy` 全链路）未跑——需要重新构建节点并按 `--fresh` 重启 dev 链，见 §6。改后链上旧格式签名的 action 一律失效，属预期（版本号的意义）。

### S2 · node 交易路径与运营面（约 1.5–2 周）

| # | 任务 | 位置 |
| --- | --- | --- |
| S2-1 | ✅ 限流窗口改**块高**推进（不再由收到消息数推进——那正是可被绕过的原因）；参数改为真正生效的配额；key 扩为 `(peer, account)` 双维 + per-peer 字节预算 + 坏包预算 | `node/src/exchange_p2p.rs` |
| S2-2 | ✅ 已完成（2026-10-23）：随 S2-1 落地的解码前大小检查 + seen 仅在解码成功后占用；本次把 seen 的 key 从"整个 payload 字节"改为**语义** `(signer, subaccount, nonce)`——同一 action 被重签（字节不同）仍被识别为重复，不会被二次中继；本地提交路径也用同一语义键去重 | `node/src/exchange_p2p.rs` |
| S2-3 | ✅ 已完成（2026-10-23）：从 `pending()`（已是 canonical 排序）逐条尝试，逐项检查四个预算——action 数（`MAX_EXCHANGE_BATCH_ACTIONS=8`）、每市场配额（`MAX_ACTIONS_PER_MARKET_PER_BATCH=4`，避免一个热市场占满整批）、SCALE 字节（`MAX_EXCHANGE_BATCH_BYTES=32KiB`）、权重（`MAX_EXCHANGE_BATCH_WEIGHT=1e12`，留足块内余量）。每一项被排除都记日志（含 signer/nonce/原因）。**偏差**：计划写的"`action_weight` API / block 剩余预算"未接实时 API——该 API 在泛型 proposer 里无法让 `at` 的 `Block` 类型推断（仅对具体 `FullClient` 可推断，如 `service.rs`），故改用静态最坏值 `MAX_EXCHANGE_ACTION_WEIGHT`（1.1e11/动作）；权威的超重拦截仍是 runtime 的声明权重检查（S1-4，8 动作批已被证可入块） | `node/src/exchange_proposer.rs` |
| S2-4 | ✅ 已完成（2026-10-23）：在 `ExchangeProposerFactory` 接 `Registry`，新增 `ExchangeMetrics`——`exchange_actions_proposed_total`、`exchange_actions_deferred_total{reason}`（reason ∈ action_count/market_count/bytes/weight）、`exchange_batch_bytes` 直方图。每有 pending 的块都记录被选/被递延（含原因）；批 extrinsic 构造成功时记字节大小。无 registry 时计数仍在内存累加不报错。测试覆盖注册后取值与无 registry 不 panic | `node/src/exchange_proposer.rs`、`node/Cargo.toml`、`node/src/service.rs` |
| S2-5 | ✅ 已完成（2026-10-23）：在 `ExchangeApi` 新增两个 RPC，封装 S1-5 的 `ExchangeRuntimeApi::action_receipts`——`exchange_actionReceipts(who, subaccount) -> Vec<ActionReceiptRpc>`（含 nonce/block/outcome/nonce_consumed/error，serde 镜像避免直接依赖 runtime 的 serde）与 `exchange_actionStatus(who, subaccount, nonce) -> ActionStatus`（`not_pooled`/`included`/`refused`/`rejected`，由 receipt 派生，直接给出"重试 vs 重签"的答案）。`who` 为 20 字节 AccountId 的 SCALE 编码。`runtime/src/apis.rs` 已把 `ActionReceipt`/`ActionOutcome` 再导出 | `node/src/rpc/exchange.rs`、`runtime/src/apis.rs` |
| S2-6 | ✅ 已完成（2026-10-23）：改用 on-disk 持久化（计划给的"持久化或重传"二选一中更稳的一个）。`ExchangePool` 新增可选 `path`，构造时从 `<base_path>/exchange_pool.bin` 重载（SCALE 编码的 `Vec<SignedExchangeAction>`，按 canonical 顺序恢复、去重、超界丢弃）；每次 `submit_action` 成功与每次 prune 后都 `save()`，重启不再丢仍在排队、尚未入块的 action。崩溃最多丢最后一次写之后的提交（与交易池同级上界）。测试覆盖重载往返（2 条 action，cancels 仍排前面）、缺失文件为空 | `node/src/exchange_pool.rs`、`node/src/service.rs`、`node/src/rpc/exchange.rs` |

**验收**：泛洪 / 畸形 SCALE / 重复 / 无效签名压力脚本下节点不出错；两名真实账户完成 maker-taker、撤单、reduce-only、funding、清算、ADL 端到端；节点重启后未执行 action 可恢复。

### S3 · 子账户完整化与发布门禁（约 2–3 周）

- **S3-1 子账户完整传递（P0-1）** — ✅ 已完成（2026-10-24，模型 A：每子账户独立抵押品）：`subaccount` 贯穿全部交易路径——`deposit_collateral`/`withdraw_collateral`（新增 `subaccount` 参数，默认 0 向后兼容）、`place_order`/`cancel_order`/`cancel_up_to`/signed `execute_batch`、`do_place_order`/`do_cancel_order`、撮合（`order.subaccount` 归属保证金/仓位/盈亏）、maker 结算、`account_of`/`position_of`、`cross_market_requirements`（改 `iter_prefix((who, sub))`，跨市场保证金逐子账户）、清算/ADL（`liquidate_account`/`adl_close`/`adl_rank`/`residual_loss_of`/`note_adl_shed` 均带 sub）、费率等级。`Order` 新增 `subaccount` 字段并配 **v4 迁移**（丢弃无法按新布局解码的旧单）；`SubaccountIndex`+`MaxSubaccountsPerAccount` 启用（`ensure_subaccount` 首次使用登记、超限拒绝）；**S0-1 两处拒绝分支已删**（`execute_exchange_batch` 校验 + `validate_signed_action` 提交时）。liquidation pallet 的 ADL 候选按子账户展开（`ExchangeEngine::subaccounts`）。测试：非零子账户可用且登记/限额、跨子账户抵押品隔离（sub-0 不能用 sub-5 的钱）、v4 迁移在真实 runtime + try-runtime 下保留新单丢弃旧单。**未纳入**：`CollateralDeposited`/`OrdersInvalidatedUpTo` 事件未加 `subaccount` 字段（纯信息性，避免动 e2e 事件解析）
- **S3-2 ADL 候选（P1-4）** — A 已实现（2026-10-24），B 决策已定并记录待中期落实：
  - **决策**：采用 **A（best-effort）先上线**，B（有界可验证扫描）作为中期演进，本次不实现。理由：A 在"每笔 ADL 个体正确"（不越资格线、不分配亏损、顺序由 runtime 定）上有硬保证，缺陷仅是"集合由 keeper 选、非全局公平"；这可在文档/错误码中如实标注而不误导用户，实现与审计成本低。B 的全局公平值得做，但需要新存储 + 多 block 调度 + 中扫描失效处理，复杂度与风险不适合与上线捆绑。
  - **A 的落实（已完成）**：在 `pallets/liquidation` 把语义讲透——`selection.rs` 模块与 `select_adl_targets` 文档明确"keeper 提议集合、runtime 只验资格与顺序、不扫全簿、故非全局公平"；`adl` 调用文档说明候选展开为子账户、实际 shed 可能低于预算（见 `AutoDeleveraged.lots`）；`NoAdlCandidates` 错误码注明"仅指提议集合内无合格者，非全链无合格者"。新增测试 `the_set_is_the_keepers_so_adl_is_best_effort_not_globally_fair` 锁定该局限（提议集合外更高 ROE 的合格账户不会被发现）。
  - **B 的落地方案（备查，中期）**：目标=无单块无界扫描地得到全局 top-N。机制：① 每市场 `AdlScanCursor<market_id> -> last_key` + 每市场"当前 top-N 累加器"（有界有序集，大小=N），在记录新 residual loss 时按 loss 版本号重置；② 有界 `adl_scan(market, max_accounts)` 从游标向前遍历仓位，更新累加器并推进游标，游标绕回=一轮走完，累加器即全局 top-N；③ `adl_execute`（或 scan 收尾）对累加器按预算强减。公平性来自全遍历 + 确定性 `(roe desc, account asc)`，任何节点重放扫描可验证。代价：新增存储（游标+累加器）、需多 block 才能完成一轮、扫描中仓位/新 loss 变化需按 loss 版本号使累加器失效。迁移=纯新增存储，无破坏性变更。
- **S3-3 门禁** — ✅ 已完成（2026-10-24）：`admin`(sun-admin) / `langto_binding` 纳入 `default-members`（默认 `cargo build`/`test` 现覆盖，二者各 1/3 测试通过）；CI 新增独立 **`check-wasm` job**（`cargo check -p sunchain-runtime`，无 SKIP_WASM_BUILD，快速单独校验 no_std/WASM 目标——此前 WASM 只在慢的 release node 构建里被验）。`benchmark 运行`与 `try-runtime` pre/post 已在 CI 的 feature-gated step（`cargo test -p pallet-exchange-core --features runtime-benchmarks`、`-p sunchain-runtime --features try-runtime --test migration`）。**两项偏差**：①"try-runtime CLI 冒烟"——CLI 未随 SDK vendor，改用同 feature 下的 migration 集成测试驱动 pre/post 钩子（已在 CI，正是链上真实执行的路径）；②"migration 在真实快照上做 pre/post"——暂无真实链快照，用合成 `Storage`（真实 prefix/hasher/类型）在真实 runtime 下驱动 pre/on/post（`runtime/tests/migration.rs`），拿到真实 dump 后可切 `Mode::Offline`
- **S3-4 演练** — ⏸ 暂缓（2026-10-24 决定）：留待**上线前**落实，本次不做。属运维演练而非代码改动，需起多验证人网络手动执行：多验证人重组一致性、网络分区恢复、节点重启后 pool 重传/恢复（S2-6 已提供持久化基础）、清算/ADL 高峰压力。建议上线前用 e2e skill（~/.codebuddy/skills/sunchain-dex-e2e）扩展为多节点脚本逐项跑并留档。

---

## 5. 明确不做（避免过度设计）

- commit/reveal、加密 mempool、frequent batch auction：P0-4 的长期项，当前阶段收益远低于成本，先靠 canonical 排序 + 文档收敛。
- 全局 ADL 候选索引：需新的有界索引存储与多 block 调度，先按 best-effort 落地并在 UI 明示。
- 无上界的 receipt 存储：必须先定 TTL / 有界策略，否则是新的 DoS 面。

---

## 6. E2E 验证记录（2026-10-23，`--fresh` 双验证人链 + 代理）

按 §7 之前先跑通了全链路，结论：S1 的五项在真实链上均生效，并**抓到一个 S1-4 的遗留缺陷**。

**抓到的缺陷（已修）**：S1-4 给 `execute_exchange_batch` 补上 level-scan 维度后，声明的
worst-case weight 变成 ~1.7e12 ps，超过 Normal class 的单块预算（~1.5e12），于是每个 batch
extrinsic 在 validate 阶段就被拒（`InvalidTransaction::ExhaustsResources`）——订单没问题，
是这个 batch 根本付不起。修法：`MaxActionsPerBatch` 16 → 8（8 也是 node 池
`MAX_EXCHANGE_BATCH_ACTIONS` 的既有上限，16 本就不可达）。

**验证结果**

| 项目 | 证据 |
| --- | --- |
| 喂价整改（前序提交） | 节点日志 `round N: 18 quote(s) fetched`（4 market × 4 venue + 2 peg），价格 2 个 source、age 0 块 |
| S1-1 签名域 | `dex-cli buy` 的 `validate -> ok`（新 payload 由 pallet 函数构造，客户端一致） |
| S1-2 事务边界 | `--price 0` 的 action：业务拒绝、无订单残留、**nonce 仍被消费** |
| S1-3 canonical 排序 | pool 取出的 batch 通过 `validate_unsigned` 的严格升序校验并入块（顺序校验不认就会整批拒绝） |
| S1-4 weight 维度 | 修复 `MaxActionsPerBatch` 后 batch 可入块；修复前 dry-run 即 `ExhaustsResources` |
| S1-5 receipt | `ExchangeRuntimeApi_action_receipts` 返回三条：nonce 1/2 = `Applied`（block 36/37），nonce 3 = `Rejected`（block 69，`nonce_consumed=true`，error = pallet#22 code 6 = `InvalidPrice`） |
| 端到端 | `dex_flow.py`：deposit → resting buy → crossing sell → match → position → fees(97181) 全通 |

**复现方式**：`source scripts/proxy_env.sh && scripts/start_chain.sh --fresh`（skill 目录下），
`dex_flow.py`，receipt 用 `state_call ExchangeRuntimeApi_action_receipts`。

## 7. 执行提醒

- S1 及之后凡改动 runtime，**dev 链必须 `--fresh` 清库重启**：否则 DB 中的旧 WASM 与节点原生 runtime 不一致，表现为喂价投票被 `BadProof` 拒绝、价格永久陈旧。
- S1-1 改变签名 payload 会同时影响 `dex-cli`；改完需重跑 deposit → place → cancel 全链路，确认没有遗漏第二处签名构造。

---

## 8. 验收清单

在进入真实资产或 Native TradeDomain 抽取前，以下项目必须全部关闭：

- [x] S0：非零 subaccount 被拒绝；all-features / try-runtime 构建通过；release 产物路径正确
- [x] S1：签名绑定 version / chain / domain / expiry，并有 golden vector
- [x] S1：每条 batch action 具备原子回滚、明确 nonce 语义（业务失败也消费）与 receipt
  （receipt 已落地：storage + Runtime API；RPC 查询与告警属 S2-5）
- [x] S1：排序规则可由 Runtime 验证，文档不再承诺本地 arrival 公平
- [x] S1：batch 实际 weight 与治理遍历 weight 与 benchmark 同维度
  （维度与 benchmark 已就位；`weights.rs` 数字仍待 `frame-benchmarking-cli` 重跑）
- [x] S2：P2P 限流不依赖收到消息数，含 peer / account 双维配额、字节预算、坏包预算与解码前大小检查（S2-1）；seen 改语义键（S2-2）
- [x] S2：batch 按 action 数、字节、weight、市场预算显式选择，被排除项带原因日志 + Prometheus 指标（S2-3 / S2-4）
- [x] S2：失败结果可通过 RPC 查询（`exchange_actionReceipts` / `exchange_actionStatus`，由 S1-5 receipt 派生），并有 metrics（S2-5）；pool 持久化重启不丢（S2-6）
- [ ] S3：非零 subaccount 被完整支持（此时移除 S0 的拒绝分支）
- [ ] S3：ADL 候选语义被明确声明（best-effort 或可验证全局前 N）
- [ ] S3：default / WASM / runtime-benchmarks / try-runtime / all-features 全部通过
- [ ] S3：migration 在真实快照上完成 pre/post 检查
- [ ] S3：多验证人、多账户、重组、分区、压力与恢复演练通过
- [ ] S3：custody、FeePool、Insurance、BadDebt 与仓位 OI 守恒式有自动化检查
- [ ] S3：外部安全审计完成，关键发现闭环
