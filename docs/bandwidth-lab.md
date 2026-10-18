# Bandwidth-aware experiments

Run `cargo run --locked --example bandwidth_lab`. Two senders offer six 1000-byte
IPv4 packets at the same virtual instant to a shared FIFO bottleneck. The three
runs compare 1 Mbit/s with 3000- and 6000-byte queues, then 10 Mbit/s with a
6000-byte queue. The expected delivered/drop counts are 3/3, 6/0 and 6/0;
last arrivals are 24 ms, 48 ms and 4.8 ms. These are modeled times, not host
throughput measurements. Larger buffers absorb this burst at the cost of delay.

`coordinator_lab` also uses bandwidth-limited links in both directions while
running UDP discovery, TCP connection setup, partition injection and recovery.

## Composition

Create a `Network::bottleneck(Bandwidth { bits_per_second, queue_bytes,
overhead_bytes })`, then attach its handle using
`network.set_bottleneck(&sender, &receiver, Some(&bottleneck))`. Attach the same
handle to several directed edges for incast or shared-medium contention. Use
separate handles for independent/full-duplex links. `AccessPoint` exposes the
same methods. Pass `None` to detach. Existing queued packets keep their deadlines;
detaching does not cancel reservations. Create another handle for a different
rate. This is explicit scenario configuration, not inference from capture data.

The FIFO uses caller-supplied virtual time and integer nanosecond arithmetic:
service = ceil((IPv4 packet bytes + overhead bytes) * 8 * 1e9 / bits_per_second).
Arrival = previous serialization finish (or now, whichever is later) + service
+ link propagation delay. A packet occupies its full accounted byte size until
serialization finishes. Propagation and delayed receiver polling do not occupy
the serializer. `Network::advance` releases expired reservations;
`SimDevice::next_deadline` reports the earliest queued arrival for event-driven
polling. Advancing the shared fabric clock remains the scenario owner's job.

The byte limit includes the packet in service. An oversized admission or full
byte queue drops the newly offered packet and consumes its send buffer; it is
not a retryable device backpressure event. Existing unshaped unicast queues keep
their partial-send/backpressure behavior. A full destination packet queue on a
shaped link instead consumes/drops the packet before reserving bandwidth.
The destination queue bounds all ready and delayed packets, independently of
the byte limit; provision it generously to isolate bottleneck behavior.

Partition, MTU and periodic loss run before serialization and consume no
bandwidth. Propagation and alternating delay run after serialization and can
reorder arrivals. Broadcast reserves service separately for each recipient, in
port creation order; it models replicated L3 traffic, not a single radio
transmission. Broadcast recipient allocation failure can drop a copy after
service is reserved. No fairness is implied by shared FIFO admission order.
Disconnect cancels associated reservations and packets. Surviving reservations
keep their deadlines; cancellation does not compact their schedule. AP stop
clears all reservations. Checked time overflow drops the affected admission
instead of silently wrapping or saturating its deadline.

## Observability and boundaries

`Bottleneck::stats()` exposes current/peak queued bytes, admissions, failed
admissions and maximum waiting time before service. `Network::stats()` separates
bandwidth overflow, timing overflow, receiver queue loss, injected faults,
disconnect loss and local backpressure. Counters are cumulative across teardown.

The scheduler lives in `simulation::bandwidth` and depends only on `core` and
`alloc`; it owns no packets, devices, clocks or OS resources. The surrounding fabric and protocol engines also compile in the
[portable core](portable-core.md); host tools and drivers remain separate.
This is a single-stage FIFO model, without qdisc emulation, ECN, flow fairness,
physical radio scheduling or driver completion/ownership-event injection.
Packet replay does not yet reconstruct these settings from Linux counters.
