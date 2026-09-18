#!/usr/bin/env python3
"""Build and export the kernel lab using an existing Multipass ARM64 Linux VM.

Everything is built inside the VM: the project tree is copied into a work
directory the helper owns, and nothing in the host checkout is compiled or
modified. Use scripts/kernel-lab-doctor.py first to check prerequisites.
"""
import argparse
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile

# cargo 1.75 in the builder VM refuses a manifest whose declared bench/test
# targets are absent, so every target path in Cargo.toml must be shipped.
PAYLOAD = ('Cargo.toml', 'Cargo.lock', 'src', 'examples', 'scripts', 'benches', 'tests')


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--instance', default='tito-burrito')
    p.add_argument('--source', default='/home/ubuntu/arm64_dev_kernel',
                   help='kernel source tree inside the VM')
    p.add_argument('--work', default='/home/ubuntu/async-net-kernel-lab',
                   help='scratch directory inside the VM, owned by this helper')
    p.add_argument('--output', type=Path, default=Path('results/kernel-lab'),
                   help='host directory to receive the bundle')
    p.add_argument('--jobs', type=int, default=0, help='0 selects the VM CPU count')
    p.add_argument('--force', action='store_true', help='replace an existing --output directory')
    p.add_argument('--export-only', action='store_true',
                   help='export the last completed bundle without rebuilding')
    a = p.parse_args()
    if not a.work.startswith('/') or not a.source.startswith('/'): p.error('use absolute VM paths')
    if not 0 <= a.jobs <= 64: p.error('jobs must be 0..64')
    if not shutil.which('multipass'): p.error('multipass is not on PATH')
    dest = a.output.resolve()
    if dest.exists() and not a.force:
        p.error(f'{dest} already exists; pass --force to replace it or choose another --output')
    project = Path(__file__).resolve().parent.parent
    remote = a.work + '/project'

    def execute(args, **kw):
        return subprocess.run(['multipass', 'exec', a.instance, '--', *args], check=True, **kw)

    def capture(args):
        return execute(args, capture_output=True, text=True).stdout.strip()

    state = subprocess.run(['multipass', 'info', a.instance, '--format', 'csv'],
                           capture_output=True, text=True)
    if state.returncode or 'Running' not in state.stdout:
        p.error(f'instance {a.instance!r} is not running; start it with: multipass start {a.instance}')

    if a.export_only:
        export_bundle(a.instance, capture(['cat', a.work + '/latest-bundle.txt']), dest, a.force)
        return

    # Refuse unmanaged work directories before unpacking anything into the VM.
    preflight = ('from pathlib import Path; import shutil, sys; p=Path(sys.argv[1]); '
                 'assert not p.exists() or (p/".async-net-kernel-lab").exists() or not any(p.iterdir()), '
                 '"work directory is not owned by this helper"; '
                 'p.mkdir(parents=True,exist_ok=True); (p/".async-net-kernel-lab").touch(); '
                 'shutil.rmtree(p/"project", ignore_errors=True); (p/"project").mkdir()')
    execute(['python3', '-c', preflight, a.work])
    with tempfile.TemporaryFile() as payload:
        with tarfile.open(fileobj=payload, mode='w') as tar:
            for item in PAYLOAD:
                tar.add(project / item, arcname=item,
                        filter=lambda x: None if '__pycache__' in x.name else x)
        payload.seek(0)
        execute(['tar', '-xf', '-', '-C', remote], stdin=payload)

    jobs = a.jobs or min(4, int(capture(['nproc']) or 2))
    command = ['python3', './scripts/build-kernel-lab.py', '--source', a.source,
               '--work', a.work, '--jobs', str(jobs)]
    # Run from the copied tree so its relative script path and Cargo.toml resolve.
    script = ('set -o pipefail; export PATH="$HOME/.cargo/bin:$PATH"; cd ' + shlex.quote(remote) +
              ' && ' + shlex.join(command) + ' 2>&1 | tee ' + shlex.quote(a.work + '/build.log'))
    print(f'Building in {a.instance}:{remote} with -j{jobs}', flush=True)
    try:
        execute(['bash', '-lc', script])
    except subprocess.CalledProcessError:
        raise SystemExit(f'build failed; full log: multipass exec {a.instance} -- '
                         f'tail -50 {a.work}/build.log')
    export_bundle(a.instance, capture(['cat', a.work + '/latest-bundle.txt']), dest, a.force)


def export_bundle(instance, bundle, dest, force):
    if not bundle: raise SystemExit('no completed bundle recorded in the VM; run a build first')
    dest.parent.mkdir(parents=True, exist_ok=True)
    # Transfer into a fresh staging directory, then publish under the chosen name.
    with tempfile.TemporaryDirectory(prefix='.kernel-export-', dir=dest.parent) as name:
        subprocess.run(['multipass', 'transfer', '--recursive', instance + ':' + bundle, name], check=True)
        stage = Path(name) / Path(bundle).name
        if not (stage / 'manifest.json').is_file(): raise RuntimeError('incomplete bundle transfer')
        if dest.exists():
            if not force: raise RuntimeError(f'{dest} appeared during transfer')
            shutil.rmtree(dest)
        stage.rename(dest)
    print(f'Local bundle: {dest}', flush=True)
    print(f'Next: python3 scripts/run-kernel-lab.py --bundle {dest} --qemu /path/to/qemu-system-aarch64',
          flush=True)


if __name__ == '__main__':
    try: main()
    except KeyboardInterrupt: sys.exit(130)
