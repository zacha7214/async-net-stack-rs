#!/usr/bin/env python3
"""Decode NSTL v1 UDP telemetry into JSON lines. Standard library only.
No authentication: run on a trusted management network or encrypted tunnel.
"""
import argparse
import collections
import ipaddress
import json
import socket
import struct
import time

STATES = ('SynSent', 'SynReceived', 'Established', 'FinWait1', 'FinWait2',
          'CloseWait', 'Closing', 'LastAck', 'TimeWait', 'Closed', 'Reset', 'TimedOut', 'Failed')
WAITS = ('Application', 'Acknowledgment', 'PeerWindow', 'CongestionWindow',
         'FlightLimit', 'Device', 'Handshake', 'Closing', 'Terminal', 'Ready')
METRICS = ('send_buffered', 'receive_buffered', 'cwnd', 'ssthresh', 'bytes_in_flight',
           'outstanding_segments', 'rto_us', 'timeout_retransmissions', 'receive_window', 'peer_window')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bind', default='127.0.0.1')
    parser.add_argument('--port', type=int, default=9900)
    parser.add_argument('--source', required=True, help='Expected exporter IP (filter, not authentication)')
    parser.add_argument('--stale-seconds', type=float, default=3.0)
    args = parser.parse_args()
    if args.stale_seconds <= 0:
        parser.error('stale-seconds must be positive')
    source = ipaddress.ip_address(args.source)
    sock = socket.socket(socket.AF_INET6 if ':' in args.bind else socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind((args.bind, args.port))
    sock.settimeout(min(args.stale_seconds, 1.0))
    sessions = collections.OrderedDict()
    last_packet = time.monotonic()
    stale_reported = False
    while True:
        try:
            data, peer = sock.recvfrom(2048)
        except socket.timeout:
            data = None
        now = time.monotonic()
        if data is not None and ipaddress.ip_address(peer[0]) == source:
            if len(data) < 56 or data[:6] != b'NSTL\x01\x00':
                continue
            count, session, seq, progress, dropped, errors, overwritten = struct.unpack_from('!H6Q', data, 6)
            if count > 10 or len(data) != 56 + count * 112:
                continue
            records = []
            for i in range(count):
                r = memoryview(data)[56+i*112:56+(i+1)*112]
                timestamp, connection = struct.unpack_from('!QQ', r)
                if r[16] >= len(STATES) or r[17] >= len(WAITS):
                    break
                local_port, remote_port = struct.unpack_from('!HH', r, 28)
                records.append(dict(timestamp_us=timestamp, connection=connection,
                    state=STATES[r[16]], wait=WAITS[r[17]],
                    local=f'{socket.inet_ntoa(r[20:24])}:{local_port}',
                    remote=f'{socket.inet_ntoa(r[24:28])}:{remote_port}',
                    **dict(zip(METRICS, struct.unpack_from('!10Q', r, 32)))))
            else:
                previous = sessions.get(session)
                reordered = previous is not None and seq <= previous[0]
                gap = max(0, seq - previous[0] - 1) if previous and not reordered else 0
                if not reordered:
                    changed_at = now if previous is None or previous[1] != progress else previous[2]
                    sessions[session] = (seq, progress, changed_at)
                    sessions.move_to_end(session)
                    if len(sessions) > 16:
                        sessions.popitem(last=False)
                else:
                    changed_at = previous[2]
                print(json.dumps(dict(session=f'{session:016x}', sequence=seq,
                    possible_gap=gap, reordered_or_duplicate=reordered,
                    progress=progress, progress_stale=now-changed_at >= args.stale_seconds,
                    producer_drops=dropped, send_errors=errors, source_overwrites=overwritten,
                    events=records)), flush=True)
                last_packet = now
                stale_reported = False
        if now - last_packet >= args.stale_seconds and not stale_reported:
            print(json.dumps(dict(collector_warning='telemetry stale; exporter, network or host may be unavailable')), flush=True)
            stale_reported = True


if __name__ == '__main__':
    try:
        main()
    except KeyboardInterrupt:
        pass
