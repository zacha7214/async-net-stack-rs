
# Project Vision
---
**A WIP async TCP/IP stack that aims to move 10 Gbps of traffic on a single core with < 2 µs per‑packet latency, using true zero‑copy buffers.
A secondary goal, is to reach line rate with non zero copy workloads on home networks (any NIC), and identify via the resulting benchmarks which existing linux kernel drivers can support zero-copy, but have not been backported. In those cases, if I believe the evidence shows that doing so could helpful, I plan to use the data (and lessons) learned from completing this project to implement zero copy in those drivers myself. **
---

# Phase 1
---
**Setup Buffer Pool that will pre-allocate the packet pool + indices list for TAP devices, or provide an mmap ptr for zero-copy backends.
Set up tap device with benchmarks and samples to establish a baseline for which future devices can be tested against.**
---
## Phase 2
---
**Setup XDP backend on linux, /dev/bpf on macOS.**
---
## Phase 3
---
**Production quality XDP backend on linux, /dev/bpf on macOS. Full benchmarks for comparison on different well support zero-copy NICs.
The long term goal is possibly to extend support for zero copy NICs, with the ability to document the state of existing ones. Learning and improving is always a big part as well, thus I am going to avoid using existing crates for the networking backends. The idea is to really understand whats going on, and maybe provide something different enough to be useful to others.**
---


## High‑Level Architecture

```
+---------------------------+   +-----------------------------+
|  Application (async API)  |   |  Benchmark / Test Harness   |
+------------+--------------+   +--------------+--------------+
             |                                 |
   async I/O (Futures, Wakers)                |
             |                                 |
+------------v--------------+   +--------------v--------------+
|  Transport Layer (TCP)    |   |  UDP, ICMP, Raw sockets      |
+------------+--------------+   +--------------+--------------+
             |                                 |
   Packet Buffers (Bytes/Arc)                  |
             |                                 |
+------------v--------------+   +--------------v--------------+
|  Network Layer (IP)       |   |  ARP, NDP, Routing tables   |
+------------+--------------+   +--------------+--------------+
             |                                 |
   Zero‑copy Device driver(AF_XDP, DPDK,       |
                            or TAP)            |
             |                                 |
+------------v--------------+   +--------------v--------------+
|  Physical NIC (or Virtual)                         |
+-------------------------------------------------+
```
* **Zero‑copy** is achieved by **never cloning the payload**. A packet lives in a **reference‑counted buffer** (`Arc<[u8]>` or `bytes::Bytes`) that is handed from the driver → IP → TCP → application. The driver returns the buffer to the pool when the future resolves.

* **Async I/O** is built on **Tokio’s `Poll`/`Waker`** model. The driver registers its Rx/Tx queues with a **`mio::Poll`** (or Tokio’s reactor) and wakes the corresponding future when a new packet arrives or a Tx slot opens.

* **Performance tuning** knobs (MTU, NUMA, off‑load) are exposed as **runtime configuration** (env vars, CLI flags, or a tiny JSON/YAML file).
---
