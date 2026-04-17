#!/usr/bin/env python3
"""Phase 12.0D.WS push-latency validator.

Connects to /api/ws, subscribes to a single channel with a deliberately
long `pollInterval` (default 10000 ms), then records the timestamps of
each Update frame it receives for the test duration (default 45 s).

Without the 12.0D.WS CovEvent bridge, the client would only get one
Update per `pollInterval` — i.e. ~4 updates in 45 s at 10 s interval.

WITH the bridge, every CovEvent that matches the subscribed ids sets
`cov_pending=true` on the server-side WatchMeta; the next push-timer
tick (200 ms) polls the engine regardless of interval. So if the
BACnet sim is cycling values every 5 s (see `bacnet_sim.py --vary`),
we should see ~9 updates in 45 s instead of ~4 — a clear signal that
the bridge is live.

Usage:
    py tools/ws_latency_test.py [host:port] [channel] [duration_s]

Defaults: 192.168.1.11:8085, channel 102, 45 seconds.
"""

import asyncio
import json
import sys
import time
from statistics import mean, median

import websockets

async def run(host_port: str, channel: int, duration_s: float):
    url = f"ws://{host_port}/api/ws"
    print(f"[ws-latency] connect {url}, subscribe to channel={channel}", flush=True)
    print(f"[ws-latency] pollInterval=10000 (client-side floor)", flush=True)
    print(f"[ws-latency] run for {duration_s} s", flush=True)

    updates = []  # (elapsed_s, value)
    t0 = time.monotonic()

    async with websockets.connect(url) as ws:
        sub = {
            "op": "subscribe",
            "id": "sub-1",
            "watchId": None,
            "ids": [channel],
            "pollInterval": 10000,
        }
        await ws.send(json.dumps(sub))

        # Read frames until duration expires.
        while True:
            remaining = duration_s - (time.monotonic() - t0)
            if remaining <= 0:
                break
            try:
                raw = await asyncio.wait_for(ws.recv(), timeout=remaining)
            except asyncio.TimeoutError:
                break
            elapsed = time.monotonic() - t0
            try:
                msg = json.loads(raw)
            except Exception:
                print(f"  {elapsed:6.2f}s RAW {raw[:80]!r}", flush=True)
                continue

            op = msg.get("op")
            if op == "subscribed":
                rows = msg.get("rows", [])
                initial = rows[0] if rows else None
                print(
                    f"  {elapsed:6.2f}s SUBSCRIBED watchId={msg.get('watchId')}"
                    f" initial={initial}",
                    flush=True,
                )
            elif op == "update":
                rows = msg.get("rows", [])
                for r in rows:
                    if r.get("channel") == channel:
                        v = r.get("cur")
                        updates.append((elapsed, v))
                        print(f"  {elapsed:6.2f}s UPDATE  cur={v}", flush=True)
            else:
                print(f"  {elapsed:6.2f}s {op.upper() if op else '?'} {msg}", flush=True)

    # Summary (ASCII only — Windows cp1254 can't encode box-drawing chars).
    print()
    print("-" * 60)
    print(f"updates received: {len(updates)}")
    if len(updates) >= 2:
        gaps = [updates[i][0] - updates[i-1][0] for i in range(1, len(updates))]
        print(f"gaps (s): min={min(gaps):.2f} median={median(gaps):.2f}"
              f" mean={mean(gaps):.2f} max={max(gaps):.2f}")
        print()
        if median(gaps) < 7.0:
            print("RESULT: sub-client-interval pushes observed -- 12.0D.WS bridge is LIVE.")
        else:
            print("RESULT: gaps match client pollInterval -- bridge may not be active"
                  " (or sim isn't changing value). Check sim's --vary flag and that"
                  " channel is BACnet-backed.")

def main():
    host_port = sys.argv[1] if len(sys.argv) > 1 else "192.168.1.11:8085"
    channel = int(sys.argv[2]) if len(sys.argv) > 2 else 102
    duration = float(sys.argv[3]) if len(sys.argv) > 3 else 45.0
    asyncio.run(run(host_port, channel, duration))

if __name__ == "__main__":
    main()
