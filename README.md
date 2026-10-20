# Sunchain Node 

## Getting Started

```
### Build

Use the following command to build the node without launching it:

```sh
cargo build --release
```


### Single-Node Development Chain

The following command starts a single-node development chain that doesn't
persist state:

```sh
./target/release/sunchain --dev
```

To purge the development chain's state, run the following command:

```sh
./target/release/sunchain purge-chain --dev
```

To start the development chain with detailed logging, run the following command:

```sh
RUST_BACKTRACE=1 ./target/release/sunchain -ldebug --dev
```

Development chains:

- Maintain state in a `tmp` folder while the node is running.
- Use the **Alice** and **Bob** accounts as default validator authorities.
- Use the **Alice** account as the default `sudo` account.
- Are preconfigured with a genesis state (`/node/src/chain_spec.rs`) that
  includes several pre-funded development accounts.


To persist chain state between runs, specify a base path by running a command
similar to the following:

```sh
// Create a folder to use as the db base path
$ mkdir my-chain-state

// Use of that folder to store the chain state
$ ./target/release/sunchain --dev --base-path ./my-chain-state/

// Check the folder structure created inside the base path after running the chain
$ ls ./my-chain-state
chains
$ ls ./my-chain-state/chains/
dev
$ ls ./my-chain-state/chains/dev
db keystore network
```

## Lauch producation Node

### Modify chainSpec

```sh
./target/release/sunchain build-spec > plain-sunchain-chainspec.json

// Update name, id and protocolId to unique values.

./target/release/sunchain build-spec --chain plain-sunchain-chainspec.json --raw > raw_plain-sunchain-chainspec.json

// Generate a node key

../target/release/sunchain key generate-node-key --chain raw_plain-sunchain-chainspec.json --base-path ./base

// Generating key in "./base/chains/SunCred/network/secret_ed25519"
// 12D3KooWNsUMccojevvvpiDSkTaJVs19ZP7iSd71A2nEYNiL6Fz6

```

### import key to Chain Keystore

```sh
target/debug/sunchain key insert -d c_test/ --chain sunchain --scheme ed25519 --suri "immune dirt melody hour say remind era vivid oblige hope issue breeze" --key-type gran --detailed-log-output

target/debug/sunchain key insert -d chain_data/base --chain sunchain --scheme ed25519 --suri 0x74e2d3d60847d2ed1169009a1570788d3ec1d1532ad0c768e4b1c3018cf99a3a --key-type gran --detailed-log-output

target/debug/sunchain key insert -d chain_data/base --chain sunchain --scheme sr25519 --suri 0x74e2d3d60847d2ed1169009a1570788d3ec1d1532ad0c768e4b1c3018cf99a3a --key-type aura --detailed-log-output

sunchain/target/release/sunchain key insert -d chain_data/slave/base --chain sunchain --scheme ed25519 --suri 0x26712bfe38c72e6532ef36dbdff4c1c269ff6978bbd3204320159864c3fcc88a --key-type gran

sunchain/target/release/sunchain key insert -d chain_data/slave/base --chain sunchain --scheme sr25519 --suri 0x26712bfe38c72e6532ef36dbdff4c1c269ff6978bbd3204320159864c3fcc88a --key-type aura

sunchain/target/release/sunchain key generate-node-key --chain sunchain --base-path chain_data/master/base
Generating key in "chain_data/master/base/chains/SUN/network/secret_ed25519"
12D3KooWJ3aGvBqe73qYrRf38q57XJYkpJ9ZaEoE2uRaXDQQv9mq

sunchain/target/release/sunchain key generate-node-key --chain sunchain --base-path chain_data/slave/base
Generating key in "chain_data/slave/base/chains/SUN/network/secret_ed25519"
12D3KooWJYHLLQYFsYtp6JXqDvFn7JDuRZyhyNk7fyFfMrwi4xeb

sunchain/target/release/sunchain key generate-node-key --chain sunchain --base-path chain_data/relay/base
Generating key in "chain_data/relay/base/chains/SUN/network/secret_ed25519"
12D3KooWMCvtEiMVbWghogH5H4D3EnovEynHF3arXPRweYYQgoSr

```


###  starts a single-node Sunchain chain

```sh
../target/release/sunchain --chain raw_plain-sunchain-chainspec.json --base-path ./base --alice  --validator -l session=debug,aura=debug,sc_basic_authorship=debug --force-authoring


target/debug/sunchain -d chain_data/base --chain sunchain --validator -l aura=debug,sc_basic_authorship=debug,session=debug,transaction_pool=debug --force-authoring

target/release/sunchain -d chain_data/master/base --chain sunchain --validator --rpc-port 9944 --discover-local 2>&1 | tee chain_data/master/sunchain_master.log

sunchain/target/release/sunchain -d chain_data/slave/base --chain sunchain --validator --rpc-port 9955 --port 30444 --no-prometheus --discover-local --bootnodes /ip4/127.0.0.1/tcp/30333/p2p/12D3KooWJ3aGvBqe73qYrRf38q57XJYkpJ9ZaEoE2uRaXDQQv9mq 2>&1 | tee chain_data/slave/sunchain_slave.log


sunchain/target/release/sunchain -d chain_data/relay/base --chain sunchain --public-addr /ip4/192.168.1.98/tcp/30333/p2p/12D3KooWMCvtEiMVbWghogH5H4D3EnovEynHF3arXPRweYYQgoSr --port 30333 --rpc-port 9944 --no-telemetry --no-prometheus 2>&1 | tee chain_data/relay/sunchain.log

sunchain/target/release/sunchain -d chain_data/relay/base --chain sunchain --port 30333 --rpc-port 9944 --no-telemetry --no-prometheus 2>&1 | tee chain_data/relay/sunchain.log

grep "discovered peer on address" ../chain_data/relay/sunchain_relay.log

sudo ip netns exec vlan1 sunchain/target/release/sunchain -d chain_data/master/base --chain sunchain --validator --rpc-port 9944  --bootnodes /ip4/192.168.1.98/tcp/30333/ws/p2p/12D3KooWMCvtEiMVbWghogH5H4D3EnovEynHF3arXPRweYYQgoSr --no-telemetry --no-prometheus -l sub-libp2p=debug 2>&1 | tee chain_data/master/sunchain.log

sudo ip netns exec vlan2 sunchain/target/release/sunchain -d chain_data/slave/base --chain sunchain --validator --rpc-port 9944  --bootnodes /ip4/192.168.1.98/tcp/30333/ws/p2p/12D3KooWMCvtEiMVbWghogH5H4D3EnovEynHF3arXPRweYYQgoSr --no-telemetry --no-prometheus -l sub-libp2p=debug 2>&1 | tee chain_data/slave/sunchain.log

curl -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"state_call","params":["AuraApi_authorities","0x",null],"id":1}' http://127.0.0.1:9944 | jq

curl -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"system_peers","params":[],"id":1}' http://127.0.0.1:9944 | jq

```

### Enable Hole Punching for node in vlan Commuination

```sh
# Relay
sunchain/target/release/sunchain -d chain_data/relay/base --chain sunchain --listen-addr /ip4/192.168.1.98/tcp/30333 --rpc-port 9944 --no-telemetry --no-prometheus -l sub-libp2p=debug 2>&1 | tee chain_data/relay/sunchain.log

# vlan 1

sudo ip netns exec vlan1 sunchain/target/release/sunchain -d chain_data/master/base --chain sunchain --validator --rpc-port 9944 --listen-addr /ip4/10.1.1.2/tcp/30333 --bootnodes /ip4/192.168.1.98/tcp/30333/p2p/12D3KooWMCvtEiMVbWghogH5H4D3EnovEynHF3arXPRweYYQgoSr --no-telemetry --no-prometheus -l sub-libp2p=debug 2>&1 | tee chain_data/master/sunchain.log

# vlan 2

sudo ip netns exec vlan2 sunchain/target/release/sunchain -d chain_data/slave/base --chain sunchain --validator --rpc-port 9944  --listen-addr /ip4/10.1.2.2/tcp/30333 --bootnodes /ip4/192.168.1.98/tcp/30333/p2p/12D3KooWMCvtEiMVbWghogH5H4D3EnovEynHF3arXPRweYYQgoSr --no-telemetry --no-prometheus -l sub-libp2p=debug 2>&1 | tee chain_data/slave/sunchain.log

# trigger Node Found from vlan2

sudo ip netns exec vlan2 curl -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","id":1,"method":"system_addReservedPeer","params":["/ip4/192.168.1.98/tcp/30333/p2p/12D3KooWMCvtEiMVbWghogH5H4D3EnovEynHF3arXPRweYYQgoSr/p2p-circuit/p2p/12D3KooWJ3aGvBqe73qYrRf38q57XJYkpJ9ZaEoE2uRaXDQQv9mq"]}' http://127.0.0.1:9944

# verify

sudo ip netns exec vlan2 curl -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"system_peers","params":[],"id":1}' http://127.0.0.1:9944 | jq


# staking support
sunchain/target/debug/sunchain --chain sunchain --validator --unsafe-rpc-external --rpc-methods unsafe --rpc-cors all -d chain_data/master/base --force-authoring --no-telemetry -l babe=debug,sc_basic_authorship=debug,grandpa=debug,session=debug 2>&1 | tee chain_data/master/sunchain.log

sunchain/target/debug/sunchain -d chain_data/slave/base --chain sunchain --validator --unsafe-rpc-external --rpc-methods unsafe --rpc-port 9966 --rpc-cors all --listen-addr /ip4/192.168.1.98/tcp/30444 --bootnodes /ip4/192.168.1.98/tcp/30333/p2p/12D3KooWJ3aGvBqe73qYrRf38q57XJYkpJ9ZaEoE2uRaXDQQv9mq --no-telemetry --no-prometheus -l debug 2>&1 | tee chain_data/slave/sunchain.log

 sunchain/target/debug/sunchain --chain sunchain --validator --unsafe-rpc-external --rpc-methods unsafe --rpc-cors all -d chain_data/slave/base --force-authoring -l babe=debug,sc_basic_authorship=debug,grandpa=trace,session=debug 2>&1 | tee chain_data/slave/sunchain.log

sunchain/target/debug/sunchain key insert --chain sunchain -d chain_data/master/base --suri 0x74e2d3d60847d2ed1169009a1570788d3ec1d1532ad0c768e4b1c3018cf99a3a --scheme sr25519 --key-type babe 

sunchain/target/debug/sunchain key insert --chain sunchain -d chain_data/master/base --suri 0x74e2d3d60847d2ed1169009a1570788d3ec1d1532ad0c768e4b1c3018cf99a3a --scheme sr25519 --key-type imon

sunchain/target/debug/sunchain key insert --chain sunchain -d chain_data/master/base --suri 0x74e2d3d60847d2ed1169009a1570788d3ec1d1532ad0c768e4b1c3018cf99a3a --scheme sr25519 --key-type audi


sunchain/target/debug/sunchain -d chain_data/master/base --chain sunchain --validator --rpc-port 9966 --listen-addr /ip4/192.168.1.98/tcp/30444 --bootnodes /ip4/192.168.1.98/tcp/30333/p2p/12D3KooWJYHLLQYFsYtp6JXqDvFn7JDuRZyhyNk7fyFfMrwi4xeb --no-telemetry --no-prometheus -l babe=debug,sc_basic_authorship=debug,grandpa=trace 2>&1 | tee chain_data/master/sunchain.log

# Alan
sunchain/target/release/sunchain key generate
Secret phrase:       oblige action return divorce scare fruit cream hen occur feel manage derive
  Network ID:        substrate
  Secret seed:       0x875057344c45814c259b4284630131192709357a7e75713fc2ac3ed1ac991a3d
  Public key (hex):  0x82ce60f97d676efaa16715431cff1c5e58fd40b09521b802a53e4e18ec0ecb24
  Account ID:        0x82ce60f97d676efaa16715431cff1c5e58fd40b09521b802a53e4e18ec0ecb24
  Public key (SS58): 5F2DPYUXzDCNEEidfcoFaq87MeVwNLj578gW3i3nouJmg7Qn
  SS58 Address:      5F2DPYUXzDCNEEidfcoFaq87MeVwNLj578gW3i3nouJmg7Qn

sunchain/target/release/sunchain key insert --chain sunchain -d chain_data/alan/base --suri 0x875057344c45814c259b4284630131192709357a7e75713fc2ac3ed1ac991a3d --scheme sr25519 --key-type babe

sunchain/target/release/sunchain key insert --chain sunchain -d chain_data/alan/base --suri 0x875057344c45814c259b4284630131192709357a7e75713fc2ac3ed1ac991a3d --scheme ed25519 --key-type gran

sunchain/target/release/sunchain key insert --chain sunchain -d chain_data/alan/base --suri 0x875057344c45814c259b4284630131192709357a7e75713fc2ac3ed1ac991a3d --scheme sr25519 --key-type imon

sunchain/target/release/sunchain key insert --chain sunchain -d chain_data/alan/base --suri 0x875057344c45814c259b4284630131192709357a7e75713fc2ac3ed1ac991a3d --scheme sr25519 --key-type audi

sunchain/target/release/sunchain key generate-node-key --chain sunchain --base-path chain_data/alan/base

sunchain/target/release/sunchain -d chain_data/alan/base --chain sunchain --validator --rpc-port 9977 --listen-addr /ip4/192.168.1.98/tcp/30555 --bootnodes /ip4/192.168.1.98/tcp/30333/p2p/12D3KooWJYHLLQYFsYtp6JXqDvFn7JDuRZyhyNk7fyFfMrwi4xeb,/ip4/192.168.1.98/tcp/30444/p2p/12D3KooWJ3aGvBqe73qYrRf38q57XJYkpJ9ZaEoE2uRaXDQQv9mq --no-telemetry --no-prometheus -l babe=debug,sc_basic_authorship=debug,grandpa=trace 2>&1 | tee chain_data/alan/sunchain.log
```

### Running a Validator Node with Docker
Export Docker Image
```shell
docker save sunchain-node | gzip > sunchain-node.tar.gz
```
Running a Validator Node Locally
```shell
gzip -dc sunchain-node.tar.gz | docker load 
sudo docker run -d  --name my-sunchain-node   -p 16888:16888/tcp   -p 16888:16888/udp   -p 1688:1688/tcp  -v "$(pwd)/temp":/root/.codigger/.sunchain/chaindata/default_base sunchain-node 
```

```shell
target/debug/sunchain --chain sunchain -d ../chain_data/sun/base --rpc-port 46581 --validator --force-authoring --state-pruning archive --blocks-pruning archive --no-telemetry --no-prometheus

cargo run -p sun-admin -- \
  --rpc ws://127.0.0.1:46581 \
  --private-key 0x2824110040e38dc69fb9f7f90a79c2a379a390cdd8424a1793f9500fa777c378 \
  base-fee set-base-fee-per-gas 0

target/debug/sun-admin \
  --rpc ws://127.0.0.1:46581 \
  --private-key 0x2824110040e38dc69fb9f7f90a79c2a379a390cdd8424a1793f9500fa777c378 \
  --mortality 0 \
  base-fee set-elasticity 0

forge script script/DeployDMTToken.s.sol:DeployDMTToken --rpc-url $RPC_URL --broadcast --contracts contracts/DataMarket --skip-simulation --evm-version london --with-gas-price 0 --priority-gas-price 0

cast send 0x93441b8C7ebf190e9ab6654cA4a7dc23aeC59B41 "transfer(address,uint256)" 0x909d5512b4FaB5097033f805Ef4b00B6a01Eb4BB 10000ether --private-key 0x9b57e360ddb98fc2d104c428789df24316bb15c5a3911b17e26b8c907477fe5b --gas-price 0 --priority-gas-price 0 --rpc-url http://127.0.0.1:46581  
```