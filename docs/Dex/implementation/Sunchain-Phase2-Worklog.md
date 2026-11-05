# Sunchain Phase 2 工作日志

每个落地步骤都必须在此登记，包含：改了什么、为什么、验证结果、遗留项。目的是避免"做过但没记、记了但没做"。

相关文档：

- [Phase 1 详细设计](Sunchain-Phase1-Exchange-Core-Detailed-Design.md)
- [Phase 2 详细设计](Sunchain-Phase2-Exchange-Core-Detailed-Design.md)
- [设计讨论稿（基线）](Sunchain-Orderbook-DEX-Design-Discussion.md)

---

## 2026-09-15 · 准备：抽出 `exchange-primitives`

**背景**：Phase 2 需要 `PriceOracle` / `InsuranceFund` / `ExchangeEngine` 三个跨 Pallet trait。放在消费方 Pallet 会导致依赖方向混乱（`pallet-oracle` 反向依赖 `pallet-exchange-core`）。

**改动**

- 新建 `pallets/exchange-primitives`（仅依赖 `sp-runtime` / `sp-std`）：
  - `types.rs`：共享标量（`MarketId` / `PriceTicks` / `Lots` / `QuoteAmount` / `SignedQuote` / `FeePpm` / `MarginPpm` / `PPM_DENOM`）
  - `traits.rs`：`PriceOracle`、`InsuranceFund`、`ExchangeEngine`、`NoOracle`
- `pallet-oracle`：删除本地 `traits.rs`，标量类型改为复用公共 crate
- `pallet-exchange-core`：`types.rs` 改为 `pub use exchange_primitives::{...}`（`crate::types::*` 保持不变）
- workspace `Cargo.toml`：members + workspace dependency

**验证**：`cargo test -p exchange-primitives -p pallet-oracle -p pallet-exchange-core` 全过；`cargo check -p sunchain-runtime` 通过。

**遗留**：`ExchangeEngine` 只有定义，`pallet-liquidation` 尚未实现消费方（P2-B）。

---

## 2026-09-15 · P2-A 步骤 2：把 `PriceOracle` 接进 exchange-core

**改动**

- `pallet-exchange-core::Config` 新增 `type PriceOracle: PriceOracle<MarketId>`
- 新增 `mark_price_of(market_id, market)` 取代 `fresh_mark_price(&market)`：oracle 优先，取不到才回退到 Root 设置的缓存价（Phase 1 行为）
- mock 与 runtime 均使用 `NoOracle`，行为与 Phase 1 完全一致

**为什么这样做**：允许 P2-A 增量落地——`pallet-oracle` 注册进 `construct_runtime!` 之前，核心不受影响。注册后回退分支即成死代码，应与 `set_mark_price` 一并删除（代码注释已写明）。

**语义要点**：oracle 无新鲜价 → `is_fresh() == false` → 既不允许扩大风险，**也不允许清算**。这是讨论稿 §9「禁止用最后成交价触发清算 / 预言机超时进受限状态」的落点。

**验证**：`pallet-exchange-core` 52 个原有测试零回归；runtime 编译通过。

---

## 2026-09-15 · P2-A 步骤 3、4：funding 机制

**改动**

- 新建 `pallets/exchange-core/src/funding.rs`（8 个纯函数 + 8 个单测），全部对应 dYdX：
  `premium_from_impact` / `summarize_samples`（去尾取均值）/ `clamp_rate_ppm` / `funding_index_delta`（按时间比例）/ `funding_owed` / `apply_net_oi`
- `Market` 新增 `net_oi_lots` / `funding_index` / `last_funding_at` / `rate_ppm`
- `Position` 新增 `funding_index`
- 新 storage `PremiumSamples`（有界，每 tick 清空）
- `on_initialize` 驱动两级 epoch；迭代按 `MaxMarkets` 封顶
- `impact_price` + `current_premium_ppm`：dYdX 分段式
- 成交时把 maker / taker 仓位标记到当前 funding index（惰性结算前提）
- Config 常量（mock + runtime）：sample 60 / tick 1200 / rate period 9600 / 去尾 5% / clamp 0.6× / impact notional 500 USDC

**验证**：60 个测试通过（52 原有 + 8 funding）；runtime 编译通过。

**重要发现（已回写文档）**：funding 天然零和，`Σ size ≡ 0`，所以文档 §6.3 的"净持仓不平衡残值"**恒为 0**。真正的残值是**取整粉尘**。文档已更正为 §6.3「取整残值与守恒」。

**遗留（已登记到文档 §6.5）**

1. premium 的 votes 层 → 依赖 P2-D 的 proposer 注入
2. `on_initialize` 权重是占位值 → 待 benchmark
3. funding 收费 → 必须与 P2-B 的 `realized_pnl` 折入同批上线
4. `Position.funding_index` 改变 SCALE 编码 → 需 migration

---

## 2026-09-15 · P2-A 步骤 6：benchmark（第二轮）

**前置**：给 `on_initialize`（funding tick 边界）补了 benchmark——此前 `WeightInfo::on_initialize` 只有占位值。构造方式：填满 `PremiumSamples`，把块高设到 `FundingTickPeriod`，使 sample 与 tick 落在同一块（最坏情况）。

**运行**：`./target/release/sunchain benchmark pallet --pallet pallet_exchange_core --extrinsic '*' --steps 50 --repeat 20`

**实测（ref_time / PoV / reads / writes）。注意单位是微秒：1 ps = 10⁻¹² s。**

| | ref_time (µs) | PoV (B) | r/w |
|---|---:|---:|---:|
| `deposit_collateral` | 172.9 | 6 160 | 6/6 |
| `withdraw_collateral` | 213.2 | 6 160 | 7/5 |
| `place_order` 挂新档 base / 每档扫描 | 257.0 / 17.9 | 3 636 / 2 584 | 7+l / 9 |
| `place_order` 挂已有档 | 259.0 | 3 636 | 8/9 |
| `place_order` 撮合 base / 每笔成交 | 98.3 / 126.3 | 3 636 / 2 621 | 7+3m / 6+4m |
| `place_order` 清失效单 base / 每笔 | 278.1 / 77.9 | 6 158 / 2 621 | 10+s / 10+2s |
| `cancel_order` | 144.3 | 3 636 | 6/6 |
| `cancel_up_to` | 74.3 | 3 562 | 2/2 |
| `execute_batch` base / 每 action | 86.2 / 110.4 | 3 636 / 2 584 | 5+2a / 5+3a |
| `prune_orders` base / 每笔 | 35.7 / 131.8 | 3 636 / 2 621 | 4+2r / 3+3r |
| `create_market` | 63.0 | 3 636 | 3/3 |
| `set_market_status` / `set_mark_price` / `set_risk_params` | 54.6 / 41.6 / 44.7 | 3 636 | 1/1 |
| **`on_initialize`（funding tick）** | **144.9** | 6 282 | 3/2 |

**量级校验**（验证这些数字不会把区块撑爆）：单次 `place_order` 的声明最坏值

- ref_time ≈ 278 + 64×126 + 64×78 + 1024×17.9 ≈ 31.7 ms，加 DB 读写（1 290 读 ×25 µs + 394 写 ×100 µs ≈ 71.5 ms）≈ **103 ms** → 1 s 预算下约 9 笔/块（最坏情况）
- PoV ≈ 6 158 + 64×2 621 + 64×2 621 + 1024×2 584 ≈ **2.85 MiB** → 3 MiB 预算下仅能放 1 笔最坏情况

也就是说：典型挂单（0 成交、0 扫描）实测 ~0.3 ms / 6 KB，一块可放数百笔；真正扫满 1024 档的极端单会占掉几乎整个 PoV 预算。这是"声明权重按最坏值预收"的必然结果，与 §11.2 的设计一致。

`place_order` / `execute_batch` 仍按"base 取各 benchmark 零变量最大值 + 各维度 slope"合成（见 `weights.rs` 顶部注释）。

**重要观察：同一台笔记本、两次运行相差约 2 倍。**（`place_order` 每笔成交：09-14 为 64.5 ms，09-15 为 126.3 ms。）原因是这台机器共享且会降频。这恰恰印证了设计文档 §11.1 的要求：**权重必须在目标硬件上重测**，笔记本上的数字只能用于开发期验证量级。

**验证**：`cargo test -p pallet-exchange-core` 60 通过；`cargo check -p sunchain-runtime` 通过。

---

## 2026-09-15 · P2-B 步骤 1：`settle_position`（realized PnL 折入 + funding 收费）

**这是 P2-B 整批中不可拆分的第一步**（§2.4 / §7），必须与后续 migration 一起上线。

**为什么做**：Phase 1 的 `Account.collateral` 从不因亏损而减少，亏损只记在 `Position::realized_pnl` 里。其后果是两方对敲后，亏损方仍可按名义 `collateral` 提款，多空双方合计可提取的金额超过 custody 实际持有额 —— 这就是 Phase 1 §2.4 的 **H3 硬约束**，也是 Phase 1 禁止承载真实资产的根本原因。

**改动**

- `settlement::settle_position(account, position, funding_index) -> Result<QuoteAmount, MathError>`（纯函数，6 个单测）：
  1. 按 `size × (market_index − position_index)` 结算 funding，更新 `position.funding_index`
  2. 把 `realized_pnl` 折入 `Account::collateral` 并清零
  3. 两者先净额相抵；不足部分**返回给调用方**（不清零了事）
- `risk::MarketParams` 新增 `funding_index`
- 新 storage `BadDebt`（`ValueQuery`）：无法收回的亏损。funding 是零和的，赢家一定会被记账，所以这笔缺口必须显式登记，否则 §4.3 守恒式会被破坏
- `Pallet::settle_party(...)` 包装：结算 + 把缺口记入 `BadDebt`
- 调用点（惰性结算，每次触碰仓位前）：`withdraw_collateral`（**在余额检查之前**）、`do_place_order` 入口（taker）、maker 成交前
- `internal_liabilities()` 纳入 `BadDebt`（此时恒等式为 `custody + BadDebt == Σ collateral + FeePool`）；新增 `bad_debt()`

**关键实现细节**：`withdraw_collateral` 里持久化仓位要判断"是否真的需要结算"（`!flat || realized_pnl != 0 || funding_index != market.funding_index`）。第一版写成了"结算后 `!flat || realized != 0` 才写"，结果结算把这两个字段都清零后反而**没有写回**，旧值留在 storage 里 —— 已修正。

**验证**：68 个测试通过（含 2 个新的集成测试）

- `realised_loss_reduces_withdrawable_collateral`：直接验证 H3 —— 亏损后提现，可提金额被扣减，且二次结算不会重复扣
- `funding_index_move_is_charged_zero_sum_on_next_touch`：多头付 10 000、空头收 10 000，断言**净额为零**（只有两次提现离开 custody）

**遗留（必须随本批上线）**

1. **migration**：`collateral` 语义变了，且 `Position.funding_index` 改变 SCALE 编码。任何已持有 Phase 1 状态的网络必须先迁移
2. `BadDebt` 目前是**记账占位**，必须由 `pallet-insurance`（步骤 2）接管并清零
3. 结算还需接入清算 / ADL 路径（步骤 3、5）
4. `settle_party` 目前只在三个入口调用；后续新增任何触碰仓位的路径都必须先调用它

---

## 2026-09-15 · P2-B：storage migration（v0 → v1）

**为什么做**：步骤 1 给 `Position` 加了 `funding_index`。SCALE 没有字段分隔符，旧值 32 字节、新值 48 字节 —— 任何已持有 Phase 1 状态的网络升级后**所有仓位都会解码失败**。这是 P2-B 整批能否上线的门闩。

**改动**

- `pallets/exchange-core/src/migrations.rs`（新）：`v1::Migration`
- 用 `Positions::translate::<PositionV0, _>` 逐条补 `funding_index`
- **种子取市场当前的 `funding_index`，不是 `0`**：Phase 1 从没收过 funding，取 0 会让每个老仓位被追缴升级前所有 epoch 的费用
- `STORAGE_VERSION = 1` + `#[pallet::storage_version]`（pallet 此前从未声明过版本，所以链上是 0）
- runtime `type Migrations` 用 `VersionedMigration<0, 1, …>` 包一层，版本不匹配时自动 no-op
- `try-runtime` pre/post 钩子：校验条数不变，且每条仓位都对齐到市场 index
- `realized_pnl` **故意不在迁移时折入**：它按惰性结算在下次触碰时折入，而所有能动钱的路径都已先结算，留着它不可被利用

**两个坑**

1. 本版 SDK 的 `VersionedMigration` 要求 `Inner: UncheckedOnRuntimeUpgrade`，**不是** `OnRuntimeUpgrade`，而且没有 blanket impl。写成后者会报 `Executive` 的 trait bound 不满足，报错位置指向 `apis.rs::execute_block`，极易误判成 executive 的问题
2. `translate` 用旧类型解码；把新格式的值当 `PositionV0` 解会**静默成功**并产出垃圾（尾部 16 字节不被消费）。只能靠 storage version 门禁，不能指望解码失败兜底

**验证**：`migration_v1_adds_funding_index_seeded_from_market` 手工写入 32 字节旧编码（`put_raw`，用 `put` 会被加上 compact 长度前缀）再迁移，断言字段不丢且种子来自市场

---

## 2026-09-15 · P2-B：`pallet-insurance`

**为什么做**：步骤 1 把收不回来的亏损记进了 `BadDebt`，但那只是记账。§8.4 要求基金在损失被社会化（ADL）之前先兜底。

**改动**

- 新 crate `pallets/insurance`，实现 primitives 里**已存在**的 `InsuranceFund` trait（`cover` / `credit` / `available`），没有另造接口
- 资金放在自己的 pot 账户，币种与交易核心同资产；`cover` 就是把钱转到 custody —— 补的正是 custody 替赢家垫出去的那笔
- `cover` 是**全有全无**：失败即"基金耗尽"，调用方转 ADL。部分支付会被静默误处理
- exchange-core `Config` 新增 `type Insurance`；`settle_party` 先问基金，基金失败才记 `BadDebt`
- runtime 索引 23；mock 接的是**真 pallet**，不是桩

**踩到的坑：`PalletId` 截断碰撞**

mock 的 `AccountId = u64`，`into_account_truncating` 只保留派生输入的前 8 字节；`b"sun/exc"` 与 `b"sun/ins"` 前 4 字节相同 → **保险 pot 和交易 custody 是同一个账户**，基金等于付给自己（测试里表现为 `cover` 后 `available` 纹丝不动）。已把 insurance 的 id 改成 `b"insfund0"` 并加注释说明。生产环境 32 字节 AccountId 不会触发，但**以后换任何 PalletId 都值得确认前缀不撞**。

**一个诚实的结论：兜底路径目前还走不到**

短差意味着结算把账户扣到 0，而账户自己发起的任何 extrinsic 之后都会因余额不足失败并整体回滚 —— 结算也随之回滚。所以 `unpaid > 0 → cover` 这条链在**当前**（无清算）代码里不可达；真正的触发点是 P2-B 步骤 3 的清算引擎，它强制平仓，不受债务人自己保证金检查的约束。

这一步先把接线和资金流正确性验证到位 —— 测试断言 `cover` 后 custody **真的增加**、`available` **真的减少**、超额报 `FundExhausted`；可达性留给清算时再补端到端用例。

**验证**：71 测试通过；runtime 编译通过

---

## 2026-09-15 · P2-B：清算引擎（`ExchangeEngine` 落地）

**关键设计选择：清算复用 `do_place_order`。**

`do_liquidate_account` 不另写一套撮合，而是构造一个 **reduce-only IOC** 订单、限价取 `mark ± MaxLiquidationSlippagePpm` 的价格带，然后走 `do_place_order`。收益是清算**自动继承**了结算、风控（`allows_fill` 允许减仓即使已水下）、maker 再检查、fee、持久化 —— 不存在第二套会漂移的实现。市场状态门禁也是白拿的（`do_place_order` 已拒绝 CancelOnly / Paused）。

**改动**

- `risk::liquidation_lots`：§8.2 的部分清算量公式 `q ≥ (N×npl×MMR + B − E) / (a − c + npl×MMR)`，除法向上取整（多平一点是保守方向）；分母 ≤ 0 或 q ≥ N 时全额平仓
- `liquidation_price_bound`：`Sell`（平多）取 `mark × (1 − s)` 向下，`Buy`（平空）取 `mark × (1 + s)` 向上
- `impl ExchangeEngine`：`liquidate_account` + `adl_close`
- 罚金按成交名义值计，扣被清算账户，然后 `InsuranceFund::credit`（§8.4）。**转账失败不能回滚清算本身**，只把 `insurance_delta` 记为 0
- Config 新增 5 个常量（含 `MaintenanceBufferPpm`、`LiquidationPenaltyPpm`、`MaxLiquidationSlippagePpm`）

**过程中发现的一个真问题：破产账户走不了订单簿**

`settle` 把账户扣到 0 之后，`simulate_fill` 里 `collateral.checked_sub(fee)` 必然下溢 → 订单被拒 → **破产账户永远清算不掉，会卡死**。按 §8.5 处理：结算后若 `equity ≤ 0` 就**不进订单簿**，直接按 mark 价抵销平仓，结算产生的缺口交给保险基金。

这一改同时让上一步"还走不到"的兜底路径**真正可达**了 —— 新增 `liquidation_routes_uncollectable_settlement_to_the_insurance_fund` 直接验证：多头欠付 funding → 清算时结算扣光 → 保险基金支付 → `BadDebt` 仍为 0。

**验证**：74 测试通过

**未完成**

- `pallets/liquidation` crate 本身（permissionless 入口、keeper 返佣下发、weight 计量）还没建，目前 `ExchangeEngine` 只被测试直接调用
- ADL 的**候选选择**（§8.5 的 keeper 提案 + 按 roe 排序）属于步骤 5，未做；`adl_close` 目前只是"按给定价格强制抵销"这一半
- 清算目前按 `place_order` 的上界扣 block budget，偏保守，待 benchmark 校准

---

## 2026-09-15 · P2-B：`pallets/liquidation`（permissionless 入口）

**定位：这个 pallet 不持有任何风险逻辑。** 它只是薄薄的、可公开调用的门面 —— 转发给 `ExchangeEngine`、付 keeper 返佣、出审计事件。所有保证金 / PnL / 撮合决策仍然只在 `pallet-exchange-core` 里，那是唯一能动用户 collateral 的地方。依赖方向严格单向。

**改动**

- `pallets/liquidation`：`liquidate(origin, who, market_id, lots)`，permissionless
- `ExchangeEngine` 新增 `pay_keeper`：把累计返佣从 custody 付给 keeper，**上限是历次清算实际计提的金额**（`KeeperRebatePool`），所以 keeper 永远拿不到别人的 collateral
- 罚金在清算时全额从被清算账户扣除；保险份额立刻划走，keeper 份额先记入 `KeeperRebatePool` 等 keeper 来领
- runtime 索引 24；mock 里把 `LiquidationKeeperRebatePpm` 设成非 0（1_000 ppm），这样返佣路径**真的被测到**，而不是配成 0 让代码空跑

**踩到的坑**

`construct_runtime!` 加了第 24 个 pallet 后触发宏递归上限（`recursion limit reached while expanding panic_2021`）。已在 runtime 顶部加 `#![recursion_limit = "512"]`。报错位置完全不提 pallet 数量，容易误判。

**验证**：75 测试通过；`keeper_can_liquidate_and_is_paid_the_rebate` 断言无关第三方（ALICE）能提交清算、仓位真的缩小、且 keeper 的 USDC 余额增加

---

## 2026-09-15 · P2-B 步骤 4：ADL 候选的确定性排序

**为什么需要它**：对所有持仓排序是无界操作，不能上链。§8.5 的方案是 keeper 提案 + Runtime 验证 —— 但"提案"意味着 keeper 能挑人。所以安全性的关键只有一条：**执行顺序不看 keeper 的提交顺序**。

**改动**

- primitives：`AdlRank { size_lots, unrealized_pnl, roe_ppm }` + `ExchangeEngine::adl_rank`（只读）。纯内存传递，不进 storage，所以不需要 SCALE 表示
- exchange-core：`adl_rank` 复用 `RiskView` 的 `unrealized_pnl` / `equity`，不引入第二套算法；`equity ≤ 0` 时 roe 记 0（没有权益的账户不该比"盈利少"的账户更优先）
- `pallets/liquidation/src/selection.rs`：`select_adl_targets`，**纯函数** + 6 个单测
  - 过滤：反向持仓、有浮盈、`roe ≥ MinAdlRoePpm`
  - 排序 `(roe desc, account asc)`；tie-break 用 `AccountId` 的 SCALE 编码，因为 `AccountId` 不保证实现 `Ord`
  - 截断到 `MaxAdlMatchesPerCall`

**验证**：6 个单测。其中 `submission_order_never_changes_the_result` 直接锁死"提交顺序不影响结果"这条不变式 —— 这是整个方案成立的前提，值得单独一个测试。

**未接线**：`select_adl_targets` 目前没有调用方。`adl` extrinsic 属于步骤 5，见下。

---

## 2026-09-15 · P2-B 步骤 5：ADL 落地（§8.5.1 决议）

决议要点：**ADL 只减敞口，不分损失。** 已写入设计 §8.5.1，实现严格照此。

**改动**

- **`adl_execution_price`（§1）**：在被 ADL 方**有利**的方向偏移 `AdlPriceBandPpm`。多头被减仓（卖出）付得更多 → `mark + band`；空头被减仓（买入）付得更少 → `mark − band`。band **向上取整**，所以账户只会比调整后 mark 更好，绝不会更差
- **`ResidualLossOf`（§2）**：`BadDebt` 是守恒式需要的全局数；这张表是审计轨迹 —— 记的是**谁的**缺口、来自市场的哪一侧，也是 ADL 选人时用来判方向的值
- **`adl` extrinsic（§3）**：
  - 以 `residual_loss_of(bankrupt) > 0` 为闸门 —— ADL **只**响应"抵押品 + 清算 + 保险基金都没盖住"的损失，不会投机触发
  - 候选经 `selection::select_adl_targets` 排序，**不采用 keeper 的提交顺序**
  - **不修改 `ResidualLossOf`** —— 只有保险基金的 `cover` 或治理能减少它
- 新配置 `MinAdlRoePpm`、`AdlPriceBandPpm`

**过程中发现的坑**

`do_adl_close` 会调三次 `settle_party`，最后一次仓位已平 → `size_lots.signum() == 0` 把 `side_sign` 覆盖成 0，ADL 就选不出人了。改成**只在仓位非 0 时更新方向**，保留最后一次已知方向。

**验证**：77 + 6 测试。`adl_execution_price_always_favours_the_adld_account` 直接锁死 §1；`uncovered_loss_is_recorded_as_residual_loss` 断言保险基金为空时被清算 → 缺口显式入账、`BadDebt` 与之一致、且 ADL 以此为闸门。

**已知简化**

- `adl` 目前**整平**被选中账户的全部仓位（受 `MaxLiquidationsPerCall` 限个数），没有做按缺口量成比例的部分减仓。真要上线前需要重新斟酌
- 端到端的"ADL 真的平掉对手方"用例还没写：mock 里盈利方的 roe 达不到 `MinAdlRoePpm = 50_000`，需要一个薄资本的反向盈利账户。已列入待办

---

## 2026-09-16 · 修 `try-runtime` 编译（部分打通）

**根因不是代码错，是 feature 没传下去。**

一类很坑的失败模式：`sp_runtime` / `pallet_session` 等**声明方**的方法挂在 `#[cfg(feature = "try-runtime")]` 下，**实现方**（frontier 的 `UncheckedExtrinsic`、`pallet-staking`、`pallet-bags-list`）也在同样的 cfg 下实现。一旦只有声明方开了 feature，trait 多出一个方法、impl 里没有 —— 编译器报的是 **E0046「missing trait item」**，指向一个跟 try-runtime 完全无关的第三方文件，极难联想到 feature 漏配。

**修法**：在 `runtime/Cargo.toml` 的 `try-runtime` 段补齐实现方的 feature。`fp-self-contained`、`pallet-staking`、`pallet-bags-list`、`pallet-babe`、`pallet-session`、`pallet-im-online`、`pallet-offences`、`pallet-ethereum` / `pallet-evm` 等 —— 凡是直接依赖、且会实现某个带 cfg 方法的 trait 的，都要列上。

关键提示：**只加 `[dependencies]` 里已存在的 crate**，否则 cargo 直接报 feature 不存在。

**验证**：`SKIP_WASM_BUILD=1 cargo check -p sunchain-runtime --features try-runtime` 通过。

## ⚠️ 仍未打通：没有 CLI

`try-runtime-cli`（`substrate/utils/frame/try-runtime/cli`）**在这个 vendor 的 SDK 里根本不存在** —— 整个 `third_party` 下只有一个 `substrate/frame/try-runtime`。node 的 `try-runtime` feature 也只列了 `sunchain-runtime/try-runtime`，没有任何 CLI 依赖。

也就是说：**现在能编译，但没有可执行入口**，`try-runtime on-runtime-upgrade` 这类命令没法敲。要真正跑起来得从 crates.io 引 `try-runtime-cli`，而版本能否对上这个 SDK 是未知数 —— 这一步风险不小，先停在这里。

**当前的替代验证**：migration 的 pre/post 钩子和迁移逻辑本身由单测覆盖（`migration_v1_adds_funding_index_seeded_from_market` 手工写入 32 字节旧编码再迁移）。这能证明**逻辑**正确，但证明不了**在真实快照上**行为正确 —— 后者仍然只有 try-runtime CLI 能做。

---

## 2026-09-16 · 用本地快照替代 try-runtime CLI（**进行中，未闭环**）

**新增 `runtime/tests/migration.rs`**：在**真实 runtime** 上跑迁移（真实 pallet 前缀、真实 hasher、真实 `Position` 类型）—— 这是 pallet 内单测拿不到的那部分保证。写入 Phase 1 的 32 字节旧编码，然后驱动 pre / on / post 钩子。

**`frame-remote-externalities` 试过了，走不通**：它 `TestExternalities::from_raw_snapshot` 会**校验 state root**，所以手工构造的快照必须给出逐位正确的 trie root。我用 `sp_trie::LayoutV1::trie_root` 算过，快照能加载（`✅ Loaded snapshot`）但随后 trie 迭代仍报 root 非法。对合成状态复现 root 太脆，性价比不划算 —— 结论记在测试文件头注释里：将来若有**真实链上 dump**，直接用 `Mode::Offline` 载入即可。

**⚠️ 查到底了 —— 是我 migration 代码里的真 bug，不是脚手架问题**

`post_upgrade: Other("position count changed during migration")` 的真凶：

**`pre_upgrade` 用 `Positions::<T>::iter().count()` 计数，而 `iter()` 会按*当前*类型解码值。** 迁移前存储的是 32 字节旧编码，按新的 48 字节 `Position` 解码必然失败 → **被静默跳过** → `pre_upgrade` 永远数到 0；迁移后值已改写为新格式 → `post_upgrade` 数到 2。

也就是说：**这个钩子在真实链上跑必然失败**，而且失败方式恰好伪装成"迁移丢了数据"。在此之前它从未被执行过一次，所以一直没暴露。

**修法**：`pre_upgrade` / `post_upgrade` 都改用 `iter_keys().count()` —— 只数 key，不解码值。计数不可解码的条目本来就是 `pre_upgrade` 的意义所在，所以必须走 key-only 迭代。

**排查过程中的两个弯路**（都记在测试文件注释里，避免后人重踩）：

1. 先怀疑 `sp_io::storage::next_key` 只遍历 backend、不遍历 overlay（`put_raw` + `commit_all` 确实有这个坑）。改成用 `Storage` 直接构造 backend 后**仍然失败** —— 说明另有原因
2. 决定性的一步是打印 `get_raw(alice_key) = Some(32)` 而 `iter().count() = 0`：按键可读、前缀迭代读不到 → 只可能是**解码失败被跳过**，而不是数据不在

**验证**：`SKIP_WASM_BUILD=1 cargo test -p sunchain-runtime --features try-runtime --test migration` 通过；pre/post 钩子真正执行，count 校验与逐条 `funding_index` 校验都通过。

**给自己的教训**：`pre_upgrade` 里永远不要用带类型的迭代器去数**迁移前**的数据。

---

## 2026-09-16 · 清算 weight（**未完成，半途**）

**修掉的真问题：清算按 `place_order` 上界扣 block budget。**

原来无论实际成交多少，都按 `MaxMatchesPerAction / MaxPriceLevelsPerSide` 的最大值扣。清算是 permissionless 且**成批到达**的，全都按上限扣会迅速耗尽区块预算，然后把**后续的清算挡在门外** —— 正是这个预算本该防止的活性故障。改成**成交后按真实 stats 扣**：

```rust
let stats = Self::do_place_order(who, request)?;
Self::consume_block_budget(T::WeightInfo::place_order(
    stats.matches, stats.stale_removed, stats.level_scans,
))?;
```

破产分支（不进订单簿）按新增的 `WeightInfo::adl_close()` 扣。

**新增 benchmark**（`liquidate_account` 走订单簿路径、`adl_close` 走抵销路径），并加了 `impl_benchmark_test_suite!` —— **这条很关键**：它把每个 benchmark 当成普通测试跑，setup 写错会立刻失败，而不是在一次很长的 CLI run 里才以 `BenchmarkError::Stop` 的形式冒出来。

**踩到的坑（已修）**：benchmark setup 里用 `Markets::insert(bench_market::<T>())` 改标记价，**整个替换了 Market**，把 `best_bid` / `best_ask` / 档位计数全清了 → 盘口深度消失 → `LiquidationSlippageExceeded`。改成 `Markets::mutate` 只改价格字段。

**⚠️ 修好了，并顺带挖出 5 个早就坏掉的 benchmark**

`bench_liquidate_account` 的 `fund()` 报 `Stop("mint")`，打出真实错误是 **`UnknownAsset`** ——  collateral 资产在那些 benchmark 上下文里根本不存在。

排查时做了件关键的事：单独跑**已存在**的 `bench_place_order_matches`，**同样失败**。所以这不是我新写的 case 的问题。列出全部结果后，失败的是：

```
place_order_matches / place_order_rest_new_level / place_order_stale_removal
execute_batch / prune_orders
```

共同点：都要给**多个**账户注资。`fund()` 走 `deposit_collateral` → `Assets::mint_into`，而 benchmark 的若干入口并没有把资产建出来。

**修法**：加 `setup_account()`，直接写内部的 `Accounts` 存储，绕开资产 pallet。风控引擎只读 `Accounts`，所以任何不真正搬钱的 benchmark 都够用；`deposit_collateral` / `withdraw_collateral` 这两个真要搬钱的保持原样。

**结果：17 个 benchmark 全部通过（94 tests）。** 也就是说这 5 个是**早就坏的**（在 CLI 里跑一样会挂），之前只是没人跑过 —— 正是新加的 `impl_benchmark_test_suite!` 把它们照出来了。

**仍未做：真实测量。** 现在 setup 都通了，可以跑：

```
cargo run --release --features runtime-benchmarks -- benchmark pallet \
  --pallet pallet_exchange_core --extrinsic '*' --steps 50 --repeat 20
```

注意 `pallets/liquidation` 至今**没有 WeightInfo**，两个 extrinsic 硬编码 `500_000_000`，也没注册进 `runtime/src/benchmarks.rs`。

---

## 2026-09-16 · 补 CI：把默认关闭的 feature 纳入流水线

`.github/workflows/ci.yml` 新增 **Run the feature-gated tests** 步骤（在 `build` job 内，复用已装好的依赖）：

```bash
SKIP_WASM_BUILD=1 cargo test -p sunchain-runtime --features try-runtime --test migration
SKIP_WASM_BUILD=1 cargo test -p pallet-exchange-core --features runtime-benchmarks
SKIP_WASM_BUILD=1 cargo test -p pallet-liquidation
```

三条均已本地验证通过（1 / 94 / 6）。

**为什么必须加**：这两个 feature 默认都不开，而本轮和上一轮挖出的两个缺陷正是靠默认绿灯潜伏的 ——

- `pre_upgrade` 用迁移后的类型去数迁移前的值，永远报 0，在真实链上不可能通过；
- `pallet-exchange-core` 里 5 个 benchmark 直接跑不通。

默认的 `cargo test` 全绿对这两件事**一个字都不告诉你**。

**刻意没加的一条**：`cargo test -p pallet-exchange-core --features try-runtime`。单独构建该 pallet 会在第三方 `pallet-revive` 里失败 —— 它同样把 `Checkable::unchecked_into_checked_i_know_what_i_am_doing` 实现在自己的 `try-runtime` cfg 下，而它不是我们的直接依赖，无法从 runtime 传播 feature。runtime 级那条才是真正执行钩子的，够用了。原因已写在 CI 注释里。

**顺带修的**：`run-node` job 引用了模板遗留的 `solochain-template-node`（本仓库 node crate 叫 `sunchain`），改成正确的包名和二进制名 —— 那个 job 之前必然是红的。

**一个遗留风险**：clippy 步骤带 `--locked`。本轮改了多个 `Cargo.toml`，`Cargo.lock` 已随之更新（`git status` 确认已 M），提交时需一并带上，否则 CI 会因 lock 文件不一致失败。

---

## 2026-09-16 · `pallets/liquidation` 的 WeightInfo

新增 `pallets/liquidation/src/weights.rs`。设计要点：**不另立一套魔法数字，而是从 engine 的实测分量推导** —— 这个 pallet 本身几乎不干活（转发给 `ExchangeEngine` + 付 keeper + 发事件），所以

```rust
liquidate() = engine.place_order(MaxMatches, MaxMatches, MaxPriceLevels) + 2R/2W
adl(n)     = n * (engine.adl_close() + 1R/1W) + 2R/1W
```

这样只有一个数据源：engine 的 benchmark 在目标硬件上重跑后，这里的数自动跟着变。Config 新增 `type WeightInfo`，两个 extrinsic 从硬编码的 `500_000_000` 换成 `T::WeightInfo::…`。

**踩到的坑：dev-dep 循环会编出两个 crate 实例**

为了让 weights 引用 engine 的实测值，把 `pallet-exchange-core` 从 dev-dep 提升为正式依赖 —— 结果 exchange-core 的 mock 编译不过：`Test` 只实现了**其中一个**实例的 `Config`。因为 `core --dev--> liquidation --normal--> core` 这个环让 cargo 编出了两份 `pallet-exchange-core`。

解法：给 `()` 也实现 `WeightInfo`（固定常量），**mock 用 `()`，runtime 用 `SubstrateWeight<Runtime>`**。runtime 里只有一份实例，泛型约束正常满足。

**关于"注册进 benchmarks"：没做，且我认为不该做**

试过加 `[pallet_liquidation, Liquidation]`，编译报 `Benchmarking is not implemented`。要有就得自带 benchmarking 模块，但它的两个调用**没法独立构造**：造一个水下账户要读写 `pallet-exchange-core` 的 storage，而本 pallet 是对 `ExchangeEngine` 泛型、不是对 core 的 `Config` 泛型，够不着。

而且收益很低 —— 单独测它基本是在重测 engine。真正要测的工作已经由 engine 的 `liquidate_account` / `adl_close` 覆盖。已在 `runtime/src/benchmarks.rs` 写明原因，免得后人再试一遍。

**验证**：77 + 6 + 94 + 1（try-runtime migration）全通过；runtime 常规与 `runtime-benchmarks` 均编译通过。

---

## 2026-09-16 · WASM 构建修好 + 清算/ADL 实测权重（**已完成**）

**WASM 构建两个根因**：

1. mermaid 那个 `include_str!` 报错其实**早就被修过了**（Sep 10 加的 `docs` symlink）。我在 shell 里验证路径能解析才确认，没有白改第三方代码
2. **真正的残留原因是我自己的 bug**：`pallets/liquidation/src/selection.rs` 用了 `Vec` 但 no_std 下没导入 —— std 构建靠 prelude 蒙混过关，只有编 WASM 才暴露。修了之后又冒出 `cannot find macro println`（benchmarking.rs 里我遗留的诊断打印），一并清掉

**实测结果**（与文件里其他数字同一台机器，同样不是参考硬件）：

| | ref time | proof | reads | writes |
|---|---|---|---|---|
| `adl_close()` | 35_750_000 | 3_636 | 3 | 2 |
| `liquidate_account(m)` | 98_006_966 + 60_432_745·m | 3_636 + 2_621·m | 10 + 3m | 7 + 4m |

命令（关键在 `--runtime` 指向**带 runtime-benchmarks 的** blob，否则报 `Did not find the benchmarking runtime api`）：

```
cargo build -p sunchain-runtime --release --features runtime-benchmarks
./target/release/sunchain benchmark pallet \
  --runtime target/release/wbuild/sunchain-runtime/target/wasm32v1-none/release/sunchain_runtime.wasm \
  --pallet pallet_exchange_core --extrinsic 'liquidate_account,adl_close' \
  --steps 50 --repeat 20
```

**block budget 的演进**（同一个问题修了两次才算完）：

1. 最初按 `place_order` 的**配置上界**扣 → 成批清算会耗尽预算、把后续清算挡在门外
2. 改成按真实 stats 扣，但仍借用 `place_order` 的曲线
3. 现在用清算自己的实测曲线 `liquidate_account(stats.matches)`，维度是实际成交的 maker 数 —— 部分清算不再按全量付费

**验证**：77 + 6 + 94 + 1 全通过；runtime 编译 OK。commit `9fae24784`。

**WASM 能编之后还该做的**：CI 里 `cargo build`（不带 `SKIP_WASM_BUILD`）之前大概率是红的，现在应能绿 —— 值得确认一次。

---

## 2026-09-16 · 已 commit；真实 benchmark 仍卡在 WASM 构建（已被上一节解决）

**commit `4a6b3db0a`**：34 个文件，仅含本次 P2-B 的改动。`third_party/` 与 `vendor/` 下那一大批 0 行 diff 的模式变更、以及 `node/src/*`、`rust-toolchain.toml`、`docker_build/*` **没有提交** —— 那不是这次的工作，且我无法确认其意图。

**跑 benchmark 的进展**：

```
SKIP_WASM_BUILD=1 cargo build --release --package sunchain --features runtime-benchmarks
→ Finished release in 3m59s（成功）
```

但 CLI 起不来新 benchmark：

```
$ ./target/release/sunchain benchmark pallet --list --pallet pallet_exchange_core
Loading WASM from state
... 列出 15 个，没有 liquidate_account / adl_close
```

**根因**：benchmark CLI 跑的是 **WASM runtime**，而它取的是 genesis 里那份 blob（日志 `Loading WASM from state`，并提示 chain spec 方式已废弃）。那份 blob 是旧的，所以看不到新加的 benchmark。绕开的正确姿势是 `--runtime <blob>` 指向**新编出的** wasm —— 而 WASM 构建正是本项目一开始那个已知故障：

```
couldn't read third_party/polkadot-sdk/substrate/primitives/runtime/src/generic/../../docs/mermaid/extrinsics.mmd
```

之前所有 `cargo check/test` 都是靠 `SKIP_WASM_BUILD=1` 绕过去的，所以这个坑一直没被真正处理。

**下一步**：先把 WASM 构建修好（缺的是 `docs/mermaid/extrinsics.mmd`，`simple_mermaid::mermaid!` 在 `include_str!` 它），然后：

```
./target/release/sunchain benchmark pallet --runtime <新 blob> \
  --pallet pallet_exchange_core --extrinsic 'liquidate_account,adl_close' \
  --steps 50 --repeat 20
```

在此之前 `weights.rs` 的 `adl_close()` 仍是占位值。

---

## 2026-09-16 · `cargo build` 确认通过 + ADL 部分减仓

**`cargo build`（不带 `SKIP_WASM_BUILD`）：通过**（dev profile，9m06s，0 error）。

此前整个项目一直活在这个旁路里 —— 意味着 CI 的 `build` job 第一条就是红的，只是一直没人注意。WASM 构建修好后它自然变绿。

**ADL 部分减仓**（commit `35cb9e93f`）

原来每个被选中的账户都被**整平** —— 对被 ADL 方过重，而且"平多少"完全没有依据。

新规则，显式可审计：**削减的敞口名义值累计达到未覆盖损失即停**。

```text
wanted_lots = ceil(remaining_loss / notional_per_lot)
lots        = min(wanted_lots, position_size)
```

必须强调：这**不是**把损失转嫁给这些账户（§8.5.1 明确 ADL 不分损失，`ResidualLoss` 也不会因此减少）。它只是一条"削减多少敞口"的公开规则 —— 之前没有任何规则，现在是这条。

为此给 `AdlRank` 补了 `notional_per_lot`（单手在 mark 价下的名义值）：调用方知道要削减多少 quote，但 `adl_close` 收的是手数，需要换算。

**验证**：77 + 6 + 94 + 1 全通过，runtime 编译 OK。

---

## 2026-09-16 · 端到端 ADL 用例（P2-B 收尾）

新增 `adl_partially_reduces_a_profitable_opposite_account`，串起整条链：

薄资本多头破产 → 保险为空所以损失显式入账 `ResidualLoss` → keeper 提案盈利空头 →
Runtime 只切掉损失对应的那部分仓位。

**mock 不用改。** 之前一直卡在"找不到 roe 达标的盈利账户"，根因是那些账户注资
`10_000_000_000`，roe 只有 0.03%，够不着 `MinAdlRoePpm = 50_000`。两个诀窍：

1. **一笔交易同时造出两侧** —— Alice 卖 1_000、Charlie 买 1_000，多空双方一次成型，
   不需要撮合两笔
2. **薄资本**（3_100_000）让盈利方的 roe 到约 49%，轻松过线

断言：
- 减仓量 = `ceil(residual_loss / notional_per_lot)` 且 < 1_000 —— 确认是**部分**减仓
- `ResidualLoss` 金额不变 —— §8.5.1 §2，ADL 不分损失

测试：78 + 6 + 95 + 1 全通过。commit `413a46afc`。

**P2-B 五项至此全部闭环**：settle / migration / insurance / liquidation / ADL。

---

## 2026-09-16 · P2-C §9.1：key 加 SubaccountId + v1→v2 迁移

按设计决策，Phase 2 就把 key 换成 `(AccountId, SubaccountId)`，但初期只用 `SubaccountId = 0` —— 目的是让 Phase 3 不必再做一次全量迁移。

**键结构**

| storage | 旧 | 新 |
|---|---|---|
| `Accounts` | `(AccountId)` | `(AccountId, SubaccountId)` |
| `Positions` | `(AccountId, MarketId)` | `(AccountId, SubaccountId, MarketId)` |
| `UserOrderByClientId` | `(AccountId, ClientId)` | `(AccountId, SubaccountId, ClientId)` |

新增 `SubaccountIndex`（有界遍历子账户，避免无界的 Prefix 迭代器）与配置
`MaxSubaccountsPerAccount` / `MaxClearingMarkets`。

**迁移的两个坑**

三个 storage 的 pallet/storage 前缀都没变，所以旧条目仍在同一前缀下、只是 key 更短。迁移逐条解码旧 key、以 `(…, 0, …)` 重写、再 kill 旧 key。

1. **必须先收集、后写入**。新 key 落在正在遍历的前缀里，边遍历边写会让迭代器回头撞上自己刚写入的条目
2. **`pre_upgrade` 依旧只能数 raw key** —— 和上修 v1 时完全同一个坑：存储里的还是旧编码，用新类型迭代会全部跳过、永远报 0。这次一开始就写对了

`STORAGE_VERSION` 1 → 2，runtime 的 `Migrations` 元组加上 `VersionedMigration<1, 2, …>`。

**验证**：79（含新用例：手工写入旧格式 key → 迁移后可按子账户 key 读到、旧 key 已清除、`SubaccountIndex` 已登记）+ 6 + 96 + 1 全通过。commit `8a470f43f`。

**未完成**：§9.2 跨市场有界风险聚合（`MaxClearingMarkets` 目前只加了配置，聚合逻辑与 `TooManyClearingMarkets` 未落地）、§9.3 费率等级。

---

## 2026-09-16 · P2-C §9.2：跨市场有界风险聚合

需求改为跨市场求和：

```text
initial_requirement     = Σ_markets |notional| × IMR + reserved
maintenance_requirement = Σ_markets |notional| × MMR
```

**实现方式**：`RiskView` 增加 `other_initial` / `other_maintenance`，由调用方（唯一能读 storage 的地方）通过 `with_other_markets` 注入。好处是单市场的纯函数单测一行没动，跨市场只在真正做决定时生效。

**有界遍历**：`cross_market_requirements` 走 `Positions::iter_prefix((who, sub))`，一旦见到 `MaxClearingMarkets` 个仓位就停并返回 `over_limit`。

**超限的处理是拒绝，不是少算。** `TooManyClearingMarkets` —— 静默只聚合前 N 个会**低估**需求，比禁止开新市场危险得多（设计原话：宁可禁止账户再开新市场，也不让风险被低估）。

接入点：下单风控闸门（7a 最坏成交、7b 挂单保证金）、`refresh_restriction`（提现 + 成交后）、清算的可清算判定。

**为了让这条路径真的被测到**，mock 调成 `MaxMarkets = 3` / `MaxClearingMarkets = 2`。runtime 仍是 `MaxMarkets = 1`，所以生产上暂时不会触发 —— 纯基础设施先行，这点必须记清楚。

**踩到的一个连锁**：`create_market_requires_force_origin` 原来硬编码"创两个市场就应拒绝"（假设 `MaxMarkets = 1`）。调大 `MaxMarkets` 后它会误判。改成填满 `MaxMarkets` 个再断言拒绝 —— 测试不该依赖具体的常量取值。

**验证**：80（新增 `exceeding_max_clearing_markets_is_refused`）+ 6 + 97 + 1 全通过，runtime 编译 OK。commit `0eb46e887`。

---

## 2026-09-16 · P2-C §9.3：费率等级与负 maker fee

```rust
pub struct FeeTier { maker_fee_ppm, taker_fee_ppm, maker_rebate_ppm }
FeeTiers<FeeTierId, FeeTier>
AccountFeeTier<(AccountId, SubaccountId), FeeTierId>
```

Phase 1「maker fee 不得为负」的限制放开：`rebate > fee` 时 maker 净收入。

**返佣的安全前提（设计明确要求，也是能开负费率的前提）**：rebate 从 `FeePool` 支出并**参与守恒**，绝不是从别人的抵押品里出。池子不够 → `InsufficientFeePool`，由保险基金或治理补充，而不是静默稀释。

新增治理入口 `set_fee_tier` / `set_account_fee_tier`（`None` 回退到市场自带费率）。`MarketParams` 加 `maker_rebate_ppm`；`simulate_fill` 里 maker 侧先扣 fee 再加 rebate；成交后 `fee_pool` 相应扣减。

**两个新测试**
- `maker_rebate_is_paid_from_the_fee_pool`：fee 100ppm / rebate 300ppm → maker 净收 12_000，且 `FeePool` 精确等于 `30_000 + 6_000 − 18_000`（证明钱来自手续费池）
- `rebate_with_empty_fee_pool_is_refused`：零费率收不到手续费时返佣必须报错，不能静默稀释

**P2-C 三项全部完成**：§9.1 键结构 / §9.2 有界聚合 / §9.3 费率等级。commit `35e04d83f`。

---

## 2026-09-16 · 修 v1 迁移的 key 布局缺陷（review 发现项，已修）

按你的判断处理：**开发阶段没有需要升级的生产数据，所以只修到"不误导、不阻塞后续验证"为止，不为一次性版本做额外加固。**

**修了什么**：v1 的 `translate` 按三段 key 解析，而 v0 存的是两段 → 存量仓位全部被丢弃。改成和 v2 一样走 raw key 收集 + 手工解两段 key；`pre/post_upgrade` 同步改成数 raw key。

**测试为什么之前没抓到**：它用 `Positions::<Test>::hashed_key_for(...)` 写入，那是**新**布局的 key。改成手工拼 v0 两段 key，并顺带串起完整链路：

```
v0 两段 key + 32 字节旧值
  -> v1：原地升级为 48 字节，仍不可被当前 accessor 找到
  -> v2：重新挂 key，accessor 才能读到
```

**顺带发现的隐患**：`Blake2_128Concat::hash(x)` 返回的已是 `hash ++ x`，测试里又额外 append 了一次 `x`。单段的 v2 用例侥幸没暴露，两段的 v1 用例直接解出错位的 `market_id`。两处都修了。

**明确没做的**（按开发阶段的取舍）：不为 v0/v1 布局补更严的迁移加固，不写多版本组合的矩阵测试。真要上线前再补。

---

## 2026-09-16 · P2-C 整体 review（缺陷已修，见上一节）

### 🔴 v1 迁移在真实 v0 布局上会丢数据（最高优先级）

§9.1 把 `Positions` 从 `(AccountId, MarketId)` 换成了 `(AccountId, SubaccountId, MarketId)`。之后 v1 迁移里的

```rust
Positions::<T>::translate::<PositionV0, _>(|(who, _sub, market_id), old| { … })
```

会按 **3 段** key 去解析，而 v0 时代存的是 **2 段** key（`SubaccountId` 是 v2 才引入的）。结果是存量仓位**全部解析失败被丢弃**。

更糟的是**现有测试测不出来**：它用 `Positions::<Test>::hashed_key_for((ALICE, DEFAULT_SUBACCOUNT, MARKET))` 写入，那是**新**布局的 key，不是 v0 的。所以测试一直绿，缺陷一直藏着 —— 和之前 `pre_upgrade` 那个 bug 同一种"测试用错了布局"。

**修的方向**：v1 必须像 v2 那样走 raw key 收集 + 手工解 2 段 key（v2 已经这么做了）。我改到一半、测试没跑绿，**因此已回退**，不把未验证的迁移代码留在树里。

**验证时要用 v0 的 2 段 key 手工写入**（`prefix ++ hash(who) ++ who ++ hash(market) ++ market`），断言 v1 后在原地读到 48 字节新值、且只有 v2 之后才能被当前 accessor 找到 —— 顺便就把 v0→v1→v2 整条链测了。

### 🟡 次要观察

1. **运行时 `MaxMarkets = 1`**，所以 §9.2 的跨市场聚合在生产上不会触发，只在 mock 的多市场配置下被测到。启用多市场前必须复核。
2. **CI 的 feature-gated 步骤没覆盖 `pallet-liquidation`**（它只有 6 个单测）。已包含在 `cargo test -p pallet-liquidation` 里，但没有 benchmark 覆盖。
3. `insufficient_collateral_blocks_oversized_orders` 之类的用例依赖具体常量取值，调 `MaxMarkets` 时踩过一次（已修成"填满再断言"）。其他地方可能还有同类隐患。

### 已确认没问题的部分

- §9.1 的键结构变更：`Accounts` / `Positions` / `UserOrderByClientId` / `SubaccountIndex` 全部改到位，82 个测试通过
- §9.3 的返佣：确实从 `FeePool` 出，`InsufficientFeePool` 守卫有效，守恒不被破坏
- WASM 构建、`cargo build`、benchmark CLI 全通

---

## 待办（按 §2.4 顺序）

- [x] P2-A：benchmark —— `on_initialize` 与 funding 的真实权重（含 proof_size）
- [ ] **在目标硬件上重测权重**（现有数字仅开发期参考，两次运行差 2 倍）
- [ ] P2-A：premium votes 层（P2-D 时补）
- [x] P2-B：`settle_position` + `realized_pnl` 折入 collateral + funding 收费（破坏性语义变更，不可拆分）
- [x] P2-B：**migration**（`Position.funding_index` 编码）—— 与本批同上线
- [x] P2-B：`pallet-insurance` —— 接管 `BadDebt`（兜底路径待清算接入后才可达）
- [x] P2-B：`pallet-liquidation` —— 引擎 + permissionless crate 均已落地
- [x] P2-B：清算接入后补 `unpaid > 0 → cover` 的端到端用例（已随引擎一起验证）
- [x] 修 benchmark setup（`UnknownAsset`）并加 `setup_account()` —— 17 个 benchmark 全绿，其中 5 个是早就坏的
- [ ] 评估：把 `--features runtime-benchmarks` 也纳入 CI，和 `try-runtime` 一起
- [ ] 跑真实 benchmark 填 `weights.rs` 的 `adl_close()`（现为占位值）
- [x] `pallets/liquidation` 的 `WeightInfo`（由 engine 实测分量推导）—— 不单独注册 benchmark，理由见 2026-09-16 日志
- [x] P2-B 步骤 4：ADL 候选的确定性排序（`selection::select_adl_targets`）—— **尚无调用方**
- [x] P2-B 步骤 5：ADL 部分减仓（按缺口量，不再整平）
- [x] P2-B 步骤 5：端到端 ADL 用例（`adl_partially_reduces_a_profitable_opposite_account`）
- [x] 确认 `cargo build`（不带 `SKIP_WASM_BUILD`）通过 —— 已绿，此前是红的
- [x] 修 `pre_upgrade` 用 `iter()` 数迁移前数据的 bug（改为 `iter_keys()`）—— 详见 2026-09-16 日志
- [ ] **把 `--features try-runtime` 纳入 CI**：默认 `cargo test` 不带该 feature，此 bug 正是靠默认绿灯潜伏下来的
- [ ] 若有真实链上 dump：改用 `frame-remote-externalities` 的 `Mode::Offline` 载入，替换掉手工构造的快照
- [ ] 评估：把 `try-runtime` feature 列表改成自动覆盖（如统一走 `frame` umbrella 或写一个检查脚本），避免以后加 pallet 再次漏配
- [x] P2-C §9.1：key 加 `SubaccountId` 维度 + v1→v2 migration
- [x] P2-C §9.2：`MaxClearingMarkets` 有界跨市场风险聚合 + `TooManyClearingMarkets`
- [ ] 生产启用多市场时（runtime `MaxMarkets` 从 1 调大），复核 §9.2 的聚合路径 —— 目前只在 mock 的多市场配置下被测到
- [x] P2-C §9.3：费率等级与负 maker fee（`InsufficientFeePool` 守卫已落地）
- [x] P2-C 三项全部完成 + 整体 review
- [x] 🔴 修 v1 迁移的 key 布局缺陷（commit `e4af59df7`）
- [ ] 上线前补：多版本迁移的组合验证（开发阶段按取舍暂不做）
- [ ] 补 `pallet-liquidation` 的 CI 覆盖（现只有 6 个单测，无 benchmark）
- [ ] P2-D：B1 交易池优先级 → B2 排序 → B3 Exchange Batch
- [ ] P2-E：Runtime API / RPC / Indexer / 告警
- [ ] P2-F：`pallet-bridge`（独立威胁模型与审计）

## 2026-09-16 · P2-D：Exchange Batch —— Runtime 侧完成，node 侧 B1 落地、B2 侦查完毕

### 已完成

- Runtime 侧全部完成：`execute_exchange_batch`（签名授权 / nonce 防重放 / §12.3 确定性
  排序 / continue-on-error）、`ExchangeRuntimeApi`（§12.2）、类型层与测试。
- `execute_exchange_batch` 是 **unsigned** + `ValidateUnsigned`。原因：批次由 proposer
  构造，而它的出块密钥是共识密钥，签不了 extrinsic（runtime 的 `Signature` 是
  secp256k1 / `EthereumSignature`）。authorization 本来就来自每个 action 自己的签名，
  开放 origin 不授予任何东西。`ValidateUnsigned` 只接受 `TransactionSource::Local`。
- 设计稿里的 `author: AccountId` 参数**尚未加入** —— 返佣还没接到批次上，加了就是没人
  用的字段。runtime 已有 `pallet_authorship`（`Authorship`），接返佣时加
  `type Author: FindAuthor` 即可。
- node 侧 B1：`node/src/exchange_pool.rs`，专用内存池（去重 / 有界 / §12.3 排序 /
  nonce 淘汰），7 个单测。校验由调用方注入，因此可脱离 client 单测。

### 一处规范内部的取舍（改了，待复核）

§12.3 是 class 优先，于是同一 subaccount 的 `Cancel(nonce=2)` 会排在 `Place(nonce=1)`
前面。若要求 nonce 严格 `+1`，**一个排序正确的批次反而会被拒**。改成严格递增（允许
跳号），防重放能力不变。

### B2（proposer 打包）侦查结果 —— 下轮直接从这里开始

**trait 定义**（不在 `sc-*` 里，在 primitives）：
`third_party/polkadot-sdk/substrate/primitives/consensus/common/src/lib.rs`

- `:77` `Environment<B>`：`type Proposer` / `type CreateProposer`（`Future + Send + Unpin`）
  / `type Error: From<Error> + std::error::Error` / `fn init(&mut self, &B::Header)`
- `:177` `Proposer<B>`：`type Error` / `type Proposal`（`Future + Send + Unpin`）
  / `type ProofRecording` / `type Proof` / `fn propose(...)`
- `:93` `pub struct Proposal<Block, Proof> { block, .. }`

**接线点**：
- `node/src/service.rs:666` 现在直接 `sc_basic_authorship::ProposerFactory::new(...)`
- `service.rs:680` 塞进 `BabeParams { env: proposer, .. }`
- 池类型 `sc_transaction_pool::TransactionPoolHandle<Block, FullClient>`；
  `FullClient` 在 `service.rs:65`（`pub(crate)`）
- runtime 的 `UncheckedExtrinsic` 是 **frontier 的**
  `fp_self_contained::UncheckedExtrinsic<Address, RuntimeCall, Signature, TxExtension>`；
  构造 unsigned 的构造函数**尚未验证**（我试的 `new_bare` 没编译过）

**第一轮尝试撞到的 4 个错误，逐个修**：
1. `propose` 在这个版本**按值取 `self`**（不是 `&mut self`）—— 要先 clone 出 `Arc`，
   再用 `self.inner.propose(..)` 消耗掉内层
2. `Header::hash()` 找不到 —— 需要 `use sp_runtime::traits::Header`
3. std `Mutex::lock()` 返回 `Result`（中毒），不像 parking_lot 直接给 guard
4. `submit_one` 参数类型不符 —— 大概率要 `&BlockId`，xt 类型是 `<B as BlockT>::Extrinsic`

**推进方式**：先只做"包装器编译通过 + 池子接入"，**先不接选取逻辑**；改一个错误编译
一次，不要攒着。

### 两个"编译通过 ≠ 做完"的教训（都发生在这一轮）

1. `SKIP_WASM_BUILD=1` 会系统性掩盖 no_std 问题。`migrations.rs` 的 `collect_raw` 用了
   `Vec` 但没导入，一直没暴露，直到真正构建 WASM runtime 才炸。涉及 pallet 的改动
   至少跑一次真构建。
2. `internal_liabilities()` 只统计 `account.collateral`，漏了**已实现但未折入的 PnL**
   （结算是惰性的）和**未实现盈亏**。所以 `custody == internal_liabilities()` 只在仓位
   全平时成立。已补 `unsettled_pnl()`，§18.6 故障注入演练 6 个检查点现全部严格相等。

### 更正

CI 里**早就有** feature-gated 测试步骤（`try-runtime` + `runtime-benchmarks`），
还带注释说明为什么必须保留。上面待办清单里那两条 `- [ ]` 是笔误，实际已完成。

## 2026-09-16 · node 侧 `Block` / RuntimeApi 类型关系侦查（为提交入口做准备）

只侦查，不改代码。

### 1. runtime 里有两个 `Block`，只有 Extrinsic 不同

| | 定义 | Extrinsic |
|---|---|---|
| 具体 `Block` | `runtime/src/lib.rs:178` | `fp_self_contained::UncheckedExtrinsic<..>`（frontier 类型化） |
| `opaque::Block` | `runtime/src/lib.rs:70` | `sp_runtime::OpaqueExtrinsic`（字节） |

**Header 相同**，差异只在 Extrinsic。

### 2. 谁用哪个

- `node/src/service.rs:37` — `use sunchain_runtime::{self, apis::RuntimeApi, opaque::Block}` → **opaque**
- `node/src/rpc/mod.rs:40` — `use sunchain_runtime::{opaque::Block, ..}` → **opaque**
- `runtime/src/apis.rs:70-79` — `use super::{.. Block ..}` → **具体 Block**

结论：node 侧一律 opaque；runtime 侧 `apis.rs` 里写的是具体 Block，但
`impl_runtime_apis!` 生成的是对 `Block: BlockT` **泛型**的 impl，所以对 opaque 也成立
—— 这正是 `C::Api: BabeApi<Block>` 能在 `create_full` 里成立的原因。

`FullClient`（`service.rs:65`）= `TFullClient<Block, RuntimeApi, WasmExecutor<HostFunctions>>`，
其中 `Block` 是 opaque，`RuntimeApi` 是 `sunchain_runtime::apis::RuntimeApi`。

### 3. 现有 RPC 的约束模式（照抄这个）

`node/src/rpc/mod.rs:145-190`，`create_full` 的 where 子句：

```
C: ProvideRuntimeApi<Block>
    + sc_client_api::BlockBackend<Block>
    + CallApiAt<Block>
    + HeaderBackend<Block>
    + AuxStore
    + HeaderMetadata<Block, Error = BlockChainError>
    + Sync + Send + 'static,
C::Api: substrate_frame_rpc_system::AccountNonceApi<Block, AccountId, Nonce>,
C::Api: pallet_transaction_payment_rpc::TransactionPaymentRuntimeApi<Block, Balance>,
C::Api: sc_consensus_babe::BabeApi<Block>,
C::Api: BlockBuilder<Block>,
C::Api: sp_api::ApiExt<Block>,
C::Api: fp_rpc::ConvertTransactionRuntimeApi<Block>,
C::Api: fp_rpc::EthereumRuntimeRPCApi<Block>,
C: BlockchainEvents<Block> + UsageProvider<Block> + StorageProvider<Block, B>,
P: TransactionPool<Block = Block, Hash = sunchain_runtime::opaque::Hash> + 'static,
SC: SelectChain<Block> + 'static,
B: sc_client_api::Backend<Block> + Send + Sync + 'static,
CT: fp_rpc::ConvertTransaction<<Block as BlockT>::Extrinsic> + Send + Sync + 'static,
CIDP: CreateInherentDataProviders<Block, ()> + Send + 'static,
```

所以加自定义 RPC 的正确做法：在上面加一条 `C::Api: ExchangeRuntimeApi<Block>`，
其余约束 `create_full` 已经全部声明过，调用点本来就满足。

### 4. 上一轮为什么失败（待验证的猜测）

加了 `C::Api: sunchain_runtime::apis::ExchangeRuntimeApi<Block>` 之后，
`service.rs:625` 的调用点报 `RuntimeApiImpl<..>: BlockBackend<..>` 与
`CallApiAt<..>` 不满足。但这两条**本来就在** `create_full` 的 where 里、对
`FullClient` 成立，所以大概率不是真的缺 bound，而是：

- 我插入 bound 的位置不对（可能插进了非 where 子句的位置，导致约束被解析到别处）；
- 或 `ExchangeRpc::<_, Block>` 里的 `B` 被推断成了**具体 Block** 而不是 opaque。

**下轮第一件事**，用一条命令看完整错误链再动手：

```
cargo check -p sunchain 2>&1 | grep -B5 -A15 "BlockBackend"
```

确认到底是 `C` 还是 `C::Api` 缺 bound，以及 `B` 被推断成哪个 Block。

### 5. 备用方案

若 `C::Api: ExchangeRuntimeApi<opaque::Block>` 确实不成立，就把
`node/src/rpc/exchange.rs` 的 impl 改成针对**具体 `FullClient`** 手写（去掉泛型），
牺牲通用性换确定性。

### 6. 提交入口的其余待办（与类型问题无关）

- `exchange_pool` 必须在 RPC 闭包**之外**创建：闭包是 `move`，
  需要 `let exchange_pool_rpc = exchange_pool.clone();` 让它拿一份，
  proposer 再用外层的那份
- 入口形态定为 `exchange_submitAction(Bytes)`，传 SCALE 编码的
  `SignedExchangeAction`，避免 RPC 层用 serde 复刻 runtime 类型

## 2026-09-17 · P2-D 端到端打通：批次进块的根因与完整排障过程

### 症状

动作能被 RPC 接受、能进专用池、proposer 每块都在提议，但链上**永远看不到执行**。
日志里 `maintain txs=(0, 0)` 始终为零。

### 根因（非常反直觉）

`ValidateUnsigned` 里我写了：

```rust
if !matches!(source, TransactionSource::Local) {
    return InvalidTransaction::Call.into()
}
```

**交易池在每个块之后会对池里所有交易重新校验（re-validation）**，而重新校验时传入的
`TransactionSource` **不是** `Local`。于是批次第一次提交被接受、紧接着被判非法、驱逐出池 ——
它永远等不到进块。

改成只拒绝 `External` 即可：

```rust
if matches!(source, TransactionSource::External) {
    return InvalidTransaction::Call.into()
}
```

**教训**：不要用 `TransactionSource` 做白名单。它是"这笔交易从哪来"的提示，不是身份凭据，
在 re-validation 时会变。批次的安全性来自每个 action 自己的签名，与 source 无关。

### 排障路径（每一步都有验证，不是猜的）

| 步骤 | 做法 | 结论 |
|---|---|---|
| 1 | 起 `--dev` 链，真实 secp256k1 签名提交 | RPC 接受；v=0/v=1 只有一个能通过 → 验签是真在验 |
| 2 | 池 depth 观测 `pool.status().ready` 前后 | `0 → 1`：submit_one 的 Ok 确实表示进池 |
| 3 | 修 proposer 重复投递（take_pending → 确认后移除） | proposing 从 223 次降到 1 次 |
| 4 | prune 任务加心跳 | 任务正常运行，排除"任务没跑" |
| 5 | **校准仪器**：确认 `sp_runtime::print` 可见 | 用 `pallet-staking` 的 `runtime::staking:` 输出证明仪器有效 |
| 6 | 在 `execute_exchange_batch` 开头加无条件 print | 0 次 → "从未派发"成为**已验证**事实 |
| 7 | 试 `OpaqueExtrinsic::from_bytes`（受支持入口） | 仍 0 次 → 排除编码问题 |
| 8 | 试 `longevity` 1 → 10 | 仍 0 次 → 排除过早驱逐 |
| 9 | **`chain_getBlock` 查块内容（纯 RPC，零构建）** | 块里只有 1 个 inherent，**批次不在块里** → 锁定根因 |
| 10 | 放宽 source 检查 | 块里 2 个 extrinsic，ENTERED=1，dropped=1 → 解决 |

第 9 步是转折点：它是唯一一次"直接看证据"，其余都是在猜。

### 验证结果（修复后）

```
块#13: 2 extrinsic
   [0] 0x2805…  (inherent)
   [1] 0xb90105160e…  ← 我们的批次
ENTERED: 1               execute_exchange_batch 被派发
dropped: 1               nonce 被消费，prune 移出池
块#14..#16: 1 extrinsic  不再重复投
```

### 方法论教训（这次最值钱的部分）

1. **先校准仪器，再信它的读数。** 我曾基于"没有打印"断言结论，而那个 print 是否真的
   会出现在日志里从未验证 —— 这不会报错，只会给假的确定感。
2. **用直接证据，不要满足于间接证据。** 我曾把"投进池"当成"上链执行"。
   `chain_getBlock` 一行就看清真相。
3. **不要凭印象写 SDK 接口。** 这轮两次（`Verify` 签名、`OpaqueExtrinsic` 编码）都被
   编译器纠正。查定义 30 秒，猜要 3 轮。
4. **起链验证前先确认只有一个节点进程。** 这轮有 4 次因为旧节点占着 9944 端口，导致
   RPC 打到旧二进制上、结论失真。

### 当前 P2-D 状态

Runtime 侧 / B1 专用池 / B2 proposer / 提交入口 / 方案 B（确认后移除）—— 全部完成并验证。

遗留：
- ExchangePool 是内存态，节点重启会丢失（按决定，交给客户端重传）
- 池满时拒绝新动作，保留已在重试的
- 尚未做：P2P 传播、premium votes 层

## 2026-09-17 · P2-D 收尾：提交入口、方案 B、P2P 传播（均已双节点验证）

### 完成清单

| 项 | 说明 |
|---|---|
| Runtime 侧 | 签名动作类型、`ActionNonce`、`execute_exchange_batch`（unsigned）、`ExchangeRuntimeApi` |
| B1 专用交易池 | `node/src/exchange_pool.rs`，去重/有界/§12.3 排序，7 单测 |
| B2 proposer 打包 | `node/src/exchange_proposer.rs`，包装 `Environment`/`Proposer` |
| 提交入口 | `exchange_submitAction` RPC，传 SCALE 字节（不把 runtime 类型复刻进 serde） |
| 方案 B | 动作确认上链后才移出池 |
| P2P 传播 | `node/src/exchange_p2p.rs`，`/sun/exchange/1`，去重/背压/限流，4 单测 |

### 关键决策

**1. `execute_exchange_batch` 是 unsigned + `ValidateUnsigned`（方案 A）**

批次由 proposer 构造，而它的出块密钥是共识密钥，签不了 extrinsic（runtime 的
`Signature` 是 secp256k1 / `EthereumSignature`）。authorization 本来就来自每个 action
自己的签名，开放 origin 不授予任何东西。

设计稿里的 `author: AccountId` 参数**尚未加入** —— 返佣还没接到批次上。runtime 已有
`pallet_authorship`（`Authorship`），接返佣时加 `type Author: FindAuthor` 即可。

**2. 动作确认上链后才移出池（方案 B）**

最初 proposer 用 `take_pending()` 取走即清空，于是"提案被丢弃/重组/块满"都会让动作
永久消失，而 RPC 已经返回过成功 —— 这个接口在说谎。改成：proposer 只读不取；后台任务
订阅 `import_notification_stream`，每导入一个块对队列重跑 `validate_exchange_action`，
不再通过的才移除（即链上消费了它的 nonce）。

**3. 池满时拒绝新动作**，保留已在重试的。

**4. `ExchangePool` 是内存态**，重启丢失。决定不做持久化，交给客户端重传。

### 踩过的坑（按代价排序）

**① `ValidateUnsigned` 的 source 检查把批次挡在块外（最严重，排障最久）**

```rust
if !matches!(source, TransactionSource::Local) { return InvalidTransaction::Call }
```

**交易池在每个块之后会对池里所有交易重新校验，而重新校验时的 source 不是 `Local`**。
于是批次第一次提交被接受、紧接着被判非法、驱逐出池 —— 永远等不到进块。
改成只拒绝 `External`。

**教训**：不要用 `TransactionSource` 做白名单。它是"这笔交易从哪来"的提示，不是身份凭据。
详细的十步排障过程见上一节「批次进块的根因与完整排障过程」。

**② P2P `SetConfig` 的 `in_peers/out_peers` 是 0/0**

从 GRANDPA 抄骨架时带过来的。GRANDPA 自己管理 peer 集所以能用 0；gossip 协议用 0 就
永远不会开子流。改成 25/25 + `NonReservedPeerMode::Accept`。

**③ 本地提交的 action 从未广播**

只在"收到消息"时转发，而 RPC 直接写池、gossip 任务看不见。加了 channel：RPC 接受后把
编码投进去，gossip 任务转发。

**④ proposer 重复投同一个批次**

用 `pending()` 只读不取却没清理 → 每个块都投同一条（223 次），第二轮起被池回以
`TemporarilyBanned`。被方案 B 的"确认后移除"一并解决。

**⑤ `propose()` 按值取 `self`**

这个 SDK 版本的签名是 `fn propose(self, ...)`，不是 `&mut self`。

**⑥ `OpaqueExtrinsic` 要用 `from_bytes`**

`From<UncheckedExtrinsic> for OpaqueExtrinsic` 内部用的就是 `from_bytes` —— 这才是受支持
的入口，把类型化 extrinsic encode 后直接 decode 成 `B::Extrinsic` 不等价。

**⑦ `sp_runtime::print` 在 no_std 下才可靠**

pallet 里用 `log::error!` 编译不过（`log` 是 optional 且只在 std）。改用
`sp_runtime::print`（走 host function）。**且必须先校准仪器** —— 本仓库可用
`pallet-staking` 的 `runtime::staking:` 输出来证明它可见。

**⑧ `future::select` 的借用**

`select!` 需要 `.fuse()`，而 `next_event()` 返回 boxed future 不适用。改用
`future::select` 后，未选中的分支仍持有 `service` 的可变借用，导致 `relay` 冲突 ——
先折叠成普通枚举释放借用才通过。

**⑨ 两个节点占 9944 端口（发生 4 次）**

旧节点没杀干净，RPC 打到旧二进制上，结论失真。**起链验证前必查进程数**。

### 验证方式（全部真实起链，非仅单测）

单节点 `--dev`：
- 出块正常，无 panic
- `exchange_submitAction` 注册可见
- 真实 secp256k1 签名：v=0/v=1 中正确的那个 ACCEPTED，另一个被拒 → 验签是真在验
- 批次进块（`chain_getBlock` 看到 2 个 extrinsic）、`execute_exchange_batch` 派发、
  nonce 消费、prune 移除

双节点：
- B 用 `--bootnodes` 连上 A，双方各 1 条 `substream opened`
- 向 A 提交 → B 收到 `gossip: accepted from <A>`，`prune tick: 1 queued`

### 当前测试

```
cargo build --package sunchain              OK
cargo test  -p sunchain --lib exchange      11 passed
cargo test  -p pallet-exchange-core          95 passed
cargo test  -p pallet-liquidation             7 passed
```

### 剩余

- premium votes 层（设计文档推迟到 P2-D，依赖 proposer 注入；现在 proposer 已有）

## 2026-09-17 · 喂价守护进程：三家端到端 + 报价币种转换层

### 参考实现的位置

**`/home/alan/alan/dydx/v4-chain`**

今后所有"dYdX v4 怎么做"的问题，直接读这个目录，**不要联网搜索**。
路径也已写进 `docs/Sunchain-Phase2-Exchange-Core-Detailed-Design.md`。

### 从 v4 源码读到的机制（不是凭记忆）

| 机制 | v4 出处 | 结论 |
|---|---|---|
| 单所价格 | `pricefeed/client/price_function/util.go` `GetMedianPricesFromTickers` | **median(ask, bid, last)**，不是最后成交价 |
| 指数价 | `pricefeed/client/price_encoder/price_encoder.go` `GetIndexPrice` | 跨所**中位数**（`lib.Median`），要求 ≥ `MinExchanges` |
| 报价币种转换 | 同上 `convertPriceUpdate` | × rate / ÷ raw / 取倒数，四种组合 |
| 转换率不足 | 同上 | **报错丢弃 ticker**，不退回未转换价格 |
| 支持的交易所 | `price_function/` 目录 | 12 家，见下 |
| 故障不再静默 | `okx.go` | OKX 的错误是**带内返回**（`code != "0"`） |

12 家：binance、bitfinex、bitstamp、bybit、coinbase_pro、crypto_com、gate、
huobi、kraken、kucoin、mexc、okx。已实现 3 家。

### 三个设计决策

**1. 交易所列表用枚举而非字符串**

配置写错在编译期就报错；"哪些交易所在支撑我们的指数价"这个问题，
读这个文件就能回答，不用去翻配置。

**2. coinbase 端点偏离 v4（目前唯一已知偏离）**

v4 用的是 `api.pro.coinbase.com`，现在返回 **503** —— Coinbase 把 Pro 并入
Advanced Trade 后停用了这个主机名。换成 `api.exchange.coinbase.com`，
已验证 200 且 schema 完全一致（`ask`/`bid`/`price`），只改主机名。
**原因和验证日期写在代码注释里。**

> 教训：**"follow dYdX"不能盲从**。参考实现自己也会过期，接外部 API
> 一定要真机验证。这次是先跑三家（A2 而非 A1）才抓到的。

**3. 转换率不足法定人数时，丢弃而不是回退**

是其二的另一面。把"以 USDT 计的 BTC"当成"以 USD 计的 BTC"报出去，
比不报更糟 —— **因为它看起来是好的**。已写成测试钉住这条反回退性质。

### 实测证据（2026-09-17，BTC）

```
conversion rate  USDT-USD   okx 0.999140 / coinbase 0.999060 → 0.999100

venue          symbol      raw             converted
binance        BTCUSDT     76915.950000    76846.725645
okx            BTC-USDT    76917.100000    76847.874610
coinbase_pro   BTC-USD     76843.100000    76843.100000

median WITHOUT conversion   76915.950000
median WITH conversion      76846.725645
```

**三家的分歧从 73.9 降到 4.77（约 15 倍）。**

### 一个险些漏掉的错误

在加转换层之前，指数价**系统性偏高约 0.09%（约 69 美金）**。它不会报错、
看起来完全正常，且会直接流进 premium 和清算（保证金、未实现盈亏都按
mark/index 算）。USDT 平时贴近 1 所以误差小；**USDT 一旦脱锚（历史上到过
0.95）误差放大到 5% 量级**。

这个偏差之所以能被发现，是因为"三家价格不一致"时我去查了原因，
而不是把它当成噪声。**看到多个源不一致时，先问为什么，不要直接取中位数了事。**

### 验证方法

```bash
cargo run --bin pricefeed-check           # 打印 raw / converted 双中位数
cargo run --bin pricefeed-check --usd-min 3   # 故意饿死转换率，验反回退
```

本机网络受限（OKX 的 DNS 被劫持到 169.254.0.2、coinbase 只解析到 IPv6），
**开代理即可跑**：

```bash
export HTTPS_PROXY="http://172.21.144.1:8001"
export HTTP_PROXY="http://172.21.144.1:8001"
```

### 当前测试

```
cargo test -p sunchain --lib pricefeed    20 passed
```

### 待办顺序（已调整）

原计划"先铺开剩下 9 家"，**改了**。理由：剩下 9 家多数也是 USDT 计价，
继续铺开只是增加冗余；而这条 feed **还没有真正喂进链** —— `pallet-oracle`
里可能还是空的。**先打通闭环**更重要：

1. ⬜ **接进 `pallet-oracle`**（`submit_prices` 的轮询循环）
2. ⬜ **接 proposer 每块注入 premium vote**（依赖 1 的指数价）
3. ⬜ 铺开剩下 9 家

做 1 会暴露下一批真问题：oracle 源怎么授权、轮询间隔、上链权重和手续费
谁付。这些比再接 9 家有价值得多。

## 2026-09-18 · 喂价守护进程：从交易所到链上（P2-A 收尾）

### 全链路

```
12 家交易所(已接 3) → 单所 median(ask,bid,last)
                    → 折算到市场报价币种 (USDT)
                    → 跨所中位数 (≥ MinExchanges)
                    → price_to_ticks
                    → 注入式投票 (unsigned, 内层签名)
                    → pallet-oracle 跨源中位数 → index price → premium / 清算
```

### 关键决策（三条，都做过取舍）

**1. 目标币种是市场的报价币种（USDT），不是 USD**

dYdX v4 把各家统一到 USD，因为它自己的市场就是 USD 报价。照搬到我们会让
index 变 USD、impact 是 USDT，premium 里凭空多出 USDT 脱锚量级的偏移。

真机两轮（USDT / USDT / USD-转来）：
```
76314.01 / 76333.30 / 76257.22→76323.95    分歧 56.9 → 9.9
76352.01 / 76348.70 / 76267.19→76333.16    分歧 81.5 → 15.5
```
约 5 倍改善。**幅度不如之前那次 15 倍是对的**：现在只有一家需要移动，
占多数的 USDT 两家原地不动 —— 这本身就是正确性的特征。

**2. coinbase 端点偏离 v4**

v4 的 `api.pro.coinbase.com` 现在返回 503（Coinbase 把 Pro 并入
Advanced Trade 后停用）。换 `api.exchange.coinbase.com`，schema 一致。
**唯一已知偏离，原因写在注释里。** 教训：参考实现自己也会过期。

**3. 喂价走注入式投票，不是普通交易（免费）**

`app/msgs/app_injected_msgs.go` 证实 v4 的 `MsgUpdateMarketPrices` 是
出块者注入的内部消息，"reserved for proposing validator's use only"，
没有手续费 / nonce / 余额 / 排队。

此前是普通签名交易 → **源账户余额耗尽就静默停摆**：交易被拒、价格不再
更新，系统看起来只是"价格变旧"。这是最坏的失败模式。

改法：extrinsic 层 unsigned，身份靠内层签名 + 白名单 + nonce 递增。
与批次不同，这一条**允许从网络来**（propagate=true）—— 产价格的节点
不一定是出块者，鉴权靠签名而非来源，反而更安全。

### 踩过的坑

- `pow10(scale)`：把比例因子 1_000_000 当成指数，算 10^1000000 直接
  溢出成 None。**三个测试同时失败才暴露**；只看一个会以为实现没写完。
- `EthereumSigner` 没有 `DecodeWithMemTracking`（第二次遇到）。这次的解法
  比上次好：`Verify::verify` 本来就收 `AccountId`，调用参数直接用它，
  Signer 只作约束 —— 绕开限制而不是硬凑 derive。
- 新 call 两次插错位置：genesis 段在 `#[pallet::call]` **之前**，
  以及插到了调用块闭合括号之后。
- 批量文本替换静默未命中（写了 tab 但文件是空格 / 名字不同）——
  **每次替换都 assert**，这件事已经让我白跑三四次了。

### 遗留

- **未接进 `service.rs`**：`use crate::oracle_feed` / `crate::pricefeed`
  在 service.rs 里解析失败，而同文件的 `crate::exchange_p2p` 能用。
  原因未查明，疑似 service.rs 被两个 crate 各编译一次（bin 侧未声明
  这两个模块）。`run` 目前**无调用者**。
- 市场列表是硬编码的（链上还没有市场），tick_size 应从
  `Market::tick_size_quote` 读。
- 注释里还留着一笔债：接入 oracle 时要 "delete `set_mark_price`"，未做。
- pallet-oracle 的 `WeightInfo` 暂用 `Pallet` 自身（没有 weights.rs）。

### 测试

```
pallet-oracle        22 passed（含注入投票 5 条）
pallet-exchange-core 101 passed
exchange-primitives   7 passed
sunchain --lib pricefeed  20 passed
```

### 端到端验证（2026-09-18）

```
链下独立算出的中位数   77311.02 USDT
链上 MarketPrices     77294 USDT  (source_count=2, 写于 block 5)
```

差 17 USDT（0.022%）。**`updated_at` 停在 block 5 不是 bug** ——
`MinPriceChangePpm` = 500（0.05% ≈ 38 USDT），17 的变动没过门槛
所以不重写。门槛正在按设计工作。

### 一个需要单独评估的设计问题

`MinSources = 2` 是"多少个白名单**账户**必须一致"。而我们链下已经有
跨所中位数这另一层保护（三家取中位、抗单所操纵）。这两层不是一回事。

- 单节点 dev 链只有一个操作者，2 永远达不到 → 加了第二个 dev source
- 真链上每个 validator 持自己的 key，但**他们跑同一份喂价代码时，
  投票并不独立** —— 这个中位数的实际保护力度需要单独评估

不要把它当成"已有冗余"而放松链下那层。

### 本次新增工具

- `node/src/bin/oracle-price.rs` —— 直接读链上 `MarketPrices`。
  "published a price vote" 只证明提交成功；这个才证明聚合真的发生了。
- `node/src/bin/pricefeed-check.rs` —— 真机跑三家，打印转换前后双中位数

## 2026-09-18 · 两层中位数的职责（结论：B）

指数价上有**两层**中位数，容易误以为有两重保护。拆开看：

### 层一：链下跨所中位数 —— 真保护

Binance / OKX / Coinbase 是独立机构、独立订单簿、独立故障模式。三家取
中位，要操纵必须同时动三家。这是实质性的。

### 层二：链上跨 source 中位数（MinSources = 2）—— 是**可用性**

所有 validator 跑同一份喂价代码、从同样交易所取数、用同样转换规则。
同一时刻采样 → 投票必然相同 → 中位数等于任何一个。

它能防：
- 某个 validator 进程挂掉 / 网络断 → 投不出票（容错）
- **单个** validator 恶意投错 → 中位数确实能过滤掉它

它防不了：
- **全体 validator 被同一个上游数据源污染**。Binance 的 API 被劫持时，
  所有 validator 会一致地投出错误价格，中位数毫无作用。

### 结论（一期采用 B）

不改代码，只把职责说清楚：

> 链上那层是**容错**，不是抗操纵；抗操纵全靠链下那层跨所中位数。

理由：A（保持现状不说明）的风险是**把冗余误当成安全** —— 将来有人因为
"我们已经有两个源了"而放松链下那层。已写入 pallet-oracle 的 MinSources
注释，避免这个误解。

### C 留给后续（未做）

让 validator 的视图**真的不同**，例如每个 validator 配置不同的交易所
子集、或不同的采样时间偏移，使第二层获得真实独立性。

代价：增加运维复杂度，并引入新的分歧来源（不同 validator 看到的价格
天然不同，需要判断分歧多少才算异常）。

真要做时还要注意一个冲突：这会让链上不再有唯一"正确"的观测值 ——
不同 validator 投票不同，中位数才有意义，但也就没了可对照的基准。

## 2026-09-18 · 市场创建：genesis + 治理，以及 LiquidityTier

### v4 是怎么做的

市场**通过治理创建**，不在 genesis（`MsgCreatePerpetualMarketProposal` + `MsgCreateClobPair`）。
而且参数分挂两个模块：

| 模块 | 管的 |
|---|---|
| perpetuals | `BaseAtomicResolution`、`QuoteAtomicResolution`、`LiquidityTier`、`DefaultFundingPpm` |
| clob | `StepBaseQuantums`、`SubticksPerTick`、`QuantumConversionExponent`、`Status` |

`LiquidityTier` 是关键抽象：初始保证金、维持保证金、impact notional 按档位
分（large-cap / mid-cap / long-tail），**多个市场共用一档**。我们的 MarketConfig
把这三个平铺在单个市场里，没有这个概念。

换算的权威定义在 `x/clob/types/quantums.go`：
```
每个 base quantum 的价格 = subticks × 10^quantumConversionExponent
名义额 = subticks × baseQuantums × 10^quantumConversionExponent
```

### 决策（四项）

1. `base_precision` **按币种设**，跟 v4（BTC -10 之类），不统一
2. **引入 LiquidityTier**，现在改比以后大改便宜
3. **genesis 和治理都要**：起链有默认市场，后续靠治理添加
4. dev 建 4 个市场，覆盖三档

### 治理设计（先记录，本次未实现）

**现状：runtime 里只有 `pallet_sudo`，没有 collective / democracy / referenda。**
所以"治理"目前就是 root。

要真正实现 v4 那套，需要：

- 一个治理 origin（council 或 referendum），替换 `type ForceOrigin = EnsureRoot`
- 上市场走**提议 + 投票**，不是管理员直接下发
- 提议里要能校验参数（v4 在 msg 的 `ValidateBasic` 里做），否则一个格式
  错误的提议可以直接把链搞坏

还要考虑：**下架/改档位**比上架更危险。改一个已存在的市场的
LiquidityTier 会立刻影响所有持仓的保证金，需要单独的流程和延迟。

本次只做 genesis 配置；`create_market` 仍是 root。治理流程留待后续，
但**接口要先留对**：市场参数与"谁能改"分离，别把 root 写死在业务逻辑里。

### dev 的 4 个市场（覆盖三档）

| id | 品种 | 档位 | base_precision | lot_size_base | IMF / MMF |
|---|---|---|---|---|---|
| 1 | BTC-USDT | large-cap | 10^8 | 10^5 | 5% / 3% |
| 2 | ETH-USDT | large-cap | 10^8 | 10^6 | 5% / 3% |
| 3 | SOL-USDT | mid-cap | 10^8 | 10^7 | 10% / 5% |
| 4 | DOGE-USDT | long-tail | 10^8 | 10^8 | 20% / 10% |

`tick_size_quote` **逐市场配置，不能一刀切**：DOGE 用 1 USDT 一格太粗，
需要 0.001 USDT。这正是必须能读链上市场配置的原因 —— 硬编码一定会错。

### 全链路打通（2026-09-18 完成）

dev 链起链后：4 个市场全部有指数价，每块每市场一票 premium。

```
BTC   77634 ticks × 1_000_000  →  77634 USDT
ETH    2481 ticks × 1_000_000  →   2481 USDT
SOL   10553 ticks ×    10_000  →    105.53 USDT
DOGE     84 ticks ×     1_000  →      0.084 USDT
```

零警告、零 panic，source_count 都是 2。

### 最后一处硬编码也去掉了

`tick_size_quote` 原本在 service.rs 写一份、genesis 写一份。喂价改成
每轮**从链上读**（新增 runtime API `tick_size_quote`）。

理由：两份迟早不一致，而不一致会让价格差几个数量级**且完全静默**。
这正是 DOGE 不能沿用 BTC 的 tick 的原因 —— 1 USDT 一格对 DOGE 太粗。

### 起链才暴露的坑（本轮两个）

1. **genesis JSON 的字段名两边不同**。宏给 `GenesisConfig` 加
   `rename_all = "camelCase"`，但嵌套的 `MarketConfig` / `LiquidityTier`
   是自己的结构体、没有 serde 属性 → 要 **snake_case**。
   第一次报 `missing field base_asset_code` 才看出来。
2. **`MaxMarkets = 1`**：exchange-core 只允许一个市场，第 2 个即
   `MaxMarketsReached`，在 genesis_build 里 panic。而 WASM 的 panic 只显示
   `unreachable`，看不到消息 —— 是把 panic payload 打出来才读到错误名。

### 还未确认的一处行为

`current_premium_ppm` 在没有指数价时返回 `Some(0)`，所以 4 个市场都能投出
票（值为 0，因为盘口是空的）。**这个"没有指数价就投 0"是否符合预期，
我没有确认过。** 投 0 会把中位数拉向"没有溢价"，和之前拒绝投 0 的立场
需要一致。等有真实持仓时再看这个值是否合理。

### 遗留（按优先级）

1. `set_mark_price` 未删（原注释要求接入 oracle 时删）；
   `mark_price_ticks` 因此仍被 `create_market` 要求非零，genesis 得给种子值
2. **oracle 的 WeightInfo 暂用 Pallet 自身**（它没有 weights.rs）。
   ~~exchange-core 没有 weights.rs~~ —— 这句是我记错的：**exchange-core
   `pallets/exchange-core/src/weights.rs` 是存在的**。缺的只有 oracle。
3. 治理流程未实现（目前只有 Sudo，`create_market` 仍是 root）
4. 剩余 9 家交易所未接

### 还掉的债：Root-set mark price 缓存（2026-09-18）

接入 oracle 时注释自己要求删 `set_mark_price`，一直没做。它比"清理死
代码"要紧，因为**注释和实际行为相反**：

> 注释：oracle 无价时市场应视为 stale，调用方必须拒绝增长风险和拒绝清算。
> 实际：回退到 genesis 种子值（77000 之类）继续用。

也就是喂价挂掉时，保证金和清算会跑在一个可能离谱的价格上。

三处回退（mark_price_of、current_premium_ppm、funding tick）全部删掉：
无价就是无价，不编一个数。

### 两个必须先做的地基

**1. mock 需要可设置的 TestOracle。** 此前 mock 用 `NoOracle`，而那个
缓存正是让 90+ 个测试在没有 oracle 时也能跑的地基。**删缓存前必须先换
地基**，否则测试全挂 —— 我第一次就是直接删，然后 90+ 个测试一起红。

**2. TestOracle 的价格必须存在 externalities 里，不能用进程级 static。**
测试是并行跑的，static 会互相踩，表现为**时有时无**的失败 —— 这种
失败最难查。

### 存储格式变更必须迁移

`Market` 少了两个字段。**不迁移的话旧数据会错误解码，而不是报错** ——
静默错比崩溃更糟。加了 v3 迁移（translate 旧 Market），版本 2→3。

## 2026-09-18 · 喂价守护进程静默不跑（补记）

`tokio::time::interval` 的第一次 `tick()` **从未返回**。任务启动了、打了
`feed started`、然后永远停在 await 上。不崩、不报错、什么都不做 —— 从
外面看和一个没在跑的守护进程完全一样。

改成由**区块导入驱动 + 时间闸门**（与 premium feed 同机制）：每 2 秒一个
块，足够当 30 秒级喂价的时钟，且不再依赖 tokio 定时器。同时补上原本完全
静默的分支（`prices.is_empty()` 直接 continue 不吭声）。

排障中我被自己误导两次，值得记：

- 我 grep "oracle" 漏掉了 `feed started` 这行（它不含该词），据此误判
  卡在 interval 创建之前
- 一次"不过滤日志"的对照实验是**无效的**：默认日志级别本身就隐藏自定义
  target，所以 premium 计数 0 说明不了任何问题

两次都是没确认观测手段有效就去观测。**后台任务的每个分支都要有日志出口**，
否则"没启动"和"启动了但没结果"无从区分。

## 2026-09-18 · oracle 的真实权重（补记）

此前用宏生成的占位权重（submit_prices 固定 50ms），不是测量值。喂价是
免费路径，权重不准直接影响 DoS 抗性。

真机测得（20 steps / 10 repeats）：
```
on_initialize   23949 + m*10288 + s*50805
submit_prices   随价格数与源数增长
```

两个阻塞：

1. **exchange-core 的 benchmark 依赖刚删的 mark price 缓存当价格源。**
   真实 oracle 的 trait 只读，泛型 benchmark 塞不进价格。加了 benchmark
   专用的 BenchmarkOracle（存在 externalities），runtime 在
   runtime-benchmarks 下切换过去。
2. **多组件 benchmark 会为每个组件各跑一次 setup**，于是 Sources 累积撑爆
   MaxSources、TrackedMarkets 报 AlreadyTracked —— 而两者在 WASM 里只显示
   `unreachable`。改成 setup 先 reset 再建，各用例用不同市场 id。

刻意不测 submit_price_vote：造不出有效签名，测出来是**失败路径**会低估权重。

**最重要的一条教训：`runtime-benchmarks` 这个 feature 平时不编译。** 删
mark price 缓存时 101 个测试全绿却没发现它已被打坏。
**"测试通过"不等于"所有 feature 下都能编译"。**

## Phase 2 全景梳理（2026-09-18）

### 已完成

| 子项 | 内容 | 状态 |
|---|---|---|
| P2-A | pallet-oracle、跨源中位数、genesis、喂价守护进程 | 完成，真机验证 |
| P2-B | settle_position、v0→v1 迁移、pallet-insurance、清算引擎、pallet-liquidation | 完成 |
| P2-C | subaccount 维度、v1→v2 迁移 | 完成 |
| P2-D | 确定性排序、签名动作、批次、proposer 注入、P2P、premium votes | 完成 |
| — | LiquidityTier、genesis 建 4 个市场、tick_size 从链上读 | 完成 |
| — | oracle 真实权重 | 完成 |

### 未完成（按当前判断的价值排序）

| # | 事项 | 说明 |
|---|---|---|
| 1 | **剩余 9 家交易所** | 独立、机械；从 3 家到 12 家直接提升抗操纵能力 |
| 2 | **批次的 `author: AccountId`** | 设计稿有，返佣未接。加了才能给批次提交者返佣 |
| 3 | **治理流程** | 目前只有 Sudo，`create_market` 仍是 root。设计已记录 |
| 4 | **oracle 跨 source 中位数的独立性** | 已按 B 定为"容错"；C（让 validator 视图分化）待评估 |
| 5 | ADL 端到端用例 | 三个 adl 单测已有，"真的平掉对手方"的端到端路径早期记过缺 |

### 说明

第 4 项**不是待实现，是待确认**：当前实现已按 B 落地并写明职责，将来若
要 C 需单独设计。第 5 项需先确认现有三个 adl 测试是否已覆盖。

### 决策：批次不给出块者激励（2026-09-18）

查清后的结论与设计稿不同，已在设计文档 §12.4 写明偏离：

- 批次是 unsigned + proposer 注入 → 没人付费 → `author` 参数的原用途
  （weight refund）自动失效，所以**不加 `author`**
- 剩下"要不要给出块者激励"是**产品问题**，当前决定**不给**：
  批次由出块者自己构造，边际成本小；若给则需定来源与数量，属于经济参数，
  没有真实交易量数据前定了只是猜
- 不给的最坏结果是**交易延迟**（不打包），不是资金丢失或状态错误 ——
  可见、可观测、可事后补救

若将来有"批次长期不被打包"的实测证据，再回来定。届时 runtime 已有
`pallet_authorship`，技术上可拿到出块者。

### ADL 端到端用例：已存在，旧记录作废

早期清单里"端到端 ADL 用例还没写"是**过时**的。
`adl_partially_reduces_a_profitable_opposite_account` 的注释明写"这是小单测
覆盖不到的 ADL 场景，跑完整链路"，加上另外两个 adl 测试已覆盖。
**照旧清单做事前先核对，否则会白做。**

### 12 家交易所全部接通（2026-09-18）

每家响应信封都不同：裸数组 / `{"data":…}` / `{"result":{"list":…}}` /
数组的数组 / `{"result":{"<pair>":…}}`。**没有假装它们同形**，各自单独读。

两处**端点偏离 v4**（都写了原因）：
- coinbase：`api.pro.coinbase.com` 已 503
- **mexc：v4 指向已废弃的 v2 端点，现 404；改用 v3**（字段同 binance）

修掉两个自己的 bug：
- **bitfinex 返回 JSON 数字而非字符串**，`as_str()` 一直拿到 None，表现为
  持续 "short row"。改成数字和字符串都接受。
- 验证表里 mexc 的符号写成 `BTC_USDT`（v3 要 `BTCUSDT`）—— **错的是我的验证
  代码，不是适配器**。差点据此误判适配器有问题。

离线测试覆盖全部 12 家的响应形状 + 断言"列表里每家都有端点"。

真机 sweep（BTC）：**11/12** 取到价。bybit 返回 403，v4 用的
`api.bybit.com/v5/market/tickers?category=spot` 在本机被拒，疑似地区限制，
非代码问题。

### 教训

v4 的端点**也会过期**（coinbase 503、mexc 404）。照抄之前必须真机验证，
否则接了 12 家里一半是死的，而接口看起来完全正常。

## 2026-09-18 · 2 验证人 sunchain 链跑通 + DEX genesis 抽成共享函数

### 两个旧 keystore 都插错了 key type（不是密钥错）

`--chain sunchain` 一直不出块。查用户的旧 keystore 发现：

keystore 文件名前 8 位 hex 就是 key type：

    61757261 = aura    6772616e = gran

master / slave 里都是 **aura + gran**，而链用的是 **BABE**，要的是
`babe` / `gran` / `imon` / `audi`。

**关键证据**：genesis 里 master 的 babe 公钥 `0xf4f928ab…`，和 keystore 里那个
**aura** 文件的公钥完全相同。所以私钥从来没坏，只是当初按 Aura 插的。

keystore 文件里存的是**明文 seed**（`0x74e2d3d6…` / `0x26712bfe…`），正是
`initial_config.rs` 注释里那两把。派生关系校准过：

    master 0x74e2d3d6… → sr25519 0xf4f928ab… (babe)  ed25519 0x79a1aaef… (grandpa)
    slave  0x26712bfe… → sr25519 0x485a34a7… (babe)  ed25519 0xdfd4cf92… (grandpa)

修法：`key insert` 补插 babe / imon / audi（Sr25519）+ gran（Ed25519）。

### 一个验证方法上的错误（我自己犯的）

改成 2 验证人后，我先只起了 master，等 95 秒 `best: #0`，一度判断"2 验证人
配置有问题"。

**错了。** 两个节点都起来后立刻出块：

    master  我出的块 12 / 导入 26 / best #25
    slave   我出的块 14 / 导入 26 / best #24
    finality 推进到 #22

两边**都在出块**（12 / 14），不是一出一跟。GRANDPA 要 2/3，2 个验证人时
两个都得在线 —— 这是预期行为，不是 bug。

教训：**验证一个分布式系统的配置时，先把它该有的节点都起全**。单节点不起块
不等于配置错。这一点我已经写进 commit 信息，但那个 commit 现在需要更正。

### DEX genesis 抽成共享函数

此前 exchangeCore（3 档位 + 4 市场）和 oracle 只在 **dev spec** 里写死，
`--chain sunchain` 一个市场都没有（下不了单）、也没有 oracle 源（指数价恒空）。

抽成 `chain_spec::dex_genesis`：
- 档位 / 市场只有一份定义，两个 spec 共用（两份会静默漂移）
- **oracle 源是参数** —— 各链持有自己的喂价密钥，共用意味着替别人签价格
- 资产代码用 `code("BTC")` 生成 8 字节定长数组，不再是手抄数字

### 验证：市场真的在链上（不只是 spec JSON）

用 `state_call` 打 runtime API（没有 RPC，但 state_call 能调任何 runtime API）：

    ExchangeRuntimeApi_tick_size_quote(market_id) → Option<u128>

    market 0 (BTC)  0x000f4240 = 1_000_000  ✓ 与配置一致
    market 1 (ETH)  0x000f4240 = 1_000_000  ✓
    market 2 (SOL)  0x2710     = 10_000     ✓
    market 3 (DOGE) 0x03e8     = 1_000      ✓

spec JSON 对不等于链上对 —— 这次是**从运行中的链读出来**的。

### 当前可复现的启动方式

```bash
# 两节点都插过 babe/gran/imon/audi（只需一次）
# master
sunchain --chain sunchain --base-path .../master/base --validator \
          --rpc-port 9944 --port 30333
# slave（peer id 要从 master 日志里取，它启动后才打印）
sunchain --chain sunchain --base-path .../slave/base --validator \
          --rpc-port 9945 --port 30334 --prometheus-port 9616 \
          --bootnodes /ip4/127.0.0.1/tcp/30333/p2p/<MASTER_PEER_ID>
```

slave 默认 prometheus 端口 9615 会和 master 撞（不致命，但会报错）。
取 peer id 必须**轮询等它出现**，固定 sleep 会拿到空串导致 bootnode 解析失败。

### 剩余

- ✅ 下单工具 + DEX 全流程验证 —— 2026-10-12 完成，见「2026-10-12 · dex-cli 打通」
  （充值→喂价→下单→撮合全通，并已用真实行情重跑）
- ✅ 喂价守护进程的 run 循环 —— 2026-10-12 确认在跑：配上代理后从
  Binance/OKX/Coinbase 取真实行情，4 个市场均 age=0、2 源达成 quorum

---

## 2026-10-12 · dex-cli 打通：充值 → 喂价 → 下单 → 撮合（修掉四层静默失败）

承接 09-18 的 WIP（`053ecefad`）：`dex-cli` 能编译，但下单永远不生效。
commit `80598ace5`。

### 核心结论：失败不是一处，是四层，每层都被下一层吞掉

| # | 原因 | 表现 |
|---|---|---|
| 1 | 签名用 blake2，runtime 由 `EthereumSignature` 对 **keccak256(payload)** 做 recover | `bad signature`，没有任何线索指向哈希函数 |
| 2 | `expires_at: 0` 等于"已过期" | `InvalidExpiry`，但 **nonce 已扣**，重发报别的错 |
| 3 | 喂价用 dev keyring（Dorothy/Ethan）签，genesis 的 `Sources` 是运营地址 | 投票被拒 → 无指数价 → 每笔订单 `MarkPriceStale` |
| 4 | `execute_exchange_batch` 单笔失败只发 `ExchangeActionFailed` 事件后继续，批量仍返回 `Ok` | 连 `system_dryRun` 都永远 `Ok(Ok(()))` |

细节：

- **①**：`ecdsa::Pair::sign(msg)` 内部是 `sign_prehashed(&blake2_256(msg))`。
  两条签名路径（extrinsic 与离链 action）都必须改
  `sign_prehashed(&keccak_256(..))`。账户推导同源：地址是
  `keccak(未压缩公钥[1..])[12..]`，用 33 字节压缩公钥会算出另一个"看起来也对"的地址。
  另外 `Address = AccountId`（裸 `AccountId20`，**没有** MultiAddress 的前缀字节），
  `Signature = EthereumSignature`（不是 `MultiSignature`）。
- **②**：`do_place_order` 对 GTC 要求 `expires_at > now`。过期高度改为按当前链头相对计算
  （`--expiry`，默认 +10000），固定值今天能用明天就是过去。
- **③**：与抽 `dex_genesis` 同一个道理 —— 新建 `node/src/oracle_keys.rs`，
  genesis 取 `source_accounts()`、喂价取 `source_keys()`，同一份定义。
- **④**：批量里 nonce 在动作执行**之前**就写入，所以被拒的动作照样消耗 nonce。
  唯一可信的证据是状态本身：`Orders` 条目，或成交后的 `Position`。

### 顺带修的 oracle 缺陷：价格不移动就无法保鲜

`aggregate` 在价格变动不足 `MinPriceChangePpm` 时跳过写入，连 `updated_at` 也不动。
后果：**喂价每次都准时、市场却越过 `MaxPriceAge` 永久停摆** —— 不发事件、不报错，
只是所有订单被拒。修法：值不动时只刷新 `updated_at` 与 `source_count`。

新增测试 `pallets/oracle/src/tests.rs::a_price_that_does_not_move_stays_fresh`，
含反向对照（停止发布后仍会过期），否则下次有人"优化"掉这段没人知道。

### 第三处"两份清单漂移"：`main.rs` 与 `lib.rs`

这些源文件同时进 **lib 与 bin** 两个目标，模块清单各自一份。新增 `oracle_keys` 时
只在 `lib.rs` 加了声明，bin 侧报 `E0433`，编译器还给出误导性建议
`crate::sunchain::oracle_keys`。两边都要加。

和 `dex_genesis`、本次 `oracle_keys` 是同一类问题：**同一件事有两份定义，且没人检查**。

### 我自己犯的错：AssetId 是 `u32` 不是 `u64`

用 `u64` 拼 `Assets::Account` 的 key 读到空值 → 误判"旧 genesis 没有 USDC" → 白清了一次库。
当时那条链其实是好的。key 构造核对：`twox_128` 是两次 `xxh64`（seed 0/1）**小端**，
python 的 `xxhash.digest()` 是大端，会读出"空"。

### 验证（两节点 `--chain sunchain`，均 `--validator`）

    deposit  → USDC 1_000_000_000_000 → 997_000_000_000
    feed     → MarketPrices market0 = 60000 ticks，2 源达成 quorum
    buy      → resting orders: 1
    跨 MaxPriceAge（220s，价格未变动）后 buy → resting orders: 2（修复前这里必拒）
    buy+sell → Orders/PriceLevels 清空、Positions 生成 1 条、抵押扣手续费

测试：`pallet-oracle` 23、`pallet-exchange-core` 101、node 34，全通过。

### 出口与代理（已做成 skill）

这台机器没有直连外网，喂价会持续报 `no price for market N`（看着像喂价坏了，其实是网络）。

    export HTTP_PROXY="http://172.21.144.1:8001"
    export HTTPS_PROXY="http://172.21.144.1:8001"
    export NO_PROXY="localhost,127.0.0.1"      # 否则本地 RPC 也走代理

`reqwest` 默认认这些变量，**不需要改代码**，只在起节点时带上即可。

已建成用户级 skill `~/.codebuddy/skills/sunchain-dex-e2e/`，含起链脚本、
全流程脚本（`dex_flow.py`，每步断言）、以及踩坑清单 `references/gotchas.md`。
`dex-cli feed` 保留给无出口的机器，有代理时应该用节点自带喂价。

### 用真实行情重跑（代理通了之后）

    market 0 BTC  83,976 ticks   age 0   2 sources   （Binance 同期 83,914，取多家中位数）
    market 1 ETH   2,563 ticks   age 0   2 sources
    market 2 SOL  11,290 ticks   age 0   2 sources   → $112.90
    market 3 DOGE     88 ticks   age 0   2 sources   → $0.088

    1. index price   83976 ticks（链上喂价自己取的，非手工）
    2. deposit       USDC moved into custody: 3000000000
    3. resting buy   @83136 ticks（-1%）→ resting orders: 3
    4. crossing sell @83136            → resting orders: 2
    5. fees collected by the match: 99764

手续费自洽：名义额 166,272,000 × 600ppm（taker 500 + maker 100）= 99,763。
这一条是本次新增的断言 —— 用"有持仓"判断成交可能是上一轮留下的，
**手续费池只会因为本轮成交而增长**。

两节点 #1716 / #1717，各 1 peer，`no price` 出现 0 次。

### 剩余

- ⬜ **oracle 源现在是 dev keyring**：节点喂价只会用这两个密钥签名，所以用运营地址的
  代价就是彻底没有价格。要"每条链各自运营密钥"，得先把喂价改成可配置。
- ⬜ 喂价依赖外网 + 代理；无出口时只能 `dex-cli feed` 手工发布。
- ⬜ 批量失败只在事件里，节点侧没有任何告警；目前靠 dex-cli 回读状态兜底。
- ⬜ 撮合是**同一账户自成交** —— 只有 `0x1be31a94…` 一把私钥可用。要真实双对手方，
  需再导入一个持有 USDC 的账户及其私钥。
- ⬜ slave 默认 prometheus 端口 9615 与 master 撞（已知，不致命）。
