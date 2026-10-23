#!/usr/bin/env python3
"""Build the guest and verify UDP echo; --smoke also tests missing/legacy devices and OOM."""
import argparse
import os
import re
from pathlib import Path
import selectors
import socket
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
WORKSPACE = ROOT / 'baremetal'
TARGET = 'aarch64-unknown-none-softfloat'
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--smoke', action='store_true')
parser.add_argument('--release', action='store_true')
args = parser.parse_args()
profile = 'release' if args.release else 'debug'


def build(*extra):
    subprocess.run(['cargo', 'build', '--locked', '--manifest-path', str(WORKSPACE / 'Cargo.toml'),
                    '-p', 'async-net-qemu-virt', '--target', TARGET, *extra] + (['--release'] if args.release else []), check=True)


build()
elf = WORKSPACE / 'target' / TARGET / profile / 'net-boot'


def command(mode, port):
    cmd = ['qemu-system-aarch64', '-machine', 'virt,secure=off,virtualization=off',
           '-cpu', 'cortex-a53', '-accel', 'tcg', '-smp', '1', '-m', '128M',
           '-display', 'none', '-monitor', 'none', '-serial', 'stdio', '-nic', 'none',
           '-kernel', str(elf),
           '-global', 'virtio-mmio.force-legacy=' + ('true' if mode == 'legacy' else 'false')]
    if mode != 'absent':
        # Bind forwarding only to loopback. restrict=on drops UDP replies on the
        # tested QEMU/libslirp build; ordinary user networking permits the echo.
        cmd += ['-netdev', f'user,id=net0,restrict=off,hostfwd=udp:127.0.0.1:{port}-10.0.2.15:9000',
                '-device', 'virtio-net-device,netdev=net0,mac=52:54:00:12:34:56']
    if mode != 'no-rng':
        cmd += ['-object', 'rng-random,id=entropy0,filename=/dev/urandom',
                '-device', 'virtio-rng-device,rng=entropy0' +
                (',max-bytes=3,period=1' if mode == 'short-rng' else '')]
    return cmd


class UdpPeer:
    """Independent OS UDP peer. Exact payload comparison; no retries hide loss."""
    def __init__(self, sock):
        self.sock = sock
        self.round = 0
        self.started = False
        self.pending = set()
        self.sent = 0
        self.received = 0
        self.finished = False
        self.payloads = []

    def drive(self, serial):
        if self.round == 2:
            return
        if not self.started:
            if f'NET READY: round={self.round} '.encode() not in serial:
                return
            # Include zero/one-byte datagrams, odd checksums, and the 1500-byte MTU.
            self.payloads = [bytes([self.round]), b'']
            sizes = [7, 31, 63, 257, 1023, 1472]
            for seq in range(2, 128):
                length = sizes[seq % len(sizes)]
                self.payloads.append(bytes([self.round]) + seq.to_bytes(2, 'big') +
                                     bytes((seq * 17 + i) % 256 for i in range(length - 3)))
            self.started = True
        # Establish ARP before bursts. Thereafter use eight outstanding datagrams.
        window = 1 if self.received == 0 else 8
        while self.sent < 128 and len(self.pending) < window:
            payload = self.payloads[self.sent]
            self.sock.send(payload)
            self.pending.add(payload)
            self.sent += 1
        if self.received == 128 and not self.finished:
            payload = b'ANS-DONE' + bytes([self.round])
            self.sock.send(payload)
            self.pending.add(payload)
            self.finished = True

    def receive(self):
        payload = self.sock.recv(2048)
        assert payload in self.pending, f'unexpected/duplicate UDP echo in round {self.round}: {payload[:32]!r}'
        self.pending.remove(payload)
        if self.finished:
            assert payload == b'ANS-DONE' + bytes([self.round])
            assert not self.pending
            self.round += 1
            self.started = self.finished = False
            self.sent = self.received = 0
        else:
            self.received += 1


def smoke(mode, expected, oom=False, show_serial=False):
    # Reserve an ephemeral forwarding port, then release it for QEMU. A bind race
    # fails visibly at QEMU startup rather than silently probing a different peer.
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as reserve:
        reserve.bind(('127.0.0.1', 0))
        port = reserve.getsockname()[1]
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(('127.0.0.1', 0))
        sock.connect(('127.0.0.1', port))
        sock.setblocking(False)
        peer = UdpPeer(sock) if mode in ('modern', 'no-rng', 'short-rng') and not oom else None
        with subprocess.Popen(command(mode, port), stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as proc:
            output = b''
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(proc.stdout, selectors.EVENT_READ, 'serial')
                    if peer:
                        selector.register(sock, selectors.EVENT_READ, 'udp')
                    deadline = time.monotonic() + (45 if peer else 15)
                    terminal = b'OOM HALT' if oom else b'BOOT DONE: portable UDP smoke complete when present\r\n'
                    while terminal not in output or (peer and peer.round != 2):
                        remaining = deadline - time.monotonic()
                        events = selector.select(max(0, remaining))
                        if remaining <= 0 or not events:
                            state = f'round={peer.round} echoes={peer.received} pending={len(peer.pending)}' if peer else ''
                            raise AssertionError(f'{mode}: timeout {state}: {output.decode(errors="replace")}')
                        for key, _ in events:
                            if key.data == 'udp':
                                peer.receive()
                            else:
                                chunk = os.read(proc.stdout.fileno(), 4096)
                                if not chunk:
                                    raise AssertionError(f'{mode}: QEMU exited: {output!r}')
                                output += chunk
                                if show_serial:
                                    print(chunk.decode(errors='replace'), end='', flush=True)
                                assert b'EXCEPTION:' not in output, output
                                if not oom:
                                    assert b'PANIC:' not in output, output.decode(errors='replace')
                        if peer:
                            peer.drive(output)
                markers = ['PASS heap:', 'PANIC:', 'memory allocation of 2097153 bytes failed', 'OOM HALT'] if oom else ['PASS clock:', 'PASS pages:', 'PASS heap:', 'PASS DMA:', expected]
                if peer:
                    markers += ['PASS MMIO: round=0', 'PASS MMIO: round=1',
                                'RX=64 TX=64 DRIVER_OK; RX posted=32', 'MAC 52:54:00:12:34:56',
                                'PASS MMIO reset: round=0', 'PASS MMIO reset: round=1',
                                'PASS UDP: round=0; datagrams=128', 'PASS UDP: round=1; datagrams=128',
                                'portable ARP/IPv4/UDP echo', 'rings and packet/header arenas reclaimed']
                if not peer and not oom:
                    assert b'PASS MMIO:' not in output, output
                if not oom:
                    if mode == 'legacy':
                        markers += ['UNSUPPORTED entropy MMIO version 1; TCP unavailable']
                    elif mode == 'no-rng':
                        markers += ['NO virtio-rng device; TCP unavailable; UDP remains usable']
                    else:
                        markers += ['PASS entropy: modern MMIO', '4096 bytes, bounded refill, explicit TCP sequence source',
                                    'PASS entropy reset: reinitialized; all DMA pages reclaimed']
                        if peer:
                            markers += ['PASS entropy isolation: NIC reset round=0',
                                        'PASS entropy isolation: NIC reset round=1']
                if mode == 'short-rng':
                    short = re.search(rb'PASS entropy short reads: ([0-9]+)', output)
                    assert short and int(short[1]) > 0, output
                for marker in markers:
                    assert marker.encode() in output, output.decode(errors='replace')
                description = 'allocation failure panic' if oom else f'{mode} boot, heap, DMA storage, clock, device initialization'
                if peer:
                    description += ', 256 byte-exact UDP echoes across reset (0–1472 bytes)'
                print(f'PASS {profile}: {description}')
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()


if args.smoke:
    subprocess.run(['cargo', 'test', '--locked', '--manifest-path', str(WORKSPACE / 'Cargo.toml'),
                    '-p', 'async-net-baremetal-runtime', '--lib'], check=True)
    smoke('modern', 'PASS virtio-net: modern MMIO')
    smoke('short-rng', 'PASS virtio-net: modern MMIO')
    smoke('no-rng', 'PASS virtio-net: modern MMIO')
    smoke('absent', 'NO virtio-net device')
    smoke('legacy', 'UNSUPPORTED virtio MMIO version 1')
    try:
        build('--features', 'heap-oom-test')
        smoke('modern', '', oom=True)
    finally:
        # Leave the normal runnable image even if the OOM assertion fails.
        build()
else:
    smoke('modern', 'PASS virtio-net: modern MMIO', show_serial=True)

