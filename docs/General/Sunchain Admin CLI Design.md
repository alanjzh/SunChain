这个工具本质上就是 **一个 Runtime Transaction Builder**，并不依赖 `subxt`，而是复用 `polkadot-sdk` 的编码和签名逻辑。下面是一份设计文档，按照 **Polkadot SDK stable2506 + Frontier stable2506 + EthereumSignature** 来设计。

---

# Sunchain Admin CLI 设计文档

## 1. 目标

提供一个命令行工具，用于向 SunChain Runtime 提交 **非 Ethereum Transaction**。

支持：

* sudo
* balances
* staking
* session
* babe
* baseFee
* template pallet

不支持：

* eth_sendRawTransaction
* EVM Contract Call

因为这两类已经由 `pallet_ethereum` RPC 提供。

---

# 2. 总体架构

```text
                  CLI

      sudo basefee transfer ...

               │

        Runtime Call Builder

               │

        SignedPayload Builder

               │

      EthereumSignature Signer

               │

UncheckedExtrinsic Encoder

               │

 author_submitExtrinsic RPC

               │

            Full Node
```

整个工具只负责：

> Runtime Extrinsic

不负责：

> Ethereum Transaction

---

# 3. 工程模块

```text
src/

main.rs

rpc.rs

metadata.rs

tx_builder.rs

signer.rs

encoder.rs

calls/

    sudo.rs

    balances.rs

    staking.rs

    basefee.rs

types.rs
```

---

# 4. rpc.rs

负责：

```
chain_getBlockHash

chain_getGenesisHash

state_getRuntimeVersion

system_accountNextIndex

author_submitExtrinsic
```

全部使用 jsonrpsee websocket。

输出：

```rust
pub struct RuntimeInfo {

    genesis_hash: H256,

    spec_version: u32,

    transaction_version: u32,

    nonce: u32,

    finalized_head: H256,
}
```

---

# 5. metadata.rs

读取：

```
state_getMetadata
```

用于：

Runtime Metadata Version 检查。

不参与编码。

编码直接使用 Runtime crate。

---

# 6. Runtime Call Builder

例如：

```
sudo(baseFee.setBaseFeePerGas(...))
```

构造成：

```text
RuntimeCall

↓

RuntimeCall::Sudo(

    pallet_sudo::Call::sudo {

        call:

            RuntimeCall::BaseFee(

                pallet_base_fee::Call::set_base_fee_per_gas

            )

    }

)
```

因此：

整个工具依赖：

```
sunchain-runtime
```

而不是 Metadata 动态编码。

优点：

类型安全。

---

# 7. SignedExtension

完全复用 Runtime：

```
TxExtension
```

即：

```
CheckNonZeroSender

CheckSpecVersion

CheckTxVersion

CheckGenesis

CheckEra

CheckNonce

CheckWeight

ChargeTransactionPayment

CheckMetadataHash

WeightReclaim
```

工具不重新实现。

直接：

```rust
SignedPayload::<RuntimeCall, TxExtension>::from_raw(...)
```

---

# 8. Payload Builder

输入：

```
RuntimeCall

Era

Nonce

Tip

SpecVersion

TxVersion

GenesisHash

BlockHash
```

输出：

```
SignedPayload
```

与 Runtime 完全一致。

---

# 9. EthereumSignature Signer

这是核心。

输入：

```
Ethereum Secret Key
```

输出：

```
fp_account::EthereumSignature
```

流程：

```
SignedPayload.encode()

↓

blake2_256

↓

secp256k1 recoverable sign

↓

65-byte Signature

↓

EthereumSignature
```

这里直接复用：

```
fp_account
```

不要自己实现 Signature 类型。

---

# 10. Extrinsic Builder

构造：

```
UncheckedExtrinsic::

new_signed(

    RuntimeCall,

    Address,

    EthereumSignature,

    TxExtension

)
```

得到：

```
UncheckedExtrinsic
```

随后：

```
Encode

↓

0x...

↓

author_submitExtrinsic
```

---

# 11. Call 模块

例如：

## sudo

```rust
pub fn sudo(

    call: RuntimeCall

) -> RuntimeCall
```

内部：

```
RuntimeCall::Sudo(...)
```

---

## baseFee

```
set_base_fee_per_gas()

set_elasticity()
```

返回：

```
RuntimeCall::BaseFee(...)
```

---

## balances

```
transfer_keep_alive()

force_transfer()

transfer_all()
```

---

## staking

```
bond()

unbond()

nominate()

validate()

set_controller()
```

全部返回：

```
RuntimeCall
```

---

# 12. Address

Runtime：

```
Address = AccountId
```

所以：

CLI 输入：

```
0x6de2...
```

解析：

```
H160

↓

AccountId20
```

即可。

---

# 13. CLI

例如：

```
sun-admin

sudo

basefee

--gas-price

1000000000
```

流程：

```
CLI

↓

BaseFee Builder

↓

RuntimeCall

↓

Sudo Builder

↓

RuntimeCall

↓

Sign

↓

Submit
```

---

# 14. 配置

config.toml

```
rpc="ws://127.0.0.1:9944"

private_key="0x..."

tip=0

mortality=64
```

---

# 15. 生命周期

```
Connect RPC

↓

Load Runtime Version

↓

Load Genesis Hash

↓

Load Nonce

↓

Build RuntimeCall

↓

Build TxExtension

↓

Build Payload

↓

Ethereum Sign

↓

UncheckedExtrinsic

↓

Encode SCALE

↓

Submit

↓

Watch InBlock

↓

Watch Finalized
```

---

# 16. 与 Subxt 的区别

| 项目                         | 本工具                       | subxt               |
| -------------------------- | ------------------------- | ------------------- |
| Runtime Call               | ✅                         | ✅                   |
| Metadata 动态编码              | ❌                         | ✅                   |
| 类型安全                       | ✅                         | ❌（动态接口时）            |
| `EthereumSignature`        | ✅                         | ❌（目前缺少现成 Signer 支持） |
| Self-contained Transaction | 不处理（交给 `pallet_ethereum`） | 不适用                 |
| Runtime 升级后重新编译            | 需要                        | 动态接口可减少重新编译         |

---

# 17. 可扩展性

建议把整个工具设计成两层：

```text
                 CLI

          sudo transfer ...

                 │

         Runtime Tx Builder

                 │

         Signer Trait

        ┌───────────────┐
        │EthereumSigner │
        │Sr25519Signer  │
        │Ed25519Signer  │
        │EcdsaSigner    │
        └───────────────┘
```

虽然目前只实现 `EthereumSigner`，但通过抽象一个 `Signer` trait，未来如果 Runtime 改回 `MultiSignature`，只需要替换签名器实现，不需要修改交易构造、RPC 或调用层逻辑。这也让工具能够同时适配不同账户模型，而保持统一的 CLI 和 Runtime Call Builder。

# 18. 当前实现与用法

工具位于 `admin/`，二进制名称为 `sun-admin`。它复用 `sunchain-runtime::RuntimeCall`、`TxExtension` 和 `UncheckedExtrinsic`，通过 WebSocket RPC 查询 genesis hash、区块 hash、runtime version、nonce，然后签名并调用 `author_submitExtrinsic`。

```bash
# BaseFeePerGas = 0；该调用自动包装为 sudo.sudo(base_fee.set_base_fee_per_gas(...))
cargo run -p sun-admin -- \
  --rpc ws://127.0.0.1:9944 \
  --private-key 0x<sudo-account-private-key> \
  base-fee set-base-fee-per-gas 0

# elasticity = 0；参数单位为百万分比，0 表示 0，1000000 表示 1
cargo run -p sun-admin -- \
  --rpc ws://127.0.0.1:9944 \
  --private-key 0x<sudo-account-private-key> \
  base-fee set-elasticity 0

# 发送 staking.payoutStakers(validator_stash, era)
cargo run -p sun-admin -- \
  --rpc ws://127.0.0.1:9944 \
  --private-key 0x<submitter-private-key> \
  staking payout-stakers 0x<validator-stash> <era>

# 只读查询不需要 --private-key
cargo run -p sun-admin -- --rpc ws://127.0.0.1:46581 query staking current-era
cargo run -p sun-admin -- --rpc ws://127.0.0.1:46581 query balance 0x<account>
cargo run -p sun-admin -- --rpc ws://127.0.0.1:46581 query base-fee
cargo run -p sun-admin -- --rpc ws://127.0.0.1:46581 query sudo

# 查询 validator 状态以及某个 era 是否还有待领取奖励
cargo run -p sun-admin -- --rpc ws://127.0.0.1:46581 query staking validator 0x<validator-stash>
cargo run -p sun-admin -- --rpc ws://127.0.0.1:46581 query staking pending-rewards 0x<validator-stash> <era>
cargo run -p sun-admin -- --rpc ws://127.0.0.1:46581 query staking eras-stakers-overview 0x<validator-stash> <era>
```

Ethereum 私钥对应的账户必须与 runtime 的 `AccountId`（20 字节地址）一致。BaseFee 两个 extrinsic 要求 Root，因此必须使用 Sudo 账户；`staking.payoutStakers` 本身是 signed call，不需要 Sudo。当前签名摘要使用 Keccak-256，以匹配本链 `fp_account::EthereumSignature::verify` 的实现。
