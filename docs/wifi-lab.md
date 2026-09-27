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
  iproute2 iw hostapd wpasupplicant wireless-regdb
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
Wi-Fi tools, and its default tag changed to `async-net-kernel-lab:24.04-wifi1` so
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

Python syntax, shell syntax, CLI help and test-menu checks were run on macOS.
The kernel rebuild, initramfs boot and privileged hwsim datapath have **not** been
run for this change; those are the manual checks above. A useful second check is
running the held lab twice in the same guest to confirm radio cleanup/reuse.
