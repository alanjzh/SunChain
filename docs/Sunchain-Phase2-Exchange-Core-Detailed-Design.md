# Sunchain Phase 2：风险闭环、交易层与可运营性 详细设计

> 状态：Draft，作为 Phase 2 的实现规格。  
> 依赖设计基线：[订单簿 DEX 设计讨论稿](Sunchain-Orderbook-DEX-Design-Discussion.md) 第 3 节「Phase 2（实施阶段 B）」；[Phase 1：`pallet-exchange-core` 详细设计](Sunchain-Phase1-Exchange-Core-Detailed-Design.md)。  
> 实施边界：完成风险闭环（预言机、资金费率、清算、保险基金）、多市场/子账户准备、阶段 B 的交易层与 Indexer 可运营性；**不改共识安全模型**（不替换 Babe/Grandpa，不引入 memclob）。

## 1. 决策摘要

Phase 1 交付的是一个**能撮合但不闭环**的交易核心：Mark Price 由 Root 手工设置，无资金费率，低于维持保证金的账户仅被标记 `Restricted` 冻结，既不能自动降杠杆也不能处置坏账。Phase 2 的目标是把这条风险链补齐，并让系统达到可运营状态。

三项核心决策：

**决策一：分物理 Pallet，保持热路径单 dispatch。** Phase 1 的「逻辑分层、物理合并」原则继续适用于 `markets`/`clearing`/`orderbook`/费用/资金费率——它们仍在 `pallet-exchange-core` 的一个 dispatch 内完成。新增的 `pallet-oracle`、`pallet-insurance`、`pallet-liquidation`、`pallet-bridge` 保持独立 Pallet，通过 trait 单向依赖，依赖方向与讨论稿第 5 节一致。

**决策二：从「静态 Mark Price」改为「Oracle 驱动的 Index/Mark 双价 + 资金费率 + 真实清算」。** 讨论稿第 9 节明确禁止用最后成交价触发清算。本设计中 `index price` 来自多源聚合、用于资金费率；`mark price` 是经过防空操纵处理的 `index price`、用于保证金与清算判定。二者都可能过期，过期时市场自动降级为受限状态，而非继续使用陈旧价格。

**决策三：修正 Phase 1 的结算语义。** Phase 1 把已实现 PnL 记在 `Position.realized_pnl` 上参与 `equity` 计算，但不折入 `Account.collateral`，因此 `withdraw_collateral` 可能让用户提取尚未扣除亏损的资金——承担亏损的一方仍可按名义 `collateral` 提款，亏损被隐式地留给系统承担。Phase 2 改为**每次触碰即结算**：已实现 PnL 与应计资金费立即折入 `collateral`，`realized_pnl` 保留为累计统计量。这是一次破坏性语义变更，必须随 migration 一并上线。

Phase 2 不承诺 Hyperliquid 级延迟：阶段 B 的交易池与 proposer 排序在**保留 Babe/Grandpa** 的前提下提升有效动作占比与包含延迟，并必须分别报告「入池」「被包含」「Grandpa 最终确定」三个延迟口径。[HyperCore Overview](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/overview)


## 参考实现：dYdX v4（本地源码）

**路径：`/home/alan/alan/dydx/v4-chain`**

以后遇到"dYdX 是怎么做的"这类问题，**直接读这个目录**，不要联网搜索。它是 v4 的完整
协议实现（Cosmos SDK）。下面是我们已经读过、并据此决策的部分。

### 关键文件索引

| 关注点 | 路径 |
|---|---|
| 价格源（各交易所客户端） | `protocol/daemons/pricefeed/client/price_function/` |
| 单交易所价格计算 | `protocol/daemons/pricefeed/client/price_function/util.go` |
| 指数价聚合 | `protocol/daemons/pricefeed/client/price_encoder/price_encoder.go` |
| premium vote 存储 | `protocol/x/perpetuals/types/premium_store.go` |
| premium vote 校验 | `protocol/x/perpetuals/keeper/stateful_premium_votes_validation.go` |
| premium vote 消息 | `protocol/x/perpetuals/types/message_add_premium_votes.go` |
| funding epoch 处理 | `protocol/x/perpetuals/keeper/perpetual.go` |

### 支持的交易所（v4 现状，12 家）

```
binance   bitfinex  bitstamp  bybit      coinbase_pro  crypto_com
gate      huobi     kraken    kucoin     mexc          okx
```

另有 `test_fixed_price_exchange` / `test_volatile_exchange` 两个测试用源 —— 我们照抄这个
思路：真实交易所客户端之外必须有可离线断言的源，否则守护进程没法测。

### 机制（读源码确认，非记忆）

**指数价** = 跨交易所**中位数**（`price_encoder.go` 里的 `lib.Median[uint64]`），且要求
参与中位数化的交易所数 **≥ `MinExchanges`**，不足则不出价。交易所内部再由 ticker 取一次
中位数（`GetMedianPricesFromTickers`）。

**溢价**：

```
Premium = (Max(0, ImpactBid − Index) − Max(0, Index − ImpactAsk)) / Index
```

- Impact Bid/Ask = 用 impact notional 吃单的**均价**
- Impact Notional = 500 USDC / 初始保证金率

**投票节奏**：每个块提议者投一票 `FundingPremiumVote`；sample epoch（默认 1 分钟）末取
这些票的**中位数**作为该期样本；tick epoch（1 小时）末取样本的**均值**作为最终费率。

**投票必须 clamp**：`stateful_premium_votes_validation.go` 里 `ErrPremiumVoteNotClamped`
会拒绝超出上下限的票 —— 提议者不能靠极端票把费率推走。

### 我们的差距

| 层 | 现状 |
|---|---|
| 链上指数价 | `pallets/oracle` 已有：授权源 `submit_prices` → 新鲜报价中位数 → quorum。**已对齐 dYdX 的中位数+法定人数思路** |
| premium vote 链上 | `PremiumVotes` 存储 + `submit_premium_votes`（unsigned）+ 边界取中位数。**已做** |
| **喂价守护进程** | **缺**：node 侧从 12 家 CEX 拉价并喂给 `pallet-oracle` |
| **proposer 注入投票** | **缺**：每块用"本地盘口 impact 价 + oracle 指数价"算溢价投票 |

## 2. 范围、非范围与前置条件

### 2.1 Phase 1 已交付基线（本期依赖事实）

Phase 2 的设计基于以下**已实现**的 Phase 1 事实，而非重做：

| 事实 | 位置 |
|---|---|
| `pallet-exchange-core` 单 crate 含 `types/book/risk/settlement/weights` | `pallets/exchange-core/src/` |
| `Assets` = pallet index 21，`ExchangeCore` = 22，既有 index 未移动 | `runtime/src/lib.rs` |
| 价格档双向链表 + 档内 FIFO，插入/移除 O(1) | `book.rs` |
| `RiskView` 的 `equity / initial_requirement / maintenance_requirement` | `risk.rs` |
| `allows_fill` 允许减仓或维持 IM 不增的成交 | `risk.rs` |
| maker 在撮合中不通过再检查时以 `Risk` 原因被撤单 | `lib.rs` 撮合循环 |
| `Account.status ∈ {Normal, Restricted}`，由 `refresh_restriction` 惰性翻转 | `lib.rs` |
| `Account.fee_debt` 为预留字段，Phase 1 恒为 0 | `types.rs` |
| `Position { size_lots, entry_price_ticks, realized_pnl }`，**无 `funding_index`** | `types.rs` |
| Storage 含 `FeePool`，**无 `InsuranceReserve`** | `lib.rs` |
| 11 个 dispatchable，49 个单元测试通过 | `lib.rs` / `tests.rs` |

因此 Phase 2 的新增工作是**增量**的：新增 Pallet、扩展 storage key、新增字段并在 migration 中迁移。

### 2.2 本文包含

**P2-A 价格与资金：**
- `pallet-oracle`：多源报价提交、有效性、聚合为 `index price`；
- Mark Price 的防空操纵派生与市场自动降级；
- 资金费率模型、累积 index、懒惰结算与不平衡残值处理。

**P2-B 风险闭环：**
- 结算语义变更（realized PnL / funding 折入 `collateral`）；
- 维持保证金判定、部分清算、清算罚金；
- `pallet-insurance`：保险基金来源、坏账覆盖、补充规则；
- 破产与 ADL（databasek 由 keeper 提案、Runtime 有界验证执行）。

**P2-C 多市场与账户维度：**
- `(AccountId, SubaccountId)` 键结构与一次性迁移；
- 跨市场且严格有界的风险聚合；maker/taker 费率等级。

**P2-D 阶段 B 交易层（节点侧）：**
- 业务感知交易池预校验与优先级；
- P2P 传播、按市场/账户限流；
- Babe proposer 确定性排序与 `Exchange Batch` 打包；
- 交易专属 block weight/length 配额。

**P2-E 可运营性：**
- Runtime API、Indexer 与 API 协议边界、监控与风控告警。

**P2-F 首条 EVM 生产桥：**
- `pallet-bridge` 的状态机、最终性证明模型、限额与暂停；
- 生产桥的审计与隔离要求。

### 2.3 明确不包含

- 替换 Babe/Grandpa、专用 BFT、亚秒级最终性（讨论稿 Phase 3）；
- 验证者内存订单簿（memclob）；
- 组合保证金、期权、现货保证金借贷；
- 做市激励与返佣清算；
- 多同时上线多条异构桥；
- 将 Indexer 作为余额/订单/清算的权威来源。

### 2.4 交付批次与依赖顺序

```text
P2-A 价格与资金
   │  提供 index_price / mark_price / funding_index
   ▼
P2-B 风险闭环（依赖 P2-A 的 mark price 与 funding 结算）
   │  账户可被清算、坏账有处置出口
   ▼
P2-C 账户维度与多市场（依赖 P2-B 的聚合风控）
   │
   ├── P2-D 阶段 B 交易层（可与 P2-C 并行，不依赖链上 semantics 变更）
   └── P2-E 可运营性（依赖 P2-A~C 事件稳定）
        │
        └── P2-F 生产桥（独立高风险系统，独立审计；可与 D/E 并行）
```

P2-A/B 必须串行交付：**没有真实 Mark Price 就上线自动清算是危险的**（清算会以错误价格处置用户资产）。讨论稿第 9 节「禁止使用最后成交价直接触发清算」正对应此约束。

## 3. 实现落点与 crate 结构

```text
pallets/
  exchange-core/            # 既有；扩展 funding / settle / 多市场风险聚合
    src/
      lib.rs                # dispatchables、storage、撮合循环、结算入口
      types.rs              # 类型与 migration 相关 helper
      book.rs               # 价格档与 FIFO（不变）
      risk.rs               # equity/IM/MM、**跨市场聚合 RiskView**
      settlement.rs         # 仓位/PnL/**资金费**结算，并折入 collateral
      funding.rs            # 新增：资金费率计算与 lazy accrual
      migration.rs          # 新增：Phase 2 storage 迁移
      weights.rs / benchmarking.rs / mock.rs / tests.rs
  oracle/                   # 新增 P2-A：报价源、聚合、index price、有效期
  insurance/                # 新增 P2-B：保险基金、坏账登记
  liquidation/              # 新增 P2-B：可清算判定、接管执行、ADL 验证
  bridge/                   # 新增 P2-F：远程链、存款证明、提款请求
node/
  src/
    exchange_pool.rs        # 新增 P2-D：专用交易池、预校验、优先级
    exchange_proposer.rs    # 新增 P2-D：确定性排序与 Exchange Batch 打包
    exchange_p2p.rs         # 新增 P2-D：传播、去重、限流
    rpc/exchange.rs         # 新增 P2-E：Runtime API 的 RPC 暴露
runtime/
  src/
    lib.rs                  # 注册新 Pallet，分配新 index
    configs/mod.rs          # 新 Config 与有界常量
services/
  indexer/                  # 新增 P2-E：独立进程，不在 Runtime
```

所有自定义 Pallet 必须各自具备 mock runtime、单元测试、benchmark、weights 与 migration 测试；不扩展 `pallet-template`。交易所核心的热路径仍保持单 dispatch，**不在撮合过程中跨 Pallet 调用**——`pallet-oracle` 的价格在 dispatch 入口读取一次，`pallet-insurance` 只在清算收尾时调用一次。

## 4. Runtime 配置

### 4.1 `pallet-exchange-core` 新增 Config 项

```rust
#[pallet::config]
pub trait Config: frame_system::Config {
    /* ...Phase 1 字段不变... */

    /// Oracle 提供的 index/mark 价格来源。
    type PriceOracle: PriceOracle<Self::MarketIdProvider>;

    /// 保险基金：坏账覆盖与资金不平衡残值归集。
    type Insurance: InsuranceFund<Self::Balance>;

    /// 清算人/做市类白名单（可选；空表示完全 permissionless）。
    type LiquidationKeeperOrigin: EnsureOrigin<Self::RuntimeOrigin>;

    type MaxSubaccountsPerAccount: Get<u32>;
    type MaxClearingMarkets: Get<u32>;       // 单次风险聚合读取的仓位上限
    type MaxLiquidationsPerCall: Get<u32>;
    type MaxAdlMatchesPerCall: Get<u32>;
    type MaxFundingHistory: Get<u32>;        // 环形缓冲区再平衡用
}
```

### 4.2 新增 Pallet 的 runtime trait 契约

```rust
/// P2-A：，`pallet-oracle` 实现，exchange-core 消费。
pub trait PriceOracle<MarketId> {
    /// 聚合后的现货指数价（ticks）。无新鲜数据时返回 None。
    fn index_price(market: MarketId) -> Option<PriceTicks>;
    /// 用于保证金/清算的防空操纵价格。
    fn mark_price(market: MarketId) -> Option<PriceTicks>;
    /// 价格是否足够新鲜可用于扩大风险。
    fn is_fresh(market: MarketId) -> bool;
}

/// P2-B：`pallet-insurance` 实现。
pub trait InsuranceFund<Balance> {
    fn cover(shortfall: Balance) -> DispatchResult;   // 余额不足时分次覆盖
    fn credit(surplus: Balance) -> DispatchResult;    // 罚金/残值归集
    fn available() -> Balance;
}
```

依赖方向必须严格单向，禁止 `pallet-liquidiation` 反过来调用撮合 pallet 的内部函数：撮合 pallet 只暴露 `liquidate_account` / `adl_close` 两个受控入口。

### 4.3 pallet index 分配

Phase 1 已占用到 22。Phase 2 新分配：

```rust
#[runtime::pallet_index(23)]
pub type Oracle = pallet_oracle;

#[runtime::pallet_index(24)]
pub type Insurance = pallet_insurance;

#[runtime::pallet_index(25)]
pub type Liquidation = pallet_liquidation;

#[runtime::pallet_index(26)]
pub type Bridge = pallet_bridge;
```

已发布的 index 不可重新编号；上线前必须再次核对。

### 4.4 有界常量初值（benchmark 前保守值，非主网参数）

| 常量 | 初始值 | 说明 |
|---|---:|---|
| `MaxClearingMarkets` | 8 | 单次风险聚合最多读取 8 个仓位，防止 N+1 读取 |
| `MaxSubaccountsPerAccount` | 16 | 每账户子账户上限 |
| `MaxLiquidationsPerCall` | 4 | 单次 `liquidate` 最多处置的账户数 |
| `MaxAdlMatchesPerCall` | 8 | 单次 ADL 执行最多平掉的对手账户数 |
| `MaxOracleSources` | 16 | 单个市场的报价源上限 |
| `OracleMinQuorum` | 3 | 有效聚合所需最少新鲜报价数 |
| `OracleMaxStaleness` | 20 blocks | 超过则价格失效，市场降级 |
| `FundingPeriodBlocks` | 1,200 | 当前 3 秒出块约 1 小时 |
| `FundingRateMaxAbsPpm` | 50_000 | 单期资金费率绝对上限（5%） |
| `LiquidationPenaltyPpm` | 25_000 | 清算罚金（2.5%），入保险基金 |
| `MaintenanceBufferPpm` | 10_000 | 部分清算后的 MM 安全缓冲 |
| `MaxLiquidationSlippagePpm` | 10_000 | 清算吃单的最大价格偏离 |
| `MaxBridgeDailyLimit` | 见第 14 节 | 必须由风控单独设定 |

所有这些值都必须在 benchmark 与目标硬件上重新校准，禁止以「讨论稿建议」直接作为上线参数。

## 5. 价格层：多源预言机、指数价与 Mark Price（P2-A）

### 5.1 三层价格与其用途

| 价格 | 来源 | 用途 | 允许触发清算 |
|---|---|---|---|
| `index price` | 多个现货交易所报价的中位数、反操纵处理后 | 资金费率计算、对齐现货锚点 | 否 |
| `mark price` | 由 `index price` 派生并做限幅平滑 | 保证金判定、未实现 PnL、清算触发 | 是（唯一） |
| `oracle price` | 单个签名的原始报价 | 仅作为 `index price` 输入 | 否 |
| last trade | 最近的成交价 | 仅用于 UI | **禁止** |

讨论稿第 9 节已明确禁止用最后成交价触发清算；本设计进一步规定 `mark price` 也不得直接使用未经展期的 raw aggregator 输出，以避免单一时间戳的价格尖峰造成连锁清算。

### 5.2 `pallet-oracle` 数据结构

```rust
pub struct OracleSource<BlockNumber> {
    pub key: [u8; 33],            // 压缩公钥
    pub weight_ppm: u32,          // 中位数加权；100_000 = 1 单位权重
    pub last_submitted_at: BlockNumber,
    pub last_price: PriceTicks,
    pub enabled: bool,
}

pub struct PricePoint<BlockNumber> {
    pub price_ticks: PriceTicks,
    pub submitted_at: BlockNumber,
    pub source_count: u32,
}

OracleSources<MarketId, BoundedVec<OracleSource<BlockNumberFor<T>>, MaxOracleSources>>
IndexPrice<MarketId, PricePoint<BlockNumberFor<T>>>
MarkPrice<MarketId, PricePoint<BlockNumberFor<T>>>
MarkEwmaState<MarketId, EwmaState>       // 平滑中间状态
```

### 5.3 聚合算法

`submit_prices(origin, Vec<(MarketId, PriceTicks, Signature)>)` 由 Oracle 提交者批量调用；每个 quote 验证签名、来源是否在白名单、单调 nonce，并写入临时候选集。

每 `AggregationInterval` 个区块、`on_initialize` 中按 `MaxMarkets` 有界循环执行：

```text
1. 收集所有 enabled 且 age <= OracleMaxStaleness 的报价；
2. 若新鲜源数量 < OracleMinQuorum：不更新价格 → 触发市场降级（见 5.5）；
3. index_price = 新鲜报价的加权中位数（权重四舍五入，禁止浮点）；
4. mark_price_target = index_price；
5. mark_price_t = clamp(index_price,
                        mark_price_{t-1} × (1 − MaxMarkMovePpm),
                        mark_price_{t-1} × (1 + MaxMarkMovePpm))；
6. 若 |index − mark| 在连续 N 期超过 MaxMarkDriftPpm，立即对齐（防漂移死锁）。
```

第 5 步的限幅是防空操纵的关键：即使某个瞬间聚合出的现货价格剧烈跳动，mark price 也只能在 `MaxMarkMovePpm` 允许范围内移动，清算价格因而平滑。第 6 步防止长期错位导致保证金计算失真。

所有 Medians 使用 `sp_core::U256` 中间值的**整数**比较，禁止浮点数与 `PerThing` 隐式舍入（沿用 Phase 1 第 5.1 节规则）。

### 5.4 极端差价保护

```text
|mark − index| / index > ExtremeDivergencePpm  → 市场标记 Abnormal
任何新 quote 偏离当前中位数 > SourceDeviationPpm → 拒绝该 quote 并计入 source penalty
```

连续被拒绝超过 `MaxSourceRejects` 的来源由治理暂停。**价格层不得因为坏报价直接更改用户余额**；它只能让市场降级。

### 5.5 失效降级矩阵

| 条件 | 市场状态 | 允许的操作 |
|---|---|---|
| 新鲜源 < quorum | `ReduceOnly` | 撤单、减仓；禁止开仓 |
| `Abnormal` 分歧 | `CancelOnly` | 仅撤单 |
| 价格完全缺失超过 N 期 | `Paused` | 仅治理操作 |
| 恢复正常且 quorum 满足 | 治理显式恢复 `Active` | 全部 |

降价可由 `on_initialize` 自动触发；**恢复必须经过显式治理调用**，不得自动恢复。这让系统具备「失败安全」语义：预言机出问题时市场朝向更安全的一侧收缩。

### 5.6 与 Phase 1 接口的替换

Phase 1 的 `set_mark_price` 保留但降级为**仅 testnet 的 emergency override**，生产中不得调用：

```text
if cfg!(feature = "testnet") → set_mark_price 可用
else → set_mark_price 必须由 ForceOrigin + 多重签名，且发出 ForcePriceOverride 事件
```

`Markets<T>.mark_price_ticks / mark_price_updated_at` 继续保留为 **cache**：每次 `on_initialize` 从 `pallet-oracle` 读取后写入，使撮合主循环的读取路径与 Phase 1 完全一致（零新增 storage 往返）。这是「物理合并」的具体体现——新的下层 pallet 不增加热路径开销。

## 6. 资金费率（P2-A）

### 6.1 模型

资金费率的目的是让永续合约价格锚定现货。每 `FundingPeriodBlocks` 结算一次，方向由上一个周期的溢价决定：

```text
premium_ppm   = (impact_mid_ticks − index_price_ticks) × 1_000_000 / index_price_ticks
rate_ppm      = clamp(avg_premium_ppm + interest_base_ppm,
                      −FundingRateMaxAbsPpm, +FundingRateMaxAbsPpm)
unit_payment  = rate_ppm × notional_per_lot(mark_price_ticks) / 1_000_000   // 有符号，每 lot
```

`impact_mid_ticks` 取自订单簿中间价的**冲击加权**值（用 `ImpactSizeLots` 的名义量按盘口深度计算），避免用单笔小额成交操纵 funding。必须为 `option`：盘口深度不足时不更新 `premium`，沿用上期 `rate_ppm`。

正向 `unit_payment` 表示**多头付空头**。

### 6.2 累积 index 与懒惰结算

为避免每次 funding tick 遍历所有仓位，采用全局累积 index + 触碰结算：

```text
FundingIndex[market] += unit_payment                    // 每期一次，O(1)

触碰某仓位时（成交/补/撤/清算/提取的前置检查）：
  owed = size_lots × (FundingIndex[market] − position.funding_index)
  account.collateral -= owed                            // owed<0 时为收取
  position.funding_index = FundingIndex[market]
```

`Position` 新增字段 `funding_index: SignedQuote`。至此 `Accounts.collateral` 才是唯一的可提取资金真值。

**关键不变量：** 资金费**天然是零和的**，详见 6.3。

### 6.3 取整残值与守恒

> **更正（P2-A 实现时发现）。** 本文初稿假设"净持仓不为零导致资金费不平衡"，这是**错的**。

每一笔成交都对称地给双方仓位加上 `+f` 与 `−f`，而所有仓位都从 0 开始，因此：

```text
Σ position.size_lots ≡ 0        （恒等成立，不是"通常接近"）
```

于是初稿的 `residual = net_oi_lots × unit_payment` **恒等于 0**，残值机制形同虚设。dYdX v4 同样是这种情况：perpetual 的每一个多头都对应一个空头，资金费在协议层面本来就是零和的。

**真正的残值来源是取整粉尘**：每个仓位的应付额 `owed = size × (index − position_index)` 都按"向零截断"取整（§5.1 规定），所有账户的截断误差加起来不再精确为 0。这个差额通常极小，但必须有一个确定的归属，否则守恒式会被破坏：

```text
residual = Σ(精确应付额) − Σ(截断后的应付额)
InsuranceFund::credit_or_debit(residual)
```

守恒式（第 4.3 节）：

```text
Assets::balance(USDC, custody)
  == Σ Account.collateral（按 subaccount 求和）
   + FeePool
   + InsuranceFund
   − 未结算 pending funding 的账面差额（必须为 0，因为已通过 residual 归集）
```

实现要求：

- `Market.net_oi_lots` 保留，但它**预期恒为 0**。它的作用是让这个不变量被**检查**而不是被**假设**：每次撮合后显式累加 `maker_signed + taker_signed`，并与 `Σ size` 的一致性一起纳入属性测试。若某天它不再为 0（例如引入非对称的仓位变更路径），测试会立刻发现。
- 结算路径必须把 `residual` 显式记入保险基金，不得静默丢弃。
- 属性测试断言：`Σ collateral + FeePool + Insurance == custody`，且多次 funding tick 后误差有界（每个账户每期至多 1 个原子单位的截断）。

### 6.4 极端情形

| 情形 | 处理 |
|---|---|
| `unit_payment` 为负且用户余额不足 | 结算后 collision = 0，差额记为 bad debt → P2-B 流程 |
| 长期在 ±上限 | 触发风控告警，评审 `FundingRateMaxAbsPpm` |
| 盘口无深度 | 沿用上期 `rate_ppm`，不置零 |
| 市场 `Paused` | 暂停 index 累积，暂停 funding 结算 |

### 6.5 P2-A 已确定决策与遗留项

对齐 dYdX v4 后确定、且已写入实现的决策：

| 议题 | 决策 |
|---|---|
| 价格模型 | **单价格**：`mark_price == index_price`（dYdX 没有独立 mark price，也永不使用最后成交价）。trait 上仍保留两个方法，便于将来引入限幅 |
| 聚合 | **中位数**（非加权平均），要求 ≥ `MinSources` 个新鲜源 |
| 抖动门槛 | 新增 `MinPriceChangePpm`（dYdX `MarketParam.MinPriceChangePpm`），变动不够不写盘 |
| Premium | 分段式 `(max(0, ImpactBid − Index) − max(0, Index − ImpactAsk)) / Index`；index 落在价差内或盘口不足 → 0 |
| Impact 价 | 用 `ImpactNotionalQuote`（500 USDC）名义量吃出的**基础量加权均价**；吃不满则 premium = 0 |
| Epoch | `funding-sample` = 60 块（~3 min）、`funding-tick` = 1 200 块（~1 h）、rate period = 9 600 块（8 h），故 1 h tick 只收 1/8 费率 |
| Clamp | `\|R\| ≤ FundingRateClampFactorPpm × (IMR − MMR) / 1e6`，取代写死的 5% |
| Index delta | `rate × (Δt / period) × notional_per_lot`，按时间比例，epoch 变长/变短不会多收或少收 |
| 残值 | 取整粉尘（见 §6.3 更正），不是净持仓不平衡 |
| Trait 位置 | `PriceOracle` / `InsuranceFund` / `ExchangeEngine` 放在 `pallets/exchange-primitives`，不放在消费方 Pallet 里 |

**尚未落地（必须补，勿遗漏）：**

1. **premium 的 votes 层**。dYdX 是每个区块收集 proposer 的 premium 投票，sample epoch 边界取**中位数**；我们目前一个 sample epoch 只有一个观测值（中位数即该值本身）。投票依赖 proposer 注入，属于 **P2-D**。补齐时只需在 sample epoch 内累积多个观测再取中位数，**tick 逻辑不用改**。
2. **benchmark + weights**。`on_initialize`（funding epoch 处理）目前是占位权重，必须和 funding 一起跑 benchmark 生成真实值（含 proof_size）。
3. **funding 收费**。目前只推进 `funding_index`、不扣 `collateral`。收费必须与 P2-B 的 `realized_pnl` 折入**同批上线**（§2.4 明令不可拆分）。
4. **`Position.funding_index` 的 migration**。该字段改变了 `Position` 的 SCALE 编码；任何已持有 Phase 1 状态的网络升级前必须迁移（§10）。

## 7. 结算语义变更：折入 collateral

### 7.1 Phase 1 的问题

Phase 1 中 `realized_pnl` 累加在 `Position` 上参与 `equity = collateral + realized_pnl + unrealized_pnl − fee_debt`，但：

- `Account.collateral` 从不因亏损而减少；
- `withdraw_collateral` 按 `collateral − reserved` 释放；
- 因此承担亏损的账户可以提取等于名义 collateral 的资金，而多空双方的 `realized_pnl` 净值为零，custody 仍能对上一次。

这在 Phase 1（不出 lost 也不清算）下不产生直接的资金损失，但在 Phase 2 引入提取+清算后会成为真实的 extractable bug。

### 7.2 Phase 2 语义

引入统一的 `settle_position(account, position, params, funding_index_now)`：

```text
1. 结算应计资金费（第 6.2 节），owed 计入 delta；
2. delta += position.realized_pnl − position.realized_pnl_settled；
3. 若 account.collateral + delta >= 0：
       account.collateral += delta
       position.realized_pnl_settled = position.realized_pnl
   否则（已破产）：
       shortfall = −(account.collateral + delta)
       account.collateral = 0
       InsuranceFund::cover(shortfall)
       T=emat Event::BadDebtRealized { who, amount: shortfall }
4. 更新 position.funding_index
```

此时 `equity` 简化为：

```text
equity = collateral + unrealized_pnl − fee_debt
```

`realized_pnl` 退化为累计统计量，**不再出现在 equity 定义中**。修改 `RiskView` 时必须同步修改 `risk.rs` 与所有单元测试。

### 7.3 变更影响面

- `withdraw_collateral`：改为读取已结算的 `collateral`，并在提取后重新检查 IM；
- 撮合循环：每次 `simulate_fill` 后 apply 时同时结算；
- `refresh_restriction`：仍使用新的 `equity`；
- migration 必须将所有存量 `Position.realized_pnl` 折入 `collateral`。

这是一次**破坏性状态语义变更**，必须有 `storage_version` bump 与专门 migration 测试。

## 8. 维持保证金、清算、保险基金与 ADL（P2-B）

### 8.1 可清算判定

```text
liquidatable(account, market) ⇔
    fresh_mark_price 可用 ∧
    equity(account) < maintenance_requirement(account) ∧
    market.status ∉ {CancelOnly, Paused}
```

注意：**价格失效时不得清算**（第 5.5 节降级优先），这与讨论稿「预言机超时/市场异常应进入受限状态」一致。此时靠得 withdraw/开仓被封住的色剂池secrecy。

### 8.2 部分清算

目标是最小必要地平仓，使账户回到 MM 之上并保留缓冲。设：

```text
N = |size_lots|                      待清算仓位绝对量
P = mark_price_ticks
a = (P − entry_price_ticks) × sign(size_lots)   每 lot 的已实现 PnL（有符号）
c = P × LiquidationPenaltyPpm / 1_000_000       每 lot 清算罚金
B = P × N × MaintenanceBufferPpm / 1_000_000    安全缓冲
需要：E + q(a − c) ≥ (N − q) × P × MMR + B
→    q ≥ (N × P × MMR + B − E) / (a − c + P × MMR)
```

实现要点：

- 分母 `≤ 0`（继续平仓反而恶化）→ **全额平仓**；
- `q ≥ N` → 全额平仓；
- 否则 `q = max(上式结果, MinLiquidationLots)`；
- 所有除法使用 `U256` 中间值，并统一**向上取整**（多平一点，保守方向）；
- 计算出的 `q` 在执行时还必须受第 8.3 节的 `MaxLiquidationSlippagePpm` 价格带限制（撮合部分的实际成交不得超出该滑点上限）。

### 8.3 清算执行路径

`liquidate(origin, target: (AccountId, SubaccountId), market)` 为**permissionless**（任何人都可提交），给予清算人返佣（`LiquidationPenaltyPpm` 的一部分）。

```text
1. 保证 target 满足可清算判定，否则 Err(NotLiquidatable)；
2. settle_position(target) 先结算 PnL 与 funding；
3. 计算 q（8.2）；
4. 以 IOC 形式把 q 与对手盘撮合，成交价不得劣于 `MaxLiquidationSlippagePpm` 允许的价格带；
   → 超出该价格带的对手深度不参与成交，未成交部分保留（见下）；
   → 若撮合未能完全消化且账户已 underwater（`equity ≤ 0`）：转第 8.5 节破产接管；
5. 撮合出的每笔 normal fill 仍走 maker 再检查（maker 不足额 → maker 被撤）；
6. 收取 LiquidationPenaltyPpm × notional(fill) 入 InsuranceFund；
7. emit PositionLiquidated { target, lots, avg_price, penalty, insurance_delta }
```

限制：`MaxMatchesPerLiquidation`、`MaxLiquidationsPerCall` 必须计入 weight；滑点超限必须返回明确错误（`LiquidationSlippageExceeded`），不得静默跳过。

### 8.4 `pallet-insurance`

```rust
InsuranceBalance<Balance>                 // 保险基金余额
InsuranceFeeSharePpm<()>                  // 费用池划转比例（每清算周期结算）
BadDebtLedger<Balance>                    // 历史累计坏账（可审计）
```

来源（按优先级）：

1. 清算罚金（`LiquidationPenaltyPpm` 中非返佣部分）；
2. `FeePool` 的治理划转比例 `InsuranceFeeSharePpm`；
3. funding 不平衡残值（6.3，可能为负数）；
4. ADL 结算后的剩余（8.5）。

支出顺序：`cover(shortfall)` 先扣除基金余额，**基金耗尽后才**转 ADL。禁止跳过基金直接把损失社会化给盈利交易者。

### 8.5 破产与 ADL

当 `equity ≤ 0` 且订单簿 liquidity 被 `MaxLiquidationSlippagePpm` 限制住时，账户进入破产处理：

```text
1. settle_position → shortfall = |equity|
2. InsuranceFund::cover(shortfall)
3. 若保险基金不足 → emit InsuranceFundDepleted
4. 剩余缺口用 ADL：对反向且有盈利的对手方按盈利/杠杆降序强制减仓
```

**ADL 的排序不能在线性扫描所有仓位**（对所有持仓排序是无界操作）。方案：

- ADL 候选由**permissionless keeper 提案**，Runtime 验证；
- 验证条件：同一 market、方向相反、`unrealized_pnl > 0`、`roe ≥ MinAdlRoePpm`、叶子顺序由 `(roe desc, subaccount asc)` 决定 — 保证无论 keeper 提交谁，被验证通过的行为都是确定的；
- 单次最多处理 `MaxAdlMatchesPerCall`，并发出 `AutoDeleveraged` 事件；
- 决策包括被平仓位获得的 ADL 价格（使用 `mark price`，or 更好价格以保护被 ADL 方）。

这样既避免了 unbounded iteration，又保持了「确定性重放」：执行的语义不依赖 keeper 的套利选择，只依赖被选中账户的状态。

#### 8.5.1 ADL 定价与剩余损失（决议）

**ADL 的主要目的是在极端市场条件下减少无法安全维持或正常平仓的风险敞口。ADL 本身不得被解释为对未覆盖损失的隐性社会化机制。**

##### 1. ADL Execution Price

被选中参与 ADL 的账户，其被减仓部分应按照**定义明确、可审计**的 ADL Execution Price 结算。

ADL Execution Price 基于可信 Mark Price 或预先定义的公允价格基准计算，**不得仅因账户被选中参与 ADL 而对该账户施加未披露的价格惩罚**。

记 `P_mark_adjusted` 为依据指数价格、价格保护带及市场异常条件计算得到的有效标记价格，则：

```text
多头仓位被减仓（其卖出）： P_adl >= P_mark_adjusted
空头仓位被减仓（其买入）： P_adl <= P_mark_adjusted
```

即**成交价一律向被 ADL 方有利的方向偏移**，偏移量为 `AdlPriceBandPpm`（§4.2）。取整方向同样取对被 ADL 方有利的一侧：多头向上取整，空头向下取整。

##### 2. ADL 与坏账

ADL 的仓位关闭或转移**不得自动视为对被 ADL 方未实现利润的没收**。

在 ADL、抵押品处置、清算收益及保险基金覆盖完成后，**任何仍未覆盖的损失应显式记录为 `ResidualLoss`**（`pallet-liquidation` 的可审计账本），而不是被任何环节静默吸收。ADL 执行后不自动冲销该记录 —— 只有保险基金的 `cover` 或治理动作可以减少它。

##### 3. 禁止隐性社会化

未覆盖损失**不得**通过以下方式自动分摊给非参与账户：

- 未披露的价格调整；
- 资金费率操纵；
- 账户余额稀释；
- 其他任何隐性方式。

任何针对 ADL 参与账户的损失分配机制，如将来引入，必须作为**独立的 Explicit ADL Loss Allocation 机制**定义，并明确规定：

| 要求 | 内容 |
|---|---|
| 适用条件 | 何时触发 |
| 优先级规则 | 与保险基金、ADL 的先后顺序 |
| 最大损失范围 | 单次/单账户上限 |
| 定价或损失计算公式 | 可复现的公式 |
| 审计与事件记录要求 | 必须可事后重放 |

**该机制不得由 ADL Execution Price 的模糊定义隐含实现。** 换言之：ADL 只减敞口，不分损失。

##### 4. 落地映射

| 决议要点 | 实现位置 |
|---|---|
| `P_adl` 向被 ADL 方有利偏移 | `pallet-liquidation::adl_execution_price` |
| 取整方向取有利侧 | 同上（多头 ceil / 空头 floor） |
| 剩余损失显式记录 | `pallet-liquidation::ResidualLossLedger` + `ResidualLossRecorded` 事件 |
| ADL 不自动冲销坏账 | `adl` extrinsic 不修改 `ResidualLossOf` |
| 保险基金优先于 ADL | `settle_party` 先 `cover`，失败才记 residual |

### 8.6 Phase 1 `Restricted` 的处置

Phase 1 的 `Restricted` 是终态（账户自己 reduce-only）。Phase 2：

- `Restricted` 账户在每笔可能触碰它的操作（成交、funding tick 的 lazy settle、keeper 的清算）中会被 reconcile；
- 若 `equity ≥ MM` → 转回 `Normal`；
- 若 `equity < MM` 且可被清算 → 走 8.3；
- 若因 Lack of liquidity 暂时无法清算 → 保持 `Restricted` 并 emit `LiquidationBlocked`。

## 9. 多市场与子账户（P2-C）

### 9.1 键结构一次性变更

讨论稿第 15 节把「是否一开始就支持多子账户」列为开放问题；本文决策为：**Phase 2 就把 key 换成 `(AccountId, SubaccountId)`，但初期仅通过 migration 使用 `SubaccountId = 0`**，避免 Phase 3 再做一次全量 migration。

```rust
Accounts<(AccountId, SubaccountId), Account>
Positions<(AccountId, SubaccountId, MarketId), Position>
UserOrderByClientId<(AccountId, SubaccountId), ClientOrderId>
SubaccountIndex<AccountId, BoundedVec<SubaccountId, MaxSubaccountsPerAccount>>
```

`SubaccountIndex` 支持有界地遍历某账户的全部子账户（避免直接使用无界的 Prefix 迭代器）。

迁移：所有存量 `Accounts/Positions/UserOrderByClientId` 加 `(0)` 维度，所有存量 `Position` 的 `realized_pnl` 折入 `collateral`（第 7.3 节）。

### 9.2 跨市场风险聚合

同一 `(Account, Subaccount)` 内**跨市场全仓**：

```text
initial_requirement      = Σ_markets |position_notional| × IMR + reserved_open_order_margin
maintenance_requirement  = Σ_markets |position_notional| × MMR
```

由于 storage key 前缀已知，可用严格有界的 SysteMc;**遍历上限 = `MaxClearingMarkets`**：当账户仓位市场数超过该上限时：

- 拒绝新增市场仓位（`TooManyClearingMarkets`）；
- 而非静默只聚合前 N 个（会低估风险）。

这是一个**安全优先的限制**——宁可禁止账户再开新市场，也不曾风险被低估。

### 9.3 费率等级

```rust
pub struct FeeTier {
    pub maker_fee_ppm: FeePpm,
    pub taker_fee_ppm: FeePpm,
    pub maker_rebate_ppm: FeePpm,   // 负费率：maker 返佣
}
FeeTiers<FeeTierId, FeeTier>
AccountFeeTier<(AccountId, SubaccountId), FeeTierId>
```

Phase 1「maker fee 不得为负」的限制在 Phase 2 放开，但返佣必须从 `FeePool` 支出并参与守恒：`FeePool` 可能因返佣而下溢 → 必须定义了 InsufficientFeePool 错误，并由 InsuranceFund 或治理补充。这是返佣的安全前提。

## 10. Storage 变更总表与迁移

| 项 | 变更 | 批次 |
|---|---|---|
| `Position` | 新增 `funding_index: SignedQuote`、`realized_pnl_settled: SignedQuote` | P2-A/B |
| `Position.realized_pnl` | 语义改为累计统计量，不再参与 equity | P2-B |
| `Account` | 不变（新增 `fee_debt` 可能用于 funding 欠费） | — |
| `Market` | 新增 `net_oi_lots: i64`、`funding_index: SignedQuote`、`last_funding_at`、`rate_ppm`、`impact_mid_ticks` | P2-A |
| `Accounts/Positions/UserOrderByClientId` | key 加 subaccount 维度 | P2-C |
| 新增 `SubaccountIndex` | — | P2-C |
| 新增 `FundingHistory`（环形） | 用于 UI 与审计 | P2-A |
| 新增 `ClearingMarketCount` | 每个 (account,sub) 占用了多少市场，用于有界读取 | P2-C |
| `pallet-oracle` | 全部新增 | P2-A |
| `pallet-insurance` | 全部新增 | P2-B |
| `pallet-liquidation` | 配置与 ADL 日志 | P2-B |
| `pallet-bridge` | 全部新增 | P2-F |

**迁移要求：**

1. 每个 Phase 2 batch 一个版本号 + 一个 migration 函数，`try-runtime` 必须通过；
2. P2-B 的 `realized_pnl` 折入必须在 7.3 节语义生效的同一版本完成，不可拆分；
3. P2-C 的 key 变更是全量迁移，必须在测试网先用全量状态快照演练，并测量 migration weight 是否超出单块限制（若超出，改为 lazy migration + `MultiStepMigrator`）。

## 11. Dispatchable 接口

### 11.1 `pallet-exchange-core` 变更/新增

| call index | 调用 | 权限 | 说明 |
|---:|---|---|---|
| 0–10 | Phase 1 原有 | — | 保持 call index 与 SCALE 编码不变 |
| 11 | `settle_position(subaccount, market)` | Signed | 显式结算 PnL/funding，可由任何人代/付款账户调用 |
| 12 | `update_funding(market)` | Signed | 到期的 index 累积，无需新 OSOrganizer 也可由 on_initialize 调用 |
| 13 | `set_impact_mid(market)` | ForceOrigin | 测试网覆盖；生产由 oracle 派生 |
| 14 | `create_subaccount(subaccount)` | Signed | 受 `MaxSubaccountsPerAccount` 限制 |
| 15 | `transfer_between_subaccounts(from, to, amount)` | Signed | 内部划转，不触碰 custody |

Phase 1 的 call index 0–10 必须**保持不变**，否则所有已发布的前端/Indexer/工具链会中断。

### 11.2 `pallet-oracle`

| index | 调用 | 权限 |
|---:|---|---|
| 0 | `submit_prices(prices)` | Oracle signer |
| 1 | `set_source(signer, enabled)` | ForceOrigin |
| 2 | `set_quorum(count)` | ForceOrigin |
| 3 | `force_set_index_price(market, price)` | ForceOrigin + multisig（生产限用） |
| 4 | `restore_market_status(market)` | ForceOrigin（**) 不得自动恢复） |

### 11.3 `pallet-liquidation`

| index | 调用 | 权限 |
|---:|---|---|
| 0 | `liquidate(target_subaccount, market)` | Permissionless keeper |
| 1 | `liquidate_batch(targets)` | Keeper，`≤ MaxLiquidationsPerCall` |
| 2 | `adl_close(targets)` | Keeper，验证通过才执行 |
| 3 | `set_liquidation_params(params)` | ForceOrigin（延迟生效） |

### 11.4 `pallet-insurance`

| index | 调用 | 权限 |
|---:|---|---|
| 0 | `contribute(amount)` | Signed（自愿出资，可选凭证） |
| 1 | `harvest_fees()` | ForceOrigin（按 `InsuranceFeeSharePpm`） |

### 11.5 `pallet-bridge`

见第 14 节。

## 12. 阶段 B：业务感知交易池、P2P 与 proposer 排序（P2-D）

### 12.1 总体流水线

```text
客户端签名 ExchangeAction{ subaccount, action_nonce, action, signature }
   │
   ▼
① Exchange 交易池（节点）：验签、action nonce、Runtime API 预校验、优先级打分
   │
   ▼
② 专用 P2P 传播：去重、背压、按 market/account 限流
   │
   ▼
③ Babe proposer：确定性排序
      非风险动作(settle/update_funding) → 撤单 → 下单/修改单
   │
   ▼
④ 有界 Exchange Batch extrinsic
   │
   ▼
⑤ pallet-exchange-core 确定性重放（与 Phase 1 semantics 一致）
```

### 12.2 交易池预校验

新增 Runtime API：

```rust
sp_api::decl_runtime_apis! {
    pub trait ExchangeRuntimeApi {
        fn validate_exchange_action(action: ExchangeAction) -> Result<(), DispatchError>;
        fn action_weight(action: &ExchangeAction) -> Weight;
        fn sort_key(action: &ExchangeAction, market: MarketId) -> SortKey;
    }
}
```

预校验**只用于提升吞吐与用户反馈**，Runtime 重放仍是唯一权威验证。验签/action nonce 在节点侧避免重复 offer；但即使节点实现有 bug，Runtime 也必须完整重放验证。这与讨论稿第 8.4 节一致。

### 12.3 排序规则（必须写入 Runtime 测试）

```text
SortKey = (action_class, action_nonce per (subaccount), arrival_index)
action_class: Other(non-risk) < Cancel < Place
```

- 同类别按 proposer 接收顺序的 `arrival_index`；
- 同 `subaccount` 按 `action_nonce` 严格递增；
- `arrival_index` 只对本地 view 有意义，但只要每个 proposer 用它给出序列，所有验证者重放同一个 batch 的结果就完全一致。

排序规则的 test vectors 必须作为 Runtime 集成测试的一部分，防止节点升级后 ordering 悄悄漂移。

### 12.4 `Exchange Batch` extrinsic

```text
execute_exchange_batch(
    actions: BoundedVec<SignedExchangeAction, MaxActionsPerBatch>,
    author: AccountId,          // block author，用于 weight refund 与返佣
)

**实现偏离（2026-09-18 确认，非临时）：批次没有 `author`，也没有出块者激励。**

批次改为 **unsigned + proposer 注入**（§12.4 后半段：出块密钥签不了
extrinsic，授权来自每个 action 自己的签名）。由此：

- 没有人签名 → 没有人付费 → `author` 参数的**原用途（weight refund）失效**
- 因此**不加 `author` 参数**；`ExchangeBatchExecuted` 事件也未实现

剩下的真问题是"出块者打包这个免费批次有没有激励"。**当前决定：不给**。
理由：
- 批次是出块者**自己构造**的，边际成本很小（和自己打包区块一样）
- 若给激励，需要定来源（FeePool？增发？）与数量，属于经济参数设计，
  在没有真实交易量数据前定了也只是猜
- 不给的话风险可控：最坏情况是出块者不打包批次 → 表现为**交易延迟**，
  而不是资金丢失或状态错误 —— 可见、可观测、可事后补救

若将来出现"批次长期不被打包"的实测证据，再回来定激励方案。届时需要：
加 `author` 参数、补 `ExchangeBatchExecuted` 事件、定激励来源。
runtime 已有 `pallet_authorship`，技术上可拿到出块者。
```

- 每个 action 保留用户签名，authorization 不依赖 batch 提交者；
- batch 总 weight/编码大小/单市场比例都有上限；
- 单个 action 失败时按 `continue-on-error` 还是 `abort-batch` 必须明确：**撮合类 action 失败一律跳过并发出 `ExchangeActionFailed`，不要把整个 batch 回滚**——否则一个用户对 \* 个动作具有 DoS 整个批次的能力（这比 Phase 1 `execute_batch` 更宽松，是因为 batch 由 proposer 构造、包含多用户动作，语义不同于用户自己构造的 batch）。
- 例外：`deposit/withdraw/settle` 这类资金动作仍必须原子。

### 12.5 配额与 wardrobe 保护

通过交易专属 block weight/length 配额防止 staking/治理挤占订单簿容量：

```text
ExchangeBlockWeightQuota = 0.5 × max_block_ref_time
ExchangeBlockLengthQuota = 0.4 × max_block_length
```

超限则 proposer 不再打包 Exchange action。这些值必须经 benchmark 校准，并在 Phase 1 已有的 `ExchangeOperationalWeightBudget` 基础上扩展。

### 12.6 分阶段落地建议

不要一次性实现完整流水线，建议：

1. **B1**：保留普通 extrinsic，只在交易池实现优先级与限流（低风险，可回退）；
2. **B2**：proposer 侧确定性排序（同一批 extrinsic 排序优化）；
3. **B3**：引入 `execute_exchange_batch` 合并打包（收益最大，风险也最大）。

每一级都必须有独立的 perf 报告与 rollback 开关。

## 13. Runtime API、Indexer 与可运营性（P2-E）

### 13.1 Runtime API

```rust
fn market_snapshot(market: MarketId) -> Option<MarketSnapshot>;
fn account_state(subaccount: (AccountId, SubaccountId)) -> Option<AccountState>;
fn orderbook_page(market, side, depth: u32) -> Vec<(PriceTicks, Lots)>;
fn funding_preview(market) -> FundingPreview;
fn liquidation_candidates(market, limit: u32) -> Vec<(AccountId, SubaccountId)>;
```

全部返回必须**有界**，禁止 unbounded iteration。

### 13.2 Indexer 边界

- Indexer 是**只读投影**，不得成为余额/订单/清算的权威来源；
- 遇到重组以**链上块哈希**为准回滚；
- 前端涉及资产与订单最终状态时**必须查链上**；
- HTTP 提供快照，WebSocket 提供增量：订单簿增量、成交流、K 线、用户订单/仓位/资金记录。

### 13.3 监控与风控告警

最小告警集：

| 指标 | 阈值来源 |
|---|---|
| oracle source staleness / quorum 不足 | 第 5.5 节 |
| funding rate 持续触及上下限 | `FundingRateMaxAbsPpm` |
| InsuranceFund 余额低于 `MinInsuranceReserve` | 风控设定 |
| 单市场 net OI 与 notional 敞口 | `MaxMarketOiLots` |
| 单块撮合数与 batch action 数 | benchmark baseline |
| 清算失败/受阻计数 | `LiquidationBlocked` 事件计数 |
| ADL 执行次数 | 任何 ADL 都应触发人工复盘 |

讨论稿第 12 节要求操作手册；本 swallowed：**重大参数变更必须延迟生效**，并发出明确事件。

## 14. 首条 EVM 生产桥（P2-F）

生产桥是**独立的高风险系统**，必须与 Perp 核心分别审计。讨论稿第 10.3 节已明确：多签/MPC 只能用于测试，不得包装为去信任生产桥；轻客户端证明是生产目标架构。Hyperliquid 也将 Arbitrum 桥的多签阈值、提款争议期与桥锁定纳入协议，说明桥不能作为简单充值接口对待。[Hyperliquid Bridge](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/bridge)

### 14.1 `pallet-bridge` 状态机

```text
存款：EVM SunBridgeVault 事件
   → submit_deposit_proof{ source_chain_id, vault, deposit_id, amount, recipient }
   → 验证：chain 允许、vault 白名单、日志匹配、重放检查
   → mint USDC.e → 用户 spot 余额
   → exchange-core.deposit_collateral 仍需用户显式操作

提款：exchange-core.withdraw_collateral → spot
   → request_withdrawal{ amount, dest_chain_id, dest_address }
   → burn/lock USDC.e，创建 WithdrawalRequest{ status: Pending }
   → prove finality（这边由所选证明方案决定）
   → confirm_withdrawal{ request_id } 释放
```

### 14.2 必做的安全要求

- 每条 entry 唯一标识：`source_chain_id + vault + deposit_id/nonce`；
- 仅在源链达到所选最终性后才记账；
- 将有金额为 `(chain_id, vault)` 维度的每日/单笔限额；
- `pause_bridge` 与 emergency multisig 必须可快速生效；
- 必须处理 USDC 合约冻结/升级、ERC-20 非标准行为、EVM 重组；
- 任何外部链的 gas 费用、relayer 成本与失败重试，都必须有明确定价与资金来源。

### 14.3 上线门槛

禁止「多桥同时上线」：首条链单独上线且稳定运行一个观察期后，才逐个扩展。生产桥的上线需要独立的威胁模型、审计与运营演练文档，本文不替代该文档。

## 15. 事件、错误与可观察性

### 15.1 新增事件

```rust
// P2-A oracle
IndexPriceUpdated { market_id, price_ticks, source_count, at }
MarkPriceUpdated  { market_id, price_ticks, clamped: bool, at }
OracleSourceRejected { signer, market_id, reason }
MarketAutoDowngraded { market_id, from, to, reason }

// P2-A funding
FundingSettled { market_id, rate_ppm, unit_payment, index, residual }
FundingPaid { subaccount, market_id, amount }

// P2-B liquidation / insurance
PositionLiquidated { target, market_id, lots, avg_price_ticks, penalty, insurance_delta }
PartialLiquidationSkipped { target, reason }
LiquidationBlocked { target, reason }
BadDebtRealized { target, amount, covered_by_insurance }
InsuranceFundDepleted { shortfall }
AutoDeleveraged { target, market_id, lots, price_ticks, reason }

// P2-B settlement
PositionSettled { subaccount, market_id, realized_delta, funding_delta }

// P2-C
SubaccountCreated { owner, subaccount }
SubaccountTransferred { owner, from, to, amount }

// P2-D
ExchangeBatchExecuted { author, action_count, failed_count, weight_consumed }
ExchangeActionFailed { index, reason }
```

禁止把完整订单簿快照、内部指针或全倉账号列表放进事件。

### 15.2 新增错误

```text
OraclePriceUnavailable, OracleQuorumNotMet, OracleSourceNotWhitelisted,
OracleSignatureInvalid, OracleSourceStale, MarketAutoDowngraded,
FundingIndexUnavailable, FundingRateOutOfBounds, InsufficientFeePool,
NotLiquidatable, LiquidationSlippageExceeded, NoLiquidityForLiquidation,
AdlNotEligible, TooManyAdlTargets, BadDebtUncovered,
TooManySubaccounts, TooManyClearingMarkets, SubaccountNotFound,
InsuranceFundInsufficient, BridgeChainDisabled, BridgeVaultNotWhitelisted,
BridgeDepositAlreadyProcessed, BridgeWithdrawalNotFound, BridgeLimitExceeded
```

`InvariantViolation` 仍只能表示不可恢复的内部矛盾，且必须通过测试证明外部输入不可触发。

## 16. Weight、PoV 与区块预算

### 16.1 新增 weight 签名

```rust
fn settle_position() -> Weight;
fn update_funding() -> Weight;
fn liquidate(matches: u32, markets: u32) -> Weight;
fn liquidate_batch(accounts: u32, matches: u32) -> Weight;
fn adl_close(targets: u32) -> Weight;
fn execute_exchange_batch(actions: u32, matches: u32) -> Weight;      // P2-D
fn submit_prices(count: u32) -> Weight;                               // P2-A
fn aggregate_prices(markets: u32) -> Weight;                          // on_initialize
```

`liquidate` 必须按实际 `matches` 结算 weight 并 refund，禁止用常数覆盖撮合循环（延续 Phase 1 第 11.1 节要求）。

### 16.2 `on_initialize` 预算

`pallet-oracle` 的聚合放在 `on_initialize`，循环上界为活跃市场数。必须：

- 计入 `BlockWeights` 的 mandatory class 预算；
- 用 benchmark 确定单市场聚合成本，并对 `MaxMarkets` 反推；
- 禁止在 `on_initialize` 中遍历账户或执行清算（讨论稿第 12 节表已规定 Root 更新价格不得清算）。

### 16.3 benchmark 矩阵新增

| benchmark | 变量 |
|---|---|
| `oracle_submit_prices` | `count = 1..MaxOracleSources` |
| `oracle_aggregate` | `markets = 1..MaxMarkets` |
| `settle_position` | funding 累积期数（1、10、上限） |
| `liquidate` | `matches = 1..MaxMatchesPerAction`、全仓 vs 部分仓 |
| `liquidate_batch` | `accounts × matches` 二维边界 |
| `adl_close` | `targets = 1..MaxAdlMatchesPerCall` |
| `execute_exchange_batch` | `actions = 1..MaxActionsPerBatch` |

## 17. 生命周期与边界场景

| 场景 | 规定行为 |
|---|---|
| Oracle quorum 不足 | 市场降级 ReduceOnly → CancelOnly → Paused；**期间禁止清算** |
| Index/Mark 分歧过大 | 标记 Abnormal → CancelOnly；需治理恢复 |
| Mark price 陈旧 | 拒绝扩大风险、下单、提款；允许撤单与 reduce-only |
| 账户低于 MM | permissionless 可被部分清算至 MM+buffer |
| 账户净值 ≤ 0 | 保险基金覆盖；不足则 ADL |
| 保险基金耗尽 | emit 告警；未覆盖的敞口进入待处置队列并等待 ADL/治理处理，不得隐性社会化给所有用户 |
| funding 长期单侧 | 触及 `FundingRateMaxAbsPpm` 并告警，评审参数 |
| 目标账户有多个市场仓位 | 聚合读取，超过 `MaxClearingMarkets` 禁止新市场仓位 |
| 子账户迁移 | 所有存量数据 key 加 `(0)`，一次性完成并有 try-runtime 验证 |
| 迁移 weight 超单块 | 改用 `MultiStepMigrator` 分步迁移，链上运行期间不得暂停 |
| 桥临时暂停 | Perp 核心继续运行（只影响充值/提款），二者解耦 |

## 18. 测试计划与验收

### 18.1 继承 Phase 1 的门禁

Phase 1 的 49 个单元测试、属性测试与守恒不变量必须**继续保持通过**。特别是新增 funding/insurance 后，守恒式变为：

```text
custody balance == Σ Account.collateral + FeePool + InsuranceFund
```

任何关于 account 估值 = 0 的旧断言需要更新。

### 18.2 P2-A 专项

- 报价缺失/延迟/乱序/签名无效均得到正确处理；
- quorum 边界（`MinQuorum ± 1`）不更新价格且正确降级；
- mark price 限幅：单 tick 最大跳幅、长期漂移后强制对齐；
- funding 在正/负/零费率下的支付方向正确；净 OI 非零时 residual 正确入/出保险基金；
- lazy settlement：不同 Bush{N} 期未触碰的账户一次性结算正确；
- **不能清算**：在价格不可用时 `liquidate` 必须失败。

### 18.3 P2-B 专项

- 部分清算把账户恰好带到 MM+buffer；
- 分母 ≤ 0 的情形走全额平仓；
- 滑点超限 → `LiquidationSlippageExceeded` 且不改变状态；
- 罚金分配：creator 返佣 + insurance credit 守恒；
- 破产链：equity→0→insurance 不足→ADL；
- ADL 验证拒绝不符合条件的候选（`unrealized_pnl ≤ 0` 同侧、超上限）；
-  conservation：全部过程中 custody 恒等式成立。

### 18.4 P2-C 专项

- 迁移前后 `Σ collateral` 完全一致（含 realized_pnl 折入）；
- 子账户 key 变更后所有 Phase 1 测试用例重跑；
- `MaxClearingMarkets` 边界：第 9 个市场被拒绝而非静默忽略。

### 18.5 P2-D 专项

- 排序规则 test vectors：不同 action class/nonce/arrival 下的确定序列；
- 重组：**同一组 action 以不同顺序入池，最终 batch 内容必须一致**；
- 抗审查：单市场/单账户限流不得导致动作永久丢失；
- 网络分区后的 pool 收敛；
- 三口径延迟分别报告：入池、被包含、Grandpa 最终确定。

### 18.6 Phase 2 验收门槛

1. Phase 1 全部测试 + 本文 18.2~18.5 全部通过；
2. `try-runtime` 在快照主网/测试网状态上通过所有 migration；
3. benchmark 生成真实 weights，并执行一周以上测试网压力/重组/恢复演练；
4. 清算与 ADL 在受控故障注入（人为推高/压低 mark price）下不产生 custody 失恒；
5. Indexer 重组回滚演练通过，且前端总资产校验一致；
6. Perp 核心与桥分别通过外部审计（若本文 Prelim 涉及真实资产）。

## 19. Runtime 接线清单与实施顺序

```text
1. P2-A：新增 pallet-oracle + PriceOracle trait；exchange-core 在入口读 cache 价格
2. P2-A：Market 加 net_oi/funding 字段；实现 funding.rs 与 asset migration
3. P2-B：语义变更——settle_position、realized_pnl 折入 collateral（必须同批次）
4. P2-B：pallet-insurance；LiquidationKeeper 接入 exchange-core 受控入口
5. P2-B：pallet-liquidation：部分清算、keeper permissionless、ADL keeper 提案
6. P2-C：key 加 subaccount 维度 + 全量 migration + try-runtime
7. P2-C：MaxClearingMarkets 有界风险聚合 + 费率等级（含负 maker fee）
8. P2-D：B1 交易池优先级 → B2 排序 → B3 Exchange Batch（分级上线）
9. P2-E：Runtime API、RPC、Indexer、监控告警
10. P2-F：pallet-bridge（可独立并行，单独审计）
```

每一步都应有独立的 benchmark 与回滚路径；**禁止把 P2-B 的语义变更拆开上线**。

## 20. 待确认参数与开放问题

以下问题在产品/风险/链团队确认前不得进入实现：

1. 生产桥首发链：Ethereum L1 还是 Arbitrum？（讨论稿第 15 节未决）
2. 生产桥证明模型：轻客户端 vs ZK vs 受限委员会；谁承担桥损失与外链 gas？
3. 每个上线市场的初始 IM/MM、最大杠杆、`MaxMarketOiLots` 与 tick/lot 参数分别是多少？
4. Oracle 报价源清单、更新频率、`OracleMinQuorum`、`ExtremeDivergencePpm` 具体数值？
5. `FundingPeriodBlocks`（讨论稿建议小时级）与 `FundingRateMaxAbsPpm` 的项目群确认值？
6. `LiquidationPenaltyPpm` 中清算人返佣与保险基金的比例？
7. 是否允许负 maker fee（返佣）；若允许，`FeePool` 下溢的处理与资金来源？
8. `MaxClearingMarkets` 上限与「拒绝新市场仓位」的 UI/运营处理？
9. ADL 是否第一期就启用，还是先用 insurance + 治理兜底？
10. `execute_exchange_batch` 的 `continue-on-error` 是否对所有 action 类别适用（第 12.4 节已给出方向，需确认）？
11. Indexer 是否需要 IPvSS 级别的 DoS 防护与付费 API？
12. benchmark 硬件、Wasm 执行模式与目标负载模型的最终定义？

**任何真实 USDC、生产跨链桥、自动清算的启用，都必须在本详设之外经过单独设计、故障注入演练与审计。**

## 附录：与 Phase 1 的行为差异清单

| 行为 | Phase 1 | Phase 2 |
|---|---|---|
| Mark Price | Root 手工设置 + 有效期 | Oracle 多源聚合 + 限幅 + 自动降级 |
| 资金费率 | 无 | 累积 index，触碰结算，残值入保险基金 |
| 已实现 PnL | 记在 Position，参与 equity | 立即折入 collateral，退化为统计量 |
| `equity` 公式 | `collateral + realized + unrealized − fee_debt` | `collateral + unrealized − fee_debt` |
| 低于 MM | 标记 `Restricted` 冻结 | permissionless 部分清算 → 破产 → 保险 → ADL |
| 提款的严格性 | 按名义 collateral | 按已结算 collateral（扣除了亏损与 funding） |
| 保险基金 | 不存在 | `pallet-insurance`，罚金/费用分成/funding 残值 |
| 账户维度 | `(AccountId)` | `(AccountId, SubaccountId)` |
| 市场数 | 1 | 多（受 `MaxClearingMarkets` 有界聚合限制） |
| maker fee | 不得为负 | 可负（返佣），受 `FeePool` 守恒约束 |
| 交易路径 | 普通 extrinsic | 阶段 B 交易池 → 确定性排序 → 批量重放 |
