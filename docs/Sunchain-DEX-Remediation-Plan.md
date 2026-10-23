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
- **S1-2 action 级事务 + nonce 语义（P0-2）**
  用 `frame_support::storage::with_transaction` 包住单条 action 的**业务**部分；nonce 写入留在事务外（业务失败也消费，防重放），业务失败 → `TransactionOutcome::Rollback`。改动 `lib.rs:1085-1128`。
  测试：撮合中途失败、`FeePool` 不足、价格失效、风险拒绝、跨 action 互不污染，断言 storage 与执行前逐项相等。
- **S1-3 canonical 排序（P0-4）**
  `SortKey` 第三分量 `arrival_index` → `action_hash`；`validate_unsigned` 校验 batch 已按 SortKey 升序、同 `(signer, subaccount)` nonce 升序，否则 `InvalidTransaction`。改动 `types.rs:502/528`、`lib.rs:1063-1070`、`lib.rs:643-676`、`node/src/exchange_pool.rs:76-83`。文档改为"段内确定性"，不再承诺 arrival 公平。
- **S1-4 weight 计量（P1-3）**
  累计 `MatchStats{matches, stale_removed, level_scans}` 三项；`WeightInfo::execute_batch` 增加 `level_scans` 维度并重跑 benchmark（现有 benchmark 只放 resting 单且用 `RawOrigin::Signed`，与 `ensure_none` 不符）；`set_liquidity_tier` 独立 weight + benchmark。改动 `lib.rs:1072/1092/1123`、`weights.rs:37/141`、`benchmarking.rs:403`、`lib.rs:982`。
- **S1-5 最小可用 receipt（P1-5 第一步）**
  新增**有界** `ActionReceipts`：`(signer, subaccount) -> BoundedVec<(nonce, status, error_code, block)>`，`on_initialize` 按 TTL 清理；receipt 带 `nonce_consumed` 字段。

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
| S2-1 | 限流窗口改**单调时钟或区块高度**（不再由收到消息数推进）；参数改为真正生效的配额；key 扩为 `(peer, signer)` 双维 + 全局字节预算 | `node/src/exchange_p2p.rs:102-108`、`:135` |
| S2-2 | 解码前做**显式大小检查**；seen 改语义 key `(signer, subaccount, nonce)` 且在**解码后**插入；垃圾包走独立 per-peer 计数，不占用 action seen set | `exchange_p2p.rs:178-197` |
| S2-3 | 显式 batch builder：按 canonical 顺序逐条尝试，逐项检查 action 数 / SCALE 字节 / `action_weight` API / 市场配额 / block 剩余预算 | `node/src/exchange_proposer.rs:91-151` |
| S2-4 | 未纳入原因的日志与 Prometheus 指标（当前 0 个 exchange 专用指标） | `exchange_proposer.rs`、`service.rs` |
| S2-5 | RPC `exchange_actionStatus(signer, subaccount, nonce)`：未入池 / 已入块 / 已拒绝 / 已回退 | `node/src/rpc/exchange.rs` |
| S2-6 | pool 持久化或重传（现在是纯内存 `BTreeMap`，重启全丢；prune 任务只删不重播） | `node/src/exchange_pool.rs:33`、`service.rs:698-743` |

**验收**：泛洪 / 畸形 SCALE / 重复 / 无效签名压力脚本下节点不出错；两名真实账户完成 maker-taker、撤单、reduce-only、funding、清算、ADL 端到端；节点重启后未执行 action 可恢复。

### S3 · 子账户完整化与发布门禁（约 2–3 周）

- **S3-1 子账户完整传递（P0-1）**：`subaccount` 显式传入 `do_place_order` / `do_cancel_order` / `account_of` / `position_of` 及 13 处热路径写、`UserOrderByClientId` 键、风险聚合、清算、费率等级、事件、Runtime API；补非零子账户端到端测试；完成后移除 S0-1 的拒绝分支。
- **S3-2 ADL 候选（P1-4）**：短期在 `pallets/liquidation` 明确 best-effort 语义（注释/文档/错误码），不声称全局公平；中期选"有界可验证扫描 + 分页游标 + 多 block 调度"。
- **S3-3 门禁**：CI 增加独立 WASM job、try-runtime CLI 冒烟、benchmark 运行（当前只编译）、`admin` / `langto_binding` 纳入 default-members；migration 在真实快照上做 pre/post。
- **S3-4 演练**：多验证人重组、网络分区、节点重启、pool 重传、清算高峰。

---

## 5. 明确不做（避免过度设计）

- commit/reveal、加密 mempool、frequent batch auction：P0-4 的长期项，当前阶段收益远低于成本，先靠 canonical 排序 + 文档收敛。
- 全局 ADL 候选索引：需新的有界索引存储与多 block 调度，先按 best-effort 落地并在 UI 明示。
- 无上界的 receipt 存储：必须先定 TTL / 有界策略，否则是新的 DoS 面。

---

## 6. 执行提醒

- S1 及之后凡改动 runtime，**dev 链必须 `--fresh` 清库重启**：否则 DB 中的旧 WASM 与节点原生 runtime 不一致，表现为喂价投票被 `BadProof` 拒绝、价格永久陈旧。
- S1-1 改变签名 payload 会同时影响 `dex-cli`；改完需重跑 deposit → place → cancel 全链路，确认没有遗漏第二处签名构造。

---

## 7. 验收清单

在进入真实资产或 Native TradeDomain 抽取前，以下项目必须全部关闭：

- [x] S0：非零 subaccount 被拒绝；all-features / try-runtime 构建通过；release 产物路径正确
- [x] S1：签名绑定 version / chain / domain / expiry，并有 golden vector
- [ ] S1：每条 batch action 具备原子回滚、明确 nonce 语义（业务失败也消费）与 receipt
- [ ] S1：排序规则可由 Runtime 验证，文档不再承诺本地 arrival 公平
- [ ] S1：batch 实际 weight 与治理遍历 weight 与 benchmark 同维度
- [ ] S2：P2P 限流不依赖收到消息数，含 peer / account 配额与大小检查
- [ ] S2：batch 按 action 数、字节、weight、市场预算显式选择，未纳入有指标
- [ ] S2：失败结果可通过 RPC 查询，并有告警与指标
- [ ] S3：非零 subaccount 被完整支持（此时移除 S0 的拒绝分支）
- [ ] S3：ADL 候选语义被明确声明（best-effort 或可验证全局前 N）
- [ ] S3：default / WASM / runtime-benchmarks / try-runtime / all-features 全部通过
- [ ] S3：migration 在真实快照上完成 pre/post 检查
- [ ] S3：多验证人、多账户、重组、分区、压力与恢复演练通过
- [ ] S3：custody、FeePool、Insurance、BadDebt 与仓位 OI 守恒式有自动化检查
- [ ] S3：外部安全审计完成，关键发现闭环
