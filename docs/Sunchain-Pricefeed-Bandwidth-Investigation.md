# 喂价（pricefeed）流量调查与整改记录

日期：2026-10-23
范围：`node/src/pricefeed.rs`、`node/src/oracle_feed.rs`、`node/src/service.rs`、`node/Cargo.toml`
相关文档：[Phase 2 工作日志](Sunchain-Phase2-Worklog.md)、[Phase 2 详细设计](Sunchain-Phase2-Exchange-Core-Detailed-Design.md)

---

## 1. 问题描述

接入 Binance / OKX / Coinbase 三家喂价源后，节点在很短时间内产生了异常大的出网流量。需要回答两件事：**流量是怎么跑出来的**，以及**这是否正常**。

结论先行：行为与代码一致，没有异常外联，但实现本身在反复下载"全市场行情"，属于能跑但严重浪费的实现。dev 链无所谓，上生产必须先改。

---

## 2. 调查过程

### 2.1 实测各端点响应体积（走代理，curl）

| 端点 | 单次响应 | 备注 |
| --- | --- | --- |
| Binance `/api/v3/ticker/24hr` **不带 symbol** | **1,916,610 B**（3 739 个交易对），weight 82 | 我们只用它取 1 个 symbol |
| Binance 同上 + `?symbol=BTCUSDT` | 559 B，weight 2 | 差 3 400 倍 |
| OKX `/api/v5/market/tickers?instType=SPOT` | 371,716 B（1 186 个 inst） | 无过滤参数 |
| OKX `/api/v5/market/ticker?instId=BTC-USDT` | 359 B | |
| Coinbase `/products/BTC-USD/ticker` | 185 B | 已按 symbol 拉，无问题 |
| 启用 gzip 后 | Binance 284 KB、OKX 101 KB | 当时 reqwest **未开 gzip** |

### 2.2 一轮到底发了哪些请求

配置是 4 个 market（BTC/ETH/SOL/DOGE），每个 market 3 个源 + 2 个 peg 源；`collect()` 对**每个 market 独立跑一遍**（含 peg），于是：

- Binance 全量 ×4
- OKX 全量 ×8（4 个市场源 + 4 次 peg）
- Coinbase ×8

按真实顺序跑完一轮实测：**10,641,932 B ≈ 10.6 MB**（纯 body，未计 HTTP 头/TLS）。

### 2.3 三个放大器

1. **端点不带 symbol**：拿 3 739 / 1 186 条行情，只用其中 1 条。
2. **每个 market 重复拉**：同一 URL 一轮拉 4 次，peg 也每 market 重拉一次 → OKX 全量 8 次/轮。
3. **未开 gzip**：`node/Cargo.toml` 里 reqwest 是 `default-features = false` + `rustls-tls/json/blocking`，压缩默认关闭。

### 2.4 折算流量

间隔 30 s（`FeedConfig.interval`）→ ≈21 MB/分钟 ≈ 1.25 GB/小时 ≈ **30 GB/天/节点**；`start_node.sh` 起 master + slave 两个 validator，各跑一份 feed → ≈**61 GB/天**。

Binance 限流权重：每轮 4×82 = 328，每分钟约 656（上限 6 000），当时未触发限流，但已用掉 11%。

### 2.5 扩展到几百 market 时的选型实测

| 测试 | 结果 |
| --- | --- |
| Binance `?symbols=[…]` n=20 / 100 / 300 / 450 | 10.5 KB / 52 KB / 155 KB / 233 KB，均 200 |
| Binance `?symbols=[…]` n=500 | **HTTP 414**（URL 8 497 字符被拒）→ 每批 ≤ 400 |
| Binance weight | 单 symbol 2；300 个一批约 100 出头 |
| OKX 带 `instId=` / `uly=` 过滤 | **无效**，仍返回全部 1 186 条 → OKX 没有批量端点 |
| Kraken `?pair=A,B` | 有效，但**一个未知 pair 会让整批返回空 result** |
| Bitfinex `?symbols=tA,tB` | 有效（100 个实测通过），未知 symbol 只被忽略 |
| Coinbase 串行 20 个 `/products/X/ticker` | **耗时 54 s**（代理下 ~2.7 s/个），2 个失败 → 300 market 需 13 分钟/轮，不可行 |
| Coinbase 批量 `market/products?product_ids=` | 200 无需认证，但 ~1.2 KB/个且字段不含 bid/ask |

---

## 3. 整改决策

**决策 1：一轮拉一次、本地挑 market（与"全量 vs 单 symbol"正交，且是前提）。**
dYdX `IsMultiMarket` 的本意就是一次请求拿全部市场、本地过滤。原实现 per-market 抓取会让请求数随 market 数线性爆炸，几百 market 时无论选哪种模式都不可行。

**决策 2：能批量的批量，没批量的用一次全量。**

| venue | 模式 | 理由 |
| --- | --- | --- |
| Binance | 批量 `?symbols=[…]`，400/批 | 单 symbol ~520 B，与全量 1.92 MB 的字节交叉点在 ~3 700 个 market；几百 market 时批量完胜（省 12 倍），weight 从 82×N 降到 ~120/轮 |
| OKX | 一次全量（gzip） | 无批量端点，逐个拉受 20 req/2s 限制，300 个请求占满整个 30 s 间隔；全量 372 KB（gzip 101 KB）一次覆盖 1 186 个 SPOT inst |
| Kraken | 批量 `?pair=A,B`，20/批 | 逗号分隔有效；批失败自动退化为逐个重试 |
| Bitfinex | 批量 `?symbols=tA,tB`，100/批 | 逗号分隔有效、容错好（未知 symbol 只被忽略） |
| Coinbase | **退出价格源，只做 peg** | 无批量 ticker，实测 2.7 s/请求，几百 market 物理上做不到 |
| Mexc / Bitstamp | 逐个（保留） | 无批量端点 |

**决策 3：开启 gzip。** 全量端点压缩比约 1/7（OKX 372 KB → 101 KB），reqwest 默认不启用。

**决策 4：不上 WebSocket。** `@ticker` 每 symbol 每秒 1 条（~500 B/s），300 个即 ~150 KB/s ≈ 900 MB/h，比 REST 批量（~5 KB/s）大两个数量级；WS 的价值是延迟，不是流量。只有把间隔压到 1 s 级时才值得换。

**决策 5：symbol 拼写以实测为准。** Kraken 的 BTC 是 `XBTUSDT`、DOGE 是 `XDGUSDT`；Bitfinex 的 USDT 对是 `tBTCUST`、DOGE 是 `tDOGE:UST`（含冒号，需 percent-encode）；Kraken 老式 USD 对会用规范名回答（请求 `XBTUSD` 得到 `XXBTZUSD`），匹配要做别名归一。四家均为 USDT 计价，因此都不需要 peg 换算。

---

## 4. 改动落地

### `node/src/pricefeed.rs`

- 新增 `FetchMode { All, Batch { max }, Single }` 与 `Exchange::fetch_mode()`，每个 venue 显式声明一轮怎么问。
- `ticker_url` 全部带上 symbol（不再裸拉全量）；新增 `batch_ticker_url`；`pct()` 只编码 `:[]",` 等必需字符。
- `parse_ticker` 重构为 `parse_tickers`（一次解析全部条目，坏行跳过而不是整批失败）+ 按 symbol 查找；新增 `symbol_matches` / `kraken_alias` 处理 Kraken 别名。
- 新增 `fetch_many`：`All` → 一次请求挑我们需要的；`Batch` → 分批请求，**整批为空时退化为逐个重试**（Kraken 的未知 pair 陷阱）；`Single` → 逐个。
- 新增 6 个单测：批量 URL 构造、多条目解析、坏行跳过、被轮询的 venue 必须支持批量/全量、Kraken 别名。

### `node/src/oracle_feed.rs`

- 新增 `round_symbols`（按 venue 去重，peg 只出现一次）、`fetch_round`、`Round`、`collect_from`。
- `run()` 改为每轮先 `fetch_round` 一次，再对每个 market 调 `collect_from` 本地挑价；删除旧的 per-market 网络版 `collect`。
- 日志新增：`feed started: N market(s) over M venue(s)`、`round R: K quote(s) fetched`。

### `node/src/service.rs`

- 价格源改为 Binance + OKX + Kraken + Bitfinex（均 USDT 计价，`ConversionDetails::NONE`）。
- Coinbase 只保留在 `rate_sources`（USDT-USD peg）。
- `min_exchanges: 2` 保持：四家取中位数，一家挂掉或某 market 未上线不影响出价。

### `node/Cargo.toml`

- reqwest features 增加 `gzip`。

### `node/src/bin/pricefeed-check.rs`

- 新增 "round fetch" 段：对每个 venue 一次请求取全部 market，用于实机验证批量 URL、批量解析与各家 symbol 拼写。

---

## 5. 效果对比

### 5.1 一轮实测（4 个 market，含 gzip）

| | 改前 | 改后 |
| --- | --- | --- |
| 请求数 | 20 | 5 |
| Binance | 4 × 1,916,687 B | 1 × 698 B（批量） |
| OKX | 8 × 371,7xx B | 1 × 100,577 B（全量 gzip） |
| Kraken | — | 1 × 532 B（批量） |
| Bitfinex | — | 1 × 292 B（批量） |
| Coinbase（peg） | 8 × 185 B | 1 × 154 B |
| **合计** | **10,641,932 B** | **102,485 B（100 KB）** |

**降 99%**。其中 OKX 全量 gzip 占改后总量的 98%。

按 30 s 一轮、两个 validator 各跑一份折算：**≈61 GB/天 → ≈0.6 GB/天**。

### 5.2 300 market 估算（30 s 一轮，单节点）

| 方案 | 请求数/轮 | 字节/轮 | 折算 |
| --- | --- | --- | --- |
| 改前做法（全量 × 每 market） | 900+ | ~800 MB | 不可行 |
| 全量、一轮一次 | 2 | 2.29 MB（gzip 0.39 MB） | 6.6 GB/天（gzip 1.1 GB） |
| 单 symbol、每 market 一次 | 900 | 0.33 MB | 字节最小，但 OKX/Coinbase 限流撑不住 |
| **本方案（批量 + 全量混合）** | ~19 | ~0.3 MB | **~0.9 GB/天** |

### 5.3 验证结果

- 单元测试：`cargo test -p sunchain --lib pricefeed` **28/28 通过**。
- 实机（`cargo run --bin pricefeed-check`，走代理）：Binance / OKX / Kraken / Bitfinex 均 **4/4 market**，四家价格互相吻合（BTC 81 614 / 81 615 / 81 617 / 81 602），Kraken 别名与 `tDOGE:UST` 冒号编码解析正确。
- `cargo check -p sunchain --bins` 通过。

---

## 6. 遗留与注意

- **重启即可生效**：只改 node 侧，runtime 未变，**不需要 `--fresh`** 清库。
- 观察日志 `round R: K quote(s) fetched`：4 market × 4 venue + 2 peg = 18；少几个说明某 venue 没覆盖该 market（属预期，中位数照常工作）。
- Kraken 批量上限保守取 20；若某批含未上线 pair，会退化为 20 次单请求（慢但不丢数据）。若以后 Kraken 市场数上百，需复核其批量上限。
- OKX 全量只覆盖 SPOT（1 186 inst）。若将来上合约 market，需再加 `instType=SWAP` 一次请求。
- Binance 批量 400/批是依据实测 414 边界留的余量；market 数超过 400 会自动分批。
- 几百 market 时还需评估链上侧：一轮一笔 vote 携带 N 个价格（300 个约 3.6 KB），每 30 s 每个 signer 一笔，区块空间与验证开销需另行核对。
