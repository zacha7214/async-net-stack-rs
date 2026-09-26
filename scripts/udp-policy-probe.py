#!/usr/bin/env python3
"""Compare equal-count UDP workloads: burst/window 256 vs paced/window 32.
Echo is idempotent; this probe performs no retries. Results are observations,
not an assertion that either policy wins every metric.
"""
import argparse
import html
import json
from pathlib import Path
import secrets
import select
import socket
import struct
import time


def report(path):
    rows = json.loads(path.read_text())
    body = []
    for r in rows:
        success = 100 * r['received'] / r['requested']
        body.append('<tr><td>{}</td><td>{}</td><td>{}</td><td><meter min="0" max="100" value="{}"></meter> {:.1f}%</td><td>{}</td><td>{}</td><td>{:.2f}</td><td>{:.3f}</td></tr>'.format(
            r['repeat'], html.escape(r['profile']), html.escape(r['policy']), success, success,
            r['rtt_p99_ms'], r['expired'], r['goodput_mbps'], r['seconds']))
    out = path.with_suffix('.html')
    out.write_text('''<!doctype html><meta charset="utf-8"><title>UDP environment comparison</title>
<style>body{font:16px system-ui;max-width:1150px;margin:40px auto;background:#101827;color:#eef3ff}table{border-collapse:collapse;width:100%}td,th{padding:12px;text-align:left;border-bottom:1px solid #34445c}meter{width:100px}p{line-height:1.6;color:#b9c9e2}</style>
<h1>UDP: pacing, bounded work, and the network environment</h1>
<p>Actual run results. Each row sends the same number and size of requests. Burst uses a 256-request window; paced uses 32 and 300 requests/sec. Neither retries. Compare deadline success and p99 latency alongside throughput and total time: pacing is allowed to take longer. This is an application-policy comparison, not a maximum stack-throughput benchmark.</p>
<table><thead><tr><th>Repeat</th><th>Network</th><th>Policy</th><th>Deadline success</th><th>p99 RTT ms</th><th>Expired</th><th>Payload Mb/s</th><th>Seconds</th></tr></thead><tbody>'''+''.join(body)+'''</tbody></table>
<p>Unanswered requests expire after 500 ms. Delayed responses after expiry do not count as success. Netem models packet impairments, not RF propagation, association or Wi-Fi retransmissions. Random seeds and wall-clock scheduling are not controlled by this comparison. Repeat runs and preserve the JSON/configuration.</p>''')
    print(out)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--policy', choices=['burst', 'paced'], default='paced')
    p.add_argument('--target', default='10.77.0.2')
    p.add_argument('--port', type=int, default=9000)
    p.add_argument('--count', type=int, default=2000)
    p.add_argument('--size', type=int, default=512)
    p.add_argument('--report', type=Path)
    a = p.parse_args()
    if a.report:
        report(a.report)
        return
    if not 1 <= a.count <= 1000000 or not 16 <= a.size <= 1400:
        p.error('count 1..1000000, size 16..1400 required')
    window = 32 if a.policy == 'paced' else 256
    interval = 1/300 if a.policy == 'paced' else 0
    nonce = secrets.token_bytes(8)
    padding = bytes(a.size - 16)
    pending, latencies = {}, []
    attempted = expired = send_errors = late = unexpected = reordered = 0
    highest = -1
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 262144)
        sock.connect((a.target, a.port))
        sock.setblocking(False)
        start = next_send = time.monotonic()
        while attempted < a.count or pending:
            now = time.monotonic()
            for seq, sent in list(pending.items()):
                if now - sent >= 0.5:
                    del pending[seq]
                    expired += 1
            for _ in range(window):
                now = time.monotonic()
                if attempted >= a.count or len(pending) >= window or now < next_send:
                    break
                seq = attempted
                attempted += 1
                try:
                    sock.send(nonce + struct.pack('!Q', seq) + padding)
                    pending[seq] = now
                except OSError:
                    send_errors += 1
                next_send = now + interval
            # Bound receive work too; hostile/duplicate traffic cannot starve timers.
            for _ in range(256):
                try:
                    data = sock.recv(65535)
                except BlockingIOError:
                    break
                except OSError:
                    break
                now = time.monotonic()
                if len(data) != a.size or data[:8] != nonce or data[16:] != padding:
                    unexpected += 1
                    continue
                seq = struct.unpack_from('!Q', data, 8)[0]
                sent = pending.pop(seq, None)
                if sent is None:
                    late += 1
                elif now - sent >= 0.5:
                    expired += 1
                    late += 1
                else:
                    latencies.append((now - sent)*1000)
                    reordered += seq < highest
                    highest = max(highest, seq)
            deadlines = [sent + 0.5 for sent in pending.values()]
            if attempted < a.count and len(pending) < window:
                deadlines.append(next_send)
            if deadlines:
                select.select([sock], [], [], max(0, min(0.01, min(deadlines)-time.monotonic())))
        elapsed = time.monotonic() - start
    latencies.sort()
    def percentile(p):
        return round(latencies[int((len(latencies)-1)*p)], 3) if latencies else None
    print(json.dumps(dict(policy=a.policy, requested=a.count, sent=a.count-send_errors,
        received=len(latencies), expired=expired, send_errors=send_errors,
        late_or_duplicate=late, unexpected=unexpected, reordered=reordered,
        bytes_per_request=a.size, window=window, rate_pps=300 if interval else None,
        deadline_ms=500, seconds=elapsed, goodput_mbps=len(latencies)*a.size*8/elapsed/1e6,
        rtt_p50_ms=percentile(.5), rtt_p99_ms=percentile(.99))))


if __name__ == '__main__':
    main()
