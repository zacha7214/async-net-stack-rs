#!/usr/bin/env python3
"""Build the kernel lab bundle on an ARM64 Linux builder and export it here.

The builder is chosen with --builder: this machine, a host reachable over SSH, a
throwaway container, or a Multipass VM. Whichever it is, the project tree is
copied into a work directory this helper owns, everything is built there, and
only the finished bundle is copied back. Nothing in this checkout is compiled or
modified.

Run scripts/kernel-lab-doctor.py first to check the builder's prerequisites.
"""
import argparse
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lab_builders  # noqa: E402

# cargo 1.75 in the builder VM refuses a manifest whose declared bench/test
# targets are absent, so every target path in Cargo.toml must be shipped.
PAYLOAD = ('Cargo.toml', 'Cargo.lock', 'src', 'examples', 'scripts', 'benches', 'tests')


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    lab_builders.add_arguments(p)
    p.add_argument('--source', default='/home/ubuntu/arm64_dev_kernel',
                   help='kernel source tree: a path on the builder, or a path on THIS machine '
                        'for --builder docker, which bind-mounts it (default: %(default)s)')
    p.add_argument('--work', default=None,
                   help='scratch directory on the builder, owned by this helper; '
                        '--builder docker uses a named volume instead')
    p.add_argument('--output', type=Path, default=Path('results/kernel-lab'),
                   help='directory here to receive the bundle (default: %(default)s)')
    p.add_argument('--clone', metavar='URL',
                   help='clone this kernel repository into --source on the builder when no tree '
                        'is there yet; the usual way to fill a --source-volume')
    p.add_argument('--clone-ref', default='master', metavar='REF',
                   help='tag or branch for --clone (default: %(default)s; prefer a tag)')
    p.add_argument('--jobs', type=int, default=0, help='0 selects the builder CPU count')
    p.add_argument('--force', action='store_true', help='replace an existing --output directory')
    p.add_argument('--export-only', action='store_true',
                   help='export the last completed bundle without rebuilding')
    a = p.parse_args()
    if not 0 <= a.jobs <= 64:
        p.error('jobs must be 0..64')
    # An export needs no kernel tree: it only reads a finished bundle back out.
    builder = lab_builders.from_args(a, source=None if a.export_only else a.source,
                                     need_source=not a.export_only)
    work = a.work or lab_builders.default_work(a.builder)
    source, work = builder.paths(a.source, work)
    if not work.startswith('/') or not source.startswith('/'):
        p.error('use absolute paths for --source and --work')
    reason = builder.check()
    if reason:
        p.error(reason)
    dest = a.output.resolve()
    if dest.exists() and not a.force:
        p.error(f'{dest} already exists; pass --force to replace it or choose another --output')
    project = Path(__file__).resolve().parent.parent
    remote = work + '/project'
    print(f'Builder: {builder.describe()}', flush=True)
    if (a.builder == 'docker' and not a.source_volume and not a.export_only
            and lab_builders.case_collapsed(a.source)):
        print(f'WARNING: {a.source} sits on a case-insensitive filesystem. A kernel tree has '
              'files\n         whose names differ only in case, and they have collapsed into '
              'one, so this\n         checkout is incomplete. Use --source-volume with --clone '
              'to keep the tree\n         on the builder instead.', file=sys.stderr, flush=True)

    with builder:
        arch = builder.capture(['uname', '-sm'])
        if 'Linux' not in arch or 'aarch64' not in arch:
            raise SystemExit(f'the builder must be ARM64 Linux; it reports {arch!r}. '
                             'The kernel is built natively, not cross-compiled.')
        if a.export_only:
            export_bundle(builder, builder.capture(['cat', work + '/latest-bundle.txt']),
                          dest, a.force)
            return
        prepare_source(builder, a, source)

        # Refuse unmanaged work directories before unpacking anything into them.
        preflight = ('from pathlib import Path; import shutil, sys; p=Path(sys.argv[1]); '
                     'assert not p.exists() or (p/".async-net-kernel-lab").exists() or not any(p.iterdir()), '
                     '"work directory is not owned by this helper"; '
                     'p.mkdir(parents=True,exist_ok=True); (p/".async-net-kernel-lab").touch(); '
                     'shutil.rmtree(p/"project", ignore_errors=True); (p/"project").mkdir()')
        builder.run(['python3', '-c', preflight, work], check=True)
        with tempfile.TemporaryFile() as payload:
            with tarfile.open(fileobj=payload, mode='w') as tar:
                for item in PAYLOAD:
                    tar.add(project / item, arcname=item,
                            filter=lambda x: None if '__pycache__' in x.name else x)
            payload.seek(0)
            builder.run(['tar', '-xf', '-', '-C', remote], stdin=payload, check=True)

        jobs = a.jobs or min(4, int(builder.capture(['nproc']) or 2))
        command = ['python3', './scripts/build-kernel-lab.py', '--source', source,
                   '--work', work, '--jobs', str(jobs)]
        # Run from the copied tree so its relative script path and Cargo.toml resolve.
        script = ('set -o pipefail; export PATH="$HOME/.cargo/bin:$PATH"; cd ' + shlex.quote(remote) +
                  ' && ' + shlex.join(command) + ' 2>&1 | tee ' + shlex.quote(work + '/build.log'))
        print(f'Building in {remote} with -j{jobs}', flush=True)
        code, _ = builder.run(['bash', '-lc', script])
        if code:
            raise SystemExit(f'build failed; the full log is {work}/build.log on the builder')
        export_bundle(builder, builder.capture(['cat', work + '/latest-bundle.txt']), dest, a.force)


def prepare_source(builder, a, source):
    """Make sure a kernel tree is actually present at `source` on the builder."""
    if builder.run(['test', '-f', source + '/Makefile'])[0] == 0:
        return
    if not a.clone:
        raise SystemExit(
            f'no kernel tree at {source} on the builder. Point --source at one, or let this '
            'script fetch it:\n'
            '  --clone https://github.com/torvalds/linux.git --clone-ref vX.Y')
    print(f'Cloning {a.clone} at {a.clone_ref} into {source} on the builder', flush=True)
    if builder.run(['git', 'clone', '--depth', '1', '--branch', a.clone_ref, a.clone, source])[0]:
        raise SystemExit(f'cloning {a.clone} at {a.clone_ref} failed on the builder')


def export_bundle(builder, bundle, dest, force):
    if not bundle:
        raise SystemExit('no completed bundle recorded on the builder; run a build first')
    dest.parent.mkdir(parents=True, exist_ok=True)
    # Copy into a fresh staging directory, then publish under the chosen name.
    with tempfile.TemporaryDirectory(prefix='.kernel-export-', dir=dest.parent) as name:
        builder.pull(bundle, name)
        stage = Path(name) / Path(bundle).name
        if not (stage / 'manifest.json').is_file():
            raise RuntimeError('incomplete bundle transfer')
        if dest.exists():
            if not force:
                raise RuntimeError(f'{dest} appeared during transfer')
            shutil.rmtree(dest)
        stage.rename(dest)
    print(f'Local bundle: {dest}', flush=True)
    print(f'Next: python3 scripts/qemu-lab.py --bundle {dest}', flush=True)


if __name__ == '__main__':
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
    except subprocess.CalledProcessError as failure:
        raise SystemExit(f'command failed with exit status {failure.returncode}: '
                         + ' '.join(map(str, failure.cmd)))
