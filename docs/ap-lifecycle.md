# Short-lived firmware access point lab

The first slice is `simulation::AccessPoint`: an application-owned, isolated
IPv4 broadcast domain with explicit start, stop, association and disassociation.
It runs on macOS and Linux without privileges, real interfaces or sleeps. It
models the application-visible effects of an AP disappearing; it does not run
firmware or implement Wi-Fi frames, scanning, WPA, DHCP, RF or a Linux interface.
UDP discovery is a useful starting point because this stack already has virtual
services, bounded queues, expiring peer records, broadcasts and deterministic faults.

## Run it yourself

```sh
cargo run --locked --example ap_lifecycle
cargo test --locked --test ap_lifecycle
cargo test --locked --test network_lab
```

The example should print, in order:

1. One discovered firmware service at virtual time zero.
2. One queued echo reply before shutdown, then zero queued frames and an offline
   client after shutdown. The client still has one cached discovery record.
3. Zero cached peers after the one-second lease expires.
4. An offline client immediately after `start`: association must be explicit.
5. One rediscovered service and an `after restart` echo after both endpoints
   associate. `disconnected_drops` should be one, for the old delayed reply.

These new tests and the demo were supplied for manual validation, not executed
as part of this change. Formatting, whitespace checks and an offline
`cargo check --locked --offline --example ap_lifecycle --test ap_lifecycle`
passed on the development Mac. The existing network tests check for regressions in
delay, loss, broadcasts, queue bounds and partial-send ownership.

Experiments to try in `examples/ap_lifecycle.rs`:

- Leave the firmware endpoint disconnected after restart: broadcast discovery
  finds nothing. Start does not implicitly restore any endpoint.
- Replace `stop()` with `disassociate(firmware.device_mut())`: only that endpoint
  disconnects, and queued packets to/from it disappear.
- Add `drop_every: 2` or `alternating_delay` to a directed `Link`, then repeat
  discovery/requests to exercise application retries and out-of-order replies.
- Advance less than the lease interval during the outage to retain stale peers.
- Drop/recreate the firmware `UdpPool` while keeping the client alive to model
  application state loss. Reuse the IP after dropping the old device and explicitly
  associate the newly created port. The example itself preserves both pools.

## Contract and boundaries

`AccessPoint::default()` starts stopped. `port()` allocates a disconnected
`SimDevice`; put it in a `UdpPool`, bind its service and use `device_mut()` for
association. Both client and firmware service endpoints use the same explicit
attachment API. This is a connectivity model, not an 802.11 station-role model.
`start`, `stop` and repeated association are idempotent. Dropping the AP calls
`stop`, even if devices/pools outlive the controller.

Each AP owns separate routes and a separate virtual clock. Duplicate IPs across
APs are allowed; there is no bridge or roaming between them. Unicast and limited
broadcast only reach associated endpoints of that AP. Multicast and subnet-directed
broadcast are not implemented. Link faults are directed; configure both directions
if desired. Configuration, fault sequence counters, addresses and virtual time
survive stop/start; use `set_link` to reset an edge's sequence.

Stopping clears every fabric ingress queue, including ready and delayed packets.
Disassociation clears queued traffic both to and from that endpoint, including
broadcast copies at recipients. Dropping any `SimDevice` now also purges traffic
to/from it, for both plain `Network` ports and AP ports. Plain `Network` ports
otherwise retain their existing always-online behavior. A link partition still
preserves already-queued packets, so it differs deliberately from disassociation.

Sending a nonempty batch from an offline device returns `NotConnected` and
consumes no frames. Sending to a known offline destination silently consumes and
drops the packet. Offline broadcast recipients are skipped. `recv` on an offline
device clears the supplied output batch and returns zero. `alloc` remains usable.
`FabricStats::disconnected_drops` counts purged queue entries and offline unicast
drops; it does not count skipped broadcast recipients or rejected offline sends.

Already-delivered packets and application TX queues are outside the fabric's
ownership and are not revoked. In particular, `UdpPool::flush` preserves queued
TX on `NotConnected`, and `poll` returns that error if it has pending TX (it expires
leases before attempting the flush). Handle this error during outages; pending
requests will be retried after association. To simulate loss of firmware TX,
bindings and peer state, drop/recreate its pool. Ports reserve their IPs until
their devices are dropped. Repeated stop/start reuses ports; repeatedly creating
new devices leaves port slots/arenas in the fabric until the fabric is dropped,
so use a fresh AP between long churn experiments.

## A feasible path toward the larger lab

1. **ICMP probes:** echo replies already exist in `net::Responder`, including
   checksums. ICMP has no connection handshake. Add an echo request builder,
   reply parsing/correlation, bounded outstanding probes and virtual deadlines.
   Then distinguish reachability, timeout and late replies across AP restarts.
   `UdpPool` currently only processes UDP; probe handling needs separate dispatch.
2. **Linux application integration:** connect real processes to an isolated lab
   through TUN/network namespaces and a packet pump into the simulator. Use Linux
   TCP sockets at both ends initially for real TCP behavior while this stack
   controls faults. The current AP model alone cannot attach an OS application.
3. **Native TCP if useful:** build transport dispatch and bounded connection
   state, handshake/sequence validation, stream buffers, ACK/retransmission timers,
   receive windows, congestion behavior and close/reset handling in stages. A
   SYN/SYN-ACK demo alone would not give reliable application streams. Kernel TCP
   in the Linux lab lets us exercise application failures before undertaking this.
4. **Actual Wi-Fi lifecycle:** use Linux `mac80211_hwsim`, `hostapd` and
   `wpa_supplicant` for virtual radios and association/authentication, with an
   application supervisor that starts/stops hostapd alongside the firmware
   process. Add address configuration/DHCP and discovery, then bridge to the
   packet impairment layer. This would test OS reconnect behavior that the
   in-memory model does not cover. The first standalone
   [hwsim lab](wifi-lab.md) now provides association, static addresses, UDP
   discovery and TCP echo; firmware-lifetime coupling and packet integration
   remain subsequent steps.

The [Linux wireless hwsim documentation](https://wireless.docs.kernel.org/en/latest/en/users/drivers/mac80211_hwsim.html)
describes virtual radios and an AP/station example using hostapd and
wpa_supplicant. That is the proposed foundation for a later Linux-only lab;
hardware-specific firmware behavior would still require explicit modeling.
