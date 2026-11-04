#!/usr/bin/env python3
"""Poll sff_status across N validator RPC ports and report SFF-vs-GRANDPA health.

Produces quantifiable multi-validator data:
  * steady-state gap = fastConfirmedFrontier - grandpaFinalizedNumber (and the older
    fastConfirmedHeight - grandpaFinalizedNumber) for every node;
  * serving / degraded / conflicts / frontier-null status;
  * cumulative STALL / Skip vote counts read from each node's log (catch-up / liveness).

Usage:
  python3 scripts/sff-multi/measure_sff.py --rpcs 9944,9945,9946,9947 [--logs-dir /tmp/sff-multi]
                                          [--duration 60] [--interval 3]
"""
import argparse
import json
import subprocess
import sys
import time
import urllib.request

# Talk to the local node directly — never route localhost RPC through the ambient
# HTTP(S) proxy (which cannot reach 127.0.0.1 and would make every call fail).
_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def rpc(port, method):
    body = json.dumps({"id": 1, "jsonrpc": "2.0", "method": method, "params": []}).encode()
    req = urllib.request.Request(
        f"http://localhost:{port}",
        data=body,
        headers={"Content-Type": "application/json"},
    )
    try:
        with _OPENER.open(req, timeout=2) as r:
            return json.load(r).get("result")
    except Exception:
        return None


def log_counts(log_path):
    try:
        stall = subprocess.run(
            ["grep", "-c", "advance_rounds: STALL", log_path],
            capture_output=True, text=True,
        ).stdout.strip()
        skip = subprocess.run(
            ["grep", "-c", "signed Skip", log_path],
            capture_output=True, text=True,
        ).stdout.strip()
        return int(stall or 0), int(skip or 0)
    except Exception:
        return 0, 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rpcs", required=True, help="comma-separated RPC ports")
    ap.add_argument("--logs-dir", default="/tmp/sff-multi", help="dir with nodeI.log")
    ap.add_argument("--duration", type=int, default=60)
    ap.add_argument("--interval", type=float, default=3.0)
    args = ap.parse_args()

    ports = [int(p) for p in args.rpcs.split(",") if p.strip()]
    end = time.time() + args.duration

    # running max gap / STALL / Skip across the whole run
    max_gap = {p: None for p in ports}
    total_stall = {p: 0 for p in ports}
    total_skip = {p: 0 for p in ports}

    print(f"# measuring {len(ports)} nodes for {args.duration}s every {args.interval}s")
    print("t  " + "  ".join(f"node{p}" for p in ports))
    while time.time() < end:
        line = f"{int(time.time()%100000):5d} "
        for p in ports:
            d = rpc(p, "sff_status")
            if not d:
                line += f"  down      "
                continue
            fc = d.get("fastConfirmedHeight")
            frontier = d.get("fastConfirmedFrontier")
            g = d.get("grandpaFinalizedNumber")
            gap = (frontier - g) if isinstance(frontier, int) else None
            fallback_gap = (fc - g) if isinstance(fc, int) else None
            conflicts = d.get("conflicts", 0)
            serving = d.get("serving", False)
            degraded = d.get("degraded", False)
            fnull = "null" if frontier is None else frontier
            line += f" g={gap}({fnull}) c={conflicts}{'S' if serving else '-'}{'D' if degraded else '-'}"
            if isinstance(gap, int):
                if max_gap[p] is None or gap > max_gap[p]:
                    max_gap[p] = gap
        print(line)

        # accumulate STALL/Skip from logs
        for p in ports:
            log_path = f"{args.logs_dir}/node{ports.index(p)}.log"
            s, k = log_counts(log_path)
            total_stall[p] = max(total_stall[p], s)
            total_skip[p] = max(total_skip[p], k)
        time.sleep(args.interval)

    print("\n# summary")
    for p in ports:
        g = max_gap[p]
        print(f"node{p}: max_gap(frontier-grandpa)={g}  STALL(cum)={total_stall[p]}  Skip(cum)={total_skip[p]}")


if __name__ == "__main__":
    main()
