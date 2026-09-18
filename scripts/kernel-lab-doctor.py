#!/usr/bin/env python3
"""Check every prerequisite for the QEMU kernel lab and print how to fix gaps.

Reports on this host and on the selected builder (--builder). Read-only: it
installs nothing and modifies nothing, beyond starting and removing a container
when the builder is Docker. Exit status 1 means a required check failed;
warnings alone still exit 0.
"""
import argparse
import json
from pathlib import Path
import platform
import shutil
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lab_builders  # noqa: E402

OK, WARN, FAIL = 'ok', 'warn', 'FAIL'
VM_TOOLS = ('make', 'gcc', 'flex', 'bison', 'bc', 'cpio', 'nm', 'readelf', 'busybox', 'cargo')


class Report:
    def __init__(self): self.rows, self.failed = [], False

    def add(self, status, name, detail, fix=''):
        if status == FAIL: self.failed = True
        self.rows.append((status, name, detail, fix))

    def show(self, title):
        print(f'\n== {title} ==')
        width = max((len(r[1]) for r in self.rows), default=0)
        for status, name, detail, fix in self.rows:
            print(f'  [{status:4}] {name:<{width}}  {detail}')
            if fix and status != OK: print(f'{"":>11}{"":<{width}}  -> {fix}')


def probe(cmd, **kw):
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=30, **kw)
        return r.returncode, r.stdout + r.stderr
    except (OSError, subprocess.SubprocessError) as e:
        return 127, str(e)


def check_host(r, qemu, backend):
    v = sys.version_info
    r.add(OK if v >= (3, 11) else FAIL, 'python3',
          f'{v.major}.{v.minor}.{v.micro}', 'run-kernel-lab.py needs Python 3.11+ (hashlib.file_digest)')
    r.add(OK, 'platform', f'{platform.system()} {platform.machine()}')

    if qemu is None:
        found = shutil.which('qemu-system-aarch64')
        qemu = Path(found) if found else None
    if qemu is None or not qemu.is_file():
        r.add(FAIL, 'qemu', 'not found',
              'pass --qemu /path/to/qemu-system-aarch64, or: brew install qemu')
    else:
        code, out = probe([str(qemu), '--version'])
        r.add(OK if not code else FAIL, 'qemu', out.splitlines()[0] if out else 'unusable', str(qemu))
        _, accel = probe([str(qemu), '-accel', 'help'])
        names = accel.split()
        r.add(OK if 'tcg' in names else FAIL, 'qemu accel tcg',
              'available' if 'tcg' in names else 'missing', 'TCG is required for --debug')
        wanted = 'hvf' if platform.system() == 'Darwin' else 'kvm'
        r.add(OK if wanted in names else WARN, f'qemu accel {wanted}',
              'available' if wanted in names else 'missing',
              'only TCG runs will work; fine for correctness, not for performance')
        _, devs = probe([str(qemu), '-device', 'help'])
        _, nets = probe([str(qemu), '-machine', 'virt', '-netdev', 'help'])
        vhost = 'virtio-net-pci' in devs and 'vhost-user' in nets
        r.add(OK if vhost else WARN, 'qemu vhost-user',
              'supported' if vhost else 'missing',
              'needed only for the XDP data path; --no-lab-nic boots without it')
        # Stock QEMU also advertises vhost-user, so the check above cannot tell a
        # lab build from a distribution one. qemu-lab.py records what it applied.
        record = qemu.parent/'qemu-lab.json'
        try: patches = json.loads(record.read_text()).get('patches', [])
        except (OSError, ValueError): patches = None
        patched = patches is not None and any('queue-reset' in p for p in patches)
        r.add(OK if patched else WARN, 'qemu lab patches',
              ', '.join(patches) if patched else
              ('none recorded' if patches is not None else f'no {record.name} beside the binary'),
              'the data path needs the queue-reset patch; build with: '
              'python3 scripts/qemu-lab.py --build-only')

    for name in ('lldb', 'gdb', 'aarch64-elf-gdb'):
        path = shutil.which(name)
        if path: r.add(OK, f'debugger {name}', path)
    if not any(shutil.which(n) for n in ('lldb', 'gdb', 'aarch64-elf-gdb')):
        r.add(FAIL, 'debugger', 'none found', 'install Xcode command line tools, or: brew install gdb')

    r.add(OK if shutil.which('cargo') else WARN, 'cargo',
          shutil.which('cargo') or 'missing',
          'needed only to build the host vhost_user_net backend')
    if backend is not None:
        r.add(OK if backend.is_file() else WARN, 'host backend',
              str(backend) if backend.is_file() else 'not built',
              'cargo build --locked --release --example vhost_user_net')


def check_builder(r, builder, source, work):
    def vm(*args):
        return builder.run(list(args), capture=True)

    _, arch = vm('uname', '-sm')
    good = 'Linux' in arch and 'aarch64' in arch
    r.add(OK if good else FAIL, 'vm arch', arch.strip(), 'the builder must be ARM64 Linux')

    _, missing = vm('bash', '-lc',
                    'export PATH="$HOME/.cargo/bin:$PATH"; for t in ' + ' '.join(VM_TOOLS) +
                    '; do command -v $t >/dev/null || echo $t; done')
    gaps = missing.split()
    r.add(OK if not gaps else FAIL, 'vm toolchain', 'complete' if not gaps else 'missing: ' + ' '.join(gaps),
          'sudo apt-get install -y build-essential flex bison bc libssl-dev libelf-dev '
          'busybox-static cpio' + ('; install Rust via rustup' if 'cargo' in gaps else ''))

    code, _ = vm('bash', '-lc', 'readelf -l "$(command -v busybox)" | grep -q INTERP')
    r.add(OK if code else FAIL, 'vm busybox', 'static' if code else 'dynamically linked',
          'sudo apt-get install -y busybox-static  (the initramfs has no shared libraries)')

    code, _ = vm('test', '-f', source + '/Makefile')
    r.add(OK if not code else FAIL, 'kernel source', source + ('' if not code else ' has no Makefile'),
          'pass --source with the kernel tree inside the VM')
    if not code:
        _, rel = vm('bash', '-lc', f'make -s -C {source} kernelversion 2>/dev/null')
        r.add(OK, 'kernel version', rel.strip() or 'unknown')
        code, _ = vm('test', '-e', source + '/.config')
        r.add(OK if code else FAIL, 'source tree clean',
              'no in-tree .config' if code else 'in-tree .config present',
              f'the lab builds out-of-tree; run: make -C {source} mrproper')

    _, free = vm('bash', '-lc', "df -BG --output=avail " + work.rsplit('/', 1)[0] + " | tail -1")
    try: avail = int(free.strip().rstrip('G'))
    except ValueError: avail = -1
    r.add(OK if avail < 0 or avail >= 15 else WARN, 'vm disk',
          f'{avail}G free' if avail >= 0 else 'unknown',
          'a debug-info kernel build plus bundle needs roughly 15G')
    _, mem = vm('bash', '-lc', "free -m | awk '/^Mem:/{print $2}'")
    try: total = int(mem.strip())
    except ValueError: total = -1
    r.add(OK if total < 0 or total >= 2048 else WARN, 'vm memory',
          f'{total}M' if total >= 0 else 'unknown',
          'linking vmlinux with DWARF needs roughly 2G; lower --jobs if the build is killed')


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    lab_builders.add_arguments(p)
    p.add_argument('--source', default='/home/ubuntu/arm64_dev_kernel',
                   help='kernel source tree on the builder, or here for --builder docker')
    p.add_argument('--work', default=None, help='scratch directory on the builder')
    p.add_argument('--skip-builder', action='store_true', help='check this host only')
    p.add_argument('--qemu', type=Path, help='QEMU binary to inspect; default searches PATH')
    p.add_argument('--backend', type=Path, default=Path('target/release/examples/vhost_user_net'))
    a = p.parse_args()

    host = Report()
    check_host(host, a.qemu, a.backend)
    host.show('host')
    failed = host.failed
    # "--instance ''" has always meant "host checks only"; --skip-builder is the
    # way to say that for the builders where an instance name is meaningless.
    if not a.skip_builder and not (a.builder == 'multipass' and not a.instance):
        builder = lab_builders.from_args(a, source=a.source)
        report = Report()
        reason = builder.check()
        report.add(OK if not reason else FAIL, 'builder', builder.describe(), reason)
        if not reason:
            work = a.work or lab_builders.default_work(a.builder)
            source, work = builder.paths(a.source, work)
            try:
                with builder:
                    check_builder(report, builder, source, work)
            except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                report.add(FAIL, 'builder', 'unusable', str(error))
        report.show(f'builder: {builder.describe()}')
        failed = failed or report.failed
    print('\nFAIL entries block the lab; warn entries only limit which runs are possible.'
          if failed else '\nAll required checks passed.')
    print('Next: python3 scripts/kernel-lab-build.py '
          + ' '.join(lab_builders.forward(a)) + ' --output results/kernel-lab')
    return 1 if failed else 0


if __name__ == '__main__': sys.exit(main())
