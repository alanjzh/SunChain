# Sunchain Phase 1：`pallet-exchange-core` 详细设计

> 状态：Draft，作为 Phase 1 的实现规格。  
> 依赖设计基线：[订单簿 DEX 设计讨论稿](Sunchain-Orderbook-DEX-Design-Discussion.md)。  
> 实施边界：只实现阶段 A 的链上交易核心；不实现 Indexer、专用交易池、memclob、生产桥、清算、资金费率或新共识。

## 1. 决策摘要

Phase 1 新建一个性能关键 Pallet：`pallet-exchange-core`。它在**一次 Runtime dispatch**内完成市场参数读取、账户抵押、保证金检查、订单簿撮合、仓位/PnL、手续费和订单状态更新。代码内部以 `markets`、`risk`、`book`、`settlement` 模块分层，但不将这些热路径拆成互相调用的 Pallet。

Phase 1 的可交易产品是单一抵押资产的线性 USDC 永续合约，初始只启用一个 `BTC-USD` 市场。所有 GTC 订单是有状态链上订单；IOC/带保护限价的市价单在当前 dispatch 完成后不留在订单簿中。该选择借鉴 dYdX v4 对"状态订单"和短期订单的区分，但**不实现**其短期订单/memclob 路径。dYdX v4 也将订单簿动作和保证金检查结合，并在挂单成交时再次检查风险；本设计采用同一安全原则。[dYdX order message](https://github.com/dydxprotocol/v4-chain/blob/main/protocol/x/clob/types/message_place_order.go) [dYdX order-book types](https://github.com/dydxprotocol/v4-chain/blob/main/protocol/x/clob/types/orderbook.go)

Phase 1 的性能目标是建立可重复的链上基线，而不是承诺 Hyperliquid 级的亚秒最终性。当前共识仍为 Babe/Grandpa；阶段 B 才接入专用交易池、P2P 和 proposer 排序。

## 2. 范围、非范围与前置条件

### 2.1 本文包含

- `pallet-exchange-core` crate、Runtime 注册和 `pallet-assets` 接线；
- 测试 USDC 的存取款、内部抵押账本；
- Root 管理的市场创建、状态切换和测试 Mark Price；
- GTC、IOC、Post-only、Reduce-only、保护性市价 IOC；
- 价格优先、时间优先的链上连续撮合；
- 单市场、单账户的初始保证金检查、仓位、已实现/未实现 PnL 和 maker/taker 费用；
- 有界 storage、可 benchmark 的 weight、性能/守恒测试。

### 2.2 明确不包含

- 生产跨链桥及真实资产；Phase 1 只接收 Runtime 中已创建的测试 `USDC.e` 资产；
- 多抵押资产、组合保证金、隔离保证金、多子账户；
- 多源预言机、资金费率、强制清算、保险基金、ADL；
- FOK、条件单、止损、TWAP、EIP-712 relayer；
- 链下 Indexer、WebSocket、P2P 订单传播和自定义交易池；
- 修改 Babe/Grandpa 或改变共识安全模型。

### 2.3 Phase 1 采用的简化

1. 一个 `MarketId`，但数据模型保留 `MarketId`，避免后续迁移。
2. 一个抵押资产 `CollateralAssetId`，由 Runtime 常量指定。
3. 一个账户等于一个交易账户；`SubaccountId` 预留字段固定为 `0`。
4. Mark Price 仅允许 Root/测试预言机更新，并附有效期；价格过期时禁止扩大风险。
5. 不自动清算。若 `equity < maintenance_margin`，仅将账户标记为 `Restricted`，禁止扩大仓位、撤单外的交易和提款；Phase 2 接管该状态并实现清算。

### 2.4 硬约束（P0，不可协商）

以下约束不是"建议"，而是 Phase 1 能够存在的**前提条件**。任一条被违反，Phase 1 的实现就不能承载任何有价资产：

**H1：Phase 1 在任何情况下都不得承载真实资产。** 唯一抵押品是 Runtime genesis 创建的测试 USDC（见 §4.3）。不得接入桥接资产、真实 USDC、或任何可在外部市场兑换的资产。

**H2：Phase 1 不得开放"提款到外部"的通路。** `withdraw_collateral` 只把资产退回同一条链上的同一账户；不得接 EVM 提款、跨链提款或任何形式的出金通道。

**H3：已实现 PnL 不折入抵押，因此 Phase 1 的账户在经济上并不自洽。** 设计上 `equity = collateral + realized_pnl + unrealized_pnl − fee_debt`，但 `Account.collateral` 从不因亏损而减少。其后果是：一笔对敲之后，亏损方仍可按名义 `collateral` 提款，多空双方的 `realized_pnl` 净额无人承担——双方合计可提取的金额超过 custody 实际持有的金额。§4.3 的资产守恒式之所以仍然成立，是因为 `realized_pnl` **不在**守恒式的定义里。

在 Phase 1（不开放出金、不承载真实资产）下这只是账面缺陷；一旦引入出金或真实资产，它就是可直接兑现的漏洞。Phase 2 §7 的"已实现 PnL 折入 `collateral`"是**必须**随自动清算一起上线的修复项，不得拆分。

**H4：§16 中的参数未全部确认前，不得宣称 Phase 1 完成。** 特别是测试 USDC 的 genesis 分配、市场精度与风险参数必须已落地（当前状态见 §16）。

## 3. 实现落点与 crate 结构

新增目录：

```text
pallets/exchange-core/
  Cargo.toml
  src/
    lib.rs                 # FRAME Pallet、storage、dispatchables
    types.rs               # SCALE 类型、整数辅助函数
    book.rs                # 价格档、队列、撮合循环
    risk.rs                # equity、IM/MM、reduce-only 校验
    settlement.rs          # 仓位、PnL、费用、保证金释放
    weights.rs             # benchmark 生成，初期手写占位
    benchmarking.rs
    mock.rs
    tests.rs
```

工作区和 Runtime 的修改目标：

```text
Cargo.toml                         # 增加 pallet-exchange-core workspace dependency/member
runtime/Cargo.toml                 # 增加 pallet-assets、pallet-exchange-core 及 std/features
runtime/src/lib.rs                 # 注册 Assets 和 ExchangeCore，新增 pallet index，旧 index 不移动
runtime/src/configs/mod.rs         # Assets 与 ExchangeCore Config、参数、PalletId
runtime/src/benchmarks.rs          # 加入 ExchangeCore benchmark
```

`pallet-template` 保留到 ExchangeCore 测试网稳定后再单独移除；本设计不要求删除它。

## 4. Runtime 配置

### 4.1 依赖

`pallet-exchange-core/Cargo.toml` 应依赖 `codec`、`scale-info`、`frame-support`、`frame-system`、`sp-runtime`、`sp-core`、`sp-arithmetic`；均使用当前 workspace 的 `default-features = false` 模式。实现资产转账时使用 FRAME `fungibles` trait，而不是直接耦合 `pallet_assets::Pallet`。

### 4.2 `Config` 契约

以下是目标接口；具体 trait bound 按仓库当前 Polkadot SDK 的 `fungibles::Inspect/Mutate/Transfer` 签名落地。

```rust
#[pallet::config]
pub trait Config: frame_system::Config {
    type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
    /// `Mutate` (which implies `Unbalanced::transfer`) is enough: the core only ever moves the
    /// collateral asset between a user and its custody account. `fungibles::Transfer` is not
    /// required and is deliberately not bound.
    type Assets: Inspect<Self::AccountId, AssetId = AssetIdOf<Self>, Balance = BalanceOf<Self>>
        + Mutate<Self::AccountId>;
    type AssetId: Parameter + Member + Copy + MaxEncodedLen + TypeInfo;
    type Balance: Parameter + Member + Copy + MaxEncodedLen + TypeInfo
        + AtLeast32BitUnsigned + CheckedAdd + CheckedSub;
    type ForceOrigin: EnsureOrigin<Self::RuntimeOrigin>;
    type PalletId: Get<PalletId>;
    type CollateralAssetId: Get<Self::AssetId>;
    type MaxMarkets: Get<u32>;
    type MaxOpenOrdersPerAccount: Get<u32>;
    type MaxPriceLevelsPerSide: Get<u32>;
    type MaxOrdersPerLevel: Get<u32>;
    type MaxMatchesPerAction: Get<u32>;
    type MaxActionsPerBatch: Get<u32>;
    /// Upper bound on the orders one `prune_orders` call may remove.
    type MaxPrunePerCall: Get<u32>;
    type MaxOrderLifetime: Get<BlockNumberFor<Self>>;
    type MaxMarkPriceAge: Get<BlockNumberFor<Self>>;
    /// Per-block budget for exchange actions; see §11.2.
    type MaxExchangeActionsPerBlock: Get<u32>;
    type ExchangeBlockWeightBudget: Get<Weight>;
    type WeightInfo: WeightInfo;
}
```

初始 Runtime 常量建议以 benchmark 前的保守值启动：

| 常量 | 初始值 | 说明 |
|---|---:|---|
| `MaxMarkets` | 1 | Phase 1 仅 BTC-USD |
| `MaxOpenOrdersPerAccount` | 64 | 用户维度硬上限 |
| `MaxPriceLevelsPerSide` | 1,024 | 插入/寻找相邻档位的最大扫描量 |
| `MaxOrdersPerLevel` | 256 | 防止单价格档无限膨胀 |
| `MaxMatchesPerAction` | 64 | 一次下单最多吃 64 个 maker |
| `MaxActionsPerBatch` | 16 | 单签名批处理的上限 |
| `MaxPrunePerCall` | 64 | 单次 `prune_orders` 最多移除的订单数 |
| `MaxOrderLifetime` | 28,800 blocks | 当前 3 秒出块约 24 小时 |
| `MaxMarkPriceAge` | 20 blocks | 当前约 60 秒；仅测试网参数 |

以上都不是主网参数。benchmark 后必须按 block weight、PoV 与目标硬件重新设定。

### 4.3 `pallet-assets` 与托管账户

新增 `Assets` pallet；`AssetId = u32`、`Balance = u128`，并在 **genesis** 创建测试 USDC。该资产的参数由 Runtime 常量统一定义（`runtime/src/configs/mod.rs`），chain spec 与 `Config` 必须引用同一组常量，不得各自硬编码：

| 参数 | 值 | 说明 |
|---|---:|---|
| `COLLATERAL_ASSET_ID` | `1` | 必须与 `Config::CollateralAssetId` 一致 |
| `COLLATERAL_ASSET_DECIMALS` | `6` | 与 USDC 一致；只影响 metadata 与前端展示 |
| `COLLATERAL_ASSET_MIN_BALANCE` | `1` | 一个原子单位，避免平仓后留下无法清除的 dust |
| `is_sufficient` | `true` | **必需**，见下文 custody 说明 |
| genesis 分配 | 见 chain spec | `node/src/chain_spec.rs::test_usdc_genesis_json`；owner 为 sudo key |

Exchange Core 的确定性托管账户为：

```rust
let custody = T::PalletId::get().into_account_truncating();
```

`deposit_collateral` 将 USDC 从用户转到 `custody`，并增加内部 `Collateral`；`withdraw_collateral` 反向转账。Core 不铸造、销毁或直接给第三方转账 USDC。

每个区块/每次测试均应验证资产守恒不变量：

```text
Assets::balance(USDC, custody)
  == Σ Account.collateral + FeePool + InsuranceReserve(Phase 1 恒为 0)
```

因 `pallet-assets` 可能有最小余额/手续费规则，Runtime 配置必须使 custody 能长期存在，且测试资产不扣除意外 transfer fee。

上表的 `is_sufficient = true` 正是这条要求的具体落实：custody 是纯 `PalletId` 派生账户、**没有任何原生币余额**，若测试 USDC 不是 sufficient 资产，`pallet-assets` 无法为它建立资产账户，`deposit_collateral` 会在第一次调用时就失败。单元测试 `custody_account_holds_every_collateral_unit` 覆盖两件事：custody 在零原生余额下可以持有并反复充值该资产；以及 §4.3 的守恒式在充值/提款/再充值后始终成立。

## 5. 基础类型与精度

### 5.1 禁止浮点数

Runtime 禁止 `f32`、`f64`、`PerThing` 隐式舍入用于资产结算。所有外部价格和数量先经前端转换为整数：

```rust
type MarketId = u32;
type OrderId = u64;
type ClientOrderId = u64;
type PriceTicks = u64;
type Lots = u64;
type QuoteAmount = u128;
type SignedQuote = i128;
type FeePpm = u32;          // 1_000_000 = 100%
type MarginPpm = u32;       // 1_000_000 = 100%
```

Phase 1 的市场参数为 `tick_size_quote`（一个 price tick 对应的 USDC 原子单位）和 `lot_size_base`（一个 lot 对应的 BTC 原子单位）。

```text
notional(price_ticks, lots)
  = price_ticks × tick_size_quote × lots × lot_size_base / base_precision
```

实现必须使用 `sp_core::U256` 保存乘法中间值，随后检查能否安全落入 `u128`。全部保证金、费用和风险需求向上取整；PnL 及用户可得金额向下取整。任何溢出都返回 `ArithmeticOverflow`，不可饱和处理。

### 5.2 枚举

```rust
pub enum Side { Buy, Sell }
pub enum TimeInForce { GoodTilCancelled, ImmediateOrCancel }
pub enum MarketStatus { Active, ReduceOnly, CancelOnly, Paused }
pub enum AccountStatus { Normal, Restricted }
pub enum OrderStatus { Open, Filled, Cancelled, Expired, RiskCancelled }
pub enum CancelReason { User, IocRemainder, Expired, Risk, NonceInvalidated, MarketPaused }
```

保护性市价单编码为 `IOC + limit_price_ticks`：买方限价必须大于零，卖方限价必须大于零。UI 可根据可接受滑点计算保护价；Runtime 不接受没有价格上限的市价单。

## 6. 数据结构与 Storage

### 6.1 Market

```rust
pub struct Market<BlockNumber> {
    pub base_asset_code: [u8; 8],       // "BTC"；仅展示/事件用途
    pub quote_asset_code: [u8; 8],      // "USD"
    pub tick_size_quote: QuoteAmount,
    pub lot_size_base: u64,
    pub base_precision: u64,
    pub initial_margin_ppm: MarginPpm,
    pub maintenance_margin_ppm: MarginPpm,
    pub maker_fee_ppm: FeePpm,
    pub taker_fee_ppm: FeePpm,
    pub max_position_lots: Lots,
    pub status: MarketStatus,
    pub mark_price_ticks: PriceTicks,
    pub mark_price_updated_at: BlockNumber,
    pub best_bid: Option<PriceTicks>,
    pub best_ask: Option<PriceTicks>,
    pub bid_level_count: u32,
    pub ask_level_count: u32,
    /// Monotonic order id source for this market. See §6.3 for why it lives here and not on
    /// the account.
    pub next_order_id: OrderId,
}
```

`tick_size_quote`、`lot_size_base`、`base_precision` 在市场存在订单或仓位后不可更改。Phase 1 仅允许 Root 更新 `status`、Mark Price 和受延迟治理保护的风险参数。

### 6.2 账户与仓位

```rust
pub struct Account {
    pub collateral: QuoteAmount,
    pub reserved_open_order_margin: QuoteAmount,
    pub fee_debt: QuoteAmount,          // Phase 1 应始终为 0；预留升级
    pub invalid_before: ClientOrderId,  // 所有更小 client_order_id 逻辑失效
    pub open_order_count: u32,
    pub status: AccountStatus,
}

pub struct Position {
    pub size_lots: i64,                 // 正数 long，负数 short
    pub entry_price_ticks: PriceTicks,  // size == 0 时必须为 0
    pub realized_pnl: SignedQuote,
}
```

Storage：

```rust
Markets<MarketId, Market<BlockNumberFor<T>>>
Accounts<T::AccountId, Account>
Positions<(T::AccountId, MarketId), Position>
FeePool<QuoteAmount>
```

账户只有一份抵押余额；Position 虽按 `MarketId` 存储，但 Phase 1 仅创建一个市场，确保每次风险计算最多读取一个仓位。

### 6.3 订单、价格档与双向队列

```rust
pub struct Order<AccountId, BlockNumber> {
    pub owner: AccountId,
    pub market_id: MarketId,
    pub client_order_id: ClientOrderId,
    pub side: Side,
    pub price_ticks: PriceTicks,
    pub original_lots: Lots,
    pub remaining_lots: Lots,
    pub reserved_margin: QuoteAmount,
    pub tif: TimeInForce,
    pub post_only: bool,
    pub reduce_only: bool,
    pub placed_at: BlockNumber,
    pub expires_at: BlockNumber,
    pub prev_order: Option<OrderId>,
    pub next_order: Option<OrderId>,
}

pub struct PriceLevel {
    pub head: Option<OrderId>,
    pub tail: Option<OrderId>,
    pub total_lots: Lots,
    pub order_count: u32,
    pub prev_price: Option<PriceTicks>,
    pub next_price: Option<PriceTicks>,
}
```

Storage：

```rust
Orders<(MarketId, OrderId), Order<T::AccountId, BlockNumberFor<T>>>
PriceLevels<(MarketId, Side, PriceTicks), PriceLevel>
UserOrderByClientId<(T::AccountId, ClientOrderId), (MarketId, OrderId)>
```

订单 ID 由**市场级**单调计数器 `Market.next_order_id` 递增产生，而不是账户级计数器：`Orders` 的 key 是 `(market_id, order_id)`，若使用账户级计数器，不同账户在同一市场会产出相同的 `order_id` 并互相覆盖。`UserOrderByClientId` 用于 O(1) 撤单。每个订单同时处于一个价格档 FIFO 链表中；撤单无需移动 `Vec` 内元素。

`client_order_id` 必须严格大于 `invalid_before` 且对当前账户唯一。`cancel_up_to(n)` 将 `invalid_before = max(old, n)`，语义上立即取消全部 `client_order_id < n` 的订单，但不扫描 storage。旧订单在下次撮合、显式撤单或 `prune_expired` 时被物理移除并释放保证金。

### 6.4 Storage 不变量

- `Order.remaining_lots > 0`；订单只存在于其所属 `PriceLevel` 的链表一次；
- `PriceLevel.order_count == 0` 当且仅当该 level 不存在；
- `head.prev_order == None`，`tail.next_order == None`；相邻指针互相一致；
- bid 价格链从 `best_bid` 向 `next_price` 严格递减；ask 链严格递增；
- `Market.bid_level_count/ask_level_count` 与实际 level 数相同，且不超 Config 上限；
- `Account.open_order_count` 等于该账户未物理删除订单数；
- `Account.reserved_open_order_margin == Σ Order.reserved_margin`；
- `Position.size_lots == 0` 时 `entry_price_ticks == 0`；
- 任何失败 dispatch 不得改变上述任一状态。

## 7. Dispatchable 接口

所有用户调用均先 `ensure_signed(origin)`；治理调用使用 `T::ForceOrigin`。所有用户调用在失败时原子回滚。

| call index | 调用 | 权限 | 作用 |
|---:|---|---|---|
| 0 | `deposit_collateral(amount)` | Signed | 转入测试 USDC 并记账 |
| 1 | `withdraw_collateral(amount)` | Signed | 在风险检查后转出可用 USDC |
| 2 | `place_order(request)` | Signed | 下单、撮合、必要时挂单 |
| 3 | `cancel_order(client_order_id)` | Signed | O(1) 撤单 |
| 4 | `cancel_up_to(client_order_id)` | Signed | O(1) 逻辑批量撤单 |
| 5 | `execute_batch(actions)` | Signed | 同一账户的有界原子动作批次 |
| 6 | `prune_orders(market, side, max)` | Signed | 清理过期/逻辑失效的最优订单 |
| 7 | `create_market(config)` | ForceOrigin | 创建市场 |
| 8 | `set_market_status(market, status)` | ForceOrigin | 暂停、仅撤单、仅减仓 |
| 9 | `set_mark_price(market, price)` | ForceOrigin | 测试网 Mark Price |
| 10 | `set_risk_params(market, params)` | ForceOrigin | 仅测试网，生产应延迟生效 |

### 7.1 `OrderRequest`

```rust
pub struct OrderRequest<BlockNumber> {
    pub market_id: MarketId,
    pub client_order_id: ClientOrderId,
    pub side: Side,
    pub price_ticks: PriceTicks,
    pub quantity_lots: Lots,
    pub tif: TimeInForce,
    pub post_only: bool,
    pub reduce_only: bool,
    pub expires_at: BlockNumber,
}
```

验证顺序固定如下，避免昂贵计算和 storage 写入发生在简单输入错误之前：

1. market 存在且状态允许该类型动作；
2. `price_ticks > 0`、`quantity_lots > 0`，且不超过市场/账户上限；
3. `expires_at > now && expires_at <= now + MaxOrderLifetime`；IOC 的 `expires_at = now`，不持久化；
4. `client_order_id > invalid_before` 且没有重复的 `UserOrderByClientId`；
5. `post_only` 只能配合 GTC；`post_only && reduce_only` 可用但仍需风险验证；
6. Reduce-only 的方向必须与现有仓位相反，且数量不超过可减少仓位；
7. Post-only 不得跨越当前最优对手价；
8. Mark Price 未过期；以订单全额最坏成交价模拟后的初始保证金必须满足。

`execute_batch` 使用 `BoundedVec<ExchangeAction, MaxActionsPerBatch>`。可含 `Place`、`Cancel`、`CancelUpTo`，按调用者提供顺序执行；任一 action 失败则整个 batch 回滚。它不是阶段 B 的 proposer batch，也不接受第三方签名订单。

## 8. 核心风险计算

### 8.1 数学定义

```text
unrealized_pnl = position_size × (mark_price - entry_price)
equity         = collateral + realized_pnl + unrealized_pnl - fee_debt
position_im    = abs(position_size) × mark_price × initial_margin_ratio
position_mm    = abs(position_size) × mark_price × maintenance_margin_ratio
required_im    = position_im + reserved_open_order_margin
```

所有 quote 结果都经 `U256` 中间值计算。`equity` 允许为负；风险比较必须在有符号域进行。账户允许扩大风险当且仅当：

```text
equity_after >= required_im_after
```

提款后必须仍满足同一条件；若账户是 `Restricted`，提款一律拒绝。

### 8.2 订单保证金预留

为避免遍历所有挂单，Phase 1 使用保守预留：

```text
order_reserved_margin = ceil(notional(limit_price, quantity) × initial_margin_ppm / 1_000_000)
```

下单前将该值加入 `reserved_open_order_margin`，再执行全额最坏情况风险检查。成交 `filled_lots` 后，按成交量比例向上释放该订单的预留；取消或到期时释放全部剩余预留。该模型可能比组合保证金占用更多 USDC，但能保证 O(1) 风控，并适合 Phase 1。

### 8.3 maker 再检查

每次 taker 尝试吃一个 maker 前，Runtime 在内存中模拟该次 fill 对双方的仓位、已实现 PnL、手续费、剩余订单预留和初始保证金影响：

1. maker 的订单已过期、被 nonce 逻辑取消或 Reduce-only 已无可减少仓位：删除 maker，释放剩余预留，继续；
2. maker 成交后 `equity < required_im`：以 `Risk` 删除 maker，释放预留，继续；
3. taker 成交后 `equity < required_im`：停止 taker；IOC 取消余量，GTC 仅在剩余量重新检查后挂单；
4. 双方通过：执行该 fill。

这复用了 dYdX v4 的重要原则：挂单不是一次性永久通过风险检查，挂单在将被成交时需要再次检查，以防 Mark Price 变化使账户不足抵押。[dYdX order-book types](https://github.com/dydxprotocol/v4-chain/blob/main/protocol/x/clob/types/orderbook.go)

## 9. 撮合与结算算法

### 9.1 订单簿操作

**插入新价格档**：从 `best_bid` 或 `best_ask` 沿 `PriceLevel` 指针最多扫描 `MaxPriceLevelsPerSide` 个 level，找到相邻价格后更新四个相邻指针及 Market 最优价。超过上限返回 `TooManyPriceLevels`；不得静默线性扫描。

**入队**：写入 `Orders`，将旧 `tail.next_order` 指向新订单，新订单 `prev_order = old_tail`，更新 `tail/head/total_lots/order_count`。

**移除**：利用订单内 `prev_order/next_order` 更新最多两个邻居、一个 PriceLevel；level 为空时同时拆除价格链节点并更新 best price。所有步骤 O(1)。

### 9.2 `place_order` 伪代码

```text
validate(request)
load account, market, position
reserve full worst-case order margin in memory
remaining = request.quantity_lots
matches = 0

while remaining > 0 and matches < MaxMatchesPerAction:
    price = opposing_best_price(market, request.side)
    if no price or price does not cross request.price: break

    maker_id = PriceLevel(price).head
    maker = Orders[maker_id]
    if maker is stale / expired / nonce-invalid / reduce-only-not-reducible:
        remove_and_release(maker, reason)
        continue

    fill_lots = min(remaining, maker.remaining_lots, reduce_only_limit(request), reduce_only_limit(maker))
    if fill_lots == 0:
        remove_and_release(the non-reducible order, Risk)
        continue

    trade_price = maker.price_ticks
    simulate maker and taker after fill at trade_price
    if maker fails IM:
        remove_and_release(maker, Risk)
        continue
    if taker fails IM:
        break

    settle_fill(maker, taker, fill_lots, trade_price)
    decrement both remaining lots and level total_lots
    release pro-rata order reserve for both sides
    remove each fully-filled order
    matches += 1

if remaining == 0: emit OrderFilled
else if tif == IOC: release taker remaining reserve; emit OrderCancelled(IocRemainder)
else:
    recompute risk for resting remainder
    if passes: insert at price-level tail
    else: release remainder; emit OrderCancelled(Risk)
```

`matches == MaxMatchesPerAction` 时不得继续访问订单簿。若 GTC 尚有剩余，它只在不跨价、通过风险检查后入簿；若请求仍可跨越盘口，则取消剩余并发出 `MatchLimitReached`，防止"看似 GTC、实际无限执行"的语义歧义。

### 9.3 仓位与 PnL

令原仓位为 `q`，成交方向为 taker/maker 得到的有符号 `f`（Buy 为正，Sell 为负），成交价为 `p`。

```text
同向加仓：
  new_entry = floor((abs(q) × entry + abs(f) × p) / abs(q + f))

反向平仓：
  close = min(abs(q), abs(f))
  realized_pnl += close × (p - entry) × sign(q)

反向且穿仓：
  剩余仓位方向为 sign(f)
  新 entry = p

平至零：
  entry = 0
```

对 maker 和 taker 都调用同一个 `apply_position_delta`，保证没有两套会漂移的结算逻辑。手续费：

```text
maker_fee = ceil(notional(fill) × maker_fee_ppm / 1_000_000)
taker_fee = ceil(notional(fill) × taker_fee_ppm / 1_000_000)
```

Phase 1 maker fee 不得为负；所有费用从 `Account.collateral` 扣除并累加到 `FeePool`。如扣费后导致 IM 不足，该次模拟失败，不允许形成负保证金成交。

### 9.4 Reduce-only 精确定义

Buy reduce-only 仅可减少空仓；Sell reduce-only 仅可减少多仓。每次成交前：

```text
max_reduce = abs(current_position)
allowed_fill = min(order.remaining_lots, max_reduce)
```

当 `max_reduce == 0` 时订单立即取消；当订单部分可减少时，以 `allowed_fill` 缩量成交。GTC Reduce-only 可以因用户其他交易使其失效；这时按上述 maker 再检查删除，绝不反向开仓。

## 10. 事件、错误与可观察性

### 10.1 Events

事件仅输出应用重建状态所需字段；不重复存储完整订单。

```rust
CollateralDeposited { who, amount }
CollateralWithdrawn { who, amount }
MarketCreated { market_id }
MarketStatusChanged { market_id, status }
MarkPriceUpdated { market_id, price_ticks, at }
OrderAccepted { who, client_order_id, market_id, order_id }
TradeExecuted { market_id, maker, taker, price_ticks, lots, maker_order_id, taker_client_order_id }
OrderCancelled { who, client_order_id, reason, released_margin }
OrderFilled { who, client_order_id }
AccountRestricted { who, equity, maintenance_margin }
MatchLimitReached { who, client_order_id, remaining_lots }
```

对一个 batch 最多发出 `MaxActionsPerBatch + 2 × MaxMatchesPerAction` 类事件；该上限必须计入编码大小与 benchmark。不要将完整订单簿快照或全部内部指针放入 Event。

### 10.2 Errors

最少定义：

```text
MarketNotFound, MarketNotActive, MarketReduceOnly, MarketCancelOnly,
InvalidPrice, InvalidQuantity, InvalidExpiry, DuplicateClientOrderId,
ClientOrderIdInvalidated, PostOnlyWouldCross, ReduceOnlyWouldIncrease,
InsufficientCollateral, MarkPriceStale, AccountRestricted,
OrderNotFound, NotOrderOwner, OrderAlreadyInvalidated,
TooManyOpenOrders, TooManyPriceLevels, TooManyOrdersAtLevel,
TooManyMatches, TooManyActions, ArithmeticOverflow,
AssetTransferFailed, InvariantViolation, ExchangeBlockBudgetExceeded
```

`ExchangeBlockBudgetExceeded` 表示单块交易预算已耗尽（§11.2），是**可恢复**的输入相关错误：用户应等到下一个区块重试。

`InvariantViolation` 只能表示不可恢复的内部状态矛盾；测试必须覆盖其不可由外部输入触发。普通用户输入不得复用该错误。

## 11. Weight、PoV 与区块预算

### 11.1 Weight 接口

```rust
pub trait WeightInfo {
    fn deposit_collateral() -> Weight;
    fn withdraw_collateral() -> Weight;
    fn place_order(matches: u32, stale_removed: u32, level_scan: u32) -> Weight;
    fn cancel_order() -> Weight;
    fn cancel_up_to() -> Weight;
    fn execute_batch(actions: u32, matches: u32, stale_removed: u32) -> Weight;
    fn prune_orders(removed: u32) -> Weight;
    fn create_market() -> Weight;
    fn set_market_status() -> Weight;
    fn set_mark_price() -> Weight;
    fn set_risk_params() -> Weight;
}
```

不要用常数 weight 覆盖撮合循环。dispatch 应计算实际 `matches/stale_removed/level_scan` 并在 `PostDispatchInfo` 中返还未使用 weight。若 FRAME 宏无法直接声明三个动态参数，使用最大预收 weight 并在返回值中精确 refund。

### 11.2 区块预算

当前 Runtime 的 `RuntimeBlockWeights` 最大 ref-time 约为 2 秒，normal ratio 为 75%。未经阶段 B 改造的通用交易池无法严格预留"交易专属 lane"，因此 Phase 1 的所有交易调用都保持 **Normal 类（不做 Operational 提权）**，并用两个常量给交易动作封顶：

| 常量 | 当前值（**benchmark 前占位**） | 作用 |
|---|---:|---|
| `ExchangeOperationalWeightBudget` | `1 × WEIGHT_REF_TIME_PER_SECOND` + 2 MiB PoV | 单块 `ExchangeCore` 动作的 ref-time / PoV 上限；取最大块权重的一半，与阶段 B 的 `ExchangeBlockWeightQuota = 0.5 × max_block_ref_time` 对齐 |
| `MaxExchangeActionsPerBlock` | 256 | 单块 `ExchangeCore` 动作条数的粗粒度兜底 |

实现要点：

- 记账放在 `BlockExchangeUsage<(block, actions, weight)>`，**惰性重置**（首次读到新块时归零），因此不需要 `on_initialize`，也没有每块的额外 storage 写。
- 记账使用**实际（refund 后）权重**，而不是声明权重。原因：`place_order` 的声明权重是三个独立维度的最坏值之和（单次最坏 PoV ≈ 2.9 MiB），按声明值记账会导致一块只能放 1~3 笔订单，预算失去意义。区块的 weight/length 硬上限仍是安全边界，本预算只负责"交易不得挤占其它业务"的公平性。
- 有 refund 的三个调用（`place_order` / `execute_batch` / `prune_orders`）在执行末尾按实际权重记账；四个平价调用（声明权重 = 实际权重）在入口记账。
- 覆盖全部 7 个用户调用；治理调用（index 7–10）不占预算。
- 超限返回 `ExchangeBlockBudgetExceeded`，整个 extrinsic 回滚（不留下半成品状态）。
- 两个常量都必须由 benchmark 结果反推：单块可容纳动作数 ≈ `ExchangeOperationalWeightBudget / 单次动作实测权重`。占位值下这个数会偏小，属于预期的保守状态。

Phase 1 benchmark 报告至少包含：Wasm compiled/native 执行时间、reads/writes、proof size、单 action 最大事件大小、每块成功撮合数、失败订单比例和 1/3/16 action batch 的 p50/p95/p99。

## 12. 生命周期与边界场景

| 场景 | 规定行为 |
|---|---|
| GTC 到期 | 不在 `on_initialize` 全表扫描；在成为最优 maker、被用户撤销或 `prune_orders` 时惰性移除 |
| `cancel_up_to` 后旧订单 | 立即失去成交资格；惰性物理删除，释放保证金 |
| maker 保证金不足 | 不成交，移除 maker，发 `RiskCancelled` |
| taker 保证金不足 | 停止继续撮合；IOC 取消余量；GTC 仅可在剩余不跨价且风控通过时挂单 |
| 撮合达到上限 | 不继续读订单簿；跨价余量取消，非跨价 GTC 可入簿 |
| GTC 余量挂单前风险重检失败 | 释放余量保证金并发出 `OrderCancelled(Risk)`；**已成交部分保留**（§9.2 第 9 步） |
| Reduce-only GTC 未成交余量 | 与 IOC 不同，**允许挂入订单簿**（§9.4）；成交资格由 maker 再检查保证，绝不反向开仓 |
| Mark Price 过期 | 拒绝扩大仓位、下单和提款；允许撤单、Reduce-only IOC 和 Root 恢复价格 |
| 市场 `ReduceOnly` | 只接受 reduce-only；允许撤单 |
| 市场 `CancelOnly/Paused` | 只允许撤单；禁止撮合和资金风险扩大 |
| 账户低于 MM | 标记 `Restricted`；允许撤单和 reduce-only IOC，禁止提款/普通下单 |
| Root 更新 Mark Price | 仅更新价格与时间；不得遍历账户或执行清算 |

## 13. 测试计划与验收

### 13.1 单元测试

- 整数转换、向上/向下取整、`U256` 溢出；
- FIFO：相同价格先挂订单先成交；买卖价格优先正确；
- 插入/删除价格档后 `best_bid/best_ask`、全部前后指针正确；
- 部分成交、完全成交、穿仓、平仓、反转和 PnL；
- maker/taker 费用和费用池守恒；
- 每一种 TIF/flag/市场状态；
- 到期、nonce 失效、Reduce-only 缩量及风险取消；
- 存取款后的 IM/MM 检查；
- 每个错误路径 state 完全不变。

### 13.2 属性与 fuzz 测试

随机生成有界订单序列、撤单、Mark Price 变动和 batch，逐步断言第 6.4 节全部不变量。特别验证：

```text
custody asset balance == internal collateral + fee pool
Σ order remaining at level == PriceLevel.total_lots
任何 open order 均可沿 level 链表恰好访问一次
无 reduce-only fill 会增加绝对仓位
无账户能在 equity < required_im 时扩大仓位或提现
```

### 13.3 Benchmark 矩阵

| benchmark | 变量 |
|---|---|
| `place_order` 无成交并新建 level | level scan、账户挂单数 |
| `place_order` 无成交并进入已有 level | level queue 长度 |
| `place_order` 吃单 | `matches = 1..MaxMatchesPerAction` |
| `place_order` 移除过期/失效 maker | stale removal 数 |
| `cancel_order` | head/middle/tail/唯一订单 |
| `cancel_up_to` | 固定 O(1) |
| `execute_batch` | actions、matches 的二维边界 |
| `prune_orders` | `removed = 1..max` |

测试网验收先用测试 USDC、一个市场、固定硬件和公开脚本；不把 RPC/Indexer 的吞吐写进链上性能结果。

## 14. Runtime 接线清单

实现顺序：

1. 新增 `pallet-exchange-core` 的 types、mock runtime、整数/风险单元测试；
2. 接入 `pallet-assets`，创建测试 USDC 与 custody 存取款；
3. 注册 `ExchangeCore` 到 `runtime/src/lib.rs` 的新 pallet index，既有 index 不变；
4. 在 `runtime/src/configs/mod.rs` 配置 `Assets`、`CollateralAssetId`、`PalletId` 和所有有界常量；
5. 实现市场与 Mark Price 治理调用；
6. 实现 GTC 插入/撤单，再实现 IOC 撮合；
7. 接入 position/PnL/费用/风险模拟；
8. 补齐 benchmark，生成 `weights.rs`，调整 block 限额；
9. 在本地链进行压力、重组和故障恢复演练。

Runtime 中建议以新 pallet index `21` 注册（当前已用到 `20`）：

```rust
#[runtime::pallet_index(21)]
pub type Assets = pallet_assets;

#[runtime::pallet_index(22)]
pub type ExchangeCore = pallet_exchange_core;
```

实际使用前必须再次确认 Runtime 的已占 index；任何已经发布的 index 均不可重新编号。

## 15. 阶段 B 的兼容预留（不在本期实现）

Phase 1 的 `OrderRequest`、`ExchangeAction`、`client_order_id`、事件与 `Exchange Batch` 必须 SCALE 稳定。阶段 B 可以在不改变 Core 状态机的前提下，将用户动作送入专用交易池和 proposer：

```text
预校验/限流/P2P 传播（节点）
    → 确定性排序（proposer）
    → 有界 Exchange Batch（Runtime）
    → 与 Phase 1 相同的 Exchange Core 执行
```

交易池的预校验永远不能取代 Runtime 风险检查；所有验证者必须从区块数据独立重放相同结果。Phase 1 不存储任何节点本地内存订单状态，因而不引入 memclob 崩溃恢复问题。

## 16. 开始编码前必须确认的参数

以下参数留空时不得进入实现：

1. ~~测试 USDC `AssetId`、小数位和 genesis 分配~~ —— **已确认**：见 §4.3（asset id `1`、6 位小数、`min_balance = 1`、`is_sufficient = true`，在 `dev` 与 `local` 两个 chain spec 的 genesis 中创建，owner 为 sudo key）；
2. BTC 的 `lot_size_base`、`base_precision`、价格 tick 与 quote 精度；
3. 初始/维持保证金率、最大仓位、maker/taker 费用；
4. 测试 Mark Price 更新权限、频率和过期窗口；
5. Phase 1 对账户 `Restricted` 的 UI/运营处理；
6. benchmark 硬件、Wasm 执行模式及目标负载；
7. `PalletId` 常量及 custody 账户的资产最低余额配置。

任何真实 USDC、生产跨链桥、自动清算或缩短区块时间的决定，都必须在本详设之外经过单独设计与审计。
