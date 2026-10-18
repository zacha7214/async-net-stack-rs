#!/usr/bin/env python3
"""Linux-only, privileged smoke test using isolated loopback UDP traffic."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--capture", default="target/debug/net-capture")
    parser.add_argument("--experiment", default="target/debug/net-experiment")
    args = parser.parse_args()
    
    if os.uname().sysname != "Linux":
        raise SystemExit("This smoke test needs Linux and packet capture privileges.")
    
    with tempfile.TemporaryDirectory(prefix="capture-bundle-smoke-") as temporary:
        root = Path(temporary)
        receiver = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        receiver.bind(("127.0.0.1", 0))
        receiver.settimeout(0.1)
        sender = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        port = receiver.getsockname()[1]
        
        with (root / "collector.log").open("w+") as log:
            process = subprocess.Popen([
                args.capture, "--interface", "lo", "--duration", "1",
                "--interval", "0.1", "--root", str(root / "traces"),
                "--filter", f"udp and port {port}",
            ], stdout=log, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 75
                while process.poll() is None:
                    if time.monotonic() > deadline:
                        raise RuntimeError("collector timeout")
                    sender.sendto(b"bundle-smoke", ("127.0.0.1", port))
                    receiver.recvfrom(4096)
                    time.sleep(0.02)
                    
                if process.returncode:
                    log.seek(0)
                    raise RuntimeError(log.read())
                output = subprocess.check_output([
                    args.experiment, "--root", str(root / "traces")
                ], text=True)
                
                report = json.loads(output)
                assert report["summary"]["delivered"] > 0, report
                assert report["summary"]["bytes_verified"], report
                assert report["collection"]["counter_samples"] >= 2, report
                assert not report["environment_applied"], report
                bundle = Path(report["source"]).parent
                explicit = json.loads(subprocess.check_output([
                    args.experiment, str(bundle)
                ], text=True))
                
                assert explicit == report
                print("Linux bundle collection, latest selection, and explicit replay passed.")
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                sender.close()
                receiver.close()


if __name__ == "__main__":
    main()
