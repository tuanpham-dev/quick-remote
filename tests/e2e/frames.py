#!/usr/bin/env python3
"""Summarizes a qr-client --latency-log: frame gaps and latency percentiles.

usage: frames.py LOG [--from SEC] [--to SEC]   (prints JSON)"""
import json, sys
args = sys.argv[1:]
log = args[0]
lo = float(args[args.index("--from") + 1]) if "--from" in args else 0
hi = float(args[args.index("--to") + 1]) if "--to" in args else 1e9
rows = [json.loads(l) for l in open(log) if l.strip()]
if not rows:
    print(json.dumps({"frames": 0})); sys.exit(0)
t0 = rows[0]["at_us"]
rows = [r for r in rows if lo <= (r["at_us"] - t0) / 1e6 <= hi]
gaps = [(b["at_us"] - a["at_us"]) / 1000 for a, b in zip(rows, rows[1:])]
synced = [r for r in rows if r["synced"]]
def pct(v, p):
    v = sorted(v)
    return round(v[min(len(v) - 1, int(len(v) * p))], 2) if v else None
tot = [r["total_us"] / 1000 for r in synced]
print(json.dumps({
    "frames": len(rows),
    "max_gap_ms": round(max(gaps), 1) if gaps else None,
    "p50_ms": pct(tot, 0.5), "p95_ms": pct(tot, 0.95), "p99_ms": pct(tot, 0.99),
    "parts_p50_ms": {k: pct([r[k + "_us"] / 1000 for r in synced], 0.5)
                     for k in ("capture", "encode", "network", "decode")},
}))
