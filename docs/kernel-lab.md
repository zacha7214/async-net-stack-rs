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
| `scripts/qemu-lab.py` | host | Get a patched, signed lab QEMU, then pick and launch a test |
| `scripts/kernel-lab-doctor.py` | host | Check every prerequisite here and on the builder; changes nothing |
| `scripts/kernel-lab-build.py` | host | Copy the tree to the builder, build there, export the bundle here |
| `scripts/lab_builders.py` | host | The builders: local, ssh, docker, multipass |
| `scripts/kernel-lab.Dockerfile` | — | The container image `--builder docker` builds on first use |
| `scripts/build-kernel-lab.py` | ARM64 Linux | The actual build; normally invoked by the driver above |
| `scripts/kernel-lab.config` | — | The Kconfig fragment defining the lab kernel |
| `scripts/kernel-lab-init.sh` | guest | PID 1 inside the initramfs |
| `scripts/run-kernel-lab.py` | host | Boot a bundle under QEMU |
| `scripts/debug-kernel-lab.py` | host | Emit/run the LLDB or GDB command sequence |

## 0. One command from a fresh checkout

`scripts/qemu-lab.py` is the front door. It does the parts that are otherwise
transcribed by hand from [vhost-user-lab.md](vhost-user-lab.md) — clone QEMU at
the known-good tag, apply `patches/`, configure and build `aarch64-softmmu`,
code-sign it for HVF — and then either tells you how to build a kernel bundle or
offers the tests that your bundle supports:

```sh
python3 scripts/qemu-lab.py
```

Defaults: `./qemu` if that is already a checkout, otherwise `~/qemu`; tag
`v11.0.1`; build directory `build-mac-vhost` on macOS and `build-lab` on Linux.
`--qemu-src`, `--ref` and `--build-dir` override each of those, `--build-only`
stops after QEMU is verified, and `--dry-run` prints every command without
running it.

Every stage is idempotent, so rerunning after a failure repeats only what is
still missing. An existing checkout is reused rather than re-cloned; a patch that
is already applied is detected by a reverse-apply check and skipped; an existing
build directory keeps its configuration unless you pass `--reconfigure`; and a
binary that QEMU's own build already signed is left alone.

The build is configured headless — no Cocoa, PVG, VNC, GTK, SDL or curses. The
lab always runs with `-display none`, so none of it is used, and leaving it out
keeps the build off host frameworks that move between SDK releases. A
`ParavirtualizedGraphics` link failure in a build directory configured months ago
is the usual way a Mac rebuild breaks; `--reconfigure` is the fix, and the script
says so when ninja fails in a directory it did not just configure.

Each build writes `qemu-lab.json` beside the binary recording the source commit,
the patches applied, the configure line and the signing result. That is what
later runs read to tell a patched lab QEMU from a stock one, because nothing in
the binary itself reveals whether the queue-reset patch is in. Passing `--qemu`
at an arbitrary binary skips the build; without a `qemu-lab.json` beside it the
data-path tests are not offered.

With no bundle at `results/kernel-lab`, the script looks for one in the Multipass
builder (`--instance`, `--work`) and offers to pull it: `--export` to skip the
prompt, `--no-export` to never touch the builder. Only the bundle is copied. Do
not `multipass transfer -r` the whole work directory — that is the kernel build
tree and cargo target as well, several times the size of what you need. If there
is no finished bundle either, the script stops and prints the steps in sections 1
and 2 below. With a bundle, it checks the manifest checksums and offers:

| Test | Needs | What it is |
| --- | --- | --- |
| Boot to a busybox shell | bundle | Section 3; works with stock QEMU too |
| Debug early ARM64 boot | bundle | Section 4, paused at reset with a GDB stub |
| AF_XDP zero-copy data path | bundle, patched QEMU | Section 5, the actual experiment |
| Forced-copy control | bundle, patched QEMU | The control that gives the address match meaning |
| Zero-copy then copy | bundle, patched QEMU | Both, back to back, against the same build |
| Host protocol smoke test | nothing | `vhost-user-smoke.py`; no VM, no root, seconds |

Before each one it prints where the results land and what to look for in them,
then launches it through `run-kernel-lab.py` with the right accelerator (HVF or
KVM where available, TCG for the debug session). `--test NAME` skips the menu for
unattended use; `--list-tests` names them.

The rest of this document is what the script automates, and what to read when a
step fails.

## 1. Choose a builder and check it

The kernel is built natively on ARM64 Linux, not cross-compiled, because the
kernel, the initramfs and the guest receiver are built together. Where that
happens is `--builder`, understood by `kernel-lab-doctor.py`, `kernel-lab-build.py`
and `qemu-lab.py` alike:

| `--builder` | Needs | Use it when |
| --- | --- | --- |
| `multipass` (default) | `--instance` | You already keep a Multipass VM |
| `ssh` | `--ssh [USER@]HOST` | Any other VM, a Pi, a cloud instance |
| `docker` | a container runtime | You would rather not maintain a VM |
| `local` | ARM64 Linux here | You are already on the right machine |

```sh
cd /path/to/async-net-stack-rs
python3 scripts/kernel-lab-doctor.py --builder ssh --ssh kernel-box \
  --source /home/you/linux
```

Every `FAIL` line prints the command that fixes it. `warn` lines only narrow what
you can run; a missing `vhost_user_net` backend, for example, still leaves the
whole debugging path available. The checks that matter most:

* The builder must be **ARM64 Linux** — the kernel is built natively, not cross-compiled.
* `busybox` must be **statically linked** (`busybox-static`). The initramfs
  contains no shared libraries other than those the receiver itself needs.
* The kernel source tree must have **no in-tree `.config`**. The lab builds
  out-of-tree with `O=`; run `make mrproper` in that tree once if it is dirty.
* Python **3.11+** on the host, for `hashlib.file_digest`.

A builder owes the driver only two things — run a command, and copy a finished
directory back — so `scripts/lab_builders.py` is where a new one goes, and the
build logic never learns about it.

### `--builder ssh`

The target is whatever `ssh` accepts, so put the host in `~/.ssh/config` and make
sure key authentication succeeds without a prompt; the builder sets `BatchMode`,
so a password prompt is a failure rather than a hang. `--ssh-option` passes extra
`-o` settings, repeatably. The bundle comes back through `tar` over the
connection, which preserves modes and symlinks in one round trip.

This one backend covers UTM, Parallels, VMware, VirtualBox, Lima
(`limactl show-ssh`), a Raspberry Pi and an Ampere or Graviton instance.

### `--builder docker`

```sh
colima start --arch aarch64 --cpu 4 --memory 6 --disk 60
python3 scripts/kernel-lab-build.py --builder docker \
  --source-volume async-net-kernel-src \
  --clone https://github.com/torvalds/linux.git --clone-ref v7.2 \
  --output results/kernel-lab
```

On macOS the runtime still runs a Linux VM — nothing runs Linux userspace without
a Linux kernel — but it is one shared VM nobody has to name or maintain, and the
build environment inside it is disposable. The image is built from
`scripts/kernel-lab.Dockerfile` on first use, a container is started for the
build and removed afterwards (`--keep-container` leaves it for inspection), and
`--volume` names the volume holding `build/` and `cargo-target/`, so rebuilds
stay incremental. `docker volume rm async-net-kernel-lab-work` is the reset.

**Keep the kernel tree off macOS.** This is not a preference:

> macOS filesystems are case-insensitive by default, and a Linux kernel tree
> contains files whose names differ only in case — `net/netfilter/xt_RATEEST.c`
> and `net/netfilter/xt_rateest.c`, thirteen such pairs in v7.2. Checked out on
> APFS they collapse into one file each, so the tree is silently incomplete and
> `git status` reports it permanently modified.

So `--source-volume NAME` keeps the tree in a named volume on the runtime VM's
own filesystem, where case is significant, and `--clone URL --clone-ref REF`
fills that volume on first use. This also avoids the second problem with a
bind-mounted tree: a kernel is tens of thousands of files, and a macOS bind mount
is materially slower than the VM's own disk. `docker volume rm async-net-kernel-src`
discards it.

`--source` still bind-mounts a tree from this machine, which is the right choice
on Linux, where the filesystem is case-sensitive and the mount is free. The
driver warns if it detects a collapsed tree. It is mounted read-write because git
refreshes its index while the builder records the source commit and diff for
`manifest.json`, and the container checks that the mount is not empty — a path
the runtime does not share is mounted as an empty directory rather than refused.

Use `--engine podman` for Podman. `--platform` picks the container platform; on
Apple Silicon `linux/arm64` is native, and on an x86 host it is emulated and far
too slow for a kernel compile.

## 2. Build the bundle

```sh
python3 scripts/kernel-lab-build.py \
  --instance tito-burrito \
  --source /home/ubuntu/arm64_dev_kernel \
  --output results/kernel-lab
```

The driver copies `Cargo.toml`, `Cargo.lock`, `src/`, `examples/`, `benches/`,
`tests/` and `scripts/` into `--work` on the builder (default
`/home/ubuntu/async-net-kernel-lab`, `~/async-net-kernel-lab` for `local`, the
named volume for `docker`) and builds there. It refuses to use a work directory
it does not own, marking its own with `.async-net-kernel-lab`. Add `--force` to
replace an existing `--output`, and `--export-only` to re-download the last
completed bundle without rebuilding.

Bundles accumulate in the work directory under names like
`bundle-20260918-113700-7.3.0-rc3-xdp-lab+`, so the build time and kernel release
are visible without opening `manifest.json`; `latest-bundle.txt` names the newest
finished one. `--export-only` re-downloads that bundle without rebuilding, which
is also what `qemu-lab.py` runs when it offers to pull one for you.

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

## The edit / build / boot loop

Changing kernel code and seeing the result is one command, provided the source
lives where the builder can see it and you edit it there.

Keep a container attached to the same volumes the build uses. It does nothing but
hold the source open; builds still run in their own throwaway container:

```sh
docker run -d --name kernel-lab-edit --platform linux/arm64 \
  -v async-net-kernel-src:/src -v async-net-kernel-lab-work:/work \
  async-net-kernel-lab:24.04 sleep infinity
```

Attach an editor to it. In VS Code or Cursor, the palette entry is
**Dev Containers: Attach to Running Container**; from the shell, the same thing
with the container name hex-encoded into the authority:

```sh
code --folder-uri "vscode-remote://attached-container+$(printf '%s' \
  '{"containerName":"/kernel-lab-edit"}' | xxd -p | tr -d '\n')/src"
```

Then edit, save, and run one command:

```sh
python3 scripts/qemu-lab.py --rebuild \
  --builder docker --source-volume async-net-kernel-src \
  --test boot --grep LAB_EDIT_MARKER
```

`--rebuild` builds on the builder and re-exports the bundle before the test. The
`boot` test boots with no lab NIC, captures the console, stops the VM at
`--timeout` and prints what matched `--grep`, exiting non-zero when nothing does —
so it is equally usable as a check in a script. Drop `--grep` to read the whole
boot log. The kernel build is incremental, so a one-line change is a short
rebuild rather than a full one.

`--rebuild` works with every builder. Over SSH, edit through
**Remote-SSH** on the same host and use `--builder ssh --ssh HOST --source /path/to/linux`.

### Why this loop avoids the AF_XDP failure

`--test boot` and `--test shell` never bind an AF_XDP socket, so they do not go
near the guest driver path that currently fails. On a stock v7.2 guest,
`--mode zero-copy` dies in the driver while setting up the XSK pool:

```
virtio_net virtio1: rejecting DMA map of vmalloc memory
WARNING: drivers/virtio/virtio_ring.c:3844 virtqueue_map_single_attrs
  xsk_bind → xp_assign_dev → virtnet_xdp → virtqueue_map_single_attrs
```

`--mode copy` does not hit it — that path skips the driver's `XDP_SETUP_XSK_POOL`
call — and the bind succeeds, but no packets arrive. In that run the backend
reported `access_platform: false`, where section 5 expects it to be true, so
feature negotiation is the first thing to look at rather than the data path.

None of this blocks kernel work: the boot loop above exercises everything from
`primary_entry` to userspace without touching the lab NIC.

## Other architectures

Everything here is ARM64 today, and the arch assumptions are concentrated in a
handful of places rather than spread through the logic. Supporting an x86_64 host
building an x86_64 kernel, and cross-compiling when host and target differ, is
mostly a matter of turning those constants into one target description.

### The target table

One record per architecture, looked up everywhere a constant is hardcoded now:

| Field | arm64 | x86_64 | riscv64 |
| --- | --- | --- | --- |
| kernel `ARCH=` | `arm64` | `x86` (not `x86_64`) | `riscv` |
| Built image | `arch/arm64/boot/Image` | `arch/x86/boot/bzImage` | `arch/riscv/boot/Image` |
| Cross prefix | `aarch64-linux-gnu-` | `x86_64-linux-gnu-` | `riscv64-linux-gnu-` |
| Rust target | `aarch64-unknown-linux-*` | `x86_64-unknown-linux-*` | `riscv64gc-unknown-linux-*` |
| Container platform | `linux/arm64` | `linux/amd64` | `linux/riscv64` |
| QEMU binary, machine | `qemu-system-aarch64`, `virt` | `qemu-system-x86_64`, `q35` | `qemu-system-riscv64`, `virt` |
| Console | `ttyAMA0`, `earlycon=pl011,…` | `ttyS0`, `earlyprintk=serial` | `ttyS0`, `earlycon` |

### What changes, by file

* `build-kernel-lab.py` — the arch guard, `ARCH=` on every `make`, the image path
  copied into the bundle, the `arch/<name>` subtree exported for source-level
  stepping, and `EARLY_SYMBOLS`, which is entirely ARM64: `primary_entry`,
  `__cpu_setup`, `__enable_mmu`, `__primary_switched`. The x86_64 landmarks are
  `startup_32`, `startup_64`, `secondary_startup_64` and `x86_64_start_kernel`;
  on RISC-V they are `_start`, `setup_vm` and `_start_kernel`. Only `start_kernel`
  and `setup_arch` are shared.
* `scripts/kernel-lab.config` — the fragment is applied over `allnoconfig`, so it
  has to name **every** symbol the lab needs, and several are ARM64 platform
  symbols: `ARM64_4K_PAGES`, `ARM_GIC_V3`, `PCI_HOST_GENERIC`,
  `SERIAL_AMBA_PL011_CONSOLE`. Split it into a common fragment and one per arch,
  and make the `required`/forbidden symbol checks per-arch alongside it.
* `run-kernel-lab.py` and `launch-vhost-vm.py` — machine type, `-cpu`, the QEMU
  binary name, and the console in the kernel command line.
* `debug-kernel-lab.py` — **not a tweak**. It reads the 64-byte ARM64 Image
  header, recomputes QEMU's `0x40000000 + text_offset` placement from
  `hw/arm/boot.c`, and slides `vmlinux` symbols by `physical - _text`. A bzImage
  has an unrelated header and a real-mode entry stub, so the load address and the
  physical-to-virtual relationship have to be worked out again per architecture.
* `qemu-lab.py` — `--target-list` and the accelerator, which is only available
  when the host and target architectures match: HVF for arm64 on an Apple Silicon
  Mac, KVM for x86_64 on x86_64 Linux, TCG otherwise.
* `lab_builders.py` — `TARGET_PLATFORM` becomes the target's container platform,
  and the doctor's builder-architecture check becomes a comparison against the
  target rather than a literal `aarch64`.

### `allnoconfig` or `defconfig`

Using the arch `defconfig` as the base and merging the lab fragment over it with
`merge_config.sh` is the portable route, and it is how a new architecture gets
working quickly. It costs what makes this lab good for early-boot work: the
current kernel is small, has no modules, no KASLR and a precisely known symbol
set. A defconfig base brings modules and a large driver set back, so the fragment
would have to disable them explicitly and the post-build check would have to hold
the line. Keeping `allnoconfig` and writing a per-arch fragment is more work per
architecture and worth it; a `--config-base defconfig` escape hatch is the way to
bring up an architecture that is not fully described yet.

### Prefer a matching builder over cross-compiling

Three parts of the build are genuinely awkward to cross-compile:

* **busybox.** The initramfs needs a *target-arch* static busybox, and
  `shutil.which('busybox')` finds the builder's own. A container or VM of the
  target architecture gets this for free; cross-compiling means shipping or
  building one per arch.
* **The guest receiver.** `cargo build --target …` is easy, but the `ldd` step
  that copies shared libraries into the initramfs would need the cross sysroot's
  loader rather than the build host's. Building `xdp_vm_rx` **statically** —
  musl, or `+crt-static` — removes that problem, and is worth doing regardless.
* **binutils.** `nm` and `readelf` run against `vmlinux` for the symbol table;
  cross builds need the prefixed ones.

So the cheapest path is not a cross toolchain: it is a builder whose architecture
already matches the target. `--builder docker --platform linux/amd64` on an x86_64
host, or `--builder ssh` to a machine of that architecture, and nothing above
applies. Cross-compiling earns its place only for a mismatch — and note that an
*emulated* container of the foreign architecture is not the answer either, because
a kernel compile under user-mode emulation takes hours. The fast mismatch path is
a **native** container running a cross toolchain: an x86_64 host, an x86_64
container, `CROSS_COMPILE=aarch64-linux-gnu-`, producing an arm64 kernel.

RISC-V fits the same table when it arrives; `riscv32` additionally needs
`CONFIG_32BIT` and a toolchain distributions package less consistently than
`riscv64-linux-gnu-`.

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
