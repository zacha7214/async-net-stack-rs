
## Project Vision
**“A 10 k‑LOC async TCP/IP stack that moves 10 Gbps of traffic on a single core with < 2 µs per‑packet latency, using true zero‑copy buffers.”**
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
