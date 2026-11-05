# Sunchain DEX 设计与实现 Review

> 审查日期：2026-10-23  
> 审查范围：Phase 1/2 详细设计、Phase 2 Worklog、`pallet-exchange-core`、oracle、insurance、liquidation，以及 node 侧 ExchangePool/P2P/proposer 实现。  
> 结论：当前系统可作为 dev/testnet 的 Perp DEX 原型；在修复本文件 P0 项、完成可发布构建与运行验收前，不应承载真实资产，也不应进入 Native TradeDomain 的迁移阶段。

## 1. 执行摘要

当前实现已经具备订单簿、保证金、资金费、清算、保险基金、ADL、价格喂送和 node 侧专属交易路径的主要骨架。核心问题不主要是撮合数学，而是批量执行语义、子账户、离线签名域、排序公平性和运行保障之间存在断层。

最需要立即处理的风险如下：

1. `execute_exchange_batch` 携带的非零 `subaccount` 没有贯穿实际交易路径；
2. batch 中单个 action 失败后继续执行，但没有 action 级存储回滚边界；
3. action 签名未绑定 chain、版本和统一 expiry；
4. 将 proposer 构造的 batch 数组下标称为 arrival/fairness，无法限制 proposer 重排；
5. P2P 限流按收到消息次数重置，可被流量本身绕过；
6. receipt、可观测性、try-runtime 与真实压力/重组演练未达到设计验收门槛。

## 2. 审查依据与验证状态

### 2.1 文档依据

- `Sunchain-Phase1-Exchange-Core-Detailed-Design.md`
- `Sunchain-Phase2-Exchange-Core-Detailed-Design.md`
- `Sunchain-Phase2-Worklog.md`
- `Sunchain-Orderbook-DEX-Design-Discussion.md`
- `SunChain-Protocol-Native-DEX-Execution-Design.md`

其中协议原生执行域文档本身标注为“提案”，在批准前不得作为 Runtime 或共识升级依据。它正确识别了当前实现仍经由 `execute_exchange_batch` 进入 Runtime/WASM，而非原生 TradeDomain。

### 2.2 已执行的验证

- `cargo test -p pallet-exchange-core`：通过，102 项测试通过。
- `cargo test --all-features`：失败。
  - `pallets/exchange-core/src/migrations.rs` 在 `try-runtime` 下重复导入 `Vec`；
  - 本地 `pallet-revive` 与当前 `Checkable` trait 接口不兼容。

因此，默认测试可通过不代表 migration、WASM/feature 组合和发布配置可通过。

## 3. 关键发现

### P0-1：子账户仅用于 nonce，未真正进入交易状态机

**证据**

- `SignedExchangeAction` 带有 `subaccount`；
- `ActionNonce` 也按 `(AccountId, SubaccountId)` 存储；
- 但 `execute_exchange_batch` 调用 `do_place_order`、`do_cancel_order` 时没有传递 `subaccount`；
- `account_of`、`position_of` 和热路径读写固定使用 `DEFAULT_SUBACCOUNT`；
- `CancelUpTo` 从默认子账户读取，却向 `signed.subaccount` 写入。

**影响**

非零子账户的签名 action 可能操作默认子账户的订单/仓位/抵押品，或者产生读写错位。此问题使 P2-C“子账户维度完成”的结论不成立。

**整改**

在完整改造前，Runtime 应明确拒绝 `subaccount != 0` 的 action。完整改造时，应将 `subaccount` 显式传入 account、position、order、cancel、风险聚合、清算、费率等级、事件及 Runtime API 的所有路径，并增加非零子账户端到端测试。

### P0-2：continue-on-error 没有 action 级事务边界

**证据**

`execute_exchange_batch` 在闭包内先写入 `ActionNonce`，随后执行 `do_place_order` / cancel；闭包报错后仅记录 `ExchangeActionFailed`，外层 dispatch 仍返回成功。

**影响**

失败 action 不会触发 extrinsic 级回滚。nonce 确定会在业务失败前被消耗；更严重的是，任一 `do_*` 在中途写入 storage 后再返回错误时，可能残留部分状态。这与订单、仓位、抵押品必须原子变更的要求冲突。

**整改**

为每条 action 使用 `with_transaction` 或等价存储事务：

- 签名、编码、nonce 无效：回滚且不消费 nonce；
- 业务拒绝：回滚业务状态，再按冻结协议决定是否消费 nonce；
- 成功：一次性提交业务状态与 nonce；
- 无论成功或拒绝，都输出可查询 receipt，而不只输出 event。

需补充“撮合中途失败、FeePool 不足、价格失效、风险拒绝、跨 action 失败互不污染”的测试。

### P0-3：离线签名缺少 domain separation 与统一过期控制

**证据**

当前签名 payload 是 `(subaccount, action_nonce, action)`；没有 `version`、`chain_id`、独立 domain tag 或通用 `expires_at`。只有 Place 的 order request 内部拥有订单到期高度，Cancel/CancelUpTo 没有签名级 expiry。

**影响**

在同密钥、同编码与 nonce 条件满足的链/环境中，签名可能被跨链或跨环境重放；长期有效的 cancel 类签名也扩大了泄漏后的攻击窗口。

**整改**

版本化 `TradeAction`，把 `version`、`chain_id`、domain tag、subaccount、nonce、action expiry 和完整 action 一并签名。固定哈希及 ECDSA 地址恢复规则，并为签名无效、过期、跨链重放及 nonce 缝隙写 golden vectors。

### P0-4：arrival 排序不构成可验证公平性

**证据**

Runtime 以 batch 内数组下标生成 `arrival_index` 后排序；数组由 proposer 提供。现有测试也验证“排在 batch 前面的用户先成交”。

**影响**

系统只能保证所有验证者对同一 batch 重放一致，不能防止 proposer 在构造 batch 时重排、插队或选择性纳入 action。将此称为“proposer 不能重排”或全局公平会误导产品与风险判断。

**整改**

短期：改为可验证的 canonical 顺序，例如 `(class, signer/subaccount nonce dependency, action_hash)`，并在 Runtime 验证 batch 已按该顺序排列。文档应明确这只保证段内确定性，不能消除 proposer 选择 action 集的 MEV。

长期：如产品确实要求更强公平性，应单独选择 frequent batch auction、commit/reveal 或加密 mempool，而不是把本地到达时间放进共识语义。

### P1-1：P2P 限流可由流量本身绕过

**证据**

`RateLimiter::advance()` 在每一条远端消息到达时调用；每累计 `window` 条消息就清空全部计数。限流 key 仅是 signer 字节。

**影响**

攻击者可以发送足够多的消息触发窗口重置，持续绕过每账户配额；也没有 peer、market 维度隔离。无效包先进入 seen set，亦可挤占有限去重空间。

**整改**

- 窗口改为单调时钟或区块高度，不得由收到消息数推进；
- 建立 peer、account、market 三维配额与全局字节/CPU 配额；
- 在昂贵 Runtime API 预校验之前实施大小、解码、peer 配额与重复流量控制；
- 增加 peer reset、畸形 SCALE、泛洪、重复和无效签名的压力测试。

### P1-2：proposer 的 batch 选择与资源控制未闭环

**证据**

proposer 读取全部 pending action，再以 `BoundedVec::truncate_from` 静默截断为最大 action 数；未按编码字节、真实 weight、市场份额或预计匹配复杂度选择。

**影响**

截断本身不会丢失 action（池会保留至 nonce 被链上消费），但无法满足设计中的 batch 总字节、weight、单市场比例和可解释选择策略。交易池内存态重启仍会丢失 action，需要客户端重传。

**整改**

建立显式 batch builder：根据 canonical 顺序逐条尝试加入，逐项检查 action 数、SCALE 字节、最坏/实测 weight、市场配额和 block 剩余预算；对未纳入原因进行指标记录。

### P1-3：Exchange block budget 的实际计量低估

**证据**

`execute_exchange_batch` 只累计 matches，最终以 `stale_removed = 0` 计算实际 weight；也不含价格档扫描维度。

**影响**

交易专属预算与监控数据低估实际资源使用。虽然 declared weight 仍是区块安全边界，但“DEX 不挤占其他业务”的公平配额无法被可靠执行或审计。

**整改**

让每 action 返回完整 `MatchStats`，批量汇总 matches、stale removals、level scans；weight 接口、benchmark 和 block budget 使用相同维度。对 `set_liquidity_tier` 等遍历全部市场的治理调用也应使用与扫描量匹配的 weight。

### P1-4：ADL 候选的全局公平性未被协议保证

**证据**

`adl(bankrupt, candidates)` 接受 keeper 自选的 candidate 列表，并只在该集合内做确定性筛选和排序。

**影响**

keeper 可以遗漏本应优先的高 ROE 候选；因此“排序确定”不等同于“全局候选选择确定”。这可能导致 ADL 的可预期性与公平性不满足设计文字。

**整改**

明确协议选择其一：

- 将 keeper 提供候选视为 best-effort，文档不声称全局公平；或
- 建立有界、可验证的 candidate snapshot/index，并证明选取的是全局前 N；或
- 使用固定分页游标、可验证扫描和多 block 调度。

### P1-5：失败结果不可查询，监控与用户反馈不足

**证据**

batch 业务失败只发 `ExchangeActionFailed` event；Worklog 已记录 `system_dryRun` 对单笔失败仍返回成功，当前依靠 CLI 回读订单和仓位判断。

**影响**

客户端无法可靠区分未入池、未被 proposer 纳入、已入块但业务拒绝、reorg 回退等状态；节点也无法基于结构化失败触发告警。

**整改**

引入稳定的 action id 与 receipt：包括 block/hash、执行状态、错误码、nonce 消费状态、成交摘要和最终性状态。提供 Runtime API/RPC 查询、订阅和 Prometheus 指标。

### P1-6：发布与迁移门禁未闭环

**证据**

默认 exchange-core 测试通过，但 all-features/try-runtime 组合无法构建。Phase 2 设计要求快照上的 `try-runtime`、真实 benchmark、一周以上压力/重组/恢复演练、Indexer 回滚演练和外部审计。

**整改**

先修复 all-features 与 runtime/WASM 构建；将默认、runtime-benchmarks、try-runtime、WASM、迁移快照检查纳入 CI。未通过这些门禁前，不能将“测试通过”解释为可发布。

## 4. 设计、Worklog 与实现的一致性判断

### 已具备的基础

- 单市场订单簿及价格时间优先/FIFO 的基础实现；
- 整数 tick、lot、quote 计算，避免运行时浮点；
- oracle 的多 source 聚合、staleness gate 和价格保鲜修复；
- funding、保险基金、清算和 ADL 的框架；
- 专属 pool、P2P、proposer 注入 unsigned batch 的端到端雏形；
- custody/内部负债等守恒检查的意识与部分测试。

### 主要过程偏差

1. Worklog 将 P2-C 和 P2-D 标记为完成，但当前协议原生设计已列出它们进入 shadow 前必须修正的缺口；阶段状态应从“完成”改为“功能原型完成，安全/运营门禁未完成”。
2. 端到端验证以单账户自成交为主，尚未覆盖真实多对手方、恶意 batch、重组、分区、清算高峰与崩溃恢复。
3. 设计中的 P2-E（Indexer、RPC、监控和告警）没有随着 batch 语义一并完成，导致实际运维只能通过日志和状态回读排障。
4. 新的 Native TradeDomain 文档是正确的目标架构，但仍是提案；当前 Runtime/WASM 实现不能被包装为 native execution 已落地。

## 5. 建议的整改顺序

### R0：冻结风险扩张

- 不接入真实资产、桥或生产提款；
- 拒绝非零 subaccount action，直到完整实现；
- 将当前阶段公开命名为 testnet/dev prototype；
- 明确 unsigned batch 无出块者激励的可用性风险与监控责任。

### R1：先修 Runtime 语义

1. 修复子账户的完整传递，或临时拒绝非零值；
2. 重做 batch action 的事务、nonce 与 receipt 语义；
3. 版本化签名 payload，加入 chain/domain/expiry；
4. 去除数组下标 arrival 的公平性声明，改为 canonical 排序；
5. 修复 batch 实际 weight 统计与治理遍历权重；
6. 为上述每项补齐回归、属性和状态守恒测试。

### R2：修 node 交易路径与运营面

1. 用真实时间/块高实现 peer/account/market 限流；
2. 实现显式资源受限的 batch builder；
3. 提供 action receipt RPC、订阅、告警和指标；
4. 用至少两名真实交易账户完成 maker/taker、撤单、reduce-only、funding、清算和 ADL 的端到端验证；
5. 进行重组、网络分区、节点重启和 pool 重传演练。

### R3：满足发布门禁后再进入 Native TradeDomain

1. 修复 all-features、try-runtime、WASM 与 migration snapshot 检查；
2. 冻结 action、receipt、错误码、状态 schema 和数学版本；
3. 抽出无 FRAME 依赖的确定性 `dex-engine`，以现有 pallet 作为参考实现；
4. 建立长期 native/WASM shadow 差分，覆盖 oracle stale、清算、ADL、升级和重组；
5. 只有 root/digest 连续一致、目标硬件性能达标、恢复演练和独立审计完成后，才评估将 TradeSegment 纳入区块有效性。

## 6. 验收清单

在进入真实资产或 M1 Native Engine 抽取前，以下项目必须全部关闭：

- [ ] 非零 subaccount 被完整支持，或 Runtime 明确拒绝；
- [ ] 每条 batch action 具备原子回滚、明确 nonce 语义和 receipt；
- [ ] action 签名绑定 version、chain、domain、expiry；
- [ ] 排序规则可由 Runtime 验证，且文档不再承诺本地 arrival 公平；
- [ ] P2P 限流不依赖收到消息数，包含 peer/account/market 配额；
- [ ] batch 按 action、字节、weight 和市场预算显式选择；
- [ ] batch 失败结果可通过 RPC 查询并有告警；
- [ ] default、WASM、runtime-benchmarks、try-runtime、all-features 构建均通过；
- [ ] migration 在真实快照上完成 pre/post 检查；
- [ ] 多验证人、多账户、重组、分区、压力和恢复演练通过；
- [ ] custody、FeePool、Insurance、BadDebt 与仓位 OI 守恒式有自动化检查；
- [ ] 外部安全审计完成，关键发现闭环。

