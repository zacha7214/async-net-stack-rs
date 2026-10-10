# Linux virtual Wi-Fi lab

This is the first real-Wi-Fi integration slice: two hwsim radios, an AP running
hostapd, a station running wpa_supplicant, static IP addresses, UDP broadcast
discovery and a TCP echo service. The two endpoints live in separate network
namespaces, so packets cross the simulated wireless link rather than Linux
short-circuiting communication between local addresses.

The TCP implementation is Linux's. This does not yet connect the Rust packet
simulator, run vendor firmware, implement DHCP, or automate firmware crash/AP
restart scenarios. The Python mock service is a replaceable starting point.

## Build and launch with one command

Use the existing ARM64 Linux kernel source and builder. The QEMU guest boots
the new kernel; the builder VM/container's **running kernel needs no hwsim
support**, and is neither replaced nor rebooted. No nested virtualization is
needed on the builder. QEMU runs on the host after bundle export.

For Multipass, install the added userspace prerequisites once inside the builder:

```sh
multipass exec <mutilpass instance name> -- sudo apt-get update
multipass exec <mutilpass instance name> -- sudo apt-get install -y \
  iproute2 iw hostapd wpasupplicant wireless-regdb wmediumd
```

Then rebuild, export and run the test from the checkout:

```sh
python3 scripts/qemu-lab.py --rebuild --test wifi \
  --builder multipass --instance <mutilpass instance name> \
  --source /home/ubuntu/arm64_dev_kernel
```

For an existing kernel source in a Docker source volume:

```sh
python3 scripts/qemu-lab.py --rebuild --test wifi \
  --builder docker --source-volume async-net-linux-source
```

Use your actual source volume name. To bind-mount an existing kernel tree instead,
replace `--source-volume ...` with `--source /absolute/path/to/linux`. See the
[kernel guide](kernel-lab.md) for initially populating a source volume and the
case-sensitive filesystem requirement. The container image now includes the
Wi-Fi tools, and its default tag changed to `async-net-kernel-lab:24.04-wifi2` so
an older cached image will not silently omit them. Custom `--image` users must
update their image too. Existing build work/source volumes are reused.

An already-built stock QEMU is sufficient: add
`--qemu /absolute/path/to/qemu-system-aarch64` to bypass the QEMU build. Without
that flag, the existing helper obtains/builds its usual QEMU. No virtual PCI Wi-Fi
device or vhost-user backend is needed; hwsim lives entirely inside the guest.

To rerun without rebuilding:

```sh
python3 scripts/qemu-lab.py --test wifi
```

The builder packages Python's standard library and extension dependencies,
iproute2's `ip`, `iw`, `hostapd`, `wpa_supplicant`, `wpa_cli`, available regulatory
database files and the lab script in the initramfs. This increases bundle size.
Older bundles lack a `wifi-hwsim` manifest feature and are rejected with a rebuild
hint. The `wifi` launch has a whole-VM timeout of at least 120 seconds; use
`--timeout 300` for slow TCG hosts.

## Kernel configuration

The driver is already in the Linux source tree. Build it **built-in (`=y`)** for
this lab, which deliberately has `CONFIG_MODULES` disabled. `=m` would require
module installation and loading in the guest.

The config fragment `scripts/kernel-lab.config` now contains:

```text
CONFIG_NAMESPACES=y
CONFIG_NET_NS=y
CONFIG_WLAN=y
CONFIG_WIRELESS=y
CONFIG_CFG80211=y
CONFIG_MAC80211=y
CONFIG_MAC80211_HWSIM=y
CONFIG_MAC80211_RC_MINSTREL=y
```

`CONFIG_PACKET=y`, `CONFIG_UNIX=y` and IPv4 support already existed. Crypto user
API options are also enabled for userspace tooling. Kconfig selects additional
wireless crypto dependencies. Because the builder uses **allnoconfig**, setting
only `MAC80211_HWSIM=y` cannot enable its unmet `MAC80211`/`CFG80211` dependencies.
The build script verifies the essential resolved options before compiling.
The kernel command line explicitly sets `mac80211_hwsim.radios=2`.

References: [hwsim Kconfig](https://github.com/torvalds/linux/blob/v6.12/drivers/net/wireless/virtual/Kconfig),
[mac80211 Kconfig](https://github.com/torvalds/linux/blob/v6.12/net/mac80211/Kconfig),
and the [Linux hwsim guide](https://wireless.docs.kernel.org/en/latest/en/users/drivers/mac80211_hwsim.html).

## What to look for

The guest runs `wifi-lab --guest-auto`, which selects exactly two unused hwsim
radios. It starts a WPA2-PSK AP (`async-net-lab`, lab-only password
`lab-password-only`) on channel 1, gives it `10.78.0.1/24`, and assigns the station
`10.78.0.2/24`. It waits for `wpa_state=COMPLETED`, then:

1. Broadcasts the project's `ANSP` discovery query for service ID 7 to UDP 9000.
2. Verifies the service advertisement from `10.78.0.1:9000`.
3. Connects to TCP 9001 and verifies a complete echo, handling partial reads.
4. Stops its children, removes lab addresses, restores managed interface mode,
   moves the radios back and removes its namespaces.

Expect `wifi_associated`, `wifi_probe_ok`, `wifi_lab_pass`, `wifi_lab_cleaned`,
then **`KERNEL_LAB_WIFI_RESULT=0`**. Failure or missing completion makes the host
launcher fail. The run directory's `guest-console.log` contains these events and
the guest daemon logs printed before poweroff. The Wi-Fi probe has a 30-second
shared association/discovery deadline with bounded retries.

For interactive investigation, boot a shell instead:

```sh
python3 scripts/qemu-lab.py --test shell
# Inside the guest:
wifi-lab --guest-auto --hold
```

The lab prints its namespace names, log directory and a command to repeat the
probe. Ctrl-C cleans up and returns to the shell; guest logs remain under `/tmp`.
From another guest shell you can run your application with `ip netns exec NAME ...`
or inspect `iw`, `wpa_cli` and daemon logs. Files added to the initramfs are needed
for additional programs; this guest has no package manager/network uplink.

## On an ordinary Linux machine

Prefer a disposable VM. Install the same tools and Python 3.8+; provide two
unused hwsim radios. If the running distro kernel provides a module, load it
yourself with `sudo modprobe mac80211_hwsim radios=2`. If that kernel lacks the
driver, use the rebuilt QEMU guest above. A container shares its host kernel;
changing the kernel source/config in a container does not enable its driver.

```sh
iw phy
sudo python3 scripts/linux-wifi-lab.py --ap-phy phy0 --station-phy phy1 --hold
```

Replace PHY names with the hwsim radios, not hardware radios. The launcher checks
the sysfs driver, requires one down, unaddressed managed interface per PHY, and
never loads/unloads a module or stops host networking services. NetworkManager
or another manager must not claim the selected radios while the lab runs.
Use one lab at a time in a dedicated guest: namespaces isolate IP configuration,
but other hwsim radios in the same simulated medium may still share the channel.

The launcher retains logs, removes only its own namespaces and terminates only
its child process groups on success, failure or SIGINT/SIGTERM. SIGKILL cannot
run cleanup; rebooting the disposable guest clears that state.

## Validation status

The wmediumd/netem addition has not been built, syntax-checked, or run; validation
is left to the user. A useful manual check is running the held lab twice in the
same guest to confirm radio and qdisc cleanup/reuse.

## Radio loss plus queueing faults

The launcher now supports two independently configured layers:

* `--medium wmediumd` sends simulated wireless frames through a supervised
  wmediumd process. `--ap-loss` and `--station-loss` are directional frame-error
  probabilities from 0 to 1. They apply during association too. Retries mean a
  20% frame-error probability does **not** imply 20% application packet loss.
* `--netem-ap` and `--netem-station` configure egress queues on those interfaces
  after the initial association/discovery/TCP check. They affect queued traffic,
  including ARP, independently of the radio model. AP egress primarily affects
  echo replies; station egress affects requests. They do not model RF behavior.

Defaults retain the original kernel medium with no additional qdisc. Available
netem profiles are `clean` (1000-packet limit), `delay` (40 ± 10 ms, 100-packet
limit), `lossy` (same delay plus 5% loss), `congested` (40 ± 10 ms, 1 Mbit/s,
32-packet limit), and `outage` (100% loss). These live in `NETEM` in
`scripts/linux-wifi-lab.py`; change those definitions to create another scenario.
The jitter uses netem's default distribution. A clean profile still has a finite
queue and can drop packets when overloaded.

On a disposable Ubuntu VM with two unused hwsim radios:

```sh
sudo apt-get install iproute2 iw hostapd wpasupplicant wireless-regdb wmediumd
sudo modprobe mac80211_hwsim radios=2
sudo python3 scripts/linux-wifi-lab.py --guest-auto \
  --medium wmediumd --ap-loss 0.01 --station-loss 0.20 \
  --netem-ap delay --netem-station lossy --hold
```

Skip modprobe when radios already exist. wmediumd mode requires exactly two
hwsim radios visible in the launching namespace and refuses an already running
wmediumd process. Use a dedicated VM with radios created in its initial network
namespace; namespace isolation does not make medium registration private to an
arbitrary pair of radios. The lab starts wmediumd before moving the PHYs, using
their actual MAC addresses in a saved config. It uses OFDM-only BSS rates on
channel 1 to avoid the CCK rate-table mismatch described by
[upstream wmediumd](https://github.com/bcopeland/wmediumd#rates).

For the bundled QEMU guest, rebuild using the earlier instructions, then boot
`python3 scripts/qemu-lab.py --test shell`. Inside the guest:

```sh
wifi-lab --guest-auto --medium wmediumd \
  --ap-loss 0.01 --station-loss 0.20 --compare --hold
```

The rebuilt bundle includes wmediumd, tc, their dependencies, the UDP policy
probe, and built-in `CONFIG_NET_SCHED=y` / `CONFIG_NET_SCH_NETEM=y`. Its manifest
includes `wifi-wmediumd-netem`. Older bundles must be rebuilt. The ordinary
`--test wifi` boot remains the original smoke check; use shell mode for these
longer experiments. Guest results live in RAM: copy them out before poweroff.

## UDP policy comparison and recovery observations

`--compare` runs three repetitions of four phases: the requested baseline
netem profiles, congestion in both directions, a complete station-egress outage,
then restoration of the baseline. wmediumd settings remain constant. Each phase
compares burst/window-256 with paced/window-32 at 300 requests/sec, alternating
policy order between repetitions. Every row attempts 300 requests of 512 bytes
with a 500 ms deadline and no application retries. The lab provides a separate
UDP echo endpoint at `10.78.0.1:9002` for this workload.

The printed run directory contains:

* `comparison.html`: deadline-success bars, p99 RTT, expirations and goodput.
* `comparison.json`: incremental measurements, including directions/profiles.
* `environment.json` and `wmediumd.cfg`: launch settings and radio mapping.
* Initial and per-phase JSON snapshots: addresses, `iw station dump`, and
  `tc -s -j qdisc` counters, plus daemon and per-workload logs.

An outage is a measured result, not a failed assertion. Completion does not
guarantee recovery: inspect the recovered rows against baseline. Each workload
uses a fresh UDP socket; this measures resumed network availability, not recovery
latency of a persistent application. The radio remains associated during the
netem outage; AP restarts, reassociation, DHCP renewal and SNR/path-loss/mobility
models remain separate future scenarios.

The measurements currently use Linux sockets and the Python echo service, not
the Rust UDP stack. Pacing can improve deadline success but also increases run
time; the report does not assume it always wins. Seeds and host scheduling are
not controlled, so repeat runs. A one-second gap between phase changes lets
short queues drain; replacing a qdisc can itself discard pending packets.

wmediumd logs startup/errors at level 5, avoiding per-frame log I/O. The launcher
waits for its registration message and supervises it throughout measurements;
that message alone is not proof of frame delivery. A daemon exit aborts the run
instead of accepting subsequent perfect-medium results. Cleanup removes the
lab's qdiscs before returning the radios and retains all files for inspection.
Upstream references: [wmediumd configuration](https://github.com/bcopeland/wmediumd#configuration)
and [Linux hwsim](https://wireless.docs.kernel.org/en/latest/en/users/drivers/mac80211_hwsim.html).
