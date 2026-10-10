# Composing experiments on one interface

The coordinator owns one IPv4 receive stream and dispatches packet handles into
bounded protocol ports. Each port implements `Device`, so protocol engines remain
independent of the coordinator and of each other. Existing standalone pools still
work unchanged. Dispatch moves ownership without copying payloads; this does not
make a copying backend zero-copy or establish a throughput result.

Run the unprivileged, deterministic example:

```sh
cargo run --locked --example coordinator_lab
```

Expected milestones:

```text
t=20ms: UDP discovered service; opening TCP
t=200ms: partition=true
t=800ms: partition=false
t=1.26s: recovered 25 bytes; retransmissions=1, fault_drops=1
```

The example uses simulated time and injected TCP initial sequence numbers. It
asserts delivery, retransmission, and injected loss and runs as an example test.
No wall-clock sleeps, network privileges, or VM are required. Edit the directed
`Link` configuration to add delay, alternating reordering, periodic loss, or an
MTU black hole. A partition preserves previously queued traffic; AP teardown
has different semantics and purges fabric-owned frames.

## Assembly

```rust
use async_net_stack_rs::{
    api::{Action, UdpPool, tcp::{TcpConfig, TcpPool}},
    coordinator::{Coordinator, PortConfig},
    device::LoopbackDevice,
};
use std::time::Duration;

let mut interface = Coordinator::new(LoopbackDevice::new());
let mut udp = UdpPool::new(interface.port(17, PortConfig::default())?, 32, 8)?;
let mut tcp = TcpPool::new(interface.port(6, PortConfig::default())?, TcpConfig::default())?;
let now = Duration::ZERO;
interface.poll(now, 64)?;
udp.poll(now, Duration::from_secs(10), 32, |_| Action::Ignore)?;
tcp.poll(now, 32)?;
let timer_hint = interface.next_deadline([udp.next_deadline(), tcp.next_deadline()]);
# Ok::<(), std::io::Error>(())
```

Choose a TUN device, simulation/AP port, or `EthernetIpv4<XdpDevice>` as the
underlying device. Raw Ethernet frames must pass through `EthernetIpv4` first.
Use `with_device` between polls to apply link faults or inspect backend counters.
Custom engines can subscribe to other IPv4 protocol numbers; duplicate protocol
subscriptions are rejected. Drop a port to release its queue and subscription.

Each experiment iteration calls the coordinator, then each engine with the same
monotonic timestamp. The experiment owns engine ordering, application actions,
and time advancement. Engine polling does not independently advance the physical
adapter or replenish transmission credit. `next_deadline` combines supplied
engine hints with adapter timers, including ARP. It does not account for all
application work or readiness, and must not be used alone to decide to sleep.

## Resource and failure semantics

- Each port has an independent bounded RX queue. Overflow drops that protocol's
  frame and increments its `queue_drops`; other protocols still receive traffic.
- Each coordinator poll replenishes a per-port accepted-packet TX budget. Total
  submissions between polls are bounded by the sum of port budgets. Polling again
  at the same timestamp starts another budget epoch; this is not rate limiting.
- Accepted TX prefixes are consumed. Unsent suffixes remain with their engine.
  Backend errors consume nothing and do not decrement the port budget.
- Ports share the underlying frame pool and device capacity. Budgets limit a
  sender's burst but do not reserve buffers or guarantee fairness under pool
  exhaustion. Rotate engine ordering when investigating scheduling bias.
- `received` counts dispatch into a port, `submitted` counts local acceptance,
  and neither implies peer delivery. Coordinator counters separately identify
  malformed IPv4 and protocols with no subscriber. Transport engines validate
  transport checksums and retain their own counters.

## Scope and next extensions

This first coordinator splits by IP protocol, not UDP/TCP socket tuple. One UDP
pool can contain many application services. A `UdpResolver` can use the UDP port
alongside TCP, but a separate UDP pool and resolver cannot both subscribe to 17.
Sharing those requires a socket demultiplexer or a resolver that uses the shared
UDP pool. ICMP is available to a custom subscribed engine; the coordinator does
not itself answer ping or process path-MTU errors.

Interface reconfiguration is explicit: `with_device` changes only the device.
It does not invalidate TCP connections, UDP peers, DNS caches, or engine-owned
queues. Likewise, AP teardown purges fabric traffic but not frames already
dispatched into protocol queues. Experiments must choose their desired lifecycle
policy. A coordinated configuration-generation/reset API is a future extension.

The simulator remains an L3 model without bandwidth serialization or shared
bottleneck scheduling. Useful next experiments are byte-bounded bottlenecks,
record/replay of packet events, and configuration transitions across all engines.
The coordinator is single-threaded and uses shared interior mutability; measure
its cost before using it for a high-speed datapath comparison.

`TcpPool::with_sequence_source` makes replay or platform entropy integration
possible. The default constructor still uses OS entropy. Deterministic sequence
sources belong in isolated simulations, not deployed TCP endpoints.
