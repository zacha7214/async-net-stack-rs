#!/usr/bin/env python3
"""Kernel UDP discovery and bounded-window request/reply probe for tun_pool."""
import argparse
import json
import select
import socket
import struct
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', default='10.77.0.1')
    parser.add_argument('--seed', default='255.255.255.255')
    parser.add_argument('--count', type=int, default=10000)
    parser.add_argument('--window', type=int, default=64)
    parser.add_argument('--size', type=int, default=128)
    parser.add_argument('--timeout', type=float, default=0.2)
    parser.add_argument('--rcvbuf', type=int, default=262144)
    args = parser.parse_args()
    if args.count < 1 or args.window < 1 or not 8 <= args.size <= 1472 or args.timeout <= 0 or args.rcvbuf < 1:
        parser.error('positive count/window/timeout/rcvbuf and size 8..1472 required')
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, args.rcvbuf)
        sock.bind((args.source, 0))
        peers = set()
        for _ in range(3):
            sock.sendto(b'ANSP\x01\x00' + struct.pack('!I', 7), (args.seed, 9000))
            deadline = time.monotonic() + 0.15
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not select.select([sock], [], [], remaining)[0]:
                    break
                data, peer = sock.recvfrom(65535)
                if data == b'ANSP\x01\x01' + struct.pack('!I', 7):
                    peers.add(peer)
        peers = sorted(peers)
        if not peers:
            raise SystemExit('No peers discovered; check TUN route and worker process')
        pending = {}
        next_sequence = received = expired = unexpected = reordered = 0
        highest = -1
        latencies = []
        padding = bytes(args.size - 8)
        start = time.monotonic()
        while next_sequence < args.count or pending:
            while next_sequence < args.count and len(pending) < args.window:
                peer = peers[next_sequence % len(peers)]
                payload = struct.pack('!Q', next_sequence) + padding
                sent = time.monotonic()
                sock.sendto(payload, peer)
                pending[next_sequence] = (sent, peer)
                next_sequence += 1
            now = time.monotonic()
            for sequence, (sent, _) in list(pending.items()):
                if now - sent >= args.timeout:
                    del pending[sequence]
                    expired += 1
            if not pending:
                continue
            wait = max(0, min(sent + args.timeout for sent, _ in pending.values()) - time.monotonic())
            if not select.select([sock], [], [], wait)[0]:
                continue
            payload, peer = sock.recvfrom(65535)
            now = time.monotonic()
            if len(payload) != args.size or payload[8:] != padding:
                unexpected += 1
                continue
            sequence = struct.unpack('!Q', payload[:8])[0]
            entry = pending.get(sequence)
            if entry is None or entry[1] != peer:
                unexpected += 1
                continue
            del pending[sequence]
            latencies.append((now - entry[0]) * 1e6)
            received += 1
            reordered += sequence < highest
            highest = max(highest, sequence)
        elapsed = time.monotonic() - start
        latencies.sort()
        def percentile(p):
            return latencies[min(len(latencies)-1, int((len(latencies)-1)*p))] if latencies else None
        print(json.dumps(dict(mode='kernel-udp-to-tun-pool', peers=peers, sent=next_sequence,
                              received=received, expired=expired, unexpected=unexpected,
                              out_of_order=reordered, seconds=elapsed,
                              replies_per_second=received/elapsed,
                              payload_mbps=received*args.size*8/elapsed/1e6,
                              rtt_p50_us=percentile(.50), rtt_p99_us=percentile(.99),
                              socket_rcvbuf=sock.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF))))
        if received == 0:
            raise SystemExit('No data replies received')


if __name__ == '__main__':
    main()
