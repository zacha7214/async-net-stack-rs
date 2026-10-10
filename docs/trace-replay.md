# Capture-to-regression playback

This first deliverable imports an offline capture, inventories replayable IPv4
conversations, and replays their original IP bytes in capture-record order using
virtual time. It runs without network privileges and never transmits onto a host
interface. It does not reconstruct live TCP state, applications, or route tables.

## Run

```sh
cargo run --locked --example trace_replay -- --demo
cargo run --locked --example trace_replay -- /path/to/capture.pcap
cargo run --locked --example trace_replay -- /path/to/capture.pcapng
```

The demo fixture has four UDP packets, including equal and backward timestamps,
and one unsupported ARP record. Its final report is:

```text
result: delivered=4, skipped=1, late=0, coordinator_drops=0, bytes_verified=true
```

The report includes interface metadata, each conversation's original addresses
and ports, direction counts, observed TCP flags, every normalization or skip,
and each delivered record's scheduled time, actual virtual time, and lateness.
Redirect stdout to save the report. Structural file errors fail the command;
unsupported packet records are retained, reported, and skipped. A successful
command with nonzero `skipped` is partial playback, not complete reproduction.

The example subscribes an observer to every replayed IP protocol on the
coordinator, checks each protocol's packet order and exact bytes, and verifies
that no packets were lost in dispatch. Global capture order is checked at the
TraceDevice delivery boundary; different protocol consumers can run in a
different order after dispatch.

## Supported capture formats

- Classic pcap 2.4: little/big endian, microsecond/nanosecond timestamps.
- Pcapng 1.0 sections with independent byte order, interface descriptions,
  enhanced packet blocks, obsolete packet blocks, and simple packet blocks.
- Pcapng decimal/binary timestamp resolution and signed timestamp offsets.
  Unrepresentable resolutions and pre-epoch times fail explicitly; fractions
  below one nanosecond round down with a capture note.
- Ethernet with up to two 802.1Q/802.1ad tags, raw IP (link type 101), and IPv4
  (link type 228) can feed the IPv4 replay target.

Unknown link types and non-IPv4 packets remain in the imported capture but are
not replayed. Linux cooked, loopback, Wi-Fi/radiotap, IPv6, IP options, fragments,
and truncated captures need later extensions. Bad IPv4 checksums and zero TTL
are skipped because the current coordinator rejects them. Bad or unsupported
TCP/UDP headers/checksums are reported and passed through unchanged for an engine
to inspect or reject. There is no automatic checksum or offload repair.

Pcapng interface names and timestamps are interpreted. Other options, including
packet direction flags, and unhandled block types are reported as capture notes.
Original packet bytes are retained, but the importer is not a lossless serializer
for arbitrary container metadata. Section-local interface IDs are translated to
unique file-global IDs. Interface/VLAN identity scopes the conversation inventory;
the observer replays all records into one L3 stream, not separate virtual NICs.

The readers follow the [pcap format description](https://www.ietf.org/archive/id/draft-ietf-opsawg-pcap-05.html)
and [pcapng format description](https://www.ietf.org/archive/id/draft-ietf-opsawg-pcapng-06.html).

## Time and order

The first timestamped record sets time zero, including when that record cannot
be replayed. File order is authoritative; equal timestamps retain record order.
Backward timestamps clamp to the preceding deadline and emit a record warning.
Missing timestamps use the preceding deadline, initially zero, with a warning.
Unsupported records still participate in the capture timeline.

The example advances directly to the next device deadline. No wall-clock sleep,
real-time scheduling, network-delay inference, or bandwidth model is involved.
A recorded packet is an observation at the capture point, not a newly inferred
send event. No extra simulated link latency is automatically added.

## Library composition

```rust
use async_net_stack_rs::{
    device::Device,
    trace::{Capture, Limits, ReplayConfig, Trace, TraceDevice},
};

let capture = Capture::read(std::fs::File::open("capture.pcap")?, Limits::default())?;
let trace = Trace::analyze(capture);
// Inspect trace.capture(), trace.conversations(), and trace.diagnostics().
let mut device = TraceDevice::new(trace, ReplayConfig::default())?;
let mut batch = Vec::new();
while let Some(now) = device.next_deadline() {
    device.poll_at(now)?;
    device.recv(64, &mut batch)?;
    // Inspect the packet handles here, or put device behind Coordinator.
    for event in device.take_deliveries() {
        println!("record {} delivered at {:?}", event.record, event.actual);
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`recv` releases the preceding batch as required by `Device`. Each replay packet
is copied from immutable capture storage into a pooled frame; this is not a
zero-copy capture reader. Handles keep their arenas alive after the device drops.
If consumers retain all frames, receiving returns `WouldBlock` (or a partial
batch). The cursor does not skip packets. Releasing handles permits retry, and
later delivery is recorded in `Delivery`. Drain those reports periodically;
otherwise they accumulate up to one event per replayable record.

`send` returns `Unsupported` without consuming any nonempty batch. This is
input-only recorded playback. A live TCP engine needs an explicit response
observer/scripted peer, which is not implemented here. Feeding a midstream TCP
capture to a new connection engine does not reconstruct its connection state.

Default import limits are 256 MiB of source bytes, one million packet records,
and 4,096 interfaces. Parsing currently loads the file and copies retained packet
bytes; the byte limit is not a total process-memory limit. Inventory, plans, and
reports add memory proportional to records. The default replay arena holds 64
frames of up to 65,535 bytes. An undersized configured frame fails construction
rather than silently truncating packets.

## Inventory and remaining work

Inventory is observational and covers replayable IPv4 records. Bidirectional
keys include interface, VLAN IDs, IP protocol, addresses, and TCP/UDP ports when
present. A new observed TCP opening can split a reused tuple; repeated SYNs with
the same sequence number remain together. Flags are observations, not proof of
a completed handshake or graceful shutdown. Reuse without an observed opening,
retransmission analysis, ambiguous overlapping sessions, application boundaries,
and reconstruction of missing data remain unresolved.

Original IP addresses are never rewritten. Ethernet/MAC bytes remain in capture
storage but are stripped from the L3 playback view, as are bytes beyond IPv4 total
length (padding/FCS). Routes are neither inferred nor installed: this deliverable
reproduces traffic at one observation boundary.

Next steps are interface/direction selection, machine-readable scenario manifests,
scripted peer response matching, record-targeted fault mutations, and explicit
configuration/topology reconstruction. Wall-clock pacing and driver buffer or
completion fault injection are separate from virtual-time packet replay.
