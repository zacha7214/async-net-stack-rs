# Remote telemetry and recovery boundaries

`src/telemetry.rs` exports TCP state records to an explicitly configured client
using a separate OS UDP socket and worker thread. This avoids using the stack
under investigation to report its own failures. It is a production-oriented
foundation, **not production-validated**: deployment still needs VM validation,
fault injection, overhead measurements, and authentication/network isolation.

On the client PC (substitute actual management IPs):

```sh
python3 scripts/telemetry-client.py --bind 192.168.1.10 --source 192.168.1.20 --port 9900 > telemetry.jsonl
```

Add these options to the existing `tcp_echo` invocation on the device:

```text
--telemetry-client 192.168.1.10:9900 --telemetry-bind 192.168.1.20:0
```

Both addresses must be reachable through the **host OS network**, not just the
userspace ARP/routing tables. An exclusively owned AF_XDP NIC may not provide
that path; prefer another NIC or VM management interface. No collector is started
and no traffic is sent merely by adding this code. Remote mode enables collection
and supersedes synchronous verbose event printing. Existing connection lifecycle
prints and the example's 1 ms sleep remain; this is not a timing benchmark.

## Collection and delivery contract

- Collection is opt-in. Existing TCP events are copied into a preallocated buffer.
  The producer only attempts a lock; contention and a full buffer discard records.
  It never waits for socket readiness, formats strings, reads clocks, or retries.
  This safe standard-library design is non-waiting at the API level, not wait-free
  or hard real-time. A measured SPSC ring is a possible later replacement.
- The worker swaps buffers briefly under the lock, then encodes and transmits
  outside it. Two buffers bound storage. The example drains at most 64 events per
  loop. Configurable capacity/export interval bound memory and exporter activity.
- Each datagram carries up to ten 112-byte records; maximum UDP payload is 1176
  bytes. A lower path MTU may still require reducing the batch size. There are no
  resend queues: monitoring congestion must not backpressure application work.
- Session ID, packet sequence, upstream overwrites, producer drops and local send
  errors make incompleteness visible. UDP loss/reordering remains possible;
  `possible_gap` is provisional, not a final loss count. No durable audit guarantee.
- A worker heartbeat goes out even when there are no events. A separate application
  progress counter advances only after a successful main-loop iteration. A live
  exporter with stale progress is a reason to investigate the application. A dead
  stream alone cannot distinguish network failure, host failure and worker failure.
- `status()` remains useful for local snapshots. Events suppress unchanged waits;
  changing flow metrics use `state_report_interval`. No packet payload is exported.
- `shutdown()` joins/drains outside the measured loop. Drop signals termination
  without waiting; abrupt process death can lose buffered events.

No software instrumentation has literally zero timing impact: instructions, cache
lines, atomic operations, scheduling and network traffic all matter. Establish an
uninstrumented baseline; use transition sampling or a bounded pre-trigger flight
recorder for rare timing failures. A separate management NIC/CPU reduces coupling;
external packet capture/hardware tracing is appropriate when even collection
changes the failure. Do not equate export-heartbeat health with protocol health.

The transport is unauthenticated and unencrypted. Use a trusted management network
or an authenticated tunnel (with keys managed outside this application). An IP
filter is not authentication. Do not expose this collector on a public interface.

Wire v1: big endian, `NSTL`, version byte 1, reserved byte 0, u16 record count,
then six u64 fields: random process-session ID, datagram sequence, application
progress, producer drops, socket send errors, source overwrites. Header: 56 bytes.
Record: u64 monotonic application microseconds; u64 connection ID; u8 state;
u8 wait; two reserved bytes; local and remote IPv4 addresses; local and remote
u16 ports; ten u64 metrics in the order documented by `METRICS` in the decoder.
These times are not synchronized wall clocks. TCP state/wait codes are explicit
matches in the encoder and corresponding tables in the decoder.

## Serial devices: application versus supervisor

A real serial test must be tailored to the device framing and commands. Do not
send arbitrary probe bytes to hardware. The following is a design skeleton, not
an executable protocol implementation:

```text
Disconnected -> Opening -> Synchronizing -> Healthy
                                  ^           |
                                  |           v
                              Recovering <- Suspect
```

One owner should read/write the port. Use stable device identity (USB serial number,
VID/PID plus expected identity), raw mode, explicit baud/parity/flow control and
bounded buffers. Opening some devices toggles DTR and resets firmware; handle that
as a possible new device session. Use readiness-driven I/O, partial reads/writes,
and monotonic deadlines; a successful write is not a device acknowledgment.

```text
on readable: append within buffer limit; decode bounded frames
             verify length, CRC, session and sequence before publishing
on writable: advance only the actually written prefix of the pending request
on request deadline: classify outcome as unknown, stop dependent commands
on invalid CRC/sequence/session, overflow, disconnect or repeated timeout:
    mark data stale; increment recovery generation; stop publishing healthy events
    discard incomplete parser state and invalidate pending responses
    reopen with bounded exponential backoff if transport failed
    perform documented identity/session handshake and state query
    reconcile uncertain commands; resume only after a fresh verified snapshot
on recovery budget exhaustion: preserve diagnostics and exit nonzero
```

Retry only idempotent operations or commands with device-side request-ID
 deduplication. A timed-out actuation command may already have executed; blindly
replaying it can cause harm. Flushing the input buffer is not proof of resync:
a delayed old response can arrive afterward. Session identifiers, request IDs,
framing/CRC and a verified state query are stronger evidence. If the device has no
such facilities, recovery may require a documented reset and explicit reconciliation.

The service manager owns process restart limits, startup/stop deadlines and a
watchdog. The watchdog should reflect progress of the real I/O/state machine, not
an independent thread that keeps pinging while the application is deadlocked.
On Linux, systemd provides `WatchdogSec`, `Restart=on-failure` and start-rate limits;
only enable its watchdog once the program implements `sd_notify` correctly.

A separate observer owns independent evidence: device presence/identity, USB
connect/disconnect and driver logs, process exit/liveness, resource pressure, and
line-error counters where the driver supports them. Do not have two readers steal
bytes from the same serial stream. The observer can mark an application's reports
untrusted while continuing to retain them as diagnostic evidence. A transport
heartbeat, a moving application loop and verified device transactions are three
different health signals; none substitutes for the others.

Use explicit validity (`fresh`, `stale`, `unknown`, `recovering`), monotonic age,
process session, device session and recovery generation on published readings.
Consumers reject old-generation readings after recovery. Trigger investigation
when fresh verified transactions stop, generations conflict, queue loss prevents
reconstruction, or external device/process evidence contradicts self-reports.
Do not infer exact physical state from stale software observations.

References:
- https://github.com/systemd/systemd/blob/main/man/systemd.service.xml
- https://www.man7.org/linux/man-pages/man3/termios.3.html
- https://www.man7.org/linux/man-pages/man2/TIOCMSET.2const.html
