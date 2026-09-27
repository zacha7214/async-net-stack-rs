# UDP configuration, scheduling, and local outcomes

`UdpPool::new(device, queue_capacity, peer_capacity)` remains available. It defaults
to FIFO, no pacing, no queue expiry, and no event collection. For explicit policy:

```rust
use async_net_stack_rs::api::{UdpConfig, UdpPool, Service, SendOptions};
use std::time::Duration;

let config = UdpConfig {
    queue_capacity: 256,
    peer_capacity: 64,             // discovered services
    service_capacity: 32,          // local endpoints
    max_tx_peers: 64,              // scheduling state
    per_peer_capacity: 16,
    tx_budget: 64,                 // attempts per flush
    fair_queue: true,
    bytes_per_second: Some(250_000),
    per_peer_bytes_per_second: Some(50_000),
    queue_lifetime: Some(Duration::from_millis(100)),
    event_capacity: 1024,
};
let mut pool = UdpPool::with_config(device, config)?;
let local = "10.77.0.2:9000".parse()?;
let remote = "10.80.1.2:9001".parse()?;
pool.bind(Service { address: local, id: 7 })?;
pool.advance(now)?; // caller-owned monotonic clock; use the same domain for poll
let id = pool.send_with_options(local, remote, b"sample", SendOptions {
    deadline: Some(now + Duration::from_millis(50)),
})?;
```

`device` and `now` above are supplied by the application. `advance` only advances
the UDP queue clock and expires entries; it does not receive or advance adapter
ARP timers. Continue calling `poll`. `flush` uses the last supplied time;
repeatedly calling it at a frozen timestamp does not advance pacing.

## Scheduling contract

- Total queue capacity, per-destination capacity, endpoint count and scheduling
  peer count are bounded. A peer is a destination IPv4 address plus UDP port,
  shared across local endpoints. This is not an identity or abuse-prevention policy.
- Fair mode uses packet round robin across eligible destination endpoints and FIFO
  within each endpoint. Backpressure or a permanent failure at one peer can be
  bypassed for another. This is packet fairness, not equal bandwidth for differently
  sized packets; the per-peer byte-rate limit supplies a separate bandwidth ceiling.
- Pacing charges full IPv4 datagram bytes, including the IP/UDP headers but excluding
  Ethernet overhead. Acceptance schedules the next permitted submission. Idle time
  does not accumulate burst credits. The first packet can send immediately.
- A peer's rate debt survives an empty queue until its next permitted time, so
  draining/refilling the queue cannot bypass its limiter. Such peers still consume
  scheduling slots; a new peer may temporarily receive `PeerLimit` even if the
  packet queue is empty. This is deliberate bounded state.
- `next_deadline()` is a scheduling hint, including queued expirations. Also consult
  device readiness and adapter deadlines; a ready-but-blocked backend must not cause
  an application busy loop. RX still proceeds on ordinary send backpressure.
- FIFO mode preserves FIFO head blocking, including pacing at its head. Fair mode
  is the appropriate choice when independent peers need independent progress.
- The implementation uses standard bounded containers and a bounded queue scan,
  not a custom lock-free queue. Single-packet submissions isolate device errors;
  this trades backend batching throughput for attribution. Measure that tradeoff
  in your VM before using these changes for peak-throughput comparisons.
- Discovery and automatic echo replies use the same admission, pacing and expiry
  policy as application sends. Discovery does not receive unlimited priority.

Fixed rate pacing is not adaptive network congestion control. The application
still needs appropriate congestion response, deadlines, and retry/idempotency policy.

## Events and errors

`send_with_options` returns a `DatagramId` after admission. `send_to` retains its
old `Result<()>` API. `pop_event()` returns fixed-size records without formatting
or I/O. Event collection is optional and bounded; `events_overwritten` reports
lost records. Events include endpoints, monotonic time and configuration generation.

| Outcome | Meaning |
|---|---|
| `Rejected(reason)` | Not admitted; ID is absent and the call returns an error |
| `Dropped(DeadlineExpired)` | Admitted, but expired in the UDP queue |
| `Dropped(ConfigurationChanged)` | Admitted, but invalidated before device handoff |
| `Dropped(Device(kind))` | Device synchronously rejected this datagram permanently |
| `Submitted` | The device accepted ownership; **not** proof of wire transmission or delivery |

Admission failures distinguish unavailable source, invalid destination, total queue
pressure, per-peer pressure, peer-state exhaustion, frame exhaustion and invalid
packet construction. `WouldBlock`/`Ok(0)` from a device retains traffic for a later
poll and does not generate repeated failure events. Permanent address/route/input/
permission errors remove the affected packet and allow progress; unexpected backend
errors are returned to the caller without consuming its queued packet.

**Ownership boundary:** queue deadlines end when the device accepts the packet.
An Ethernet adapter can subsequently wait for ARP, discard a packet on configuration
change, or encounter a backend error. Its existing `InterfaceEvent`/statistics
report those outcomes, sometimes in aggregate. They do not carry the UDP datagram
ID. Native ICMP errors, per-packet asynchronous device completions, end-to-end
expiry and remote acknowledgment are not implemented by this addition. Do not
infer delivery from a `Submitted` event or absence of a later error.

## Address and lease changes

For any L3 device, `unbind(address)` cancels UDP-owned output from that endpoint and
invalidates discovery state. `replace_services(&[Service])` validates a complete
replacement before mutation; changes cancel all UDP-owned output and clear learned
peers. Identical replacements are no-ops. These calls **do not configure Linux
addresses/routes** or revoke device-owned packets. TUN and simulation topology
must be configured by their respective owner.

For `UdpPool<EthernetIpv4<D>>`, use the coordinated API:

```rust
pool.configure_ipv4(
    "192.168.5.20".parse()?, 24, Some("192.168.5.1".parse()?),
    &[Service { address: "192.168.5.20:9000".parse()?, id: 7 }],
)?;
// When a lease expires without renewal:
pool.withdraw_ipv4();
```

The adapter validates the new address and directly reachable gateway before
changing anything. Every service must match the new address. A changed network
configuration replaces connected/default routes, clears old neighbors (including
static entries), and invalidates queued traffic in both layers. Reinstall custom
routes and static neighbors afterward. An identical address/route/service renewal
preserves traffic and neighbors. A service-only change also invalidates adapter
queues. `withdraw_ipv4` removes endpoints/routes/neighbors, cancels queues and
suppresses IPv4/ARP replies until configured again; `is_configured()` distinguishes
withdrawal from the last address retained in the adapter configuration.

No packet already handed to the underlying NIC can be recalled. Native DHCP
bootstrap is still a separate feature; this API is the configuration application
layer a future client can call. Do not mutate via `device_mut()` behind the pool
when you need coordinated invalidation.

## VM controls

The `tun_pool` example accepts the scheduling controls directly:

```sh
./target/release/examples/tun_pool --seconds 0 --workers 8 \
  --fair-queue --queue-capacity 256 --per-peer-capacity 16 \
  --bytes-per-second 250000 --per-peer-bytes-per-second 50000 \
  --queue-deadline-ms 100 --event-capacity 1024
```

It prints collected outcomes at shutdown, not in the packet-processing loop.
These flags affect server output; the UDP lab probe's pacing remains independent.
For the managed namespace lab, run this executable inside `udplab-server` instead
of starting a second process on the same TUN. A systemd override can replace its
`ExecStart` with that namespace invocation and these arguments.

Suggested VM checks: two competing peers (one flooding), an unresolved next hop
beside a healthy peer, pacing at a fixed then advancing clock, expiry while TX is
blocked, an identical lease renewal, an invalid replacement (old state survives),
and lease withdrawal with packets queued at both layers. Preserve event overflow
counts when comparing logs. Builds and tests were intentionally not run during
this implementation, following the project's current workflow.
