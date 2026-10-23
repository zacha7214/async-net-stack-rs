#!/usr/bin/env python3
"""Build the Raspberry Pi 4 board and boot it on QEMU raspi4b (QEMU 9.0+).

Produces the raw kernel8.img the Pi firmware loads at 0x80000, and boots that
same image (not the ELF: QEMU starts an ELF at EL3 on all cores, while a raw
image gets firmware-like EL2 entry, core 0 only, DTB in x0). Checks runtime
bring-up only: EL2->EL1 entry, PL011, heap, clock, pages and CPU-side DMA.
QEMU does not model GENET or Device-memory exclusive-access limits, so
passing here does not validate the board on silicon.
"""
import argparse
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
WORKSPACE = ROOT / 'baremetal'
TARGET = 'aarch64-unknown-none-softfloat'
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--release', action='store_true')
args = parser.parse_args()
profile = 'release' if args.release else 'debug'

subprocess.run(['cargo', 'build', '--locked', '--manifest-path', str(WORKSPACE / 'Cargo.toml'),
                '-p', 'async-net-rpi4', '--target', TARGET] + (['--release'] if args.release else []),
               check=True)
elf = WORKSPACE / 'target' / TARGET / profile / 'rpi4-boot'
image = elf.with_name('kernel8.img')


def objcopy():
    """llvm-objcopy from `rustup component add llvm-tools`, else cargo-binutils."""
    sysroot = subprocess.run(['rustc', '--print', 'sysroot'], capture_output=True, text=True,
                             check=True).stdout.strip()
    host = next(line.split()[1] for line in subprocess.run(
        ['rustc', '-vV'], capture_output=True, text=True, check=True).stdout.splitlines()
        if line.startswith('host:'))
    bundled = Path(sysroot) / 'lib/rustlib' / host / 'bin/llvm-objcopy'
    found = bundled if bundled.exists() else shutil.which('rust-objcopy')
    if not found:
        raise SystemExit('need llvm-objcopy: rustup component add llvm-tools')
    return str(found)


subprocess.run([objcopy(), '-O', 'binary', str(elf), str(image)], check=True)
print(f'image: {image}')
cmd = ['qemu-system-aarch64', '-machine', 'raspi4b', '-display', 'none', '-monitor', 'none',
       '-serial', 'stdio', '-kernel', str(image)]

output = b''
with subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                      stderr=subprocess.STDOUT) as proc:
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(proc.stdout, selectors.EVENT_READ)
            deadline = time.monotonic() + 20
            while b'BOOT DONE' not in output:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise AssertionError(f'timeout: {output.decode(errors="replace")}')
                chunk = os.read(proc.stdout.fileno(), 4096)
                if not chunk:
                    raise AssertionError(f'QEMU exited: {output.decode(errors="replace")}')
                output += chunk
                print(chunk.decode(errors='replace'), end='', flush=True)
                for failure in (b'PANIC:', b'EXCEPTION:'):
                    assert failure not in output, output.decode(errors='replace')
        for marker in ('PASS heap:', 'PASS clock:', 'PASS pages:', 'PASS DMA:'):
            assert marker.encode() in output, output.decode(errors='replace')
        print(f'\nPASS {profile}: rpi4 runtime bring-up on QEMU raspi4b')
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

