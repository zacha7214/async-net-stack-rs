#!/usr/bin/env python3
"""One entry point for a fresh checkout: get a patched lab QEMU, then run a lab test.

Clones QEMU at a known-good tag, applies the patches in patches/, configures and
builds aarch64-softmmu in its own build directory, code-signs it for HVF on
macOS, and records what it did in qemu-lab.json beside the binary. It then looks
for a kernel bundle: without one it stops and prints how to build one, and with
one it offers the experiments that bundle supports and launches the chosen one
through the existing scripts/run-kernel-lab.py.

Every stage is idempotent. An existing checkout, an already-applied patch, a
configured build directory and an already-signed binary are detected and reused,
so re-running after a failure repeats only the work that is still missing.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lab_builders  # noqa: E402

PROJECT = Path(__file__).resolve().parent.parent
UPSTREAM = 'https://gitlab.com/qemu-project/qemu.git'
KNOWN_GOOD = 'v11.0.1'
BUILD_NAME = {'Darwin': 'build-mac-vhost', 'Linux': 'build-lab'}
QUEUE_RESET = 'qemu-11.0.1-vhost-user-queue-reset.patch'
MACOS_HEADERS = 'qemu-vhost-net-macos-headers.patch'
BINARY = 'qemu-system-aarch64'
BACKEND = PROJECT/'target/release/examples/vhost_user_net'
HVF_ENTITLEMENT = 'com.apple.security.hypervisor'
# Used only if the checkout predates accel/hvf/entitlements.plist.
ENTITLEMENTS = '''<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.hypervisor</key>
    <true/>
</dict>
</plist>
'''
# The lab always runs with -display none, so no UI is built. Besides being smaller
# and quicker, this keeps the build off frameworks that drift with the host SDK:
# apple-gfx against ParavirtualizedGraphics is the one that breaks Mac rebuilds.
HEADLESS = ('cocoa', 'pvg', 'vnc', 'gtk', 'sdl', 'curses', 'gio', 'docs')
# build-kernel-lab.py publishes these; run-kernel-lab.py checksums the first four.
BUNDLE_FILES = ('Image', 'vmlinux', 'System.map', 'config', 'initramfs.cpio.gz', 'manifest.json')

TESTS = {
    'shell': dict(
        title='Boot the bundle to a busybox shell',
        blurb='No lab NIC and no backend, so this also works with a stock QEMU. The '
              'quickest confirmation that the kernel and initramfs are sound.',
        bundle=True, patched=False, boots=True, backend=False,
        results='Console only. run-kernel-lab.py still prints a run directory, which stays empty.',
        watch='"KERNEL_LAB_READY <release>" followed by a busybox prompt. Quit QEMU with Ctrl-A x.'),
    'debug': dict(
        title='Debug the early ARM64 boot path (paused at reset, GDB stub)',
        blurb='One TCG CPU stopped before its first instruction, with a loopback GDB stub. '
              'Drive it from a second terminal with scripts/debug-kernel-lab.py.',
        bundle=True, patched=False, boots=True, backend=False,
        results='No files. The debug helper prints the command files it generates under /tmp.',
        watch='QEMU prints nothing and waits. Attach, break at primary_entry, "si" through the '
              'early assembly, and source virtual.commands before continuing past __enable_mmu.'),
    'zero-copy': dict(
        title='AF_XDP zero-copy data path (the actual experiment)',
        blurb='The Rust generator writes packets straight into posted virtio RX buffers in shared '
              'guest RAM; the guest receives them through an XDP_ZEROCOPY AF_XDP socket.',
        bundle=True, patched=True, boots=True, backend=True,
        results='run-kernel-lab.py prints "Run logs: /tmp/kernel-lab-XXXX" holding host.jsonl '
                '(backend), host.log (backend stderr), guest-console.log and guest.jsonl. '
                'check-vhost-run.py runs over both logs at the end.',
        watch='"KERNEL_LAB_RESULT=0", then the checker: every packet received, zero RX drops and '
              'invalid descriptors, "zero_copy": true, and 8 matching physical addresses. The host '
              'log should show access_platform and ring_reset true plus queue stops at XSK bind.'),
    'copy': dict(
        title='AF_XDP forced-copy control run',
        blurb='Identical setup with XDP_COPY. This is the control that gives the zero-copy '
              'address match its meaning; without it the match proves nothing.',
        bundle=True, patched=True, boots=True, backend=True,
        results='Same run directory layout as the zero-copy test.',
        watch='The same counts with "zero_copy": false, and sampled physical addresses that '
              'differ from the host-side GPAs.'),
    'ab': dict(
        title='Zero-copy then copy, back to back',
        blurb='Runs both of the above in sequence against the same bundle and QEMU build.',
        bundle=True, patched=True, boots=True, backend=True,
        results='Two run directories, printed as each run starts. Keep both paths.',
        watch='Two checker summaries whose only intended differences are the zero_copy flag and '
              'the sampled addresses. Any other difference is worth investigating.'),
    'boot': dict(
        title='Rebuild-and-look: boot, capture the console, exit',
        blurb='Non-interactive boot with a time limit, for the edit/build/boot loop. Pair it '
              'with --rebuild to build the kernel first and --grep to pull your line out.',
        bundle=True, patched=False, boots=True, backend=False,
        results='The console is printed here. Nothing is left behind except the run directory.',
        watch='Your own printk. Without --grep the whole boot log is printed; with it, only '
              'matching lines, and a non-zero exit when nothing matches.'),
    'protocol': dict(
        title='Host vhost-user protocol smoke test (no VM, no root)',
        blurb='Exercises the backend over real Unix sockets, SCM_RIGHTS, shared mappings and '
              'IOTLB messages. Seconds rather than minutes; needs a C compiler.',
        bundle=False, patched=False, boots=False, backend=True,
        results='Temporary files only; failures print the backend stderr.',
        watch='A clean exit. This checks the protocol implementation, not the guest driver.'),
}
ORDER = ('shell', 'boot', 'debug', 'zero-copy', 'copy', 'ab', 'protocol')


def run(args, dry=False, **kw):
    print('+ ' + ' '.join(map(str, args)), flush=True)
    if dry:
        return None
    return subprocess.run(list(map(str, args)), check=True, **kw)


def probe(args, **kw):
    try:
        r = subprocess.run(list(map(str, args)), capture_output=True, text=True, timeout=120, **kw)
        return r.returncode, r.stdout + r.stderr
    except (OSError, subprocess.SubprocessError) as e:
        return 127, str(e)


def choose_source(explicit):
    """--qemu-src wins; otherwise an existing ./qemu checkout, otherwise ~/qemu."""
    if explicit is not None:
        return explicit.expanduser().resolve()
    here = Path.cwd()/'qemu'
    return (here if (here/'.git').is_dir() else Path.home()/'qemu').resolve()


def check_build_tools():
    system = platform.system()
    hint = ('brew install git ninja pkg-config glib pixman' if system == 'Darwin' else
            'sudo apt-get install -y git ninja-build pkg-config python3-venv '
            'libglib2.0-dev libpixman-1-dev')
    missing = [t for t in ('git', 'ninja', 'pkg-config') if not shutil.which(t)]
    if missing:
        raise SystemExit(f'missing build prerequisites: {" ".join(missing)}\n  -> {hint}')
    if probe(['pkg-config', '--exists', 'glib-2.0'])[0]:
        raise SystemExit(f'QEMU needs glib-2.0 development files\n  -> {hint}')


def obtain_source(src, ref, dry):
    if (src/'.git').is_dir():
        describe = probe(['git', '-C', src, 'describe', '--always', '--dirty'])[1].strip()
        print(f'Reusing QEMU checkout {src} ({describe})')
        if ref not in describe:
            print(f'  note: this tree is not {ref}. The patches were validated against '
                  f'{KNOWN_GOOD}; an apply failure below means the tree has moved past them.')
        return describe
    if src.exists() and any(src.iterdir()):
        raise SystemExit(f'{src} exists and is not a git checkout; choose another --qemu-src')
    src.parent.mkdir(parents=True, exist_ok=True)
    # Shallow: this tree is built, not developed in. --ref takes a tag or a branch.
    run(['git', 'clone', '--depth', '1', '--branch', ref, UPSTREAM, src], dry)
    if dry:
        return ref
    return probe(['git', '-C', src, 'describe', '--always', '--dirty'])[1].strip()


def apply_patches(src, dry):
    names = [QUEUE_RESET]
    if platform.system() == 'Darwin':
        # The header fix has to land first, or the Mac build fails before linking.
        names.insert(0, MACOS_HEADERS)
    applied = []
    for name in names:
        patch = PROJECT/'patches'/name
        if not patch.is_file():
            raise SystemExit(f'missing patch file: {patch}')
        # A reverse-apply that checks out cleanly means the patch is already in.
        if probe(['git', '-C', src, 'apply', '--reverse', '--check', patch])[0] == 0:
            print(f'  already applied: {name}')
        elif probe(['git', '-C', src, 'apply', '--check', patch])[0]:
            _, why = probe(['git', '-C', src, 'apply', '--check', patch])
            raise SystemExit(f'{name} does not apply to {src}:\n{why}\n'
                             f'Inspect the files it touches, or clone {KNOWN_GOOD} into a '
                             f'fresh --qemu-src and rerun.')
        else:
            run(['git', '-C', src, 'apply', patch], dry)
            print(f'  applied: {name}')
        applied.append(name)
    return applied


def headless_flags(src):
    """--disable-X for every HEADLESS feature this configure actually knows about.

    Older or newer trees list different features, and meson rejects an option it
    does not have, so the list is filtered against configure --help rather than
    assumed.
    """
    # QEMU's configure drops config.log and config-temp/ into its working
    # directory even for --help, so probe it from a directory we throw away.
    with tempfile.TemporaryDirectory(prefix='qemu-lab-probe-') as scratch:
        help_text = probe([str(src/'configure'), '--help'], cwd=scratch)[1]
    listed = set()
    for line in help_text.splitlines():
        # Feature rows are indented exactly two spaces: "  cocoa   Cocoa user interface".
        if line.startswith('  ') and not line.startswith('   ') and len(line.split()) >= 2:
            listed.add(line.split()[0])
    return [f'--disable-{name}' for name in HEADLESS if name in listed]


def build_qemu(src, build, jobs, reconfigure, dry):
    build.mkdir(parents=True, exist_ok=True)
    accel = '--enable-hvf' if platform.system() == 'Darwin' else '--enable-kvm'
    # --disable-werror: a git checkout defaults to -Werror, which turns an unrelated
    # new-compiler warning into a build failure on somebody else's machine.
    configure = ([str(src/'configure'), '--target-list=aarch64-softmmu', accel,
                  '--enable-vhost-user', '--disable-werror'] + headless_flags(src))
    fresh = reconfigure or not (build/'build.ninja').is_file()
    if fresh:
        run(configure, dry, cwd=build)
    else:
        print(f'Reusing the configuration in {build} (--reconfigure redoes it)')
    try:
        run(['ninja', '-C', build, BINARY] + (['-j', str(jobs)] if jobs else []), dry)
    except subprocess.CalledProcessError:
        if fresh:
            raise SystemExit(f'QEMU failed to build in {build}; the error is above.')
        raise SystemExit(
            f'QEMU failed to build in {build}, which was configured by an earlier run.\n'
            'A stale build directory is the usual cause: a host SDK or library moved\n'
            'under a configuration that was probed months ago. Reconfigure it:\n'
            f'  python3 {Path(__file__).name} --qemu-src {src} --reconfigure --build-only\n'
            'That also applies the headless feature set, which avoids the UI frameworks\n'
            'this lab never uses. Your existing binary is not touched until a link succeeds.')
    return configure


def ensure_signed(binary, build, src, dry):
    """Give the binary the HVF entitlement if QEMU's own build step did not."""
    if platform.system() != 'Darwin':
        return False
    if HVF_ENTITLEMENT in probe(['codesign', '-d', '--entitlements', '-', binary])[1]:
        print(f'  already signed for HVF: {binary}')
        return True
    plist = src/'accel/hvf/entitlements.plist'
    if not plist.is_file():
        plist = build/'lab-entitlements.plist'
        if not dry:
            plist.write_text(ENTITLEMENTS)
    run(['codesign', '--entitlements', plist, '--force', '-s', '-', binary], dry)
    if dry:
        return True
    if HVF_ENTITLEMENT not in probe(['codesign', '-d', '--entitlements', '-', binary])[1]:
        raise SystemExit('ad hoc signing did not take. Run it by hand:\n'
                         f'  codesign --entitlements {plist} --force -s - {binary}')
    print('  signed ad hoc with ' + HVF_ENTITLEMENT)
    return True


def verify(binary):
    """Confirm the built binary can actually host the lab, and return its accelerators."""
    version = probe([binary, '--version'])[1].splitlines()
    print('  ' + (version[0] if version else 'no version output'))
    # "-accel help" prints a header line and then one accelerator per line.
    accel = [line.strip() for line in probe([binary, '-accel', 'help'])[1].splitlines()[1:]
             if line.strip() and ' ' not in line.strip()]
    netdev = probe([binary, '-machine', 'virt', '-netdev', 'help'])[1]
    device = probe([binary, '-device', 'virtio-net-pci,help'])[1]
    for ok, why in ((accel, 'reports no accelerators'),
                    ('vhost-user' in netdev, 'has no vhost-user netdev'),
                    ('queue_reset' in device, 'virtio-net-pci has no queue_reset property')):
        if not ok:
            raise SystemExit(f'{binary} {why}; the data path will not work')
    wanted = 'hvf' if platform.system() == 'Darwin' else 'kvm'
    print(f'  accelerators: {" ".join(sorted(accel))}')
    if wanted not in accel:
        print(f'  note: no {wanted}; runs fall back to TCG, which is for correctness only')
    return accel


def provenance_path(binary):
    return binary.parent/'qemu-lab.json'


def write_provenance(binary, src, describe, patches, configure, signed, dry):
    record = dict(binary=str(binary), source=str(src), describe=describe, patches=patches,
                  configure=configure, signed=signed, host=f'{platform.system()} {platform.machine()}',
                  built=datetime.datetime.now().astimezone().isoformat(timespec='seconds'),
                  project=str(PROJECT))
    if not dry:
        provenance_path(binary).write_text(json.dumps(record, indent=2)+'\n')
    print(f'  provenance: {provenance_path(binary)}')
    return record


def read_provenance(binary):
    path = provenance_path(binary)
    if not path.is_file():
        return None
    try:
        return json.loads(path.read_text())
    except ValueError:
        return None


def check_bundle(bundle):
    """Return (manifest, '') for a complete, intact bundle, or (None, reason)."""
    if not bundle.is_dir():
        return None, f'{bundle} does not exist'
    manifest = bundle/'manifest.json'
    if not manifest.is_file():
        return None, f'{bundle} has no manifest.json'
    try:
        data = json.loads(manifest.read_text())
    except ValueError:
        return None, 'manifest.json is not valid JSON'
    for name in BUNDLE_FILES:
        if not (bundle/name).is_file():
            return None, f'bundle is missing {name}'
    for name, digest in data.get('sha256', {}).items():
        target = bundle/name
        if target.is_file():
            with target.open('rb') as handle:
                if hashlib.file_digest(handle, 'sha256').hexdigest() != digest:
                    return None, f'checksum mismatch: {name} (re-export the bundle)'
    return data, ''


def confirm(question):
    try:
        return input(f'{question} [y/N] ').strip().lower() in ('y', 'yes')
    except EOFError:
        return False


def builder_bundle(builder, work):
    """Path of the newest finished bundle on the builder, or None.

    build-kernel-lab.py records it in latest-bundle.txt when a build completes,
    which is what makes exporting just the bundle possible: the rest of the work
    directory is the kernel build tree and cargo target, several times its size.
    """
    if builder is None or builder.check():
        return None
    try:
        with builder:
            remote = builder.capture(['cat', work.rstrip('/') + '/latest-bundle.txt'])
    except (OSError, RuntimeError, subprocess.SubprocessError):
        return None
    return remote if remote.startswith('/') else None


def bundle_next_steps(bundle, reason):
    print(f'\nNo usable kernel bundle at {bundle}: {reason}')
    print("""
The QEMU side is ready; the guest kernel is the remaining piece. It is built
natively on ARM64 Linux, not cross-compiled here, because the lab kernel, its
initramfs and the guest receiver are all built together. Pick where that happens
with --builder; scripts/kernel-lab-build.py copies this tree in, drives
scripts/build-kernel-lab.py there, and copies only the finished bundle back.

  --builder docker    A throwaway container on a runtime you already have.
                      Nothing to name or maintain. Keep the kernel tree in a
                      volume, not on macOS: its filesystem is case-insensitive,
                      and a kernel tree has files whose names differ only in
                      case, which collapse into one and silently lose content.

                        colima start --arch aarch64 --cpu 4 --memory 6 --disk 60
                        python3 scripts/kernel-lab-build.py --builder docker \\
                          --source-volume async-net-kernel-src \\
                          --clone https://github.com/torvalds/linux.git \\
                          --clone-ref v7.2 --output """ + str(bundle) + """

  --builder ssh       Any ARM64 Linux box reachable over SSH: UTM, Parallels,
                      VMware, Lima, a Raspberry Pi, a Graviton instance. Set it
                      up in ~/.ssh/config so key auth needs no prompt:

                        python3 scripts/kernel-lab-build.py --builder ssh \\
                          --ssh kernel-box --source /home/you/linux \\
                          --output """ + str(bundle) + """

  --builder local     You are already on ARM64 Linux. No VM at all.

  --builder multipass The default:

                        multipass launch --name kernel-lab 24.04 --cpus 4 \\
                          --memory 6G --disk 40G
                        python3 scripts/kernel-lab-build.py --instance kernel-lab \\
                          --source /home/ubuntu/linux --output """ + str(bundle) + """

Whichever you choose, the builder needs build-essential, flex, bison, bc,
libssl-dev, libelf-dev, busybox-static, cpio, git and Rust via rustup; the
container image brings its own. The kernel tree must be clean, with no in-tree
.config, because the lab builds out-of-tree with O=. Run "make mrproper" once if
you are unsure. The kernel configuration comes from scripts/kernel-lab.config
(full DWARF, no KASLR, no modules, AF_XDP and virtio on) and PID 1 in the
initramfs comes from scripts/kernel-lab-init.sh.

Check a builder before using it, which reports on this host and on the builder:

  python3 scripts/kernel-lab-doctor.py --builder BUILDER --source YOUR_KERNEL_TREE

Then rerun this script; it will find the bundle and offer the tests. If you have
already built somewhere, point the same --builder flags at it and this script
pulls the finished bundle by itself. Do not copy the whole work directory by
hand: the bundle is a fraction of it, and the build tree and cargo target beside
it are not needed here.

docs/kernel-lab.md covers all of it, including what each bundle file is for.""")
    print(f'\nExpected in {bundle}: ' + ', '.join(BUNDLE_FILES))
    print('Also exported: source/ (for source-level stepping) and source-changes.patch.')


def native_accel(accelerators, debug=False):
    if debug:
        return 'tcg'
    wanted = 'hvf' if platform.system() == 'Darwin' else 'kvm'
    if wanted == 'kvm' and not os.access('/dev/kvm', os.R_OK | os.W_OK):
        return 'tcg'
    return wanted if wanted in accelerators else 'tcg'


def steps_for(key, bundle, qemu, accel, packets, pps, timeout=45):
    def lab(*extra):
        return [sys.executable, str(PROJECT/'scripts/run-kernel-lab.py'),
                '--bundle', str(bundle), '--qemu', str(qemu), *extra]

    data = ['--accel', accel, '--packets', str(packets), '--pps', str(pps)]
    return {
        'shell': [lab('--no-lab-nic', '--accel', accel, '--mode', 'shell')],
        'boot': [lab('--no-lab-nic', '--accel', accel, '--mode', 'shell',
                     '--timeout', str(timeout))],
        'debug': [lab('--no-lab-nic', '--accel', 'tcg', '--mode', 'shell', '--debug')],
        'zero-copy': [lab('--mode', 'zero-copy', *data)],
        'copy': [lab('--mode', 'copy', *data)],
        'ab': [lab('--mode', 'zero-copy', *data), lab('--mode', 'copy', *data)],
        'protocol': [[sys.executable, str(PROJECT/'scripts/vhost-user-smoke.py'),
                      '--binary', str(BACKEND)]],
    }[key]


def run_boot(step, pattern, dry):
    """Boot once, capture the console, and report what matched."""
    print('+ ' + ' '.join(map(str, step)), flush=True)
    if dry:
        return 0
    finished = subprocess.run([str(x) for x in step], capture_output=True, text=True)
    console = finished.stdout + finished.stderr
    if not pattern:
        print(console)
        return 0
    matched = [line for line in console.splitlines() if re.search(pattern, line)]
    for line in matched:
        print(line)
    if not matched:
        print(f'\nNothing on the console matched {pattern!r}. The tail of the boot log:',
              file=sys.stderr)
        print(console[-8000:], file=sys.stderr)
        return 1
    print(f'\n{len(matched)} line(s) matched {pattern!r}.')
    return 0


def available(key, have_bundle, patched):
    test = TESTS[key]
    if test['bundle'] and not have_bundle:
        return 'needs a kernel bundle'
    if test['patched'] and not patched:
        return 'needs the patched QEMU built by this script'
    return ''


def menu(have_bundle, patched):
    print('\n== Lab tests ==')
    for index, key in enumerate(ORDER, 1):
        blocked = available(key, have_bundle, patched)
        mark = f'  [unavailable: {blocked}]' if blocked else ''
        print(f'\n  {index}) {TESTS[key]["title"]}{mark}')
        for line in wrap(TESTS[key]['blurb']):
            print(f'     {line}')
    print('\n  q) quit\n')
    while True:
        try:
            choice = input(f'Select 1-{len(ORDER)} or q: ').strip().lower()
        except EOFError:
            # No terminal: --test NAME is the non-interactive way in.
            print('\n  no input available; use --test NAME (see --list-tests)')
            return None
        if choice in ('q', 'quit', ''):
            return None
        if choice.isdigit() and 1 <= int(choice) <= len(ORDER):
            key = ORDER[int(choice)-1]
            blocked = available(key, have_bundle, patched)
            if blocked:
                print(f'  {key} is unavailable: {blocked}')
                continue
            return key
        print('  not a valid choice')


def wrap(text, width=76):
    words, line, out = text.split(), '', []
    for word in words:
        if line and len(line)+1+len(word) > width:
            out.append(line)
            line = word
        else:
            line = f'{line} {word}'.strip()
    if line:
        out.append(line)
    return out


def ensure_backend(dry):
    if BACKEND.is_file():
        return
    print('Building the host generator, which this test needs:')
    run(['cargo', 'build', '--locked', '--release', '--example', 'vhost_user_net'],
        dry, cwd=PROJECT)


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--qemu-src', type=Path,
                   help='QEMU checkout to use or create; default ./qemu if present, else ~/qemu')
    p.add_argument('--build-dir', type=Path,
                   help=f'build directory; default <qemu-src>/{BUILD_NAME.get(platform.system(), "build-lab")}')
    p.add_argument('--ref', default=KNOWN_GOOD,
                   help=f'tag or branch to clone; default {KNOWN_GOOD}, the version the patches '
                        'were validated against. Use "master" for the latest.')
    p.add_argument('--qemu', type=Path,
                   help='use this already-built binary and skip clone/patch/build entirely')
    p.add_argument('--bundle', type=Path, default=PROJECT/'results/kernel-lab',
                   help='kernel bundle directory (default: %(default)s)')
    lab_builders.add_arguments(p)
    p.add_argument('--work', default=None,
                   help='the builder\'s work directory, holding latest-bundle.txt')
    p.add_argument('--export', action='store_true',
                   help='pull the builder\'s latest bundle without asking')
    p.add_argument('--no-export', action='store_true', help='never touch the builder')
    p.add_argument('--rebuild', action='store_true',
                   help='build the kernel on the builder first, then run the test')
    p.add_argument('--source', help='--rebuild: kernel tree for the builder (see kernel-lab-build.py)')
    p.add_argument('--grep', metavar='PATTERN',
                   help='boot test: print only console lines matching this regular expression, '
                        'and exit non-zero when none do')
    p.add_argument('--timeout', type=int, default=45,
                   help='boot test: seconds before the VM is stopped (default: %(default)s)')
    p.add_argument('--test', choices=ORDER, help='skip the menu and run this test')
    p.add_argument('--list-tests', action='store_true')
    p.add_argument('--build-only', action='store_true', help='stop after QEMU is built and verified')
    p.add_argument('--jobs', type=int, default=0, help='ninja parallelism; 0 leaves it to ninja')
    p.add_argument('--packets', type=int, default=10000)
    p.add_argument('--pps', type=int, default=1000)
    p.add_argument('--reconfigure', action='store_true', help='rerun configure in an existing build dir')
    p.add_argument('--dry-run', action='store_true', help='print every command without running it')
    a = p.parse_args()
    if sys.version_info < (3, 11):
        p.error('this lab needs Python 3.11+ (hashlib.file_digest)')
    if not 0 <= a.jobs <= 256:
        p.error('jobs must be 0..256')
    if not 1 <= a.packets <= 1_000_000_000 or not 0 <= a.pps <= 100_000_000:
        p.error('invalid packet count/rate')
    if a.list_tests:
        for key in ORDER:
            print(f'{key:<10} {TESTS[key]["title"]}')
        return 0

    if a.qemu is not None:
        qemu = a.qemu.expanduser().resolve()
        if not os.access(qemu, os.X_OK):
            p.error(f'{qemu} is not executable')
        print(f'== Using the supplied QEMU ==\n  {qemu}')
        accelerators = verify(qemu)
        record = read_provenance(qemu)
        patched = bool(record and QUEUE_RESET in record.get('patches', []))
        if not patched:
            print(f'  note: no {provenance_path(qemu).name} beside this binary records the '
                  'queue-reset patch,\n        so the data-path tests are not offered. Rerun '
                  'without --qemu and with\n        --qemu-src pointing at the checkout it came '
                  'from: already-applied patches are\n        detected, ninja is a no-op, and the '
                  'provenance gets written. Or pass --test\n        explicitly if you know the '
                  'patch is applied.')
    else:
        check_build_tools()
        src = choose_source(a.qemu_src)
        build = (a.build_dir.expanduser().resolve() if a.build_dir
                 else src/BUILD_NAME.get(platform.system(), 'build-lab'))
        print(f'== QEMU source ==\n  {src}')
        describe = obtain_source(src, a.ref, a.dry_run)
        print('== Patches ==')
        patches = apply_patches(src, a.dry_run)
        print(f'== Build ==\n  {build}')
        configure = build_qemu(src, build, a.jobs, a.reconfigure, a.dry_run)
        qemu = build/BINARY
        print('== Signing ==')
        signed = ensure_signed(qemu, build, src, a.dry_run)
        print('== Verify ==')
        if a.dry_run:
            print('  skipped in --dry-run')
            accelerators, patched = [], True
        else:
            accelerators = verify(qemu)
            patched = QUEUE_RESET in patches
            write_provenance(qemu, src, describe, patches, configure, signed, a.dry_run)
        print(f'\nLab QEMU: {qemu}')

    if a.build_only:
        print('\nStopping after the build (--build-only).')
        return 0

    dest = a.bundle.expanduser().resolve()
    work = a.work or lab_builders.default_work(a.builder)
    if a.rebuild:
        # The edit/build/boot loop: whatever is on the builder now becomes the bundle.
        build = [sys.executable, str(PROJECT/'scripts/kernel-lab-build.py'),
                 *lab_builders.forward(a), '--work', work, '--output', str(dest), '--force']
        if a.source:
            build += ['--source', a.source]
        run(build, a.dry_run)
    manifest, reason = check_bundle(dest)
    if manifest is None and not a.rebuild and not a.no_export:
        builder = None if (a.builder == 'multipass' and not a.instance) else lab_builders.from_args(a)
        remote = builder_bundle(builder, work)
        if remote:
            print(f'\nNo usable bundle at {dest}: {reason}')
            print(f'{builder.describe()} has a finished one:\n  {remote}')
            print('Exporting copies the bundle alone, not the kernel build tree beside it.')
            if a.export or confirm(f'Export it to {dest}?'):
                run([sys.executable, str(PROJECT/'scripts/kernel-lab-build.py'),
                     *lab_builders.forward(a), '--work', work, '--export-only',
                     '--output', str(dest)] + (['--force'] if dest.exists() else []), a.dry_run)
                manifest, reason = check_bundle(dest)
    if manifest is None:
        bundle_next_steps(a.bundle, reason)
        if TESTS[a.test]['bundle'] if a.test else False:
            return 1
        print('\nOnly the host protocol smoke test can run without a bundle.')
    else:
        print(f'\nKernel bundle: {a.bundle}\n  release {manifest["kernel_release"]}'
              f'  built from {manifest.get("source_commit", "?")[:12]}')
        if manifest.get('source_status'):
            print('  (kernel tree had local modifications; see source-changes.patch)')

    key = a.test or menu(manifest is not None, patched)
    if key is None:
        print('Nothing selected.')
        return 0
    blocked = available(key, manifest is not None, patched)
    if blocked:
        # Only reachable through --test: the menu never hands back a blocked key.
        print(f'\nWarning: {key} {blocked}. Running it anyway because you asked for it '
              'by name.', file=sys.stderr)

    test = TESTS[key]
    accel = native_accel(accelerators, debug=(key == 'debug'))
    if a.grep and key != 'boot':
        print('--grep applies to the boot test only; ignoring it here.', file=sys.stderr)
    print(f'\n== {test["title"]} ==')
    for line in wrap('Results: ' + test['results']):
        print('  ' + line)
    for line in wrap('Look for: ' + test['watch']):
        print('  ' + line)
    if key == 'debug':
        print(f'\n  Second terminal:\n    python3 {PROJECT/"scripts/debug-kernel-lab.py"} '
              f'--bundle {a.bundle} --debugger lldb')
    if test['boots']:
        print(f'  Accelerator: {accel}' + ('  (TCG is correctness only, not a rate measurement)'
                                           if accel == 'tcg' else ''))
    if test['backend']:
        ensure_backend(a.dry_run)
    print()
    steps = steps_for(key, a.bundle, qemu, accel, a.packets, a.pps, a.timeout)
    if key == 'boot':
        return run_boot(steps[0], a.grep, a.dry_run)
    for step in steps:
        try:
            run(step, a.dry_run)
        except subprocess.CalledProcessError as e:
            print(f'\nStep failed with exit status {e.returncode}. '
                  'The run directory printed above holds the logs.', file=sys.stderr)
            return e.returncode
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
    except subprocess.CalledProcessError as failure:
        raise SystemExit(f'command failed with exit status {failure.returncode}: '
                         + ' '.join(map(str, failure.cmd)))
