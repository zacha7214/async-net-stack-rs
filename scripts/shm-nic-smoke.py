#!/usr/bin/env python3
"""Run real child processes through tiny-ring wraparound, all copy/wait modes."""
import argparse
import itertools
import json
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/examples/shm_nic"))
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    cases = 0
    for mode, wait, size in itertools.product(
        ["direct", "host-copy", "guest-copy", "both-copy"], ["spin", "pipe"], [64, 1500]
    ):
        command = [binary, "--mode", mode, "--wait", wait, "--size", str(size),
                   "--ring", "8", "--batch", "3", "--packets", "10003",
                   "--warmup", "97", "--verify", "full", "--timeout", "5"]
        result = subprocess.run(command, check=True, capture_output=True, text=True, timeout=20)
        data = json.loads(result.stdout)
        m = data["measurement"]
        assert m["completed_packets"] == 10003 and m["pending"] == 0, data
        copies = {"direct": 0, "host-copy": 1, "guest-copy": 1, "both-copy": 2}[mode]
        assert m["extra_payload_copy_bytes"] == copies * 10003 * size, data
        for side in ("host", "guest_model"):
            assert data[side]["packets"] == 10003, data
            if wait == "pipe":
                assert data[side]["notification_writes"] + data[side]["notification_eagain"] == data[side]["batches"], data
            else:
                assert data[side]["notification_writes"] == data[side]["poll_calls"] == 0, data
        cases += 1
    # No-warmup handshake, smallest ring, and jumbo stride are separate boundaries.
    result = subprocess.run([binary, "--warmup", "0", "--packets", "1", "--ring", "2",
                             "--batch", "2", "--size", "9000", "--stride", "16384",
                             "--wait", "pipe", "--verify", "full"],
                            check=True, capture_output=True, text=True, timeout=20)
    assert json.loads(result.stdout)["measurement"]["completed_packets"] == 1
    for options in (["--ring", "3"], ["--batch", "0"], ["--size", "63"],
                    ["--stride", "128"], ["--packets", str(2**64 - 1)]):
        result = subprocess.run([binary] + options, capture_output=True, timeout=5)
        assert result.returncode != 0, options
    print(f"passed {cases + 1} two-process data-path cases and 5 invalid configurations")


if __name__ == "__main__":
    main()
