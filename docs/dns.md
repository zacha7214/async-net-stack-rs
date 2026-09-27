# Replaceable DNS resolution

The public `dns::Resolver` trait is the application boundary. The initial
`UdpResolver<D>` is a native, poll-driven **stub client** over this stack's UDP
pool. It asks configured DNS servers to resolve names. It does not implement a
recursive server and never consults the host resolver or `/etc/resolv.conf`.
A future complete native resolver (including TCP fallback), or an explicitly
selected host-backed resolver, can implement the same trait.

## Application integration

```rust
use async_net_stack_rs::dns::{NetworkConfig, Resolver, ResolverConfig, UdpResolver};

let mut resolver: Box<dyn Resolver> = Box::new(UdpResolver::new(
    device,
    NetworkConfig {
        local_address: "10.77.0.2".parse()?,
        servers: vec!["10.80.2.1:53".parse()?],
    },
    ResolverConfig::default(),
)?);
resolver.poll(now)?;
let query = resolver.resolve("example.test");
// In the event loop:
resolver.poll(later)?;
while let Some(completion) = resolver.pop_result() {
    // Match completion.id to the request; check result and generation.
}
```

The application supplies `device`, `now` and `later`; time must be monotonic and
share one domain throughout the resolver lifetime. `resolve` returns a query ID
or an admission error. Even cache hits are delivered through `pop_result`.
`next_deadline` is a hint; continue servicing device readiness and adapter timers.
`cancel` produces a cancellation completion for a pending query.

The initial backend owns its device receive stream and its UDP endpoint bindings.
Do **not** independently poll a second UDP/TCP pool over that device. It can wrap
TUN, a simulation device, or `EthernetIpv4<D>`. Applications needing simultaneous
DNS and application traffic over one NIC will need shared transport dispatch;
that implementation can remain behind the resolver interface. Do not create two
independent consumers of the same TUN/XDP queue as a workaround.

## Implemented behavior

- Classic DNS IN/A queries for ASCII names, with optional trailing dot and
  case normalization. International names must already be converted to ASCII
  IDNA form by the caller. No implicit search suffix or `.local`/mDNS behavior.
- Up to eight CNAME hops, including chains split across responses. Compression
  pointer traversal, decoded name length, record count and packet size are bounded.
  Only matching answer-section records supply addresses; unrelated additional
  records are not used as address answers.
- Fresh OS-random transaction IDs and source ports (1024..65535) per attempt.
  Matching source/destination endpoints, ID, QNAME, QTYPE and QCLASS are required.
  This reduces spoofing exposure but is **not authentication or DNSSEC validation**.
- Bounded retries with exponential attempt timeouts and an overall lookup deadline.
  Attempts rotate among configured servers. Set attempts_per_name high enough to
  visit every server if desired; the default is three attempts. SERVFAIL/REFUSED
  can advance to another attempt. Malformed/mismatched responses are ignored and
  counted, not cached as failures. Local submission failures have a distinct result.
- Positive caching uses the shortest address/CNAME TTL, bounded by max_cache_ttl.
  NXDOMAIN and NODATA are cached only when an applicable authority SOA supplies
  the negative TTL, using min(SOA TTL, SOA MINIMUM) and any CNAME TTL. TTL zero is
  not cached. Timeouts and transport/protocol failures are not cached.
- Pending queries plus undrained completions share a configured capacity. Completion
  records are never silently overwritten; a slow consumer eventually gets Busy.
  Cache eviction is by earliest expiry; this is a bounded cache, not an LRU promise.
- Plain UDP payloads are limited to 512 bytes (no EDNS). A validated truncated
  response yields `TcpRequired`, never a partial success. General-purpose DNS
  needs TCP support; this UDP-only client remains an initial implementation.

No DNSSEC, DoT/DoH, AAAA, arbitrary record queries, iterative recursion, hosts-file
lookup, query coalescing or native DHCP is included. No DNS server is implemented.
The Unix entropy source is `/dev/urandom`, matching this project's current hosts.
The resolver is not a hard-real-time component: parsing, cache/query storage and
entropy reads should be kept outside a timing-critical packet benchmark.

## Configuration and future DHCP

`configure(NetworkConfig)` validates before mutation. Changed settings cancel
pending lookups with `NetworkChanged`, clear cache entries and advance the generation.
An identical renewal is a no-op. `invalidate()` handles network/link changes when
the local IP and resolver list happen to remain identical. Completed results retain
an old generation; consumers can compare against `generation()` before using them.

Resolver configuration does not configure the device. The network owner must first
apply address/routes to Linux or the Ethernet adapter, then update the resolver.
A future DHCP client can pass option 6's server addresses to this API. Underlying
adapter/NIC-owned packets cannot be recalled; old replies will no longer match a
live query. Queue admission and `queries_submitted` mean queued, not delivered.

For a replacement backend, implement the Resolver trait and preserve asynchronous
completion, capacity/backpressure, cancellation and generation semantics. DNS wire
parsing and UdpPool details are private to the initial implementation. The current
public answer contract is IPv4 hostname lookup; adding other record families is a
separate API extension, not something applications should infer from raw DNS bytes.

## Isolated Linux example

Build in the VM when ready (not run during implementation):

```sh
cargo build --locked --release --features tun --example dns_lookup
sudo python3 scripts/udp-system-lab.py up --profile clean
```

Keep the lab's Rust UDP stack service **stopped** so the resolver can own `labtun`.
In one terminal, run a local DNS server in the router namespace:

```sh
sudo ip netns exec udplab-router dnsmasq --keep-in-foreground \
  --conf-file=/dev/null --no-resolv --no-hosts --port=53 \
  --interface=s0 --listen-address=10.80.2.1 --bind-interfaces \
  --address=/example.test/192.0.2.123 --local-ttl=30 --pid-file=
```

In another:

```sh
sudo ip netns exec udplab-server ./target/release/examples/dns_lookup \
  example.test --iface labtun --local 10.77.0.2 --server 10.80.2.1:53
```

The query travels from the userspace source through TUN and the server's Linux
routing to dnsmasq; replies route back to TUN. This is not host-resolver lookup.
The lab's existing DHCP-only dnsmasq service uses port 0 and does not provide DNS;
use the separate command above. A TTL allows repeated queries in one resolver
instance to exercise its cache; separate example invocations have separate caches.

On macOS, the example uses `--utun-unit 0` and prints the allocated name; configure
its routes separately. The namespace recipe is Linux-only.

Suggested VM scenarios: server stopped, first server unavailable/second healthy,
lossy replies, repeated cached queries, TTL expiry, changed DNS servers, late old
responses after invalidation, CNAME loops and a truncated answer. No builds or tests
were run while adding this implementation.

Protocol references: [RFC 1035](https://www.rfc-editor.org/rfc/rfc1035.html),
[negative caching](https://www.rfc-editor.org/rfc/rfc2308.html),
[response matching/randomization](https://www.rfc-editor.org/rfc/rfc5452.html),
[TCP support](https://www.rfc-editor.org/rfc/rfc7766.html).
