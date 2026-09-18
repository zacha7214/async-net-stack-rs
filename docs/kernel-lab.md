# The QEMU kernel lab: build, boot and debug early ARM64 boot code

This lab builds a small, purpose-made ARM64 kernel plus a self-contained
initramfs, exports them as a single **bundle**, and boots that bundle under QEMU
on your Mac. No disk image, no distro install and no root on the host are
involved, and nothing in this checkout is compiled by the build step: the kernel
and the guest receiver are both built inside a Multipass ARM64 Linux VM.

Two independent things can be done with a bundle:

* **Debug the kernel**, including the early architecture code that runs with the
  MMU off. Needs only a stock `qemu-system-aarch64`. Start here.
* **Run the AF_XDP / vhost-user data path** described in
  [vhost-user-lab.md](vhost-user-lab.md). That additionally needs the patched
  QEMU from that document and the host `vhost_user_net` backend.

## The pieces

| File | Runs on | Purpose |
| --- | --- | --- |
| `scripts/kernel-lab-doctor.py` | host | Check every prerequisite here and in the VM; changes nothing |
| `scripts/multipass-kernel-lab.py` | host | Copy the tree into the VM, build there, export the bundle here |
| `scripts/build-kernel-lab.py` | ARM64 Linux | The actual build; normally invoked by the driver above |
| `scripts/kernel-lab.config` | — | The Kconfig fragment defining the lab kernel |
| `scripts/kernel-lab-init.sh` | guest | PID 1 inside the initramfs |
| `scripts/run-kernel-lab.py` | host | Boot a bundle under QEMU |
| `scripts/debug-kernel-lab.py` | host | Emit/run the LLDB or GDB command sequence |

## 1. Check prerequisites

```sh
cd /path/to/async-net-stack-rs
python3 scripts/kernel-lab-doctor.py --instance tito-burrito
```

Every `FAIL` line prints the command that fixes it. `warn` lines only narrow what
you can run; a missing `vhost_user_net` backend, for example, still leaves the
whole debugging path available. The checks that matter most:

* The builder VM must be **ARM64 Linux** — the kernel is built natively, not cross-compiled.
* `busybox` must be **statically linked** (`busybox-static`). The initramfs
  contains no shared libraries other than those the receiver itself needs.
* The kernel source tree must have **no in-tree `.config`**. The lab builds
  out-of-tree with `O=`; run `make mrproper` in that tree once if it is dirty.
* Python **3.11+** on the host, for `hashlib.file_digest`.

## 2. Build the bundle

```sh
python3 scripts/multipass-kernel-lab.py \
  --instance tito-burrito \
  --source /home/ubuntu/arm64_dev_kernel \
  --output results/kernel-lab
```

The driver copies `Cargo.toml`, `Cargo.lock`, `src/`, `examples/`, `benches/`,
`tests/` and `scripts/` into `--work` inside the VM (default
`/home/ubuntu/async-net-kernel-lab`) and builds there. It refuses to use a work
directory it does not own, marking its own with `.async-net-kernel-lab`. Add
`--force` to replace an existing `--output`, and `--export-only` to re-download
the last completed bundle without rebuilding.

The build is incremental: the `build/` and `cargo-target/` directories persist in
the VM between runs, so a second build after editing the config is far shorter
than the first. The guest receiver is compiled *before* the kernel, so a Rust or
manifest error is reported in seconds rather than after a full kernel compile.

A finished bundle contains:

| Entry | Purpose |
| --- | --- |
| `Image` | The kernel QEMU boots with `-kernel` |
| `vmlinux` | Unstripped ELF with DWARF; the debugger's symbol source |
| `System.map`, `config` | Symbol table and the exact `.config` used |
| `initramfs.cpio.gz` | busybox + `xdp_vm_rx` + `kernel-lab-init.sh` as `/init` |
| `source/` | `arch/arm64`, `init`, `include`, `scripts/gdb` for source-level stepping |
| `manifest.json` | Kernel release, cmdline, early-boot symbol addresses, SHA-256 of every file |
| `source-changes.patch` | `git diff HEAD` of the kernel tree, so a modified tree stays reproducible |

`run-kernel-lab.py` verifies the SHA-256 of `Image`, `initramfs.cpio.gz`,
`vmlinux` and `config` against the manifest before booting, so a half-copied
bundle fails immediately instead of booting something unexpected.

## 3. Boot it

Plain boot to an interactive shell, no networking, stock QEMU:

```sh
python3 scripts/run-kernel-lab.py \
  --bundle results/kernel-lab \
  --qemu "$(command -v qemu-system-aarch64)" \
  --no-lab-nic
```

You should see `KERNEL_LAB_READY <release>` and a busybox prompt on the serial
console. Quit QEMU with `Ctrl-A x`.

`--no-lab-nic` is the important flag for kernel work: it drops the vhost-user
backend, the shared-RAM object and the lab NIC, and runs QEMU directly. Without
it, `run-kernel-lab.py` starts `vhost_user_net`, boots through
`launch-vhost-vm.py`, and expects the patched QEMU.

## 4. Debug the early architecture code

This is the part the config is specifically arranged for. The relevant settings
in `scripts/kernel-lab.config`:

| Setting | Why |
| --- | --- |
| `CONFIG_DEBUG_INFO_DWARF4=y` | DWARF4 is what LLDB reads most reliably; DWARF5 is not used |
| `# CONFIG_DEBUG_INFO_REDUCED/SPLIT is not set` | Full DWARF in one `vmlinux`, no separate `.dwo` files |
| `# CONFIG_RANDOMIZE_BASE is not set`, `nokaslr` | The load address is fixed, so a single slide is valid for the whole run |
| `# CONFIG_MODULES is not set` | One static image; no module symbol loading |
| `CONFIG_KALLSYMS_ALL=y`, `CONFIG_GDB_SCRIPTS=y` | Local early symbols stay visible; `scripts/gdb` is exported |
| `CONFIG_FRAME_POINTER=y`, `CONFIG_STACKTRACE=y` | Usable backtraces before unwind tables are live |

Start QEMU stopped at reset with a loopback-only GDB stub:

```sh
python3 scripts/run-kernel-lab.py \
  --bundle results/kernel-lab \
  --qemu "$(command -v qemu-system-aarch64)" \
  --no-lab-nic --debug
```

`--debug` implies a single TCG CPU (`--accel tcg`), which is what makes
single-stepping through MMU-off code sane. In a second terminal:

```sh
python3 scripts/debug-kernel-lab.py --bundle results/kernel-lab --debugger lldb
```

Use `--debugger gdb` if you have a cross GDB (`brew install aarch64-elf-gdb`,
then `--debugger gdb`); Apple's LLDB is the default because it ships with the
command line tools. `--emit-only` prints the command sequences instead of
launching anything, which is what you want if you prefer to drive the debugger
yourself or paste the commands into an IDE.

### Why symbols have to move

This is the part that makes early ARM64 debugging awkward, and it is worth
understanding rather than working around.

`vmlinux` records **virtual** addresses (`_text` is up in `0xffff…`). But
`primary_entry` runs with the **MMU off**, so the CPU is executing at *physical*
addresses. QEMU, for `-machine virt` with a raw `-kernel` Image, places the image
per `hw/arm/boot.c`:

```
physical = 0x40000000 + text_offset + (2MiB if text_offset < 4KiB)
```

Modern ARM64 kernels set `text_offset` to 0, so the image lands at
`0x40200000`. `debug-kernel-lab.py` reads the 64-byte ARM64 Image header out of
the bundle, recomputes this itself rather than assuming, and derives:

```
delta = physical - _text
```

It then loads `vmlinux` **slid by `delta`**, so virtual symbols resolve to the
physical addresses the CPU is actually using. That is the `entry.commands` file:
it attaches, slides the symbols, sets a hardware breakpoint on `primary_entry`,
continues, and shows you the registers and the next eight instructions.

Breakpoints here must be **hardware** breakpoints (`hbreak` / `--hardware`). A
software breakpoint would need to write into memory that is still read-only ROM
from QEMU's point of view at that stage.

From there, `si` steps the early assembly. Landmarks, all exported into
`manifest.json` by the build so you can break on them directly:

| Symbol | What it is |
| --- | --- |
| `primary_entry` | First kernel instruction; the boot CPU arrives here from QEMU's stub |
| `record_mmu_state`, `preserve_boot_args` | Capture the state the bootloader handed over |
| `init_kernel_el` | EL2 → EL1 decision and setup |
| `__cpu_setup` | MAIR/TCR/SCTLR programming, in `arch/arm64/mm/proc.S` |
| `__primary_switch`, `__enable_mmu` | Install page tables and turn the MMU on |
| `__primary_switched` | **First code running at virtual addresses** |
| `start_kernel`, `setup_arch` | Generic and arch-specific C init |

### Crossing the MMU handoff

Once `__enable_mmu` has done its work the slide is no longer correct — the CPU is
now at virtual addresses, which is what `vmlinux` natively describes. So the
debug helper emits a **second** command file that reloads `vmlinux` at slide 0
and breaks on `__primary_switched`, `start_kernel` and `setup_arch`. Its path is
printed when the helper starts; source it at the right moment:

```
(lldb) command source /tmp/kernel-debug-XXXX/virtual.commands
(gdb)  source /tmp/kernel-debug-XXXX/virtual.commands
```

The rule is simply: **slid symbols before `__enable_mmu`, unslid symbols after.**
If you continue past the MMU switch while still slid, backtraces and source
lines will be wrong in a way that looks like a kernel bug and is not.

Source-level stepping works because the bundle carries `source/`, and both
command files map the VM's build path onto it (`settings set
target.source-map` for LLDB, `set substitute-path` for GDB).

## 5. Run the XDP data path

Once the kernel boots and you want the actual networking experiment, drop
`--no-lab-nic`, use the patched QEMU, and build the host backend first:

```sh
cargo build --locked --release --example vhost_user_net
python3 scripts/run-kernel-lab.py \
  --bundle results/kernel-lab \
  --qemu /path/to/patched/qemu-system-aarch64 \
  --mode zero-copy --packets 10000 --pps 1000
```

`--mode zero-copy` or `--mode copy` makes `kernel-lab-init.sh` run `xdp_vm_rx`
against the lab NIC (MAC `02:00:00:00:00:02`) non-interactively, power the guest
off, and then runs `check-vhost-run.py` over the host and guest JSON logs.
`--mode shell` leaves you at a prompt to drive it by hand. See
[vhost-user-lab.md](vhost-user-lab.md) for what the evidence means and why the
QEMU patches are required.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| `can't find 'throughput' bench` | The VM's cargo needs every declared target path; the driver ships `benches/` and `tests/` for this reason |
| `Kconfig did not enable required options: [...]` | A symbol's dependencies are unmet in this kernel version; add them to `scripts/kernel-lab.config` |
| `source has an in-tree .config` | Run `make mrproper` in the kernel tree; the lab builds with `O=` |
| `install busybox-static first` | The packaged busybox is dynamically linked and would not run in the initramfs |
| `work directory is not owned by this helper` | `--work` points at a directory the lab did not create; choose another |
| `bundle checksum mismatch` | Incomplete copy; re-export with `--export-only` |
| `build host backend: cargo build ...` | Use `--no-lab-nic`, or build `vhost_user_net` on the host |
| Debugger connects but symbols are nonsense | You are on the wrong side of the MMU handoff; see above |
| Build killed during link | Lower `--jobs`; DWARF linking is memory-hungry |
