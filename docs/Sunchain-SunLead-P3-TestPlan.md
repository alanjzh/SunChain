# SunChain SunLead P3 — 测试计划（≥2 节点 E2E）

> 目标：在真实 2 节点网络下验证 SunLead 并行共识"能出块、能同步、能终局"，并验证在 `is_active=false` 时回退到 BABE。
> 前置：P3 代码已合入（`node/src/sunlead_consensus/` + `service.rs` 门控），`cargo check -p sunchain` 通过。

---

## 0. 关键约束（务必先读）

P3 的节点门控**在启动时读 genesis block 0 的 `is_active`**。而 runtime 侧 `SunLeadActive` 默认 `false`，且只能经 Root 调用翻转。因此：

- **要真正以 SunLead 出块做 E2E，必须让 `SunLeadActive = true` 出现在 genesis 状态里。**
- 仅用 `set_sunlead_active(true)` 运行时翻转 + 重启**不生效**（节点读的是 genesis）。

所以测试前必须先把"genesis 预置 SunLeadActive"打通（Phase 0）。

---

## Phase 0 — 让 SunLead 在 genesis 可激活（一次性代码改动）

给 `pallet-sunlead` 增加 `GenesisConfig`，并在 dev/测试 chain-spec 里置 `true`。

1. `pallets/sunlead/src/lib.rs` 增加：
   ```rust
   #[pallet::genesis_config]
   #[derive(Default)]
   pub struct GenesisConfig<T: Config> {
       pub sunlead_active: bool,
       #[serde(skip)]
       pub _phantom: sp_std::marker::PhantomData<T>,
   }

   #[pallet::genesis_build]
   impl<T: Config> GenesisBuild<T> for GenesisConfig<T> {
       fn build(&self) {
           SunLeadActive::<T>::put(self.sunlead_active);
       }
   }
   ```
2. `runtime/src/lib.rs` 的 `GenesisConfig` 组装里补 `sunlead: pallet_sunlead::GenesisConfig { sunlead_active: true }`（仅 dev/test chain-spec 置 `true`，主网默认 `false`）。
3. `node/src/chain_spec.rs` 的 `testnet_genesis` 对应位置补 `sunlead: Default::default()`（或显式 `sunlead_active: true`）。
4. 重新 `cargo build`（runtime 变了 → 旧库必须 `--fresh` 清掉，见记忆 P0）。

> 备用（更快但更脏）：临时把 `pallet_sunlead::is_active()` 实现改成 `true` 做冒烟测试，验证后再还原。不推荐进主干。

---

## Phase 1 — 构建

```bash
cd /home/alan/260909/sun_dev/sunchain
cargo build --release -p sunchain        # 或 debug，取决于机器
# 产物：target/release/sunchain
```

---

## Phase 2 — 启动 2 节点

用 dev chain-spec（已含 Alice/Bob 的 `sunl` 会话密钥，P2 已注入）。两节点各自独立 base-path，互相 bootnodes。

节点 A（Alice，权威节点）：
```bash
./target/release/sunchain \
  --chain dev --alice --validator \
  --base-path /tmp/sunlead-nodeA \
  --port 30333 --rpc-port 9944 \
  --rpc-methods unsafe --rpc-cors all \
  --fresh
```

节点 B（Bob，权威节点，连 A）：
```bash
./target/release/sunchain \
  --chain dev --bob --validator \
  --base-path /tmp/sunlead-nodeB \
  --port 30334 --rpc-port 9955 \
  --rpc-methods unsafe --rpc-cors all \
  --bootnodes /ip4/127.0.0.1/tcp/30333/p2p/<A的peer-id> \
  --fresh
```
（`A的peer-id` 从 A 启动日志 `Local node identity is: <peer-id>` 取。）

---

## Phase 3 — 校验项

### 3.1 启动日志（最直观）
- A/B 都应打印：`SunLead activation flag (is_active) = true` 与 `starting SunLead authoring (is_active = true)`。
- 若看到 `is_active = false` / 走 BABE，说明 genesis 未激活（回到 Phase 0）。

### 3.2 区块确实由 SunLead 生产
- 通过 RPC 取最新块并看 digest 是否含 `PreRuntime(SUNLEAD_ENGINE_ID, ...)`：
  ```bash
  curl -sH 'Content-Type: application/json' \
    --data '{"id":1,"jsonrpc":"2.0","method":"chain_getBlock"}' \
    http://127.0.0.1:9944 | jq '.result.block.header.digest'
  ```
  期望出现 engineId `sunl`（4 字节 ASCII）的 `PreRuntime` 条目；**不**应出现 BABE 的 `bab0`。
- 也可在 worker 里临时加 `debug!` 打印 `claim` 的 leader / slot 确认 `claim_slot` 命中。

### 3.3 双节点同步 + 终局性
- 节点 B 的 `chain_getHeader` 最佳块号随时间增长（说明从 A 同步 SunLead 区块成功）。
- GRANDPA 仍在跑：`grandpa` 日志出现 `Finalizing #N`；B 也能终局到相近高度。SunLead 只是换"谁出块"，终局性仍由 GRANDPA 负责。

### 3.4 leader 派生与 runtime 一致
- 用 RPC `state_call` 调 `SunLeadApi_leader_at(slot)` 与 `SunLeadApi_current_snapshot()`，确认节点侧 `build_schedule` 选出的 leader 与 runtime 返回一致（可在 `claim_slot` 命中时打印 `leader_id`，与 `leader_at(slot)` 比对）。

### 3.5 负向：无 scheduled `sunl` 密钥的节点不出块
- 启一个**普通验证者**（有 babe/grandpa 但没被排进当前 window 的 `sunl`）的第三节点，观察其不生产 SunLead 区块（slot 静默跳过），但仍能同步/终局。

---

## Phase 4 — 回退到 BABE（对照）

- 用**未激活**的 genesis（Phase 0 里主网默认值 `sunlead_active: false`）重复 Phase 2。
- 期望日志：`is_active = false`，走 `start_babe`，区块 digest 含 `bab0`，网络正常出块 + 终局。
- 目的：证明 P3 未破坏既有 BABE/GRANDPA 路径。

---

## Phase 5 — runtime API 单点校验（可选，RPC）

```bash
# 取当前快照
curl -sH 'Content-Type: application/json' --data '{"id":1,"jsonrpc":"2.0",
  "method":"state_call","params":["SunLeadApi_current_snapshot","0x"]}' http://127.0.0.1:9944
# 取某 slot 的 leader
curl -sH 'Content-Type: application/json' --data '{"id":1,"jsonrpc":"2.0",
  "method":"state_call","params":["SunLeadApi_leader_at","0x<scale-encoded-slot>"]}' http://127.0.0.1:9944
```
确认返回非空且与 `configuration()` 的 `params` 自洽。

---

## 验收标准

| 项 | 通过条件 |
|----|----------|
| SunLead 出块 | 激活 genesis 下，节点稳定产出含 `sunl` PreRuntime 的区块 |
| 双节点同步 | 第二节点最佳块号持续增长并追上首节点 |
| GRANDPA 终局 | 两节点均能终局到相近高度（终局性不依赖 BABE） |
| leader 一致 | 节点侧派生 leader == runtime `leader_at` |
| 负向 | 无对应 `sunl` 密钥的验证者静默跳过 slot |
| BABE 回退 | 未激活 genesis 下走 `start_babe`，行为与改动前一致 |
| 无回归 | `cargo check -p sunchain` / `-p sunchain-runtime` 绿灯；`runtime_integrity_tests` 的失败仍是 pre-existing（staking 常量） |

---

## 备注 / 待办

- Phase 0 的 genesis 注入是 E2E 跑通 SunLead 出块的前提；若先只验证"管线/验证器/门控编译与 BABE 回退"，可跳过 Phase 0 直接 Phase 4。
- 真正的运行时动态 handoff（不重启切换）属 P5；本计划按 P3 静态门控设计。
- slash 接入 `pallet-offences` 属 P4，本计划不包含。
