#!/usr/bin/env python3
"""Generate/use LLDB or GDB commands for the lab's QEMU virt direct Image boot.

Starts at primary_entry with physical symbols. Switch to virtual symbols before
continuing through the MMU handoff, as described in docs/kernel-lab.md.
"""
import argparse
import json
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile


def commands(bundle, port, debugger):
    m = json.loads((bundle/'manifest.json').read_text())
    symbols = {k: int(v, 16) for k, v in m['symbols'].items()}
    with (bundle/'Image').open('rb') as f: header = f.read(64)
    if header[56:60] != b'ARM\x64': raise ValueError('not an ARM64 Image')
    offset, size = struct.unpack_from('<QQ', header, 8)
    if not size: raise ValueError('requires a modern ARM64 Image header')
    # Matches QEMU hw/arm/boot.c for -machine virt and a raw -kernel Image.
    physical = 0x40000000 + offset + (0x200000 if offset < 4096 else 0)
    delta = physical-symbols['_text']
    entry = symbols['primary_entry']+delta
    virtual = symbols['__primary_switched']
    image = str(bundle/'vmlinux')
    if '"' in image or '\n' in image: raise ValueError('unsupported quote/newline in bundle path')
    if debugger == 'lldb':
        slide = delta & ((1 << 64)-1)
        lines = [f'target create "{image}"', 'settings set target.skip-prologue false',
                 f'settings set target.source-map "{m["source"]}" "{bundle}/source"',
                 f'gdb-remote 127.0.0.1:{port}',
                 f'target modules load --file vmlinux --slide {slide:#x}',
                 f'breakpoint set --hardware --one-shot true --address {entry:#x}', 'continue',
                 'register read pc x0 x1 x2 x3', 'disassemble --start-address $pc --count 8']
        transition = [f'target modules load --file vmlinux --slide 0',
                      f'breakpoint set --hardware --one-shot true --address {virtual:#x}',
                      'continue', 'breakpoint set --hardware --name start_kernel',
                      'breakpoint set --hardware --name setup_arch']
    else:
        lines = ['set pagination off', 'set confirm off', f'file "{image}"',
                 f'set substitute-path "{m["source"]}" "{bundle}/source"',
                 f'target remote 127.0.0.1:{port}', f'symbol-file -o {delta} "{image}"',
                 f'thbreak *{entry:#x}', 'continue', 'info registers pc x0 x1 x2 x3', 'x/8i $pc']
        transition = [f'symbol-file "{image}"', f'thbreak *{virtual:#x}', 'continue',
                      'hbreak start_kernel', 'hbreak setup_arch']
    return '\n'.join(lines)+'\n', '\n'.join(transition)+'\n'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--bundle', type=Path, required=True)
    p.add_argument('--port', type=int, default=1234)
    p.add_argument('--debugger', choices=['lldb', 'gdb'], default='lldb')
    p.add_argument('--emit-only', action='store_true')
    a = p.parse_args()
    if not 1024 <= a.port <= 65535: p.error('invalid port')
    start, transition = commands(a.bundle.resolve(), a.port, a.debugger)
    directory = Path(tempfile.mkdtemp(prefix='kernel-debug-', dir='/tmp'))
    init, mmu = directory/'entry.commands', directory/'virtual.commands'
    init.write_text(start); mmu.write_text(transition)
    verb = 'command source' if a.debugger == 'lldb' else 'source'
    print(f'Entry commands: {init}\nAt primary_entry, use si to step early assembly.\n'
          f'To continue to __primary_switched with virtual symbols:\n  {verb} {mmu}', flush=True)
    if a.emit_only:
        print(start); print(transition); return
    binary = shutil.which(a.debugger)
    if not binary: p.error(f'{a.debugger} not installed; use --emit-only or install it')
    args = [binary, '--no-lldbinit', '-s', str(init)] if a.debugger == 'lldb' else [binary, '-nx', '-x', str(init)]
    raise SystemExit(subprocess.call(args))


if __name__ == '__main__': main()
