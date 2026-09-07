# xdp-c-ref — C reference/test project for AF_XDP (zero-copy & copy)

A small, **Linux-only** C project for exercising the exact AF_XDP surface a
Rust implementation must cover, built from the canonical upstream sources
rather than written from scratch:

| File | Role | Source |
|---|---|---|
| `src/xdpsock_user.c` | Optimized user-space benchmark tool: UMEM + fill/completion/RX/TX ring management, busy-polling, batching, multi-threading, `rxdrop` / `txonly` / `l2fwd` modes, zero-copy vs copy selection, shared-UMEM mode | Linux kernel `samples/bpf/xdpsock_user.c` @ **v5.19** (last kernel release to carry it; removed in v6.0 in favor of xdp-tools/xsk-router) |
| `src/xdpsock_kern.c` + `src/xdpsock.h` | XSKMAP program with round-robin queue steering (used by `xdpsock -M`) | same |
| `src/xdp_link.c/h` | Minimal netlink (`RTM_SETLINK`/`RTM_GETLINK` + `IFLA_XDP`) attach/detach/query — replaces the `bpf_xdp_*` helpers removed from libbpf 1.0; also a C reference for the netlink path in Rust | written for this project |
| `src/xdp_caps.c` | **Feature-support prober**: zero-copy vs copy (bind probe), `XDP_USE_NEED_WAKEUP`, unaligned UMEM chunks, `XDP_OPTIONS` confirmation, queue count, driver, attached program | written for this project |
| `src/xdp_filter_kern.c` | Demo BPF program with runtime-switchable policy: redirect-all / drop-all / UDP-8080 steering / TCP-dport steering / pass-all | written for this project |
| `deps/xdp-tools` (built by `make deps`) | `libxdp.a` — contains **`xsk.c`/`xsk.h`**: the maintained UMEM/ring-management library, plus the embedded default XDP programs | [xdp-project/xdp-tools](https://github.com/xdp-project/xdp-tools) @ **v1.6.3** |
| `deps/libbpf` (built by `make deps`) | BPF object loading (map creation, ELF, attach infrastructure) | [libbpf/libbpf](https://github.com/libbpf/libbpf) @ **v1.5.0** |

`xdpsock_user.c` carries only 6 small patches (listed in its header comment):
the `<bpf/xsk.h>` → `<xdp/xsk.h>` include, three netlink-helper swaps, one
`#if LIBBPF_MAJOR_VERSION` guard, and a fallback define for
`XDP_UMEM_UNALIGNED_CHUNK_FLAG`. Everything else is upstream verbatim.

**Licenses:** `xdpsock_*` and `xdp_filter_kern.c`/`xdp_link.*`/`xdp_caps.c`
are GPL-2.0; `xsk.{c,h}` is (LGPL-2.1 OR BSD-2-Clause); `libbpf` is
(LGPL-2.1 OR BSD-2-Clause); `libxdp` is (GPL-2.0 OR BSD-2-Clause).

## 1. Install dependencies

Debian / Ubuntu:

```sh
sudo apt install gcc clang make git pkg-config \
     libelf-dev zlib1g-dev libpcap-dev libcap-dev
```

Fedora:

```sh
sudo dnf install gcc clang make git pkgconf \
     elfutils-libelf-devel zlib-devel libpcap-devel libcap-devel
```

`libpcap` is only needed because xdp-tools' `configure` requires it (it
supports their traffic generator), even though we only build the `libxdp`
subset. Optionally install `bpftool` (Debian/Ubuntu: `linux-tools-generic`
or the `bpftool` package; Fedora: `bpftool`) for the BPF demo below.

No distro `libbpf-dev` / `libxdp-dev` packages are needed — pinned versions
of both are cloned and built under `deps/` automatically.

## 2. Build

```sh
make            # clones + builds deps/libbpf and deps/xdp-tools/libxdp,
                # then builds build/xdpsock, build/xdp_caps and the BPF .o files
```

The first build downloads ~50 MB of git history. Everything runs as root or
with `CAP_BPF` + `CAP_NET_ADMIN` + `CAP_NET_RAW`.

## 3. Check feature support (zero-copy vs copy etc.)

```sh
# On a real NIC:
sudo ./build/xdp_caps -i eth0

# On a veth pair (zero-copy is NOT available; copy mode is — this output
# is exactly what your Rust implementation should also detect at runtime):
sudo ./build/xdp_caps -i veth0
```

Expected output shape:

```
kernel:         6.8.0-...
interface:      veth0 (ifindex 42)
driver:         virtual (no driver)
rx queues:      1
xdp program:    none attached
unaligned umem: SUPPORTED (XDP_UMEM_UNALIGNED_CHUNK_FLAG accepted)
zero-copy:      NOT SUPPORTED on this device/queue (Operation not supported)
copy mode:      WORKS (bind with XDP_COPY ok)
```

On a zero-copy-capable NIC (i40e/ice/ixgbe, mlx5, virtio_net, …) the
zero-copy line reports `SUPPORTED` instead. This bind-probe fallback
(`XDP_ZEROCOPY` → `EOPNOTSUPP`/`ENOTSUPP` → retry `XDP_COPY`) is the pattern
your Rust code should copy; DPDK's AF_XDP PMD does the same.

## 4. Run the benchmark tool (xdpsock)

Create a veth pair for rootless-of-hardware testing:

```sh
sudo ip link add xdp1 type veth peer name xdp2
sudo ip link set xdp1 up
sudo ip link set xdp2 up
```

veth has no native XDP, so always use `-S` (`--skb-mode`, generic XDP —
which also forces copy mode) on veth; on real NICs drop `-S`:

```sh
sudo ./build/xdpsock -i xdp1 -S -r            # rxdrop:   receive + count
sudo ./build/xdpsock -i xdp1 -S -t            # txonly:   transmit generated frames
sudo ./build/xdpsock -i xdp1 -S -l            # l2fwd:    swap MACs, echo back
sudo ./build/xdpsock -i eth0 -z -r            # force zero-copy (fails on unsupported drivers)
sudo ./build/xdpsock -i eth0 -c -r            # force copy mode
sudo ./build/xdpsock -i eth0 -r -B            # busy-poll (needs CONFIG_NET_RX_BUSY_POLL)
sudo ./build/xdpsock -i eth0 -r -b 64         # RX/TX batch size
sudo ./build/xdpsock -i eth0 -r -u            # unaligned chunks + hugepages
sudo ./build/xdpsock -i eth0 -r -q 1          # queue 1
sudo ./build/xdpsock -i eth0 -r -M            # shared UMEM, 2 sockets (loads xdpsock_kern.o)
```

If no XDP program is attached, `xdpsock` auto-attaches libxdp's built-in
redirect-all program (embedded in `libxdp.a`, no external `.o` needed);
with `-M` it loads `xdpsock_kern.o` from the working directory, so run
`-M` from `./` with `build/xdpsock_kern.o` alongside (i.e. run from the
project root as shown above). Ctrl-C prints per-second statistics
(pkts/s, Mpps, average packet size, app-stats like empty polls and
`copy_tx_sendtos` — the last one shows the kick-`sendto()` calls your Rust
TX path must replicate).

### Test against your Rust implementation

The natural A/B setup on one machine:

```
                     veth pair
   ┌───────────┐                  ┌──────────────────┐
   │ Rust      │  xdp2 ──── xdp1  │ xdpsock -S -l    │
   │ device    │  TX ──────────▶  │ (l2fwd: swaps    │
   │ (your     │                  │  MACs, echoes)   │
   │ impl)     │  ◀────────── RX  │                  │
   └───────────┘                  └──────────────────┘
```

1. `sudo ./build/xdpsock -i xdp1 -S -l` — C side bounces everything back.
2. Your Rust AF_XDP device binds queue 0 of `xdp2` (copy mode; attach the
   redirect-all program or use the default).
3. Rust TX → C RX ring → C TX ring → Rust RX ring. Compare C's reported
   stats against your Rust-side counters; they must agree to the packet.

`txonly` on the C side (`-t`) is a pure C-TX → Rust-RX load generator, and
`rxdrop` (`-r`) is a pure Rust-TX → C-RX sink.

## 5. BPF demos: custom packet policies (xdp_filter_kern.o)

`xdp_filter_kern.c` implements the classic "what do I do with this packet"
policies behind a `policy` map you can flip at runtime with bpftool:

| `policy[0]` | behaviour | real-world analogue |
|---|---|---|
| 0 | redirect **all** packets to the socket on `rx_queue_index` | default/queue steering |
| 1 | **drop** everything | L2 firewall |
| 2 | steer only **UDP dst port 8080** to queue 0, rest `XDP_PASS` | L4 service offload (bystander) |
| 3 | steer only **TCP dst port == policy[1]** (default 443) to queue 0 | L4 load-balancer steering (Katran-style) |
| 4 | `XDP_PASS` everything | observe-only mode |

Demo on the veth pair (generic mode, since veth has no native XDP):

```sh
# 1. give the pair IPs so we can generate traffic
sudo ip addr add 10.0.0.1/24 dev xdp1
sudo ip addr add 10.0.0.2/24 dev xdp2

# 2. load + attach the filter (xdpgeneric on veth; xdpdrv on real NICs)
sudo bpftool prog load build/xdp_filter_kern.o /sys/fs/bpf/xdp_filter type xdp
sudo bpftool net attach xdpgeneric pinned /sys/fs/bpf/xdp_filter dev xdp1

# 3. sink the steered traffic with xdpsock on queue 0
sudo ./build/xdpsock -i xdp1 -S -r -q 0 &

# 4. switch policy to UDP/8080 steering
sudo bpftool map update pinned /sys/fs/bpf/xdp_filter/maps/policy \
     key 0 0 0 0 value 2 0 0 0

# 5. UDP/8080 from the peer lands in the socket (xdpsock pkts/s rises);
#    everything else passes through to the stack:
echo -n hi | nc -u -q1 10.0.0.1 8080       # steered → xdpsock counts it
ping -c 3 10.0.0.1                          # passes → normal stack replies

# 6. try the other modes: value 0 (redirect all → ping stops replying,
#    xdpsock counts everything), value 1 (drop all), value 4 (pass all)

# cleanup
sudo bpftool net detach xdpgeneric dev xdp1
sudo rm -f /sys/fs/bpf/xdp_filter
```

## 6. Mapping to your Rust implementation

| C (this project) | Rust equivalent you are building |
|---|---|
| `xsk.c: xsk_umem__create` | mmap arena + `setsockopt(XDP_UMEM_REG)` → your `FramePool::from_raw_parts` |
| `xsk.c: xsk_socket__create` + `bind()` flags | socket setup; probe `XDP_ZEROCOPY`, fall back to `XDP_COPY` |
| ring prod/cons ops in `xsk.h` | shared-memory ring indices with acquire/release ordering |
| `kick_tx()` in `xdpsock_user.c` | `sendto(fd, NULL, 0, MSG_DONTWAIT)` kick + `needs_wakeup` check |
| `xdp_caps.c` probes | your runtime capability detection (driver, queues, umem flags) |
| `xdp_link.c` netlink attach | your libc-only attach path (no libxdp in Rust) |
| `xdp_filter_kern.c` XSKMAP + policy map | the "what to ignore" BPF program your loader must install |
| `xdpsock -r/-t/-l` | your `Device::recv`/`Device::send` A/B targets |

## 7. Caveats

* I built this tree on macOS and could not compile-test it; if the first
  `make` on your Linux box trips over something, it will be a one-line fix —
  paste the error.
* Kernel ≥ 5.4 recommended (unaligned chunks, `IFLA_XDP_EXPECTED_FD`);
  `xdp_caps` reports exactly what your kernel supports.
* Zero-copy requires driver + queue support; copy mode works everywhere.
* Busy-poll needs `CONFIG_NET_RX_BUSY_POLL` and the `-B` flag.
* The `-R` (reduced-capabilities) demo needs the companion
  `xdpsock_ctrl_proc` control process from the kernel tree, which is not
  shipped here; ignore `-R`.
* Modern successors to `xdpsock` in xdp-tools: `xdp-bench`, `xdp-trafficgen`
  and `xdp-forward` (flow-table based router) — buildable from the same
  `deps/xdp-tools` checkout with `make -C deps/xdp-tools` if you want more.
* Background reading: [ebpf-docs AF_XDP concept page](https://github.com/isovalent/ebpf-docs/blob/main/docs/linux/concepts/af_xdp.md),
  [kernel AF_XDP docs](https://docs.kernel.org/networking/af_xdp.html),
  [xdp-tutorial](https://github.com/xdp-project/xdp-tutorial).
