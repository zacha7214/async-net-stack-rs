# Benchmarking

The [Mac shared-memory lab](mac-shared-memory.md#runnable-example-shm_nic) adds
two-process copy-cost and notification benchmarks. Use `shm-nic-sweep.py` for
randomized repetitions and per-process CPU accounting. Its memory-only rates are
not directly comparable to the actual-device measurements below.

## Software-only baselines

```sh
cargo bench --locked --bench throughput -- --save-baseline first
# After a change, compare an identical test/filter:
cargo bench --locked --bench throughput -- --baseline first
cargo bench --locked --bench throughput -- pool_batch_api
cargo bench --locked --bench throughput -- batched_copy_pipeline
```

`alloc_recycle` preserves the original per-frame allocator benchmark.
`pool_batch_api` compares single allocation to batching at 1/8/32/64/256 frames.
`loopback_roundtrip` preserves the old workload, including payload initialization
and a full-byte checksum-like scan. Its byte count includes both copy directions.
`batched_copy_pipeline` avoids the full scan and reports each delivered payload
once; its throughput units must not be compared directly with the legacy test.
All are CPU/memory models, not evidence of NIC throughput or network latency.

## Actual devices

Build once; do not include compilation or sudo authentication in measurements.
Keep the generator on a separate CPU or machine and record its packet count.

```sh
cargo build --locked --release --features xdp,io_uring --examples
mkdir -p results
./scripts/vm-net-info.sh enp0s1 > results/environment.txt
sudo ./target/release/examples/device_bench --backend xdp --iface enp0s1 \
  --action rx --mode copy --batch 64 --cpu 2 --warmup 3 --seconds 15 > results/xdp-copy.json
# Repeat with --mode zero-copy (strict; must fail if unsupported).
# Repeat with --generic --mode copy to isolate generic/skb overhead.
```

Use the dedicated NIC's actual name and an allowed CPU ID. For XDP transmit:

```sh
sudo ./target/release/examples/device_bench --backend xdp --iface enp0s1 \
  --action tx --mode copy --batch 64 --size 1500 --warmup 3 --seconds 15 \
  --mac 02:00:00:00:00:02 --peer-mac 02:00:00:00:00:01
```

TX uses complete software-checksummed IPv4/UDP frames. `--size` includes Ethernet
for XDP and starts at IP for TUN/loopback; it excludes FCS, preamble and IFG. The
current CLI caps it at `--mtu`. Use the real source and peer MACs. `--queue` must
match the RX queue selected by hardware/RSS. Start with one combined queue in a
VM. The code handles one queue and installs one private redirect program.

Compare TUN copying backends on the same Linux guest and interface settings:

```sh
sudo ./target/release/examples/device_bench --backend tun --iface tun0 \
  --action reply --ip 10.9.0.2 --warmup 10 --seconds 20 > results/tun.json
# While running: sudo ip addr add 10.9.0.1/24 dev tun0; sudo ip link set tun0 up
# In another terminal: ./target/release/examples/udp_load 10.9.0.2:9000

# Stop the first run before opening the same TUN name again.
sudo ./target/release/examples/device_bench --backend uring --iface tun0 \
  --action reply --ip 10.9.0.2 --uring-depth 128 --rx-depth 64 \
  --warmup 10 --seconds 20 > results/uring.json
```

Opening the nonpersistent TUN device recreates it, so configure its address/link
again for each run. For a pure io_uring TX experiment use `--action tx --rx-depth
0`. RX depth is independently bounded and must leave both request and pool
capacity for TX. Unsupported io_uring setup/cancellation fails explicitly; it
does not silently fall back to the plain path.

On macOS, `--backend tun` opens a fresh utun and prints its assigned name. Use
`sudo ifconfig utunN 10.9.0.1 10.9.0.2 up`, then the same UDP client.
`--cpu` is rejected on macOS. The older `tun_bench --mode plain|reactor` is an
RX-drop comparison with a different idle policy; prefer device_bench for results.

## Reading JSON

* `measurement.rx_packets/rx_bytes` count application delivery during the timed
  interval. RX is a layer-2 count on XDP and a layer-3 count on TUN.
* `tx_accepted` is submitted/accepted, not proof of delivery. Inspect completion
  deltas, `pending_tx_at_end`, `pending_tx_after_drain`, and the peer's counts.
* `before` and `after` are cumulative backend snapshots at the measurement
  boundaries. Subtract numeric counters; `after_drain` additionally includes a
  bounded 250 ms completion drain. Warmup work already in flight may complete
  during measurement. Start peer load before the measurement window.
* `short_sends` counts backpressure encounters, not packet loss. Unsent suffixes
  are retained/retried during a run; `unsent_at_end` records the final remainder.
* `--latency` samples the first 65,536 batch-service times after warmup. The clocks add overhead;
  these percentiles are **not network or per-packet latency**. The optional
  windowed UDP client reports actual echo RTT (both directions + responder),
  loss at the end, duplicate/invalid replies, and request expirations separately.

The default busy loop favors throughput and consumes a core. Compare
`--idle-us 50` separately for power/latency tradeoffs. The ordinary UDP generator
can become the bottleneck; verify offered load before attributing a ceiling to
AF_XDP. It is not a TCP/iperf server, and UDP echo doubles packet work.

## Comparison matrix and profiling

Sweep batch 1/8/32/64/128/256, packet sizes 64/512/1500, and RX ring depths
256/512/1024/2048. Hold other variables constant; run five repetitions and report
median plus spread. Record guest/host CPU placement, frequency governor, kernel,
QEMU/vhost versions, virtio features, IRQ affinity, offloads, MTU, queue counts,
copy mode, RSS rules and coalescing. Save receiver and generator JSON together.
Report one-way payload Gbps and packets/s; derive Ethernet wire rate separately.

```sh
sudo perf stat -e cycles,instructions,cache-misses,context-switches,cpu-migrations \
  ./target/release/examples/device_bench --backend xdp --iface enp0s1 \
  --action rx --mode copy --cpu 2 --batch 64 --warmup 3 --seconds 15
sudo strace -c -e io_uring_enter,read,write,poll,sendto \
  ./target/release/examples/device_bench --backend uring --iface tun0 \
  --action rx --rx-depth 64 --warmup 10 --seconds 15
```

Use syscall tracing to diagnose call counts, not to publish throughput: tracing
changes timings. Kernel driver statistics and perf may need guest PMU exposure.
Derive io_uring requests/enter from counter deltas; do not assume one syscall
per batch when completions or partial submissions require extra progress calls.
