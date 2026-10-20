# Sunchain 订单簿 DEX（Perpetual）设计讨论稿

> 状态：讨论基线（Draft）  
> 目的：为后续需求澄清、Pallet 设计、节点设计、安全审计和开发排期提供同一份参考。  
> 非目标：本文不是最终技术规格，也不授权直接按本文上线主网。

## 1. 背景与目标

Sunchain 已具备 Solochain Runtime、Babe/Grandpa 共识、staking、原生余额及 Frontier EVM 兼容能力。本项目希望在此基础上建设类似 dYdX v4 体验、并以 Hyperliquid 的性能水位为长期对标对象的订单簿型 DEX；首要产品方向确定为 **USDC 保证金永续合约（Perpetual DEX）**，而非现货 AMM。

需要同时满足以下原则：

- 订单、成交、仓位、保证金、清算结果由链上 Runtime 确定性执行；
- 价格优先、时间优先，撮合和资金变更可审计、可重放；
- 任一交易、价格档和区块的 Runtime 工作量必须有确定上界；
- 前端可获得实时订单簿、成交、账户和市场数据；
- 保留现有 EVM 兼容性，但不维护一套重复的 Solidity DEX 状态。
- 用户能从以太坊及 EVM 兼容链安全地充值、并可将可用资金提回指定 EVM 链。

本文中“类似 dYdX v4”指订单簿永续合约、链上风险结算和链下低延迟数据服务的组合，并不意味着第一期复制其所有短期订单或验证者内存订单簿机制。

Hyperliquid 目前披露的约 20 万 orders/s 和约 0.2 秒中位端到端延迟，来自其从头构建的 HyperBFT、网络和专用 HyperCore 执行引擎；其订单簿交易池及共识逻辑也具有业务语义。[HyperCore Overview](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/overview) [Order book](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/order-book) 因此这不是在现有 3 秒 Babe/Grandpa + 通用 FRAME extrinsic 路径上“调几个参数”即可达到的目标。本项目采用“性能对标、分层逼近”的表述，而不在一期承诺相同绝对 TPS 或延迟。

## 2. 对当前 Sunchain 的判断

当前 Runtime 中已有 `pallet_balances`、Babe、Grandpa、staking、`pallet_evm` 和 `pallet_ethereum`；区块时间配置约为 3 秒。自定义业务目前仅有教学用的 `pallet-template`。

当前尚缺：

- 多资产与稳定币抵押能力；
- 子账户、保证金、仓位、已实现/未实现 PnL；
- 市场配置、订单簿、撮合和费用；
- 可信预言机、资金费率、清算、保险基金；
- EVM 兼容链的资金桥、跨链消息验证及提款执行；
- 与上述逻辑对应的基准测试、权重和攻击面测试。

因此一期以链上交易核心和可量化性能基线为主，Indexer 不是前置交付物。链上连续撮合可作为低到中频、强一致的 MVP；低延迟短期订单需要后续节点层工程。

## 3. 分阶段范围

### Phase 0：设计与测试环境

- 明确法务、地理限制、资产托管/桥接方式和治理模型；
- 本地链或测试网发行测试 USDC；
- 确定单一市场 `BTC-USD` 的精度、tick、数量步长、手续费和杠杆参数；
- 定义安全不变量及测试场景。

### Phase 1（实施阶段 A）：高性能链上订单簿 Perp 核心

- 单抵押资产（USDC）、单子账户或单一全仓账户；
- 一个市场起步，支持 Limit GTC、Limit IOC、带滑点上限的 Market IOC、Post-only、Reduce-only；
- 链上价格优先/时间优先撮合，撤单和批量撤单；
- 初始保证金检查、仓位/PnL/手续费结算；
- 管理员市场暂停开关；
- `pallet-assets` 中的桥接 USDC 表示、充值/提款请求状态机；
- 订单引擎 benchmark、区块级压测、profiling 和性能回归门禁。

一期明确不做：组合保证金、多抵押资产、期权、复杂条件单、FOK、ADL、做市激励、生产级多链桥和验证者 memclob。Indexer 仅在开发调试中可有最小事件订阅器，不作为产品或架构依赖。

### Phase 2（实施阶段 B）：风险闭环、专用交易池与可运营性

- 多源预言机、指数价、Mark Price、资金费率；
- 维持保证金、部分清算、保险基金、坏账处置；
- 多市场、多个子账户、费率等级及治理参数；
- 专用订单动作交易池、P2P 传播与 Babe proposer 的确定性区块排序；
- 首条 EVM 链（建议 Arbitrum 或 Ethereum）的生产级桥接；
- Runtime API、Indexer、监控、风控告警和操作手册。

### Phase 3：远期备查——专用低延迟共识

- 可能的 HotStuff 类专用 BFT、验证者投票与快速最终性；
- 可能的 memclob 与专用网络协议；
- EIP-712 离线订单、授权 relayer 或 EVM precompile。

在首条桥达到安全、可运营标准后，逐条扩展到其他 EVM 兼容链；不同时上线多个异构桥。

Phase 3 只记录架构方向，**不纳入当前设计、排期或验收范围**。它是节点/共识客户端工作，不是增加一个 Pallet 即可完成；仅当 A、B 完成并证明最终性延迟而非执行引擎已成为主要瓶颈时，才重新立项评估。

## 4. 总体架构

```text
用户钱包 / 前端 / 做市机器人
     │  Runtime extrinsic；EVM 链经 Bridge Vault 充值/提款
     ▼
Sunchain Runtime
 ├── pallet-assets        抵押资产
 ├── pallet-bridge        EVM 消息验证、USDC.e 铸造/销毁
 ├── pallet-exchange-core 市场、保证金、仓位、订单簿、撮合与费用（阶段 A）
 ├── pallet-oracle        指数价格 / Mark Price / funding 输入
 ├── pallet-liquidation   清算执行
 ├── pallet-insurance     保险基金及坏账覆盖
 └── pallet-fees          后续可拆分的 maker/taker 费用与归集
     │ events / runtime API
     ▼
Phase 2：Indexer ── PostgreSQL/分析库 ── HTTP + WebSocket ── 前端与机器人
```

一期链上负责全部不可替代的状态转换，前端可通过标准节点 RPC、链上 events 和存储查询获得最低限度数据。Indexer 在 Phase 2 引入，只负责查询、聚合和低延迟推送，不能成为余额、订单或清算结果的权威来源。

## 5. Pallet 边界与依赖

| Pallet | 职责 | 不应承担的职责 |
|---|---|---|
| `pallet-assets` | USDC 等抵押资产的余额、冻结和转账 | 订单或仓位计算 |
| `pallet-markets` | 市场创建、精度、费率、杠杆、交易状态 | 用户余额修改 |
| `pallet-clearing` | 子账户、保证金、仓位、PnL、订单保证金占用 | 价格档排序 |
| `pallet-orderbook` | 订单生命周期、FIFO 队列、撮合指令 | 直接转移资产 |
| `pallet-oracle` | 报价提交、聚合、有效期和价格快照 | 清算策略 |
| `pallet-liquidation` | 可清算判定、接管仓位或平仓 | 普通订单撮合 |
| `pallet-insurance` | 保险基金、坏账覆盖 | 常规费用结算 |

依赖方向应保持为：

```text
assets → clearing ← oracle
             ↑        ↑
          orderbook → markets
             ↓
        liquidation → insurance
```

`pallet-orderbook` 仅通过 `pallet-clearing` 的 trait（如 `reserve_for_order`、`apply_fill`、`release_order_margin`）申请或结算保证金，不直接读写资产余额。这样风险引擎可独立测试，未来也可替换抵押资产实现。

性能关键路径采用“**逻辑分层、物理合并**”原则：`markets`、`clearing`、`orderbook` 和费用结算可在代码中保持独立模块及 trait 边界，但 Phase 1 的实际执行应优先落在同一个 `pallet-exchange-core` crate/单次 dispatch 内。这样避免跨 Pallet 的重复编码、额外 storage 往返与中间状态，同时不牺牲模块化测试。`oracle`、`bridge`、`liquidation` 和 `insurance` 保持独立 Pallet。

## 6. 核心状态模型

所有金额均使用整数或定点数；Runtime 禁止 `f32` / `f64`。价格以 `price_ticks` 表达，数量以 `quantity_lots` 表达，乘除以 `U256` 中间值计算并统一规定取整方向。

```text
Market
  id, base_asset, quote_asset, tick_size, step_size,
  initial_margin_ratio, maintenance_margin_ratio,
  maker_fee_ppm, taker_fee_ppm, max_leverage, status

Order
  id, owner, subaccount, market_id, side,
  price_ticks, remaining_lots, order_type, time_in_force,
  post_only, reduce_only, prev_at_level, next_at_level, created_at

PriceLevel
  head_order, tail_order, total_lots, order_count,
  prev_price, next_price

Position
  quantity, entry_price, realized_pnl, funding_index, open_order_margin
```

建议的主要 storage：

- `Markets<MarketId, Market>`；
- `Orders<OrderId, Order>`；
- `PriceLevels<(MarketId, Side, PriceTicks), PriceLevel>`；
- `BestBid<MarketId>`、`BestAsk<MarketId>`；
- `UserOrders<(AccountId, SubaccountId), BoundedVec<OrderId>>`；
- `Collateral<(AccountId, SubaccountId, AssetId), Balance>`；
- `Positions<(AccountId, SubaccountId, MarketId), Position>`。

价格档内采用双向链表 FIFO 队列。不要把无限订单 ID 存入单个 `Vec`；否则撤单会线性移动元素，单个 storage value 也可能突破 PoV 和 weight 限制。价格档同样维护相邻价格指针与最优买卖价，以避免无界扫描。

## 7. 订单与撮合规则

### 7.1 用户调用

一期建议提供：

- `place_order(market, side, price, quantity, tif, flags)`；
- `cancel_order(order_id)`；
- `cancel_all(market: Option<MarketId>)`；
- `deposit(asset, amount)` 与 `withdraw(asset, amount)`；
- 管理调用：`create_market`、`set_market_status`、`set_risk_params`。

### 7.2 连续撮合

```text
买单：按最优 ask 到较差 ask 的顺序撮合，价格不得高于限价。
卖单：按最优 bid 到较差 bid 的顺序撮合，价格不得低于限价。
同价：按入队顺序成交。
成交：清算层原子更新 maker/taker 仓位、已实现 PnL、保证金、费用和订单剩余量。
剩余：GTC 订单进入价格档尾部；IOC 及市价 IOC 取消剩余部分。
```

市价单必须携带最大可接受滑点（等价于保护性限价），否则在薄盘口中存在不可接受的执行风险。`Post-only` 若会立刻成交则拒绝；`Reduce-only` 不得扩大绝对仓位。

### 7.3 可证明的资源上限

每个会遍历的路径都必须配置有界常量，并出现在 weight 公式和 benchmark 中：

- `MaxMatchesPerOrder`；
- `MaxOpenOrdersPerAccount`；
- `MaxPriceLevelsPerMarket`；
- `MaxOrdersPerPriceLevel`；
- `MaxCancelsPerCall`。

超过 `MaxMatchesPerOrder` 的 GTC 订单可按明确规则挂入簿中或拒绝；IOC 则取消未成交余量。第一期不建议加入 FOK，除非先实现有界预扫描、原子执行和完整的重量证明。

## 8. 性能目标与演进路线

### 8.1 对标含义与不可妥协项

长期对标 Hyperliquid 的不是某个宣传 TPS 数字，而是以下可验证能力：高吞吐订单/撤单、低且稳定的确认延迟、价格时间优先、订单簿/保证金/清算的一致原子状态，以及高负载下没有“服务端账本”。Hyperliquid 的公开架构将订单簿和保证金放在专用 HyperCore，并使用语义感知的 mempool 与共识排序；其公开指标约为 200k orders/s、同机房客户端中位端到端延迟约 0.2 秒。[HyperCore Overview](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/overview) [Order book](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/order-book)

当前 Sunchain 的 Babe/Grandpa、约 3 秒区块时间、通用 storage 读写和通用 extrinsic 验证路径，不能直接达到该级别。尤其“一个订单一个普通 extrinsic + 每笔多次 Runtime storage 读写”的实现不可能作为高性能终局。必须把性能视为一等设计约束，而不是在功能完成后再优化。

### 8.2 分层目标

| 阶段 | 决策状态 | 可接受目标 | 关键交付与判定 |
|---|---|---|
| A：Exchange Core | 当前实施 | 在固定基准硬件和固定市场/盘口数据上，量化提交、执行、出块、最终性的 p50/p95/p99 与 orders/cancels per block；无超重、无无界路径 | 单次 dispatch 的专用执行域、可复现实验脚本、硬件规格、原始结果和性能回归阈值 |
| B：交易池与区块构建 | 当前实施，紧随 A | 在保留 Babe/Grandpa 的前提下，提高有效动作占比和包含延迟；不承诺亚秒最终性 | 专用交易池、P2P 传播、proposer 确定性排序、批量打包、重组/抗审查/网络分区测试 |
| C：专用 BFT | 仅备查，不排期 | 若未来需要亚秒级最终性，才以相同硬件和口径比较确认延迟 | 新共识、投票、validator set、同步和轻客户端；须重新立项 |

具体数字必须先由阶段 A benchmark 得出。将 Hyperliquid 的 20 万 orders/s 或 0.2 秒直接写为一期 KPI 会制造不可交付承诺；若业务必须达到该量级，应尽早立项“专用交易执行/共识客户端”而不是仅开发 FRAME Pallet。

### 8.3 阶段 A：Exchange Core

- 将 `orderbook + clearing` 视为单一专用交易执行域，避免通用 `Vec`、无界迭代、跨 Pallet 调用和 N+1 storage 读写；
- 订单和撤单采用紧凑 SCALE 编码；允许 `batch_actions`，但每一项和整批必须有上限；
- 订单与价格档使用分页订单 arena、双向 FIFO 链表和最优价指针；撤单必须 O(1) 或为严格有界操作；
- 提供账户级 `cancel_up_to_nonce` / 失效 nonce，避免用户逐笔撤销大量订单；
- 每个订单在挂单时检查保证金，在作为 resting order 被吃单时再次检查保证金；
- 为交易动作预留明确的 block weight/length 预算，禁止 staking、治理等普通业务挤占全部撮合预算；
- 聚合成交事件，避免每次内部状态变更产生大事件；
- 首期使用 1--3 个活跃市场做压力测试；多市场并行扩展前先验证状态访问模式；
- `Runtime`、节点、存储后端和网络监控均纳入 benchmark，不以 Indexer 的吞吐替代链上吞吐。

### 8.4 阶段 B：业务感知交易池与区块构建

阶段 B 不替换 Babe/Grandpa，也不引入 memclob。它将订单动作从通用交易池路径中识别出来，并在节点侧完成预校验、限流、传播与打包：

```text
客户端签名 ExchangeAction
  → Exchange 交易池：验签、账户 action nonce、市场/限额预检查
  → 专用 P2P 传播：去重、背压、按市场和账户限流
  → Babe proposer：非订单风险动作 → 撤单 → 下单/修改单 的确定性排序
  → 有界 Exchange Batch extrinsic
  → pallet-exchange-core 确定性重放
```

- 同类别按 proposer 接收顺序；同账户按 action nonce；所有排序规则写入 Runtime 测试；
- `Exchange Batch` 的动作数量、编码大小、单市场比例和总撮合量均有上限；
- 交易池预检查只用于提升吞吐和用户反馈，Runtime 重放仍是唯一权威验证；
- 通过交易专属 block weight/length 配额防止非交易业务挤占订单簿容量；
- 当前共识下应分别报告“进入交易池”“被区块包含”“Grandpa 最终确定”三个延迟，禁止将快速包含宣传为最终成交。

### 8.5 阶段 C：仅备查

如未来证据表明执行引擎和交易池均已达标，而 Babe/Grandpa 的最终性是主要限制，才评估 HotStuff 类专用 BFT、memclob 或更低延迟网络。该选择会引入共识安全、validator set、故障恢复、区块同步、轻客户端及跨链证明的全新责任；当前不为其修改 Runtime 接口或安排开发资源。

## 9. 保证金、资金费率与清算

对账户 `a`：

```text
equity(a) = collateral + Σ unrealized_pnl
initial_margin(a) = Σ |position_notional| × IMR + open_order_requirement
maintenance_margin(a) = Σ |position_notional| × MMR
```

- 下单、成交后扩大仓位、提币：必须满足 `equity >= initial_margin`；
- 持仓维持：需要 `equity >= maintenance_margin`；
- 低于维持保证金：账户进入可清算状态；
- 清算应优先部分平仓，保留清算罚金，剩余损失才由保险基金覆盖。

价格层次建议为：

- `index price`：外部现货市场的聚合报价；
- `mark price`：用于未实现 PnL 与清算防操纵；
- `oracle price`：有有效期和来源证明的可信报价；
- `funding rate`：按固定周期结算，首期可按小时。

禁止使用最后成交价直接触发清算。预言机超时、市场异常或价格偏离超阈值时，市场应自动进入仅撤单/仅减仓/暂停等受限状态。

## 10. 跨链资金与 EVM 策略

### 10.1 资产模型与范围

Perp 清算资产必须是 Sunchain 上唯一的规范资产表示，例如 `USDC.e`（由受支持 EVM 链锁定或销毁后在 Sunchain 铸造）。`pallet-assets` 负责此资产的发行/销毁/冻结；`pallet-clearing` 只接受该资产作为一期抵押品。

一期桥接范围应是 **一个资产 + 一条 EVM 链 + 双向路径**，而不是“支持任意 ERC-20 和所有 EVM 链”。建议产品和风控团队在 Ethereum L1 与 Arbitrum 之间选定一条：Ethereum 的安全和流动性更直接但成本高；Arbitrum 的 USDC 资金体验和成本通常更适合交易用户。选定后应锁定 chain id、规范 USDC 合约地址、最小金额和确认策略。

### 10.2 目标桥接流程

```text
EVM 用户钱包
  │ approve + deposit(asset, amount, sun_recipient)
  ▼
EVM SunBridgeVault：只接收白名单规范 USDC，生成唯一 deposit_id
  │ EVM 最终性证明 / 经验证的证明提交
  ▼
pallet-bridge：验证源链、合约、日志、金额、nonce 和重放保护
  │ mint USDC.e
  ▼
pallet-assets Spot 余额 ── 用户内部转入 ── pallet-clearing Perp 抵押

提款：Perp 可用余额 → Spot → burn/lock USDC.e → withdrawal_id
  → Sunchain 最终性证明 → EVM Vault release USDC 到指定地址
```

只有完成源链最终性和消息验证的充值才能记入可交易余额；提款仅可使用未被订单/保证金占用的余额。每笔充值、铸造、销毁、释放必须由 `source_chain_id + vault + nonce/deposit_id` 唯一标识，以防重放和跨链域混淆。

### 10.3 信任模型与实施顺序

| 方案 | 用途 | 信任与风险 | 结论 |
|---|---|---|---|
| 多签/MPC 验证者证明 | 内测或受限测试网 | 桥安全性取决于签名委员会和密钥运营；不是 trust-minimized | 可用于测试，不得包装为去信任生产桥 |
| 轻客户端证明 | 生产优先方案 | Sunchain 验证 EVM 终局性/事件或 EVM 验证 Sunchain 最终性；实现和维护复杂 | 目标架构 |
| ZK 证明桥 | 后续性能/成本优化 | 依赖证明系统、verifier 与电路审计 | 轻客户端成熟后评估 |
| 第三方桥/流动性网络 | 扩展链覆盖 | 新增外部合约、运营商和流动性风险 | 不作为一期核心抵押通道 |

生产桥需要处理 EVM 链重组、区块最终性、ERC-20 非标准行为、USDC 合约升级/冻结、验证者集变更、提款挑战期、限额、暂停与密钥轮换。Hyperliquid 的 Arbitrum 桥也将超过三分之二质押权重的验证者签名、提款争议期和桥锁定机制纳入协议；这说明桥接本身是独立的高风险系统，而不是简单的充值接口。[Hyperliquid Bridge](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/bridge)

### 10.4 Bridge Pallet 的边界

建议新增 `pallet-bridge`，而非把跨链逻辑放进 `clearing`：

- `register_remote_chain` / `set_remote_config`：受治理约束的链、Vault、资产和确认策略；
- `submit_deposit_proof`：验证外部最终性与事件后铸造；
- `request_withdrawal`：冻结/销毁可用资产，创建可审计请求；
- `confirm_withdrawal` / `finalize_withdrawal`：按所选证明模型完成外部释放；
- `pause_bridge`、每日/单笔限额、异常监控和恢复流程。

桥的手续费应独立于交易手续费；外链 gas 费用、relayer 成本和失败重试都必须有明确定价与资金来源。合约只接受明确的 token allowlist；前端和合约均须阻止用户将错误资产或错误链直接转入不可恢复地址。

## 11. 链下服务与 EVM 策略

Indexer 不属于一期前置范围。Phase 2 再引入独立 Indexer，订阅区块和 Runtime events，维护市场、深度、成交、订单和账户投影，并通过 HTTP/WebSocket 提供：订单簿快照与增量、成交流、K 线、市场信息、用户订单/仓位/资金记录。

Indexer 数据可重建，遇到重组时以链上块哈希为准回滚；前端涉及资产与订单最终状态时必须以链上查询为准。

现有 Frontier EVM 兼容层应保留，但订单簿不应额外实现 Solidity 状态机。建议：

1. Phase 1 前端用 Runtime metadata/SCALE 调用 Pallet；
2. Phase 2 引入 EIP-712 签名订单与 relayer；
3. 确有用户需求时，EVM precompile 将调用映射到同一套 Pallet 状态。

## 12. 治理、权限与紧急处置

市场参数应区分权限与生效方式：

- Root/治理：市场创建、费率、杠杆、抵押资产和预言机集合；
- 风险委员会（可延迟变更）：保证金率、清算参数、价格保护参数；
- 紧急权限（多签且可审计）：暂停开仓、仅减仓、暂停市场；
- 用户：充值、提币、挂单、撤单、平仓。

任何改变保证金率、费率、tick、预言机集合的操作均应发出事件，并使用延迟生效机制。`tick_size`、`step_size` 和结算精度不应在已有订单的市场中直接修改。

## 13. 工程落点与建议目录

```text
pallets/
  markets/
  clearing/
  orderbook/
  oracle/
  liquidation/
  insurance/
  bridge/
runtime/
  src/lib.rs              # Pallet 注册
  src/configs/mod.rs      # Config、限额、参数
node/
  src/rpc/                # Runtime API 的 RPC 暴露
services/
  indexer/                # Phase 2 独立进程，不放 Runtime
```

`pallet-assets` 先作为官方依赖接入 Runtime。自定义 Pallet 必须各自拥有 mock runtime、单元测试、benchmark、weights 和 migration 测试；不要继续扩展 `pallet-template` 作为生产模块。

## 14. 测试与上线门槛

最小验收场景：

1. 创建测试 USDC 和 `BTC-USD` 市场；
2. Alice/Bob 充值、挂单、部分成交、完全成交，验证 FIFO 和费用；
3. 撤单后订单保证金准确释放；
4. Post-only、Reduce-only、IOC 和滑点保护均符合规则；
5. 价格变化后未实现 PnL、初始/维持保证金和可提余额正确；
6. 可清算账户能够被部分或全部平仓，保险基金路径可审计；
7. 订单上限、价格档上限、撮合上限均不能导致超重、无界循环或状态损坏；
8. 属性测试/fuzz 覆盖守恒关系：资产、仓位、订单剩余数量、费用、保险基金；
9. benchmark 生成真实 weights，测试网进行压力与重组演练；
10. 外部审计完成后才考虑真实资产和开放交易。

跨链额外门槛：源链重组与重复日志测试、错误 chain id/token/vault 拒绝、存款/提款重放测试、验证者集轮换、暂停与限额、外链 gas 耗尽及合约升级演练。生产桥和 Perp 核心应至少分别接受审计。

## 15. 当前待决策问题

后续设计开始前，需要产品、风险和链团队共同确认：

1. Phase 1 是否严格只使用测试 USDC；生产桥首发链选择 Ethereum 还是 Arbitrum？
2. 初期使用全仓单账户，还是从一开始支持多子账户和隔离保证金？
3. 允许的市场数、目标 TPS、最大同时挂单数和目标交易延迟是多少？
4. 预言机的数据源、签名者、更新频率、失效阈值和争议处置机制是什么？
5. 风险委员会、紧急暂停权、保险基金和费用收入分别由谁控制？
6. 是否必须支持 MetaMask 原生下单；若是，接受 SCALE 交易、EIP-712 relayer 还是 precompile？
7. Hyperliquid 对标的优先级是吞吐、确认延迟还是单一全局排序？各阶段的目标硬件与可接受 p99 是什么？
8. 阶段 B 的交易池动作编码、账户 nonce 规则、市场/账户限流和区块交易配额分别设为多少？
9. 生产桥采用轻客户端、ZK 证明还是受限验证者委员会；谁承担桥损失和外链 gas？
10. 专用 BFT/共识替换已明确为远期备查；仅在 A、B benchmark 显示最终性为主瓶颈时重新评审。

## 16. 下一步设计产出

建议按以下顺序展开，且每项完成评审后再进入下一项：

1. 性能需求文档：动作口径、硬件、负载模型、p50/p95/p99、链上/端到端统计边界；
2. 产品参数表：首市场、精度、杠杆、费率、交易规则；
3. `pallet-exchange-core` 详细状态机和风险公式；
4. `pallet-exchange-core` storage、调用、错误码、事件和 weight 模型；
5. 预言机与清算的故障模型；
6. EVM Bridge 的信任模型、合约接口、最终性证明和运营流程；
7. 阶段 B 的交易池、P2P、proposer 排序和 `Exchange Batch` 协议；
8. Runtime 集成、迁移及治理方案；
9. 测试矩阵、性能门禁、审计范围和测试网发布计划；
10. Phase 2 后再定义 Indexer/API 协议。
