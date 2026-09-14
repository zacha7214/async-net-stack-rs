#!/usr/bin/env python3
"""Randomized repeated shared-memory runs, with raw JSON and median/min/max pps."""
import argparse
import itertools
import json
import platform
import random
import statistics
import subprocess
import sys
from pathlib import Path


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--binary", type=Path, default=Path("target/release/examples/shm_nic"))
    p.add_argument("--packets", type=int, default=5_000_000)
    p.add_argument("--warmup", type=int, default=500_000)
    p.add_argument("--repeats", type=int, default=5)
    p.add_argument("--sizes", type=int, nargs="+", default=[64, 1500])
    p.add_argument("--batches", type=int, nargs="+", default=[1, 64])
    p.add_argument("--modes", nargs="+", choices=["direct", "host-copy", "guest-copy", "both-copy"],
                   default=["direct", "both-copy"])
    p.add_argument("--waits", nargs="+", choices=["spin", "pipe"], default=["spin"])
    p.add_argument("--ring", type=int, default=1024)
    p.add_argument("--stride", type=int, default=0)
    p.add_argument("--verify", choices=["headers", "full"], default="headers")
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--output", type=Path, required=True)
    a = p.parse_args()
    if a.repeats < 1:
        p.error("--repeats must be positive")
    # Refuse to replace previous measurements.
    with a.output.open("x") as output:
        cases = list(itertools.product(a.sizes, a.batches, a.modes, a.waits)) * a.repeats
        random.Random(a.seed).shuffle(cases)
        runs = []
        for number, (size, batch, mode, wait) in enumerate(cases, 1):
            command = [str(a.binary.resolve()), "--size", str(size), "--batch", str(batch),
                       "--mode", mode, "--wait", wait, "--packets", str(a.packets),
                       "--warmup", str(a.warmup), "--ring", str(a.ring), "--stride", str(a.stride),
                       "--verify", a.verify]
            print(f"{number}/{len(cases)}: size={size} batch={batch} {mode} {wait}", file=sys.stderr)
            result = subprocess.run(command, capture_output=True, text=True, timeout=600)
            if result.returncode:
                raise RuntimeError(f"{command}: {result.stderr}")
            data = json.loads(result.stdout)
            if data["measurement"]["completed_packets"] != a.packets or data["measurement"]["pending"]:
                raise RuntimeError(f"incomplete run: {data}")
            runs.append(data)
            # Preserve successful raw runs even if a later case fails.
            output.seek(0)
            json.dump({"environment": {"os": platform.system(), "release": platform.release(),
                                        "arch": platform.machine()},
                       "seed": a.seed, "complete": False, "runs": runs}, output, indent=2)
            output.truncate()
            output.flush()
        summary = []
        for size, batch, mode, wait in sorted(set(cases)):
            selected = [r for r in runs if (r["config"]["size"], r["config"]["batch"],
                        r["config"]["mode"], r["config"]["wait"]) == (size, batch, mode, wait)]
            pps = [r["measurement"]["packets_per_second"] for r in selected]
            cpu = [r["measurement"]["cpu_ns_per_packet"] for r in selected]
            summary.append({"size": size, "batch": batch, "mode": mode, "wait": wait,
                            "median_pps": statistics.median(pps), "min_pps": min(pps),
                            "max_pps": max(pps), "median_cpu_ns_per_packet": statistics.median(cpu)})
        output.seek(0)
        json.dump({"environment": {"os": platform.system(), "release": platform.release(),
                                    "arch": platform.machine()}, "seed": a.seed, "complete": True,
                   "summary": summary, "runs": runs}, output, indent=2)
        output.write("\n")
        output.truncate()


if __name__ == "__main__":
    main()
