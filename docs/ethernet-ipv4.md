# Ethernet, IPv4 routes, and ARP neighbors

`net::EthernetIpv4<D>` exposes IPv4 packets through the existing `Device` trait,
while its underlying device receives/transmits **untagged Ethernet frames**.
It connects `api::UdpPool` to AF_XDP without changing either public API. Routes,
ARP, queues, and timers run in Rust userspace; the XDP program still redirects
frames into the socket. No kernel neighbor table or netlink route lookup is used.

This is a single-interface, single-local-IPv4 endpoint adapter. It is not an IP
forwarding router. TUN and the existing `simulation::Network` already carry IP
packets and must continue to be passed directly to `UdpPool`, without this adapter.

## Code map

- `src/net/route.rs`: bounded static route table; longest-prefix lookup.
- `src/net/arp.rs`: checked RFC 826 Ethernet/IPv4 ARP parser and encoder.
- `src/net/neighbor.rs`: static, reachable, resolving, and failed states.
- `src/net/ethernet.rs`: untagged framing and minimum-frame padding.
- `src/net/interface.rs`: the adapter, bounded queues, timers, and diagnostics.
- `tests/ethernet_ipv4.rs`: a scripted Ethernet peer for unprivileged tests.
- `examples/xdp_pool.rs`: Linux AF_XDP UDP echo/client example.

The existing `net::Responder` remains available and retains its existing VLAN
handling. The new adapter deliberately ignores VLAN-tagged traffic; there is no
implicit bridging between VLANs. IPv4 options, fragmentation/reassembly, multicast,
local loopback delivery, DHCP, IPv6, and TCP are not implemented by the adapter.

## Constructing an interface

```rust,ignore
use async_net_stack_rs::{
    api::{Action, Service, UdpPool},
    device::XdpDevice,
    net::{EthernetIpv4, InterfaceConfig, route::Route},
};
use std::{net::SocketAddrV4, time::{Duration, Instant}};

let address = "192.168.1.20".parse()?;
let device = XdpDevice::new("eth1", 0)?;
let mut config = InterfaceConfig::new(address, 24, [2, 0, 0, 0, 0, 20]);
config.event_capacity = 128;
let mut interface = EthernetIpv4::new(device, config)?;
// Constructor installs 192.168.1.0/24 as an on-link route.
interface.set_gateway("192.168.1.1".parse()?)?; // Optional default route.
// Optional: bypass ARP for a known neighbor.
interface.add_static_neighbor("192.168.1.1".parse()?, [2, 0, 0, 0, 0, 1])?;
// A more-specific route overrides the default. The gateway must be on this link.
interface.add_route(Route::new("10.20.0.0".parse()?, 16, Some("192.168.1.2".parse()?))?)?;

let mut pool = UdpPool::new(interface, 128, 64)?;
let local = SocketAddrV4::new(address, 9000);
pool.bind(Service { address: local, id: 7 })?;
let start = Instant::now();
loop {
    let now = start.elapsed();
    pool.device_mut().advance(now)?;
    pool.poll(now, Duration::from_secs(5), 64, |_| Action::Echo)?;
    // Integrate device readiness and next_deadline() into your idle policy.
}
```

Use the actual interface MAC, and bind UDP services to the configured IP. Multiple
ports on that IP work; multiple local IPs on one adapter are not yet supported.
Choose a stack-owned IP, without also configuring Linux to own the same address
on that link. The AF_XDP program redirects traffic on its selected queue, so use
the existing isolated-interface/namespace lab approach. ARP and IPv4 traffic must
reach that queue; the adapter does not configure RSS or add a multi-queue manager.

For a direct cable with both endpoints in the same subnet, omit `set_gateway`.
An on-link route resolves the destination's MAC. A gateway route resolves the
gateway's MAC while retaining the original IPv4 destination. No matching route
returns `NotConnected`; an ARP timeout does not fall back to another route.
Limited broadcast and the configured subnet's directed broadcast use the
broadcast MAC, without ARP. `/31` and `/32` do not have a subnet broadcast here.

## Ownership and backpressure

`send()` validates the prospective accepted prefix before taking any frames.
`Err` consumes none. `Ok(n)` empties the first `n` slots and leaves the original
IP packets in the suffix. It means **adapter acceptance**, not device submission
or wire delivery. Backend submission happens in `advance()` or `recv()`; errors
from this later work are returned there. Allocate TX frames through the adapter,
or reuse its RX frames, to preserve pool identity and Ethernet headroom.

There are three bounded queues, implemented with standard `VecDeque`:

1. Unresolved data, with a per-neighbor limit.
2. Framed data ready for the backend.
3. ARP control packets, with a separate limit and submission priority.

The first two share `tx_capacity`; resolving a neighbor transfers ownership
between queues without increasing that total. Device partial sends remove only
the accepted prefix. Resolved peers can progress while another neighbor waits
for ARP. A full queue/table stops prefix acceptance (`Ok(0)` if nothing fits).
`UdpPool::send_to` continues to report its own queue exhaustion as `WouldBlock`.

One control frame is reserved before application allocation; the reserve is
also released for receive progress. It is not a substitute for sizing the entire
pool: leave room for RX/FILL, backend in-flight TX, the UDP application's queue,
adapter data/control queues, and handles retained by callers. With AF_XDP, retain
an appropriate `tx_reserve` as well. Arbitrarily retaining all RX handles can
still starve any finite pool.

`UdpPool::pending()` sees only its own queue. The adapter's `pending()` includes
its data/control queues but excludes backend-owned TX. Check both layers and
backend completion counters when draining or deciding whether work remains.

## Time and neighbor learning

Call `advance(now)` with monotonically increasing application time, including
while idle. `recv()` uses the most recently supplied time and processes ARP even
when data TX is blocked. `advance()` alone does not receive: continue calling
`recv()` or `UdpPool::poll()` to process replies. `next_deadline()` exposes the next
timer, but pending device work still needs readiness/progress independently.

Defaults: reachable lifetime 60 seconds; probes at one-second intervals, at most
three queued requests; resolution timeout three seconds; failure cooldown one
second. Delayed polling does not emit a burst of catch-up probes. Probe counters
count queued control frames, not confirmed transmissions. Backend backpressure
can consume the resolution deadline before requests reach the peer.

Timeout drops the adapter-owned packets for that neighbor, recycles their handles,
and records a failure event/counters. Further sends during the cooldown return
`NotConnected`. A later send can start a fresh resolution after the cooldown.
There is no per-datagram asynchronous error callback yet.

Static neighbors do not expire and cannot be overwritten by ARP. Dynamic entries
learn from valid requests for our address and from replies to an outstanding
resolution. Sender Ethernet/ARP MACs must agree. Unsolicited replies are ignored.
ARP probes with a zero sender IP are answered without creating a neighbor entry.
Claims of our IP from another MAC produce conflict events; automatic address
reconfiguration is not attempted. ARP is not authenticated.

Successful route changes discard queued data/control traffic rather than transmit
using obsolete decisions; counters record discarded data. Changing a known static
mapping to another MAC likewise discards queued traffic. `reset_link()` discards
queues and dynamic neighbors but preserves static routes/neighbors. Already
submitted backend TX cannot be revoked. Prefer configuring routes before use.

## Diagnostics

Set `event_capacity` to opt into a bounded history. `pop_event()` returns a virtual
timestamp and a route selection, ARP queueing, neighbor resolution/timeout,
submission, conflict, configuration-change, or queue-full event. The interface
instance identifies the device; route events contain destination, selected prefix,
and next-hop IP. Neighbor events supply the MAC. `stats()` is always available.

`accepted_ip`, `submitted_ip`, and backend completion counters describe different
ownership stages. None alone proves remote delivery. A timeout means no acceptable
reply arrived before the deadline; it does not diagnose a firewall or cable fault.

## Commands for user-run validation

No tests or live packet experiments were executed as part of this addition.

```sh
cargo test --locked --test ethernet_ipv4
cargo test --locked --all-targets --all-features

# Linux: build once, then use your isolated NIC/namespace and actual MAC.
cargo build --locked --release --features xdp --example xdp_pool
sudo ./target/release/examples/xdp_pool \
  --iface eth1 --ip 192.168.1.20 --prefix 24 --mac 02:00:00:00:00:20
# Add --generic for a generic-XDP copy-mode lab.
# Add --peer 192.168.1.50:9000 to initiate one request to a remote UDP echo service.
# Add --gateway 192.168.1.1 for destinations outside connected routes.
```

The example deliberately uses XDP copy mode. It is an integration example, not a
throughput benchmark or a claim of driver zero-copy support.

Protocol references: [RFC 826](https://www.rfc-editor.org/rfc/rfc826.html) for ARP
and [RFC 1122](https://www.rfc-editor.org/rfc/rfc1122.html) for IPv4 host behavior.
