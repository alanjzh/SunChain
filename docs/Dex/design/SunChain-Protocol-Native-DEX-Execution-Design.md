# SunChain 协议原生 DEX 执行域：落地设计

> 状态：提案（未批准前不得作为 Runtime / 共识升级依据）  
> 目标：在不牺牲确定性、资产守恒和可审计性的前提下，将 Perp DEX 的性能关键路径从通用 FRAME/WASM dispatch 迁移为由验证者原生执行、并由区块状态根承诺的协议执行域（下文称 `TradeDomain`）。

## 1. 决策摘要

当前 DEX 的订单簿、保证金、撮合、资金费、清算和 ADL 都在
`pallet-exchange-core` 中执行。节点已经具备 `ExchangePool`、专属 P2P 和
proposer 注入 batch 的雏形，但最终仍通过 `execute_exchange_batch` 进入 Runtime
WASM 路径。

本方案不采用“节点撮合、链上只结算”的模式，也不在 pallet 内调用不可审计的 native
host function。目标模式如下：

```text
已签名 TradeAction
   │
   ├── TradePool / Trade P2P（仅接收、预检查、背压；非权威）
   │
   ▼
区块 proposer 选择并提交 TradeSegment
   │
   ▼
所有验证者使用相同的 Native DEX Engine 重放
   │
   ├── 生成 DEX child trie 变更
   ├── 生成 receipts / events
   └── 计算 next_exchange_root
   │
   ▼
区块导入校验 root；全局 state root 间接承诺 exchange root
```

因此交易状态仍是链上共识状态：每个订单、撤单、成交、仓位和清算均由所有验证者独立
重放得出。`TradeSegment` 中携带结果承诺以便快速校验和索引，但不信任 proposer 提供的
结果。

第一期不替换 Babe/Grandpa。先让交易执行原生化、区块结构交易感知化；只有实际测量证明
共识最终性或通用区块构建成为主要瓶颈时，再单独立项专用共识。

## 2. 范围与非范围

### 2.1 本期包含

- 一个无 FRAME 依赖、纯 Rust、确定性 `dex-engine`；
- 一套 `TradeDomain` child trie 状态布局及状态根承诺；
- `TradeSegment` 区块组成、原生 block import / 重放校验；
- 从 `pallet-exchange-core` 平滑迁移到引擎的双执行验证路径；
- 交易专属 P2P、mempool、资源配额、失败回执和 RPC；
- Runtime 资产、oracle、治理与 TradeDomain 的消息边界；
- 完整的差分、属性、fuzz、重组和故障注入验收。

### 2.2 本期不包含

- 让 native proposer 单独决定成交结果；
- 将订单簿放在中心化 sequencer 或 indexer；
- 首期直接改造为 HyperBFT / HotStuff 类共识；
- 生产桥、真实 USDC、跨链抵押资产；
- 通过浮点、系统时间、网络返回值或非确定性数据库参与撮合与风控；
- 在一个过渡期内让 Runtime 与 Native Engine 同时拥有 DEX 状态写权限。

## 3. 设计约束

1. **重放一致性**：同一 parent exchange root、oracle snapshot 和有序 action 集必须得到
   相同 root、receipt 和余额变化。
2. **单一权威写者**：完成迁移后，订单、仓位、DEX collateral、资金费和保险基金只能由
   `dex-engine` 修改；Runtime 只能通过消息入队。
3. **资产守恒**：Runtime custody 资产、DEX collateral、费用池、保险基金和坏账必须有
   可复算的守恒式。
4. **有界资源**：每个 action、TradeSegment、每市场及每区块都必须有显式 action 数、字节、
   成交数、价格档扫描数和状态写入上限。
5. **可升级性**：引擎版本、编码、状态 schema 和 math 版本进入区块承诺；升级必须有
   migration、shadow 和回滚计划。
6. **公平性不伪装**：proposer 可选择 action 集是共识系统固有的 MEV 面。不得再把本地
   `arrival_index` 伪称为可验证的全局公平顺序。

## 4. 目标代码结构

```text
crates/
  dex-types/                 # SCALE 类型、固定点数学、版本化 action/receipt
  dex-engine/                # 无 std/无 FRAME 的确定性状态机
    src/book.rs
    src/clearing.rs
    src/risk.rs
    src/funding.rs
    src/liquidation.rs
    src/transition.rs
    src/error.rs
  dex-state/                 # child trie key 编码、StateReader/StateWriter adapter

node/
  src/trade_pool.rs           # 取代 exchange_pool 的最终实现
  src/trade_p2p.rs
  src/trade_proposer.rs
  src/trade_import.rs         # TradeSegment 重放、root 比对和状态提交

pallets/
  exchange-core/              # 过渡 adapter；最终保留治理/消息/API 壳
  trade-domain/               # Runtime 侧 queue、root、配置和只读接口
```

`dex-engine` 禁止依赖 `frame_support`、`sp_io`、宿主时钟、随机数和哈希 map 的迭代顺序。
所有集合须以固定 key 顺序存储或排序后处理；所有数学继续使用现有整数 tick、lot、quote
单位和显式溢出错误。

## 5. 状态模型与承诺

### 5.1 状态所有权

| 状态 | 最终权威 | 说明 |
|---|---|---|
| 用户外部资产余额 | Runtime Assets | DEX 不直接调用 Assets 转账 |
| DEX collateral / reserved margin | TradeDomain | 由 deposit/withdraw 消息更新 |
| 市场、订单簿、订单、仓位、资金费 | TradeDomain | native engine 独占写入 |
| FeePool、Insurance、BadDebt、ADL 队列 | TradeDomain | 与 clearing 同一原子状态机 |
| Oracle 聚合结果 | Runtime Oracle snapshot | 每个 TradeSegment 引用冻结 snapshot |
| 治理参数 | Runtime 配置消息 | 仅在明确定义的边界生效 |

### 5.2 child trie

所有 TradeDomain 状态存于专属 child trie。Runtime 顶层 storage 保存：

```rust
ExchangeRoot: H256
ExchangeStateVersion: u16
ActiveEngineVersion: u16
PendingDomainMessages: BoundedVec<DomainMessage, MaxDomainMessages>
```

child trie key 必须使用领域前缀和 canonical SCALE 编码，例如：

```text
market/{MarketId}
account/{AccountId20}/{SubaccountId}
position/{AccountId20}/{SubaccountId}/{MarketId}
order/{MarketId}/{OrderId}
level/{MarketId}/{Side}/{PriceTicks}
account-order/{AccountId20}/{SubaccountId}/{ClientOrderId}
action-nonce/{AccountId20}/{SubaccountId}
```

禁止复用当前 pallet 的隐式 storage key 格式作为长期协议格式。新格式需要版本号、golden
vectors 和跨语言解码测试。

### 5.3 守恒不变量

每个 transition 后必须可验证：

```text
runtime custody amount
  = Σ TradeAccount.collateral + fee_pool + insurance_reserve
    + pending_withdrawals - paid_out_withdrawals

Σ market.net_oi_lots = Σ position.size_lots = 0
```

`BadDebt` 不是资产，应单独计量，不得被守恒式静默抵消。任何不变量失败均使区块无效，
而不是仅产生日志。

## 6. TradeAction、排序与公平性

### 6.1 Action 编码

```rust
pub struct TradeAction {
    pub version: u16,
    pub chain_id: u64,
    pub subaccount: u32,
    pub nonce: u64,
    pub expires_at: BlockNumber,
    pub action: ActionKind,       // place / cancel / cancel_up_to
    pub signer: AccountId20,
    pub signature: EcdsaSignature,
}
```

签名 payload 必须包含 `version`、`chain_id`、subaccount、nonce、expiry 和完整 action，
并固定使用 keccak/Ethereum 地址恢复规则。`nonce` 在 action 被**验证并确定消费**后才前进；
失败 action 的 nonce 是否消费必须写入协议并在 receipt 中明确，建议采用：

- 签名、nonce、expiry 都有效但业务执行失败：消费 nonce，生成 `RejectedBusiness` receipt；
- 签名、nonce、编码无效：不消费 nonce，生成或不生成 receipt 由 segment policy 决定。

### 6.2 排序

TradeSegment 的 action 列表是共识输入，排序必须由引擎二次验证：

```text
class: DomainMessage < Cancel < Place
within signer/subaccount: nonce strictly ascending
between independent accounts: action_hash ascending
```

该排序提供可验证的**段内确定性**，不声称解决 proposer 对“选哪些 action 入块”的审查或
抢跑能力。若产品需要 stronger fairness，另行引入下列之一：

- frequent batch auction：固定窗口收集，按统一清算规则撮合；
- commit/reveal：先承诺订单哈希，后公开参数；
- 加密 mempool / threshold encryption：仅在共识后解密。

首期选择 canonical action hash 排序，不把未证明的本地到达时间写进 consensus 语义。

## 7. TradeSegment 与区块验证

```rust
pub struct TradeSegment {
    pub engine_version: u16,
    pub parent_exchange_root: H256,
    pub oracle_snapshot_id: H256,
    pub domain_messages: BoundedVec<DomainMessage, MaxDomainMessages>,
    pub actions: BoundedVec<TradeAction, MaxTradeActions>,
    pub receipts_root: H256,
    pub next_exchange_root: H256,
}
```

验证流程：

1. `parent_exchange_root` 必须等于 parent block 的 `ExchangeRoot`；
2. 读取并冻结对应 oracle snapshot 和待处理 Runtime→Domain 消息；
3. 校验 action 的编码、签名、chain id、expiry、大小和 action 类别；
4. 对 action 执行 canonical sort；若输入顺序不是 canonical，拒绝整个 block；
5. Native Engine 在 parent child trie 上重放；
6. 计算 receipt root 与 next exchange root，必须等于 segment 承诺；
7. 原子提交 child trie 变更和新的 Runtime 顶层 `ExchangeRoot`；
8. 将事件/receipt 写入可索引的有界日志。

不得使用“proposer 给出的成交/仓位变更”跳过第 5 步。性能优化应来自 native state machine、
批量存储和专属调度，而不是削弱验证者重放。

## 8. Runtime 与 TradeDomain 的跨域消息

### 8.1 Runtime → Domain

| 消息 | 生效点 | 说明 |
|---|---|---|
| `CreditCollateral` | 下一个 TradeSegment | Runtime 已将资产转入 custody |
| `RequestWithdraw` | 下一个 TradeSegment | engine 检查 free collateral 后锁定 |
| `OracleSnapshot` | segment 开始 | 不可在 segment 内变更 |
| `MarketConfigUpdate` | epoch / 延迟生效块 | 风险参数不得即时突变 |
| `PauseMarket` | 下一个 segment | 紧急操作可优先级更高 |

### 8.2 Domain → Runtime

| 消息 | 生效点 | 说明 |
|---|---|---|
| `WithdrawApproved` | 随后的 Runtime lane | Assets 从 custody 释放 |
| `InsuranceTransfer` | 随后的 Runtime lane | 明确资金方向与上限 |
| `DomainEventCommitment` | 同一 block | 供 indexer 订阅与证明 |

跨域消息必须具有 `message_id`、source height、目标版本和 exactly-once 语义。任何消息重放、
重复提款或失败重试均不得创造或丢失资产。

## 9. 当前实现的改造映射

| 当前组件 | 近期作用 | 最终去向 |
|---|---|---|
| `pallet-exchange-core` | 参考语义与过渡 adapter | 拆为 engine adapter + Runtime domain facade |
| `types/book/risk/settlement/funding` | 业务规则来源 | 迁入 `dex-types` / `dex-engine` |
| `ExchangePool` | 节点动作暂存 | `TradePool`，增加持久化、backpressure、per-peer 限流 |
| `exchange_p2p` | gossip 雏形 | trade protocol；按 peer + account + market 限流，以块高/单调时钟计窗 |
| `ExchangeProposer` | 注入 unsigned batch | 生成并附加 `TradeSegment` |
| `execute_exchange_batch` | 过渡执行入口 | shadow 阶段保留；cutover 后废弃交易写路径 |
| Runtime API / RPC | 预验证和提交 | 增加 domain snapshot、receipt、proof 和 segment 状态查询 |

在 shadow 前必须修正：

1. 非零 `subaccount` 要么完整传递到 account、position、order 和 cancel 路径，要么明确拒绝；
2. 移除以 batch 数组下标表达“arrival”的公平性声明；
3. P2P rate limit 改为 peer、account、market 三维，并避免“按收到消息数重置窗口”；
4. batch 失败必须提供 RPC 可查询 receipt，不能只依赖事件回读；
5. oracle signer、市场列表和 feed 配置从 dev hardcode 移到可审计配置。

## 10. 迁移阶段与 Gate

### M0：规格冻结

- 冻结 action、receipt、child trie key、数学和错误码的版本化规范；
- 将当前 P2-B/C/D 的已知语义写成 golden test vectors；
- 明确 block、segment、市场、账户、价格档和单 action 的资源上限；
- 定义运行硬件、native/WASM 版本与性能基线。

**Gate**：架构审查通过；全部开放问题有 owner 与日期。

### M1：引擎抽取

- 抽出 `dex-engine`，让 pallet 通过 adapter 调用；
- 运行逻辑仍在 Runtime/WASM，状态布局不变；
- 逐项建立现有单测到 engine 的映射；
- 添加 property test：随机 action 序列不破坏守恒与订单簿链接。

**Gate**：pallet 与 engine 对相同 fixture 得到相同状态摘要和事件摘要。

### M2：Native Shadow

- 节点在导入每个 DEX block 后从相同 parent state 原生重放；
- 比较 account/position/orderbook digest、fee/insurance/bad-debt 和 receipt hash；
- 不一致时记录完整可重放 case，并在测试网将 validator 标为 unhealthy；
- 连续运行至少 30 天，覆盖升级、重组、oracle stale、清算与 ADL。

**Gate**：零未解释差分；p99 native engine 明显优于 WASM 基线；故障恢复演练通过。

### M3：Dual-write root shadow

- 引入 child trie 但仍由 Runtime adapter 作为唯一权威；
- 每块生成 candidate `TradeSegment` 及 child trie root；
- 不将 candidate root 纳入有效性，只做比较；
- 使用真实测试网快照反复执行 migration / rollback。

**Gate**：root 连续一致；迁移可在 block weight、内存和 PoV 预算内完成。

### M4：Testnet Native TradeDomain

- 新 block import 路径将 TradeSegment 作为共识有效性的一部分；
- Runtime 停止直接写 DEX 主状态，仅发跨域消息；
- 提供 feature flag 以回退到 M3 的 Runtime adapter（仅在 cutover 窗口内）；
- 运行负载、网络分区、恶意 segment、重复消息和 reorg 演练。

**Gate**：至少两周稳定压力测试；独立安全审计；无 custody 失衡；恢复演练通过。

### M5：生产启用

- 先限制市场、仓位、action 和提款额度；
- 分阶段提升配额，所有参数有延迟生效与紧急暂停机制；
- 外部审计覆盖 engine、state transition、block import、跨域消息与迁移；
- 生产桥及真实资产另行审批，不随 M5 自动启用。

## 11. 资源与性能模型

`TradeSegment` 需独立于普通 extrinsic 定义配额：

```text
MaxTradeSegmentBytes
MaxTradeActions
MaxMatchesPerAction
MaxMatchesPerSegment
MaxWritesPerSegment
MaxMarketsTouchedPerSegment
MaxTradeRefTime / MaxTradeProofSize
```

proposer 必须在组装前使用 native engine 的 dry-run 估算资源；block import 仍以重放结果为准。
普通 Runtime lane 与 TradeDomain 各保留容量，防止 staking、治理或 EVM 流量挤占交易，
也防止交易流量饿死治理和安全操作。

性能报告至少分开统计：

- action 入池到 proposer 选择；
- proposer 选择到 block 包含；
- 包含到可执行；
- 共识提交/Grandpa 最终性；
- native engine 计算、trie 写入、网络传播和 block import 各自耗时；
- p50/p95/p99 及在重组、oracle stale、清算高峰下的尾延迟。

## 12. 测试与安全验收

### 必测

- 引擎与原 pallet 的差分测试；
- 固定点数学、价格时间优先、FIFO、撤单、reduce-only、maker 复检；
- 多市场保证金、funding、清算、保险、ADL 的守恒属性；
- action 重放、跨链 ID、签名 domain separation、nonce 缝隙与 expiry；
- 恶意 proposer：乱序、漏 action、伪 receipt root、伪 exchange root、超限 segment；
- 恶意 P2P：泛洪、重复、畸形 SCALE、无效签名、peer reset；
- Runtime↔Domain 消息的重复、乱序、丢失和重组；
- child trie migration、多版本 decoding、snapshot try-runtime；
- 模糊测试及长序列状态机测试；
- 真实多验证人压力、网络分区、节点升级和崩溃恢复。

### 上线硬门槛

1. Native 与 WASM shadow 在既定测试周期内零未解释 root 差分；
2. 所有验证者可从快照独立重放并得到相同 exchange root；
3. 资产守恒、坏账、保险基金和提款队列均可独立审计；
4. TradeSegment 恶意输入不会导致节点崩溃、无界内存、超预算或状态部分提交；
5. 外部审计完成，且关键发现闭环；
6. 生产参数来自目标硬件 benchmark，而非开发机测试值。

## 13. 开放决策

- TradeDomain 是保留在 Babe block 内的专属 lane，还是后续迁移为独立 round/consensus？
- 是否需要 frequent batch auction 或加密 mempool，以降低 proposer ordering MEV？
- child trie proof 如何开放给轻客户端、RPC 和 indexer？
- oracle snapshot 的生成时机、quorum、staleness 和 segment 内冻结规则？
- DEX collateral 是否完全内部账本，还是允许 Runtime asset 的同步可读余额？
- Native engine upgrade 是否只允许 runtime upgrade 高度生效，还是采用独立 engine version vote？
- M4 到 M5 期间的紧急回退是否安全；触发条件和最大回退窗口是什么？

## 14. 参考

- [SunChain Phase 2 Exchange Core Detailed Design](Sunchain-Phase2-Exchange-Core-Detailed-Design.md)
- [SunChain Phase 2 Worklog](Sunchain-Phase2-Worklog.md)
- [SunChain Orderbook DEX Design Discussion](Sunchain-Orderbook-DEX-Design-Discussion.md)
- [Hyperliquid HyperCore Overview](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/overview)
- [Hyperliquid Order Book](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/order-book)
