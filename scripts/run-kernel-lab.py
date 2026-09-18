#!/usr/bin/env python3
"""Boot a built kernel lab bundle on Mac/Linux, with a managed vhost-user generator.

No disk image is needed. --debug stops a single TCG CPU before its first
instruction and exposes a loopback-only GDB stub. Ctrl-C stops both lab processes.

--no-lab-nic boots the kernel alone, with no vhost-user backend and no lab NIC.
That path needs only a stock qemu-system-aarch64 and is the one to use for
debugging early architecture boot code; the vhost-user path additionally needs
the patched QEMU described in docs/vhost-user-lab.md.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--bundle', type=Path, required=True)
    p.add_argument('--qemu', type=Path, required=True)
    p.add_argument('--backend', type=Path, default=Path('target/release/examples/vhost_user_net'))
    p.add_argument('--no-lab-nic', action='store_true',
                   help='boot the kernel alone: no vhost-user backend, no lab NIC, stock QEMU')
    p.add_argument('--accel', choices=['hvf', 'kvm', 'tcg'], default='tcg')
    p.add_argument('--mode', choices=['shell', 'zero-copy', 'copy'], default='shell')
    p.add_argument('--packets', type=int, default=10000)
    p.add_argument('--pps', type=int, default=1000)
    p.add_argument('--debug', action='store_true')
    p.add_argument('--gdb-port', type=int, default=1234)
    p.add_argument('--ssh-port', type=int, default=2222)
    p.add_argument('--timeout', type=int, default=0, help='optional whole-VM time limit in seconds')
    a = p.parse_args()
    bundle, qemu, backend = a.bundle.resolve(), a.qemu.resolve(), a.backend.resolve()
    if a.debug and a.accel != 'tcg': p.error('early debug uses --accel tcg')
    if a.no_lab_nic and a.mode != 'shell':
        p.error('--no-lab-nic has no lab NIC, so only --mode shell is available')
    if not 1 <= a.packets <= 1_000_000_000 or not 0 <= a.pps <= 100_000_000:
        p.error('invalid packet count/rate')
    if not 1024 <= a.gdb_port <= 65535: p.error('invalid GDB port')
    if not a.no_lab_nic and not backend.is_file():
        p.error('build host backend: cargo build --locked --release --example vhost_user_net'
                ' (or pass --no-lab-nic to boot the kernel without it)')
    if not os.access(qemu, os.X_OK): p.error('--qemu is not executable')
    manifest = json.loads((bundle/'manifest.json').read_text())
    for name in ('Image', 'initramfs.cpio.gz', 'vmlinux', 'config'):
        digest = hashlib.file_digest((bundle/name).open('rb'), 'sha256').hexdigest()
        if digest != manifest['sha256'][name]: p.error(f'bundle checksum mismatch: {name}')
    run = Path(tempfile.mkdtemp(prefix='kernel-lab-', dir='/tmp'))
    print(f'Run logs: {run}', flush=True)
    print(f'Kernel: {manifest["kernel_release"]}', flush=True)
    host_log = host_err = generator = None
    if not a.no_lab_nic:
        host_log = (run/'host.jsonl').open('w')
        host_err = (run/'host.log').open('w')
        generator = subprocess.Popen([str(backend), '--socket', str(run/'net.sock'), '--pps', str(a.pps),
                                      '--trace-control'], stdout=host_log, stderr=host_err)
    vm = None
    try:
        if generator is not None:
            deadline = time.monotonic()+10
            while not (run/'net.sock').exists():
                if generator.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('backend failed; inspect '+str(run/'host.log'))
                time.sleep(0.02)
        cmdline = manifest['cmdline']+f' lab.mode={a.mode} lab.packets={a.packets}'
        boot = ['-kernel', str(bundle/'Image'), '-initrd', str(bundle/'initramfs.cpio.gz'),
                '-append', cmdline, '-display', 'none',
                '-serial', 'mon:stdio' if a.mode == 'shell' else 'file:'+str(run/'guest-console.log'),
                '-no-reboot']
        if a.no_lab_nic:
            # Plain direct-kernel boot: no shared RAM object and no virtio NIC, so
            # an unpatched qemu-system-aarch64 is enough for kernel-only debugging.
            args = [str(qemu), '-machine', 'virt', '-accel', a.accel,
                    '-cpu', 'max' if a.accel == 'tcg' else 'host',
                    '-smp', '1' if a.debug else '2', '-m', '1024', '-nic', 'none', *boot]
        else:
            args = [sys.executable, str(Path(__file__).with_name('launch-vhost-vm.py')),
                    '--qemu', str(qemu), '--accel', a.accel, '--socket', str(run/'net.sock'),
                    '--ram-dir', str(run), '--memory-mib', '1024', '--cpus', '1' if a.debug else '2',
                    '--ssh-port', str(a.ssh_port), '--', *boot]
        if a.debug:
            args += ['-S', '-gdb', f'tcp:127.0.0.1:{a.gdb_port}']
            print('Paused at reset. In another terminal run:', flush=True)
            print(f'python3 scripts/debug-kernel-lab.py --bundle {bundle} --port {a.gdb_port}', flush=True)
        # A separate group makes cancellation stop wrapper and QEMU together.
        vm = subprocess.Popen(args, start_new_session=True)
        try:
            code = vm.wait(timeout=a.timeout or None)
        except subprocess.TimeoutExpired:
            # Reaching --timeout is the normal end of an unattended shell-mode run.
            print(f'\nReached --timeout after {a.timeout}s; stopping the VM.', flush=True)
            return
        # A debug session usually ends by quitting QEMU, which is not a lab failure.
        if code and not a.debug: raise RuntimeError(f'QEMU exited {code}; inspect {run}')
        if a.mode != 'shell':
            console = (run/'guest-console.log').read_text(errors='replace')
            records = []
            for line in console.splitlines():
                if line.startswith('{'):
                    try: record = json.loads(line)
                    except json.JSONDecodeError: continue
                    if record.get('event', '').startswith('guest_'): records.append(record)
            (run/'guest.jsonl').write_text(''.join(json.dumps(r)+'\n' for r in records))
            if 'KERNEL_LAB_RESULT=0' not in console:
                print(console[-12000:], file=sys.stderr)
                raise RuntimeError('guest receiver did not succeed; inspect '+str(run/'guest-console.log'))
            subprocess.run([sys.executable, str(Path(__file__).with_name('check-vhost-run.py')),
                            '--host', str(run/'host.jsonl'), '--guest', str(run/'guest.jsonl'),
                            '--mode', a.mode], check=True)
    finally:
        if vm and vm.poll() is None:
            os.killpg(vm.pid, signal.SIGTERM)
            try: vm.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(vm.pid, signal.SIGKILL)
                vm.wait()
        if generator is not None:
            generator.terminate()
            try: generator.wait(timeout=5)
            except subprocess.TimeoutExpired:
                generator.kill(); generator.wait()
            host_log.close(); host_err.close()
        print(f'Host logs retained in {run}', flush=True)


if __name__ == '__main__':
    try: main()
    except KeyboardInterrupt: raise SystemExit(130)
