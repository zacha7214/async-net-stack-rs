# Managed UDP environment lab

This lab runs the real Rust TUN UDP pool behind two routed veth links, on one
Linux machine. It provides independent systemd lifecycles for provisioning,
the stack, and optional DHCP client/server processes. Nothing is installed or
started automatically. The new code has not been built or run in the authoring
session; validate it in your disposable Linux VM first.

```mermaid
flowchart LR
  C[Client namespace: UDP probe / DHCP client] -->|10.80.1.0/24| R[Router namespace: netem / DHCP server]
  R -->|10.80.2.0/24| S[Server namespace: Linux forwarding]
  S -->|TUN: 10.77.0.0/24| U[Rust UdpPool: 10.77.0.2:9000]
```

Namespaces contain all addresses, routes, qdiscs and forwarding changes. The
host's physical NICs, default routes and DNS configuration are not modified.
There is no NAT or Internet access. Paths deliberately cross namespaces; two
sockets in one host namespace could otherwise bypass the intended link.

## Setup

Requirements: Linux root, iproute2 (`ip`, `tc`), Python 3, TUN and `sch_netem` kernel
support. Optional DHCP requires dnsmasq and BusyBox `udhcpc` available as commands.
Build on the VM when ready:

```sh
cargo build --locked --release --features tun --example tun_pool
sudo python3 scripts/udp-system-lab.py up
sudo python3 scripts/udp-system-lab.py stack
```

The second command runs in the foreground until SIGTERM/SIGINT. In another shell:

```sh
sudo python3 scripts/udp-system-lab.py status
sudo python3 scripts/udp-system-lab.py compare --output /tmp/udp-results.json --repeats 3
```

Open `/tmp/udp-results.html` on your client PC. JSON retains each observation;
`udp-results.environment.json` retains profile configuration, kernel/tc versions,
and qdisc snapshots. Copy these files together. No example benchmark numbers are
fabricated. Comparisons take several minutes; they do not run automatically.

Stop stack/DHCP/probe processes before `down`. Cleanup refuses to delete namespaces
with live processes, and only operates on labs with matching ownership markers:

```sh
sudo python3 scripts/udp-system-lab.py down
```

`--name anotherlab` selects another isolated instance. Names must match across
all commands. Use `--config PATH` for customized profiles; `--binary PATH` selects
the stack executable. Currently addresses and topology are intentionally fixed
inside each instance; namespaces allow concurrent instances with the same IPs.

## systemd management

Place the checkout and built binary at `/opt/async-net-stack-rs`, owned by root and
not writable by untrusted users: these lab launchers run privileged. Inspect the
unit files, then install explicitly:

```sh
sudo cp deploy/systemd/async-net-*.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl start async-net-stack@udplab.service
sudo systemctl status async-net-stack@udplab.service
sudo journalctl -u async-net-stack@udplab.service -f
```

The stack requires/provisions `async-net-lab@udplab.service`. Stopping the stack
leaves network state available for inspection. Stopping the lab stops dependent
services before cleanup. Restarting only the stack preserves routing and qdisc
state. Start-rate limits bound crash loops. There is no readiness/watchdog claim:
`Type=simple` says the process started, not that UDP is responding. Use an external
probe for protocol health. No units are enabled at boot by these commands.

These are lab service templates, not a general production network manager. They
supervise the TUN stack; they do not manage physical NICs, mirror kernel routes
into `EthernetIpv4`, or add native DHCP support to the Rust stack.

## Impairments and routing changes

`config/udp-lab.json` defines clean, lossy, bufferbloat, burst-loss and reorder
profiles using explicit netem arguments. Both directions are independently
controlled on router egress: `s0` carries requests, `c0` carries replies.

```sh
sudo python3 scripts/udp-system-lab.py profile --profile lossy
sudo python3 scripts/udp-system-lab.py profile --profile reorder --direction reply
sudo python3 scripts/udp-system-lab.py fault --fault link-down
sudo python3 scripts/udp-system-lab.py fault --fault restore
sudo python3 scripts/udp-system-lab.py fault --fault route-blackhole
sudo python3 scripts/udp-system-lab.py fault --fault restore
sudo python3 scripts/udp-system-lab.py fault --fault address-change
```

A one-direction profile leaves the other direction unchanged. `restore` restores
static client address/routes/link state, not qdiscs; use `profile` separately.
Stop DHCP client management before manual address faults so it does not race the
fault injector. With DHCP active, obtain a new lease instead of static restore.

Run a longer probe while injecting faults from another shell:

```sh
sudo ip netns exec udplab-client python3 scripts/udp-policy-probe.py --policy paced --count 100000
```

The probe uses a connected kernel UDP socket. UDP connect selects a peer and local
route/address; it does not establish a reliable session. Changing the client
address can leave the old socket unusable even though a new socket works. The
probe intentionally does not hide this by reconnecting. Compare old and newly
started probes, route lookup (`ip -n udplab-client route get 10.77.0.2`), neighbor
state, and packet capture in both directions. An accepted send is not delivery.

## DHCP experiments

```sh
sudo systemctl start async-net-dhcp-server@udplab.service
sudo systemctl start async-net-dhcp-client@udplab.service
sudo journalctl -u async-net-dhcp-client@udplab.service -u async-net-dhcp-server@udplab.service -f
```

The router leases `10.80.1.50..80/24` for two minutes and advertises itself as gateway.
The client hook validates addresses, handles bound/renew/deconfig, preserves an
unchanged address on renewal, and never changes host-shared `resolv.conf`.
The stack's service address stays fixed at `10.77.0.2`. DHCP configures the kernel
client only; it does not magically change a userspace stack's address/neighbor maps.

Useful experiments:

| Event | What to inspect | Expected application lesson |
|---|---|---|
| Same lease renewed | Source address and uninterrupted replies | A renewal should not recreate all sockets |
| DHCP server stopped past lease expiry | Renew/rebind attempts, address removal, route state | Lease expiry differs from temporary packet loss |
| DHCP server restarted before expiry | Lease continuity and existing UDP traffic | DHCP availability is not the data path |
| Client restarted / address changes | Old socket versus fresh socket; source endpoint | Reconcile local configuration and rediscover peers |
| Wrong/missing gateway | On-link DHCP success versus routed UDP failure | Getting an address does not prove reachability |
| Bursty loss during renewal | DHCP retries alongside UDP deadlines | Control traffic and data can fail differently |

For outage tests, stop only `async-net-dhcp-server@udplab`; the router and stack
continue. Capture DHCP with `ip netns exec udplab-client tcpdump -ni eth0 'port 67 or port 68'`.
BusyBox builds differ in available client features; retain its logs. To experiment
with alternate DHCP options, edit the isolated dnsmasq arguments in the launcher
and restart its service; changing options does not instantly update existing leases.

## Reading the comparison

Both policies send 2000 unique 512-byte echo requests and never retry. Burst uses
256 in-flight requests with no pacing. Paced uses 32 in-flight requests and a
300 requests/sec cap. Both enforce 500 ms deadlines and validate reply session,
sequence, size and contents. The target operation is idempotent echo.

The report shows deadline success, p99 RTT, expiry count, payload goodput and total
completion time. Pacing can improve delivery by offering less instantaneous load,
while taking longer. This is not an equal-offered-rate congestion-control study.
Large queues may trade fewer drops for unacceptable latency; they are not always
better or always worse. No policy can recover a truly absent route. Requests with
send errors are counted separately from requests accepted locally but expired.

Each profile is repeated, policy order alternates, and qdisc counters are retained.
The defaults do not pin random seeds or CPU scheduling. Configure netem `seed` only
if your installed kernel/iproute2 supports it. Even seeded packet impairment does
not make a wall-clock workload deterministic. The experiment restores the selected
`--profile` after comparisons, not an arbitrary preexisting qdisc arrangement.

## Wireless fidelity and simulation

Netem is the quick path for delay, loss bursts, reordering and queue pressure.
It does not model RF contention, roaming, authentication, PHY rate adaptation or
802.11 retries. Use the existing [hwsim Wi-Fi lab](wifi-lab.md) for AP/station and
association behavior. That lab currently uses its own kernel-side services;
connecting it to this routed TUN topology is a separate integration step.

For deterministic virtual-time experiments without Linux privileges, keep using
[pool_lab](network-lab.md). Its modeled performance is not interchangeable with
this kernel/TUN benchmark. Compare application outcomes across the two, not raw
throughput numbers as if they measured the same execution path.

Primary references: [netem](https://www.man7.org/linux/man-pages/man8/tc-netem.8.html),
[dnsmasq](https://thekelleys.org.uk/dnsmasq/docs/dnsmasq-man.html),
[systemd service lifecycle](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html).
