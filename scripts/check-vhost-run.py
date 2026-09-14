#!/usr/bin/env python3
"""Check matching host and guest JSONL logs for the VM DMA experiment."""
import argparse
import json
from pathlib import Path


def read(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def check(host, guest, mode):
    results = [r for r in guest if r.get("event") == "guest_result"]
    if not results:
        raise ValueError("guest_result missing; the receiver did not finish")
    result = results[-1]
    sid = result["session"]
    zero = mode == "zero-copy"
    required = (result["ok"] and result["received"] == result["requested"]
                and result["zero_copy"] == zero and result["address_check"]
                and result["samples"] > 0 and result["pending_tx"] == 0
                and result["rx_invalid"] == 0 and result["rx_dropped"] == 0)
    if not required:
        raise ValueError(f"incomplete/failed guest evidence: {result}")
    generated = [r for r in host if r.get("event") == "session_generated" and r["session"] == sid]
    if len(generated) != 1 or generated[0]["packets"] != result["received"] or generated[0]["payload_staging_copies"] != 0:
        raise ValueError("matching completed host session missing or has wrong count/copy path")
    ready = [r for r in guest if r.get("event") == "guest_ready" and r["session"] == sid]
    if (len(ready) != 1 or ready[0]["attach"] != "Driver"
            or ready[0]["zero_copy"] != zero or not ready[0]["address_check"]):
        raise ValueError("native driver XDP attachment was not reported")
    if zero and not any(r.get("event") == "features" and r["access_platform"] and r["ring_reset"] for r in host):
        raise ValueError("host did not negotiate ACCESS_PLATFORM and RING_RESET")
    samples = [r for r in guest if r.get("event") == "guest_sample" and r["session"] == sid]
    addresses = {r["sequence"]: r["frame_gpa"] for r in host if r.get("event") == "rx_sample" and r["session"] == sid}
    if (len(samples) != result["samples"]
            or [s["sequence"] for s in samples] != list(range(len(samples)))):
        raise ValueError("guest sample count disagrees with its summary")
    for sample in samples:
        if addresses.get(sample["sequence"]) != sample["backend_frame_gpa"]:
            raise ValueError("host and guest packet identity/address samples disagree; keep matching --samples counts")
        if sample["same_frame"] != zero or sample["umem_frame_gpa"] is None:
            raise ValueError(f"unexpected physical frame identity for {mode}: {sample}")
        same = int(sample["umem_frame_gpa"], 0) == int(sample["backend_frame_gpa"], 0)
        if same != zero:
            raise ValueError("physical addresses contradict the reported identity")
    if result["physical_matches"] != (len(samples) if zero else 0):
        raise ValueError("physical match count disagrees with expected copy mode")
    return sid, len(samples), result["received"]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--host", type=Path, required=True)
    p.add_argument("--guest", type=Path, required=True)
    p.add_argument("--mode", choices=["zero-copy", "copy"], default="zero-copy")
    a = p.parse_args()
    sid, samples, packets = check(read(a.host), read(a.guest), a.mode)
    print(f"PASS: session {sid}, {packets} packets, {samples} physical-address samples, {a.mode}")


if __name__ == "__main__":
    main()
