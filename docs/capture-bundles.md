# Linux capture bundles and portable experiments

`net-capture` collects a packet trace and its Linux configuration/counter context.
`net-experiment` loads that bundle and runs the current observed-packet replay.
The collector requires Linux; the runner and both executable builds also work on
macOS. No kernel module or BPF loader is required for this first collection tier.

## Collect and replay

Install `tcpdump` on the capture machine. Install `iproute2` and `ethtool` to obtain
more context; unavailable or unsupported optional tools are reported explicitly.
Build the executables as your normal user, then grant capture privileges when
running the collector:

```sh
cargo build --locked --release --bins
sudo ./target/release/net-capture --interface eth0 --duration 30 --interval 1
./target/release/net-experiment > /tmp/experiment-report.json
```

Replace `eth0` with the selected interface. The collector does not modify routes,
offload settings, queues, or addresses, and does not request promiscuous mode. Snaplen is
65,535 bytes. To focus a trace:

```sh
sudo ./target/release/net-capture --interface eth0 --duration 10 \
  --filter 'tcp port 9000 or udp port 9000' --max-packets 100000
```

A completed bundle is printed with its absolute path. It can be replayed directly
or copied as a directory to another machine, including a Mac:

```sh
./target/release/net-experiment results/traces/trace-EXAMPLE
./target/release/net-experiment /copied/trace-EXAMPLE --report /tmp/new-report.json
```

The runner also accepts a plain `.pcap`/`.pcapng` file or a bundle's `manifest.json`.
A plain capture produces a replay report without collection context. `--report`
requires a new output file and never overwrites existing captures or reports.

## Default trace location

Both executables use the same lookup:

1. `--root DIR`, when supplied.
2. `ASYNC_NET_TRACE_DIR`, when set.
3. `results/traces/` under an enclosing project workspace.
4. The build-time workspace's `results/traces/`, if that workspace still exists.
5. `results/traces/` under the current directory for a relocated executable.

An explicit source path bypasses latest lookup. Without one, `net-experiment`
selects the compatible completed manifest with the greatest `finished_unix_ns`.
Failed, collecting, and incompatible-version bundles are excluded. If the selected
completed bundle is damaged, replay reports an error instead of silently choosing
older data. No existing bundle is overwritten. `results/` is already ignored by Git.
When using sudo with a custom store, pass `--root` explicitly if sudo strips the
`ASYNC_NET_TRACE_DIR` environment variable.

## Bundle layout and schema

```text
trace-<creation-time>-<pid>-<suffix>/
  manifest.json
  capture.pcap
  environment.json
  counters.jsonl
  capture.stderr.log
  raw/
    <sequence>-<tool>.stdout.txt
    <sequence>-<tool>.stderr.txt
```

All raw-tool paths in metadata are relative to the bundle. The importer uses fixed
names for its required artifacts; it never executes a recorded command.

| Artifact | Schema | Contents |
| --- | --- | --- |
| `manifest.json` | `async-net.capture.v1` | Completion status, interface, requested duration, observed capture interval, filter, packet limit, command, collector version, capture size/count, clock description, stop reason |
| `environment.json` | `async-net.environment.v1` | Before/after command results and selected `/proc` configuration |
| Each `counters.jsonl` line | `async-net.counters.v1` | Sample phase, timestamps, normalized interface counters, read errors, driver counters, system statistics, command results |
| Runner output | `async-net.experiment.v1` | Capture inventory, diagnostics, per-record delivery timing, byte verification, collection availability and counter deltas |

Nanosecond timestamps are decimal strings to avoid JSON consumers losing precision.
`monotonic_ns` is relative to collector startup. `unix_ns` supplies a wall-clock
anchor for each sample; sampling is not atomic and every command has its own start
and end timestamps. Packet clocks are whatever tcpdump records. These anchors are
not hardware clock synchronization, and the runner does not infer nanosecond causal
ordering between a counter read and a captured packet.

Command results contain `program`, `args`, relative `stdout`/`stderr` paths,
`status`, `exit_code` when available, and parsed `data` for JSON output or `text`
otherwise. Status can be `ok`, `unavailable`, `failed`, `error`, `timeout`,
`output_limit`, `cancelled`, or `deadline`. `unavailable` is not a zero counter.
Some skipped commands have no output file. Optional commands have bounded runtime
and output checks; their failure does not invalidate a successful packet capture.

The collector atomically publishes `status: complete` only after packet parsing
and required metadata validation succeed. Collection failures leave `status: failed`
and an error in the same directory. Ctrl-C requests graceful capture termination;
a valid flushed capture can complete with `stop_reason: interrupted`. A forced
termination, malformed/oversized capture, or invalid report leaves a failed bundle.
SIGKILL or a crash can leave `collecting`, which is also excluded from latest lookup.

## Collected information

Before and after capture:

- Kernel identity and tcpdump version.
- Interface details, addresses, IPv4/IPv6 routes, IPv4 policy rules, and neighbors in
  the collector's current network namespace.
- NIC driver/firmware information, offloads, ring parameters, coalescing,
  channels, and RSS configuration where supported.
- Socket TCP/UDP memory and TCP information via `ss`.
- Selected socket/backlog settings, interrupt and softirq counters.

Before, during, and after capture:

- Numeric `/sys/class/net/<interface>/statistics` counters, including traffic,
  errors, drops, missed RX packets, and FIFO errors where exposed.
- JSON link and qdisc statistics from `ip` and `tc`.
- Raw `ethtool -S` output plus numeric driver counter fields.
- Raw softnet, protocol, CPU-pressure, and memory-pressure statistics where exposed.

The interval is a minimum gap after one sample finishes before the next begins;
command runtime adds to that gap. Initial/final samples bracket the capture, so
their counter deltas may include traffic outside the exact packet-capture window.
Counters cover the interface even when the packet filter selects only one flow.
Reported deltas are suppressed when observed samples show a reset/wrap; missing
fields remain missing. Driver-specific counter names are retained without assuming
identical meanings across hardware.

This tier has no route-change subscription: before/after snapshots can miss a
transient change. It does not record per-frame ownership, DMA completion events,
instantaneous ring occupancy, or the full state of another network namespace.
Those require subsequent event collectors or application instrumentation.

## Experiment behavior and limits

The runner ingests the configuration and samples, reports availability and
interface-counter deltas, and replays supported IPv4 traffic through the
coordinator. `environment_applied: false` is explicit: configuration is not yet
installed into simulated endpoints, and counter increases are not converted into
packet losses or delays. No host configuration is changed by replay.

Capture ends on the requested duration, packet limit, interruption, or an observed
128 MiB size threshold. The size check is best effort between samples; high rates
can overshoot. A file beyond the replay import limit (256 MiB or one million
records) fails finalization rather than becoming a default trace that cannot be
loaded. Use a narrower filter or shorter capture for high-rate interfaces. Long
captures can also reach the 64 MiB counter-log ingestion limit. Raw capture-drop
statistics are retained in `capture.stderr.log`; they are not silently treated as
zero and are not yet parsed into the normalized counter schema.

Collection changes CPU/I/O load, and this version does not quantify that overhead.
Current replay supports the same Ethernet/raw-IPv4 subset as the [capture replay
guide](trace-replay.md); a completed collection can have partial replay coverage.
Capture size validation detects changed lengths, not same-size content edits;
cryptographic bundle integrity is not implemented.

## Validation

Unprivileged tests exercise the entire collector lifecycle with a fixture-only
capture process, including graceful termination, unavailable tools, metadata
validation, and failed captures not becoming latest. CLI tests cover default and
explicit paths, incomplete bundles, version errors, reset counters, and report
write protection.

For real Linux validation, the dedicated CI job installs tcpdump/iproute2/ethtool
and runs a loopback-only UDP capture:

```sh
cargo build --locked --bins
sudo python3 scripts/capture-bundle-smoke.py
```

The smoke test checks that the resulting bundle replays both by latest selection
and by explicit path. The collector fixture tests and runner can execute on macOS;
live Linux capture is not exercised by the macOS tests.
