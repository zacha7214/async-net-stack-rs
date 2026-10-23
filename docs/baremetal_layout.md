# Bare-metal layout: shared runtime and board crates

`baremetal/` is its own Cargo workspace, separate from the host workspace so
guest `panic = "abort"` and linker settings never reach host tools. It holds one
board-independent runtime crate and one thin crate per board:

```
baremetal/
  Cargo.toml          workspace + shared profiles; one Cargo.lock and target/
  runtime/            async-net-baremetal-runtime: #![no_std] + alloc, host-tested
  qemu-virt/          QEMU virt board: virtio regression target (net-boot)
  rpi4/               Raspberry Pi 4 board: runtime bring-up only (rpi4-boot)
```

The rule for deciding where code goes: **if it would be wrong on another board
with the same architecture, it belongs to the board crate.** Addresses, clock
rates, memory sizes, entry-state checks and the scenario run by `kernel_main`
are board facts. Drivers, allocators, ownership machinery and AArch64 startup are
parameterized by those facts and live in the runtime.

## What lives where

| Concern | Runtime (`baremetal/runtime`) | Board crate |
| --- | --- | --- |
| Entry | `arch/aarch64/entry.S`: stack, BSS, fatal vectors, call `kernel_main(x0)` | `src/boot.S` `_start`: core parking, EL checks/drop, MMU state checks |
| Memory layout | `link/runtime-sections.ld`: section order and symbol contract | `linker.ld`: `ENTRY`, `MEMORY { RAM }`, the three sizes, `INCLUDE` |
| Heap | `memory::heap::LockedHeap`, `memory::heap_region()` | `#[global_allocator]` static and its `WORDS` |
| Pages / DMA | `memory::page`, `memory::dma::DmaAllocator`, `memory::page_pool_region()` | Whether the pool is coherent, and its CPU→device address mapping |
| Console | `pl011::Uart` + `pl011::Config` (base, clock, baud) | The `Config` constant; `console()` for panic/exception paths |
| Timer | `arch::aarch64::timer` (generic counter) | Nothing, unless firmware leaves `CNTFRQ_EL0` unset |
| Virtio | `virtio::{mmio, split, net, rng}`, `virtio::probe::find(Window, Kind)` | The `Window` of MMIO slots, if the board has virtio at all |
| Self-tests | `selftest::{heap, clock, pages, dma}` print the `PASS ...` markers | Which ones to run, and in what order |
| Panic / exceptions | Vectors call `exception_fatal(esr, elr)` | `#[panic_handler]` and `exception_fatal`, using the board console |

The runtime never names a board. Nothing in it is gated on a board feature.
Some modules are gated on architecture (`arch::aarch64`, `selftest`, `virtio::mmio::Mmio`),
so the remaining host tests run on any build machine.

## The board contract

A board crate supplies exactly these:

1. **`_start` in `.text.boot`.** It must leave core 0 at EL1 with interrupts
   masked and the MMU in the state the board documents, then `b __runtime_start`
   with `x0` unchanged. Other cores branch to `__runtime_park`.
2. **`linker.ld`.** Declares `ENTRY(_start)`, a `MEMORY` region named `RAM`, plus
   `__stack_size`, `__heap_size` and `__page_pool_size`, then
   `INCLUDE runtime-sections.ld`. The runtime's build script puts that file on
   the linker search path. The sizes must agree with the board's
   `LockedHeap<WORDS>` (64 B granules × 64 per word) and page-allocator `WORDS`
   (64 pages per word). Initialization fails loudly if they don't.
3. **`build.rs`.** Passes `-T linker.ld` to its binaries.
4. **`kernel_main(boot_arg: usize) -> !`, `exception_fatal(esr: u64, elr: u64) -> !`
   and a `#[panic_handler]`.** All three are `#[no_mangle]` where applicable.
5. **A DMA domain decision.** `DmaAllocator::new` requires a coherent, no-IOMMU
   domain (see `memory::dma`). "Coherent" means the device sees CPU writes, and
   the CPU sees device writes, without any cache maintenance. Three setups
   qualify: caches/MMU off (both boards today), a Normal Non-cacheable mapping of
   the pool, or hardware-coherent DMA. A board that wants cacheable DMA buffers
   needs a `DmaMemory` implementation that cleans and invalidates cache lines.

### Adding a board

Copy `rpi4/` (the smaller of the two), then:

- Rename the package and binary, and add the directory to `members` in
  `baremetal/Cargo.toml`.
- Set the RAM origin and sizes in `linker.ld` and the addresses in `src/board.rs`.
- Adapt `_start` to the firmware's hand-off: entry EL, which cores run, and where
  the boot argument is passed.
- Run the self-tests first. Bring up the NIC only once those pass.

A new NIC driver belongs in the runtime if another board could use it, such as
a MAC IP block that appears on several SoCs. Board-only glue stays in the board
crate: clocks, pin muxing, firmware mailbox calls and PHY reset GPIOs. Either
way, the driver implements the portable `Device` trait over a `DmaAllocator`,
exactly as `virtio::net::VirtioNet` does.

## Boards

**QEMU virt (`qemu-virt`, binary `net-boot`).** This is the regression target.
`scripts/qemu-virt-net.py --smoke` runs the runtime host tests. It then boots the
modern, short-RNG, no-RNG, absent-NIC and legacy-NIC configurations with byte-exact
UDP echo, plus the allocation-failure build. Contract: EL1 entry, MMU and caches
off, virtio at QEMU's fixed 32-slot window. See the [virtio plan](virtio-net-plan.md).

**Raspberry Pi 4 (`rpi4`, binary `rpi4-boot`).** Runtime bring-up only.
`_start` parks secondary cores and drops from EL2 to EL1. It also enables EL1
counter access and preserves the DTB pointer. The board then runs the shared
self-tests on PL011 UART0 (`0xFE201000`, 48 MHz).
`scripts/rpi4-qemu-boot.py` builds the raw `kernel8.img` and boots it on QEMU
`raspi4b` (QEMU 9.0+, so not run in CI). The raw image matters: QEMU starts an
ELF at EL3 on all cores, but gives a raw image firmware-like EL2 entry. For real
hardware, copy `kernel8.img` to the boot partition and add `arm_64bit=1`,
`enable_uart=1` and `dtoverlay=disable-bt` to `config.txt`.

## Known limits

- **Untested on Pi 4 silicon.** With the MMU off, every data access is Device
  memory. The heap lock's exclusive load/store may never succeed there, which
  QEMU does not model. Enabling the MMU (identity map: RAM cacheable, peripherals
  Device-nGnRE, DMA pool Non-cacheable) is the next Pi milestone. It keeps the
  coherent `DmaAllocator` contract valid without cache maintenance.
- `DmaAllocator` uses a `PageAllocator<16>`, so a DMA pool larger than 4 MiB
  needs that bound lifted.
- Each board repeats its ~15-line panic and exception handlers, because they
  depend on the board console. Factor them out if a third board would copy them
  unchanged.
- The device tree is not parsed yet. `kernel_main` receives the pointer, and
  boards still hard-code addresses.

