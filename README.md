
# Project Vision
---
**A 10 k‑LOC async TCP/IP stack that moves 10 Gbps of traffic on a single core with < 2 µs per‑packet latency, using true zero‑copy buffers.**
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
   Packet Buffers (Bytes/Arc)                |
             |                                 |
+------------v--------------+   +--------------v--------------+
|  Network Layer (IP)       |   |  ARP, NDP, Routing tables   |
+------------+--------------+   +--------------+--------------+
             |                                 |
   Zero‑copy Device driver (AF_XDP, DPDK, or TAP) |
             |                                 |
+------------v--------------+   +--------------v--------------+
|  Physical NIC (or Virtual)                     |
+-------------------------------------------------+
```
* **Zero‑copy** is achieved by **never cloning the payload**. A packet lives in a **reference‑counted buffer** (`Arc<[u8]>` or `bytes::Bytes`) that is handed from the driver → IP → TCP → application. The driver returns the buffer to the pool when the future resolves.

* **Async I/O** is built on **Tokio’s `Poll`/`Waker`** model. The driver registers its Rx/Tx queues with a **`mio::Poll`** (or Tokio’s reactor) and wakes the corresponding future when a new packet arrives or a Tx slot opens.

* **Performance tuning** knobs (MTU, NUMA, off‑load) are exposed as **runtime configuration** (env vars, CLI flags, or a tiny JSON/YAML file).
---
