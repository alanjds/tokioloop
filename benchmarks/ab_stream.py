"""Interleaved hot-swap A/B benchmark for the STREAM path.

Why this exists: single, non-interleaved benchmark runs on a shared/noisy box
swing by ±15 pp and routinely mislead. This harness measures every variant
*back-to-back within each repetition* (so machine drift hits all variants
equally), pins server and client to disjoint core sets, and reports the
per-variant **median across reps** plus its share of the asyncio reference.

A "variant" is one of:
  - a stdlib/uvloop loop (asyncio / uvloop), used as a stable reference, or
  - a tokioloop build, identified by a stashed `_rloop*.so` that is hot-swapped
    into `rloop/` before the server starts. This lets us A/B *code variants*
    (baseline vs stageN) of tokioloop against each other on the same box.

Usage:
  uv run --no-sync python benchmarks/ab_stream.py \
      --reps 5 --duration 8 --sizes 1024,10240,102400 \
      --variant asyncio:loop=asyncio \
      --variant uvloop:loop=uvloop \
      --variant baseline:so=/tmp/variants/baseline.so \
      --variant stage1:so=/tmp/variants/stage1.so

Each --variant is `name:loop=<loopname>` or `name:so=<path-to-.so>`.
"""

import argparse
import glob
import json
import os
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import time
from pathlib import Path

WD = Path(__file__).resolve().parent
ROOT = WD.parent
SO_GLOB = str(ROOT / "rloop" / "_rloop*.so")
SERVER = WD / "server.py"
CLIENT = WD / "client.py"


def _live_so() -> str:
    matches = glob.glob(SO_GLOB)
    if not matches:
        raise SystemExit(f"no built .so at {SO_GLOB}; run `make build-dev` first")
    return matches[0]


def parse_variant(spec: str):
    # name:loop=asyncio  |  name:so=/path/to.so
    name, _, rhs = spec.partition(":")
    kind, _, val = rhs.partition("=")
    if kind not in ("loop", "so"):
        raise SystemExit(f"bad --variant {spec!r}; expected name:loop=X or name:so=PATH")
    return name, kind, val


def wait_ready(host, port, deadline):
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.05)
    return False


def run_once(loop, server_cores, client_cores, host, port, duration, size, mode="streams", timeout=30):
    """Start a pinned echo server (streams or proto), run a pinned client, return rps."""
    py = sys.executable
    mode_flag = "--proto" if mode == "proto" else "--streams"
    server_cmd = [
        "taskset", "-c", server_cores, py, str(SERVER),
        "--loop", loop, mode_flag, "--addr", f"{host}:{port}",
    ]
    srv = subprocess.Popen(server_cmd, preexec_fn=os.setsid)  # noqa: S603
    try:
        if not wait_ready(host, port, time.monotonic() + 10):
            raise RuntimeError(f"server({loop}) did not become ready on :{port}")
        client_cmd = [
            "taskset", "-c", client_cores, py, str(CLIENT),
            "--addr", f"{host}:{port}", "--msize", str(size),
            "--duration", str(duration), "--concurrency", "1",
            "--timeout", str(timeout), "--output", "json",
        ]
        out = subprocess.run(client_cmd, check=True, capture_output=True)  # noqa: S603
        data = json.loads(out.stdout.decode())
        return float(data["rps"])
    finally:
        try:
            os.killpg(os.getpgid(srv.pid), signal.SIGKILL)
        except ProcessLookupError:
            pass
        srv.wait()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--duration", type=int, default=8)
    ap.add_argument("--sizes", default="1024,10240,102400")
    ap.add_argument("--server-cores", default="0-1")
    ap.add_argument("--client-cores", default="2-3")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=26500)
    ap.add_argument("--mode", choices=["streams", "proto"], default="streams")
    ap.add_argument("--variant", action="append", default=[], dest="variants")
    ap.add_argument("--ref", default="asyncio", help="variant name used as 100%% reference")
    ap.add_argument("--json-out", default=None)
    args = ap.parse_args()

    if not args.variants:
        raise SystemExit("at least one --variant required")
    variants = [parse_variant(v) for v in args.variants]
    sizes = [int(s) for s in args.sizes.split(",")]
    live_so = _live_so()

    # results[name][size] = [rps per rep]
    results = {name: {sz: [] for sz in sizes} for name, _, _ in variants}

    for rep in range(args.reps):
        # rotate the port each rep to dodge TIME_WAIT on rapid restarts
        port = args.port + rep
        for sz in sizes:
            for name, kind, val in variants:  # interleaved: all variants per (rep,size)
                if kind == "so":
                    shutil.copyfile(val, live_so)
                    loop = "tokioloop"
                else:
                    loop = val
                try:
                    rps = run_once(loop, args.server_cores, args.client_cores,
                                   args.host, port, args.duration, sz, mode=args.mode)
                except Exception as e:  # noqa: BLE001
                    print(f"  WARN rep{rep} size{sz} {name}: {e}", file=sys.stderr)
                    rps = float("nan")
                results[name][sz].append(rps)
                print(f"rep{rep} size{sz:>6} {name:<12} rps={rps:,.0f}", flush=True)

    # restore the live .so to baseline so the tree is not left on a random variant
    if any(k == "so" for _, k, _ in variants):
        base = next((v for n, k, v in variants if k == "so"), None)
        if base:
            shutil.copyfile(base, live_so)

    def med(name, sz):
        vals = [v for v in results[name][sz] if v == v]  # drop NaN
        return statistics.median(vals) if vals else float("nan")

    print("\n=== STREAM A/B medians (rps) ===")
    header = f"{'size':>8} | " + " | ".join(f"{n:>12}" for n, _, _ in variants)
    print(header)
    print("-" * len(header))
    for sz in sizes:
        row = f"{sz:>8} | " + " | ".join(f"{med(n, sz):>12,.0f}" for n, _, _ in variants)
        print(row)

    if args.ref in results:
        print(f"\n=== % of {args.ref} ===")
        print(header)
        print("-" * len(header))
        for sz in sizes:
            ref = med(args.ref, sz)
            cells = []
            for n, _, _ in variants:
                pct = 100.0 * med(n, sz) / ref if ref == ref and ref else float("nan")
                cells.append(f"{pct:>11.1f}%")
            print(f"{sz:>8} | " + " | ".join(cells))

    if args.json_out:
        Path(args.json_out).write_text(json.dumps(
            {"reps": args.reps, "duration": args.duration, "sizes": sizes,
             "results": {n: {str(s): results[n][s] for s in sizes} for n, _, _ in variants}},
            indent=2))
        print(f"\nwrote {args.json_out}")


if __name__ == "__main__":
    main()
