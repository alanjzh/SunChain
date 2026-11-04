#!/usr/bin/env bash
# Fault injection for the multi-validator SFF testbed: kill one validator, hold it
# down for OUTAGE seconds, then restart it standalone (re-sync from peers) and let
# the network observe the gap while it is gone and the self-heal / catch-up after
# it returns.
#
#   bash scripts/sff-multi/fault_inject.sh <node_index> <outage_seconds>
#
# Requires the nodes to have been started by start_multi.sh (which writes
# $ROOT/node$I.cmd). The other nodes keep running, so with N>=4 a 1-node outage
# still leaves GRANDPA (need 3/4) and SFF (need 3/4 of stake) able to make progress.

set -euo pipefail
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}localhost,127.0.0.1"
export no_proxy="$NO_PROXY"

ROOT="/tmp/sff-multi"
NODE_IDX="${1:-3}"
OUTAGE="${2:-60}"

CMD_FILE="$ROOT/node$NODE_IDX.cmd"
BASE="$ROOT/node$NODE_IDX"
LOG="$ROOT/node$NODE_IDX.log"

if [[ ! -f "$CMD_FILE" ]]; then
    echo "no $CMD_FILE — start the cluster with start_multi.sh first" >&2
    exit 1
fi

echo "== killing node$NODE_IDX =="
pkill -f "base-path $BASE" 2>/dev/null || true
sleep 2

echo "== node$NODE_IDX down for ${OUTAGE}s (other nodes still running) =="
echo "   measure the gap now, e.g. in another shell:"
echo "   python3 scripts/sff-multi/measure_sff.py --rpcs 9944,9945,9946,9947 --logs-dir $ROOT --duration $OUTAGE"
sleep "$OUTAGE"

echo "== restarting node$NODE_IDX (re-sync from peers) =="
# shellcheck disable=SC2093
bash "$CMD_FILE" > "$LOG" 2>&1 &
for _ in $(seq 1 40); do
    grep -q "Imported #[0-9]*" "$LOG" 2>/dev/null && break
    sleep 1
done
echo "node$NODE_IDX restarted; watch it catch up:"
echo "python3 scripts/sff-multi/measure_sff.py --rpcs 9944,9945,9946,9947 --logs-dir $ROOT --duration 60"
