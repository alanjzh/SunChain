# SunChain SunLead P2 — Runtime Wiring 工作日志

> 日期：2026-10-26
> 阶段：P2（Runtime 接入，不影响现有 BABE/GRANDPA 出块）
> 前置：P1（crates/sunlead-primitives、sunlead-schedule、sunlead-verifier）已完成并通过测试；设计决策已确认（签名算法 SR25519、排程语义不变）。

---

## 0. 目标

在不扰动现有 BABE/GRANDPA 出块的前提下，把 SunLead 的"确定性 leader"能力接入 Runtime：

1. 新增 `pallet-sunlead`，在每个 epoch 边界冻结 authority 快照 + 计算 schedule（root / leaders / windows）。
2. 为 Runtime 的 `SessionKeys` 新增第 5 把会话密钥 `sunl`（SR25519，与 BABE 对齐）。
3. 暴露 `SunLeadApi` runtime API（configuration / current_snapshot / leader_at / schedule_proof），供 P3 的节点侧派生与验证使用。
4. 把 `sunl` 密钥接入 chain-spec（开发/生产 genesis 均已包含）。

P2 不实现区块生产/导入管线、不做 slash 接入 `pallet-offences`——那是 P3/P4。

---

## 1. 落地的代码改动

### 1.1 `crates/sunlead-primitives`（补充 Runtime 侧所需 derive）
- `SunLeadScheduleParams` 增加 `TypeInfo`, `MaxEncodedLen` 并补 `Default`（用于 `ValueQuery` 存储）；其余结构体（`SunLeadPreDigestBody/PreDigest/Authority/AuthoritySnapshot/ScheduleContext/EquivocationProof`）补 `TypeInfo`。
- 导入 `codec::MaxEncodedLen`、`scale_info::TypeInfo`。

### 1.2 `crates/sunlead-schedule`
- `ComputedSchedule` 增加 `Encode, Decode, TypeInfo`。
- 新增 `SunLeadScheduleProof`（含 `root/proof/number_of_leaves/leaf_index`，带 `TypeInfo`）——因为 vendored 的 `binary_merkle_tree::MerkleProof` 没有 `TypeInfo`，不能直接作为 runtime API 返回类型。`schedule_proof()` 改为返回 `SunLeadScheduleProof`，`verify_schedule_entry()` 同步接受该类型。

### 1.3 新增 `pallets/sunlead`（FRAME v2 pallet）
- `Cargo.toml`：按仓库约定引用 workspace 下的 frame-support/frame-system/sp-runtime/sp-std/codec/binary-merkle-tree，以及 `sunlead-primitives`、`sunlead-schedule` 的 path 依赖；feature 开关对齐（`std`/`try-runtime`/`runtime-benchmarks`）。
- `lib.rs`：
  - `Config`：`Session`/`Keys`/`Staking`/`Babe`/`Grandpa`/`ChainId`，外加 `StakingAccountConvert: Convert<AccountId, StakingAccountId>`（用 `sp_runtime::traits::Identity` 实例化，因本链三者同为 H160）。`Keys: OpaqueKeys + SunLeadKeyLookup + codec::FullCodec`。
  - 存储：`ScheduleParams`(ValueQuery)、`EpochSeed`(ValueQuery)、`Snapshot`(unbounded)、`Schedule`(unbounded)、`CurrentAuthoritySetId`、`LastSession`(u32,ValueQuery)、`SunLeadStorageVersion`。
  - `on_initialize`：session 变化时调用 `build_current_snapshot` 重建快照 + schedule 并存盘。
  - `build_current_snapshot`：从 `pallet_grandpa::CurrentSetId` 取 authority_set_id；从 `pallet_babe::Randomness` 取 finalized epoch 随机性作为 epoch_seed；遍历 `pallet_session::Validators`，经 `NextKeys` 取 `sunl` 公钥（经 `SunLeadKeyLookup`），经 `pallet_staking::eras_stakers` 取 exposure.total 作为 stake；组装 `SunLeadScheduleContext` 调 `sunlead_schedule::build_schedule`，存 `Snapshot`/`Schedule`。
  - 对外方法：`configuration`、`current_snapshot`、`leader_at`、`schedule_proof`（供 runtime API 转发）。

### 1.4 Runtime 注册
- 根 `Cargo.toml`：`members` 增加 `pallets/sunlead`；`workspace.dependencies` 增加 `pallet-sunlead`。
- `runtime/Cargo.toml`：增加 `pallet-sunlead`、`sunlead-primitives`、`sunlead-schedule` 依赖；在 `std`/`runtime-benchmarks`/`try-runtime` feature 中补齐对应条目。
- `runtime/src/lib.rs`：在 `#[frame_support::runtime]` 宏中以 `pub type SunLead = pallet_sunlead;` 注册（pallet_index = 26，紧接 `Oracle = 25`），`max` 同步 +1。
- `runtime/src/configs/mod.rs`：
  - `impl pallet_sunlead::Config for Runtime`（Session/Keys/Staking/Babe/Grandpa/ChainId 全部 = Runtime，StakingAccountConvert = Identity）。
  - `impl_opaque_keys!` 的 `SessionKeys` 增加 `pub sunl: SunLeadSessionKey`（第 5 把）。
  - 新增 `SunLeadKeyLookup` trait 的 `impl for SessionKeys`（`sunl_key()` 返回 `self.sunl.clone()`）。
  - 新增 `SunLeadSessionKey` 占位类型，实现 `BoundToRuntimeAppPublic<Public = SunLeadId>` 与 `OneSessionHandler<Runtime::AccountId>`（no-op 占位；P3 再接真实共识逻辑）。

### 1.5 Runtime API
- `runtime/src/apis.rs`：在 `decl_runtime_apis!` 中新增 `SunLeadApi`（4 个方法）；在 `impl_runtime_apis!` 中以 `impl self::SunLeadApi<Block> for Runtime` 实现（转调 pallet-sunlead）。

### 1.6 Node / chain-spec
- `node/src/chain_spec.rs`：`session_keys` 与 `authority_keys_from_alice` 增加第 5 个参数 `sunl: SunLeadId`（Alice 种子生成）；`testnet_genesis` 的 `initial_authorities` 元组扩为 7 元组；`session_keys(...)` 调用补 `x.6.clone()`；导入 `sunlead_primitives::SunLeadId`。
- `node/src/chain_spec/initial_config.rs`：所有 authority 元组类型（6→7）、`session_keys_json`（补 `sunl` 字段）、`session_keys`、`session_keys_from_seed`、`session_keys_json(...)` 调用及两个 dev 元组均补 `SunLeadId`；导入 `sunlead_primitives::SunLeadId`。
- `node/Cargo.toml`：增加 `sunlead-primitives` 依赖（features = ["std"]），并在 std feature 中启用 `sunlead-primitives/std`。

---

## 2. 编译期踩坑与修复（共 14 条）

1. **WASM 构建要求 trait 带路径**：`impl_runtime_apis!` 中的 `impl SunLeadApi<Block> for Runtime` 必须写成 `impl self::SunLeadApi<Block> for Runtime`（与同文件 `ExchangeRuntimeApi` 一致）。
2. **`MerkleProof` 无 `TypeInfo`**：runtime API 不能直接返回 vendored `binary_merkle_tree::MerkleProof`；新增 `SunLeadScheduleProof` 包装类型承载 root/proof/number_of_leaves/leaf_index。
3. **`sunl` 会话密钥需 `OneSessionHandler` + `BoundToRuntimeAppPublic`**：`impl_opaque_keys!` 字段类型必须同时满足两者。`SunLeadSessionKey` 占位实现两者（no-op），对齐 polkadot 的 `ParachainSessionKeyPlaceholder` 模式。
4. **`OneSessionHandler` 泛型参数 T 未约束（E0207）**：占位类型必须 concrete 于 Runtime 的 `AccountId`，故用 `impl OneSessionHandler<<Runtime as frame_system::Config>::AccountId> for SunLeadSessionKey`（非泛型 T，避免 unconstrained）。
5. **`BoundToRuntimeAppPublic` 路径**：它是 `sp_runtime::BoundToRuntimeAppPublic`（crate 根），不是 `sp_runtime::traits::BoundToRuntimeAppPublic`。
6. **`codec`/`scale_info` 缺失**：`sunlead-primitives`、`sunlead-schedule` 补 `MaxEncodedLen`、`TypeInfo` 导入与 `codec`/`scale-info` 依赖（及 std feature）。
7. **`Snapshot`/`Schedule` 含 `Vec` 的 StorageValue 需 `#[pallet::unbounded]`**（先误用 `without_storage_info`，后改 `unbounded`）。
8. **`<T::AccountId>` 在 storage 类型别名中报错**：改为全限定 `<T as frame_system::Config>::AccountId`。
9. **`pallet_session::SessionIndex` 私有**：pallet 内部用 `u32` 表示 session 索引。
10. **`Keys` 需 `codec::FullCodec`**：`NextKeys::<T::Session>::get()` 要求值类型 `FullCodec`，给 `Config::Keys` 加 `codec::FullCodec` bound。
11. **AccountId 泛型耦合**：`Session`/`Staking` 的 `ValidatorId` 必须 = frame_system 的 `AccountId`；引入 `StakingAccountConvert: Convert<...>`（运行时 = `Identity`）。
12. **chain-spec 元组宽度**：`initial_authorities`/dev 元组从 6 扩到 7；注意 `authority_keys_from_alice` 返回类型用 4 空格缩进、`configure_accounts`/`testnet_genesis` 参数用 8 空格缩进，两处都要补 `SunLeadId`（replace_all 只命中 4 空格块，8 空格块需单独补）。
13. **node 缺 `sunlead-primitives` 依赖**：在 `node/Cargo.toml` 增加并启用 `std` feature。
14. **P1 测试回归**：`schedule_proof` 返回类型变更后，`verify_schedule_entry` 与测试需同步改为 `SunLeadScheduleProof`（已修）。

---

## 3. 验证结果

| 检查 | 结果 |
|------|------|
| `cargo check -p pallet-sunlead` | 通过 |
| `cargo check -p sunlead-primitives -p sunlead-schedule -p sunlead-verifier` | 通过 |
| `cargo check -p sunchain-runtime`（含 WASM 构建） | 通过（exit 0） |
| `cargo check -p sunchain`（node 二进制） | 通过（exit 0） |
| `cargo check -p sun-admin` | 通过（exit 0） |
| `cargo test -p sunlead-primitives -p sunlead-schedule -p sunlead-verifier` | 全绿（2 / 10 / 8） |
| `cargo test -p sunchain-runtime` | 4 通过；**1 个失败（pre-existing，见 §4）** |
| `cargo check`（默认成员，= 用户日常构建） | 通过（exit 0） |

---

## 4. 未做 / 已知问题（均非 P2 引入）

- **`runtime_integrity_tests` 失败（pre-existing）**：staking 配置 `BondingDuration = 1` 与 `SlashDeferDuration = 1`（configs/mod.rs:642-643，开发值，真实值被注释），违反 `SlashDeferDuration < BondingDuration` 不变量。**与 SunLead 无关**，是仓库既有误配置。修复需调整 staking 常量（建议恢复注释中的真实值），但超出 P2 范围，需单独确认。
- **`cargo check --workspace` 失败（pre-existing）**：`pallet-revive-fixtures` 的 build script 在 rustc 1.96-nightly 上无法编译 polkavm 合约 fixtures（`panic_immediate_abort`/core 编译错误）。已在 `runtime/Cargo.toml` 注释中记录（§11.2），是工具链与 vendored SDK 不兼容问题，仅 `--workspace` 触发，默认成员构建不受影响。
- **P0（仓库改名 / `--fresh` 清库）**：仍按计划在 P0 阶段执行，P2 未动。
- **P3+**：区块生产/导入管线、SunLead 出块/验证、slash 接入 `pallet-offences`、P3 时把 `SunLeadSessionKey` 占位升级为真实共识逻辑。

---

## 5. 下一步建议

1. 处理 §4 的两个 pre-existing 问题（staking 常量、pallet-revive-fixtures 工具链），或确认二者暂缓。
2. 进入 **P3**：让节点侧在出块/导入时调用 `SunLeadApi` 派生并验证 slot 的确定性 leader，把 `SunLeadSessionKey` 占位升级为有实际行为的会话处理器；并接 `pallet-offences` 实现 slash。
3. 在 `pallet-sunlead` 补单元测试（mock runtime + genesis 注入 `sunl` 密钥，验证 `build_current_snapshot`/`leader_at`/`schedule_proof`）。
