# Virtual UDP pools and the Linux network lab

The missing layer between raw devices and useful network experiments was a
stateful endpoint API: binding multiple services, finding workers, queuing
traffic under backpressure, and reproducing faults without wall-clock sleeps.
`api::UdpPool` and `simulation::Network` provide that layer. Existing backend
and benchmark infrastructure is unchanged.

## Quick start: reproducible fan-out and incast

```sh
cargo run --locked --release --example pool_lab -- --workers 8 --rounds 10000 --batch 64
cargo run --locked --release --example pool_lab -- \
  --workers 32 --rounds 10000 --batch 256 --delay-us 2000 --reorder-us 5000 --drop-every 17
```

One client pool discovers worker pools over limited-broadcast UDP, distributes
128-byte requests round-robin, and collects echo replies. Each worker has its
own frame arena and bounded ingress queue. Impairments apply only to replies,
so this exercises fan-in pressure at the client independently of request loss.
The demo advances virtual time by 100 microseconds per round and drains for
1000 additional rounds (100 ms); very long delays can leave unanswered requests
at stop. `queued_requests` is application queue acceptance, not wire delivery.
`unanswered_at_stop` includes dropped **and still pending** requests. Discovery
retries three times; extreme loss can leave some or all workers undiscovered.

The JSON includes wall time, simulated time, completed replies per second,
out-of-order replies, and fabric counters. Compare batch and worker counts on
the same machine/build. These numbers measure this userspace model, including
polling, checksums and scheduling; they are not Linux/NIC throughput results.
Cross-worker fan-in can reorder replies even with no injected reordering.

## API building blocks

```rust
use async_net_stack_rs::{
    api::{Action, Service, UdpPool},
    simulation::Network,
};
use std::{net::SocketAddrV4, time::Duration};

let network = Network::default();
let address: SocketAddrV4 = "10.77.0.2:9000".parse()?;
let device = network.port(&[*address.ip()], 256, 64)?;
let mut worker = UdpPool::new(device, 128, 256)?;
worker.bind(Service { address, id: 7 })?;
worker.poll(network.now(), Duration::from_secs(5), 64, |request| {
    // Inspect request.source / destination / borrowed payload here.
    Action::Echo
})?;
worker.flush()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

- `bind` creates a virtual UDP endpoint; several addresses can share one device.
  Configure the fabric routes or Linux routes separately. It does not create
  operating-system sockets or assign interface addresses.
- `send_to` queues a payload from a bound endpoint. `WouldBlock` leaves the
  caller's payload untouched. Retry after polling. `flush` submits a prefix and
  retains the unsent suffix. There are no implicit retransmissions.
- `poll` submits previous TX, receives a bounded batch, expires peers and invokes
  a callback for application traffic. `Echo` reuses the RX packet; `Ignore`
  releases it. To implement fan-out, RPC correlation, sharding or custom replies,
  record decisions in the callback and call `send_to` after `poll` returns.
- `discover` sends a service-ID query to a unicast seed or
  `255.255.255.255:port`. Matching bound services reply from their own endpoint.
  `peers` contains bounded, expiring records; repeat discovery to refresh leases.
  There is no background timer or hidden thread. All times must be monotonic.
- `device_mut` exposes backend-specific progress/completion methods when using
  io_uring. Submission acceptance is distinct from asynchronous completion.

The discovery wire message is exactly 10 bytes: `ANSP`, version byte `1`, kind
byte (`0` query, `1` advert), and a big-endian `u32` service ID. Advertisements
identify the service endpoint by their UDP source address. This reserved protocol
is unauthenticated and intended for isolated labs; peer records are observations,
not verified identities. IPv4 UDP zero checksums are accepted. Invalid checksums,
fragments and IPv4 options are rejected. Ethernet needs an explicit adapter;
AF_XDP cannot be plugged directly into this L3 pool.

## Deterministic fault experiments

Configure each directed edge with `Network::set_link(&from, &to, Link { ... })`
before moving the devices into pools, or later via `device_mut()`.

| Mechanism | Experiment |
| --- | --- |
| `delay` + `alternating_delay` | Tail latency and reordering without random seeds |
| `drop_every` | Repeatable UDP loss and application timeout behavior |
| `partitioned` | Asymmetric reachability, stale discovery and recovery |
| `mtu` | Silent oversized-packet loss / PMTU black holes |
| Port queue size + frame count | Incast, head-of-line blocking and pool starvation |

Reconfiguration resets that edge's loss sequence, but already queued packets
retain their delivery times. The Nth **accepted** packet is dropped; a retry of
an unsent packet does not advance the sequence. Unicast congestion is checked
before faults. Full unicast queues return backpressure. Broadcast congestion
or exhausted recipient frames drops that recipient's copy, avoiding duplicate
broadcast delivery on retry. A partition does not cancel packets already queued.

Unicast moves owned packet handles without payload copies. A receiver retaining
a packet keeps a sender arena frame occupied; this deliberate coupling makes
frame-starvation scenarios visible. Broadcast copies into recipient arenas.
The fabric has fixed 2048-byte frames (normal TX reserves headroom) and bounded
packet queues, uses deadline sorting, and does not model bandwidth, Linux qdisc
scheduling, retransmission, ICMP errors, routing protocols or Ethernet neighbors.
It is a small topology/fault model, not a large-scale discrete-event simulator.

## Linux: real kernel UDP talking to virtual workers

```sh
cargo build --locked --release --example tun_pool
sudo ./scripts/linux-pool-lab.sh --count 100000 --window 64 --size 128
sudo env NETEM='delay 2ms 1ms loss 1%' \
  ./scripts/linux-pool-lab.sh --count 10000 --window 128
sudo env NETEM='rate 10mbit limit 64' \
  ./scripts/linux-pool-lab.sh --count 10000 --window 256 --size 1400
```

Requires Linux, `unshare`, `ip`, Python 3 and TUN support; `tc` and the netem kernel
module are needed only for impairment runs. Run as root or with the necessary
namespace/network capabilities. The launcher **always** creates a fresh network
namespace, starts a TUN worker process, configures `10.77.0.1/24` and an explicit
limited-broadcast route, and cleans up the worker on exit. Namespace teardown
removes its interface, routes and qdisc. No host interfaces are changed.

Set `TUN_POOL_BIN` to an absolute executable path if Cargo uses a custom target
directory.

Eight virtual workers at `10.77.0.2` through `.9`, port 9000, share a TUN device.
The Python client uses a real kernel UDP socket, discovers their addresses and
sends a bounded window of requests across them. It verifies replies' source,
sequence, size and payload, then prints completed reply rate, payload goodput,
p50/p99 RTT, timeouts, reordering and the effective socket receive buffer size.
The launcher also prints interface/qdisc counters. Expired requests are not
retransmitted; late replies count as unexpected if encountered before exit.
Percentiles cover successful requests only, so interpret them alongside loss.

Useful comparisons:

1. Sweep `--window 1`, `32`, `256`, `1024`: find where RTT and loss grow relative
   to completed throughput. Compare qdisc drops with application response drops.
2. Sweep `--rcvbuf 4096` versus `262144`: observe kernel UDP receive queue pressure
   during multi-worker response bursts. Record the effective value, which may
   differ from the request.
3. Apply delay/loss/rate limits with `NETEM`: examine queue buildup and timeouts.
   The qdisc is on **TUN egress (kernel requests toward the stack)**; it does not
   impair replies injected through TUN. Discovery also traverses it.
4. Vary `--size` up to 1472: compare packet rate and payload goodput. Use simulator
   `Link::mtu` for a repeatable silent PMTU failure; it emits no ICMP error.

The Python generator, timed polling and single process serving all virtual
workers are part of these measurements. This is a behavior demo, not a claimed
maximum packet rate. For datapath ceilings use the existing device benchmarks.
The library's L3 API also accepts Linux io_uring TUN; AF_XDP/Ethernet neighbor
resolution, reliable transfers, automatic load balancing, IPv6 and TCP remain
future work.

## Validation

`tests/network_lab.rs` exercises discovery, transfers, partitions/healing, lease
expiry/refresh, bounded peers/responses, broadcast congestion, delayed reordering,
MTU/loss faults, malformed UDP and partial-send ownership without privileges.
The macOS all-target/all-feature and no-default-feature suites, strict Clippy,
and baseline/impaired simulator runs passed. Linux validation used the existing
Ubuntu 24.04 Multipass VM with each TUN run isolated in a new network namespace.

Smoke observations (2026-09-18, 10,000 requests of 128 bytes, eight workers):

| Run | Replies | Timeouts | p99 RTT | qdisc drops |
| --- | ---: | ---: | ---: | ---: |
| Baseline, window 64 | 10,000 | 0 | 401 us | 0 |
| `delay 2ms 1ms loss 1%`, window 128 | 9,896 | 104 | 3,088 us | 104 |

The impaired run also observed 8,450 out-of-order replies. These are single-run
functional smoke observations with a Python generator, not controlled performance
claims. Netem's random loss/jitter differs between runs; the userspace fabric's
fault schedule is deterministic.
