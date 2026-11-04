#!/usr/bin/env bash
# Launches 4 `--chain dev` validators for SFF multi-validator measurement and
# fault injection.
#
#   bash scripts/sff-multi/start_multi.sh [--fresh]
#
# The `--chain dev` genesis is now DETERMINISTIC: the dev validator count is a
# fixed compile-time constant (see `DEV_GENESIS_VALIDATORS` in chain_spec.rs), so
# every node builds the *identical* genesis block regardless of how it is
# started. That kills the old SunLead multi-validator partition, where differing
# `SFF_DEV_VALIDATORS` env values across nodes produced four different genesis
# hashes and silently split the network into separate chains.
#
# Topology: STAR — every node bootstraps from node0 (alice), so all 4 reliably
# land on one network. We still build ONE shared raw spec and force
# `chainType: "Local"` (a Development chain would otherwise let the client
# rebuild/reset genesis state), then assert all 4 nodes share the same genesis
# hash before declaring success.
#
# Each node writes its exact launch command to $ROOT/node$I.cmd so fault_inject.sh
# can restart it standalone (re-sync from peers) after a kill.

set -euo pipefail

if [[ -f /home/alan/.codebuddy/skills/sunchain-dex-e2e/scripts/proxy_env.sh ]]; then
    source /home/alan/.codebuddy/skills/sunchain-dex-e2e/scripts/proxy_env.sh
fi
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}localhost,127.0.0.1"
export no_proxy="$NO_PROXY"

SUNCHAIN_DIR="${SUNCHAIN_DIR:-/home/alan/260909/sun_dev/sunchain}"
# Dev genesis now fixes the validator set at 4 (see DEV_GENESIS_VALIDATORS in
# chain_spec.rs); launch exactly those four so GRANDPA (2/3 of 4 = 3 votes) and
# the SunLead schedule are fully covered.
SEEDS=(alice bob charlie dave)
N=${#SEEDS[@]}
ROOT="/tmp/sff-multi"
RPC_BASE=9944
P2P_BASE=30333

FRESH=0
for a in "$@"; do
    case "$a" in
        --fresh) FRESH=1 ;;
    esac
done

if [[ "$FRESH" -eq 1 ]]; then
    rm -rf "$ROOT"
    echo "wiped $ROOT (fresh deterministic dev genesis, $N validators)"
fi

cd "$SUNCHAIN_DIR"
pkill -f "chain dev" 2>/dev/null || true
for _ in $(seq 1 30); do pgrep -f "chain dev" >/dev/null 2>&1 || break; sleep 1; done
sleep 1
mkdir -p "$ROOT"

# Build ONE shared raw chain-spec. Because the dev genesis is deterministic, this
# step is belt-and-suspenders: every node would build the identical genesis from
# `--chain dev` alone. We still share the file and force `chainType: "Local"` so
# the client consumes the raw storage verbatim (a `Development` chain would let it
# rebuild/reset genesis state), then assert all nodes agree on the genesis hash.
SPEC="$ROOT/spec_raw.json"
./target/debug/sunchain build-spec --chain dev --raw --disable-default-bootnode > "$SPEC" 2>/dev/null
if [[ ! -s "$SPEC" ]]; then
    echo "failed to build shared spec $SPEC" >&2
    exit 1
fi
# The generated spec carries `chainType: "Development"`, which makes the client
# DISCARD the raw genesis storage and REBUILD the genesis from the runtime's dev
# preset — non-deterministically (and with a different validator set). Force
# `chainType: "Local"` so the client uses the deterministic raw storage as-is.
python3 - "$SPEC" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d["chainType"] = "Local"
json.dump(d, open(p, "w"))
PY
echo "built shared spec $SPEC (N=$N, chainType=Local)"

extract_peer() {
    local log="$1" rpc="$2"
    for _ in $(seq 1 60); do
        local p
        p=$(grep -oE "Local node identity is: [A-Za-z0-9]+" "$log" 2>/dev/null | tail -1 | awk '{print $NF}')
        if [[ -z "$p" ]]; then
            p=$(curl -s -m 2 -H "Content-Type: application/json" \
                -d '{"id":1,"jsonrpc":"2.0","method":"system_localPeerId","params":[]}' \
                "http://localhost:$rpc" 2>/dev/null \
                | python3 -c "import sys,json; print(json.load(sys.stdin)['result'])" 2>/dev/null || true)
        fi
        [[ -n "$p" ]] && { echo "$p"; return; }
        sleep 1
    done
    echo ""
}

start_node() {
    local i="$1" boot="$2"
    local BASE="$ROOT/node$i" SEED="${SEEDS[$i]}" RPC=$((RPC_BASE+i)) P2P=$((P2P_BASE+i))
    mkdir -p "$BASE"
    # All nodes load the SAME shared raw spec (built once above) so they build an
    # identical, deterministic genesis instead of each rebuilding from the patch.
    local CMD="./target/debug/sunchain --chain $SPEC --validator --$SEED \
        --base-path $BASE --rpc-port $RPC --port $P2P \
        --rpc-methods unsafe --rpc-cors all \
        -l sff=debug \
        --unsafe-force-node-key-generation $boot"
    echo "$CMD" > "$ROOT/node$i.cmd"
    # shellcheck disable=SC2086
    eval "$CMD" > "$BASE.log" 2>&1 &
    echo "node$i ($SEED): rpc=$RPC p2p=$P2P"
}

# node0 first — capture its peer id, then everyone else bootstraps from it.
start_node 0 ""
PEER0=$(extract_peer "$ROOT/node0.log" "$RPC_BASE")
if [[ -z "$PEER0" ]]; then
    echo "could not read node0 peer id" >&2
    exit 1
fi
echo "node0 peer=$PEER0"

BOOT0="--bootnodes /ip4/127.0.0.1/tcp/$P2P_BASE/p2p/$PEER0"
for i in $(seq 1 $((N-1))); do
    start_node "$i" "$BOOT0"
done

for _ in $(seq 1 60); do
    OK=1
    for i in $(seq 0 $((N-1))); do
        grep -q "Imported #[0-9]*" "$ROOT/node$i.log" 2>/dev/null || OK=0
    done
    [[ $OK -eq 1 ]] && break
    sleep 2
done

# Guard against silent genesis mismatch: every node must share the same genesis
# hash, otherwise they are different chains and will fork at block 1. Fail loudly.
GEN0=$(curl -s -m 3 -H "Content-Type: application/json" \
    -d '{"id":1,"jsonrpc":"2.0","method":"chain_getBlockHash","params":[0]}' \
    http://localhost:$RPC_BASE 2>/dev/null | python3 -c "import sys,json;print(json.load(sys.stdin)['result'])" 2>/dev/null)
BAD=0
for i in $(seq 1 $((N-1))); do
    p=$((RPC_BASE+i))
    g=$(curl -s -m 3 -H "Content-Type: application/json" \
        -d '{"id":1,"jsonrpc":"2.0","method":"chain_getBlockHash","params":[0]}' \
        http://localhost:$p 2>/dev/null | python3 -c "import sys,json;print(json.load(sys.stdin)['result'])" 2>/dev/null)
    if [[ "$g" != "$GEN0" ]]; then
        echo "GENESIS MISMATCH: node0=$GEN0 node$i=$g  -> nodes are different chains, will fork" >&2
        BAD=1
    fi
done
[[ $BAD -eq 1 ]] && exit 1

echo "all $N nodes importing, shared genesis $GEN0. logs: $ROOT/node*.log"
echo "measure:  python3 scripts/sff-multi/measure_sff.py --rpcs $(seq -s, $RPC_BASE $((RPC_BASE+N-1))) --logs-dir $ROOT"
