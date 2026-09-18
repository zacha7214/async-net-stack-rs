#!/usr/bin/env python3
"""Run on ARM64 Linux: build an isolated kernel and self-contained lab initramfs.

Needs gcc, make, flex, bison, bc, libssl-dev, libelf-dev, busybox-static, cpio,
binutils, Python 3, and Rust/Cargo. Does not install/reboot the builder's kernel.
"""
import argparse
import datetime
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile


# Early-boot landmarks exported into the manifest so the debugger helper and the
# documented walkthrough can break on them without re-reading System.map.
EARLY_SYMBOLS = ('_text', 'primary_entry', 'record_mmu_state', 'preserve_boot_args',
                 'init_kernel_el', '__cpu_setup', '__enable_mmu', '__primary_switch',
                 '__primary_switched', 'secondary_startup', '__secondary_switched',
                 'start_kernel', 'setup_arch')


def run(args, **kw):
    print('+ ' + ' '.join(map(str, args)), flush=True)
    return subprocess.run(list(map(str, args)), check=True, **kw)


def output(args, **kw):
    return subprocess.check_output(list(map(str, args)), text=True, **kw).strip()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--source', required=True, type=Path)
    p.add_argument('--work', required=True, type=Path)
    p.add_argument('--jobs', type=int, default=2)
    a = p.parse_args()
    if platform.system() != 'Linux' or platform.machine() not in ('aarch64', 'arm64'):
        p.error('run this builder inside ARM64 Linux')
    if not 1 <= a.jobs <= 64: p.error('jobs must be 1..64')
    source, work = a.source.resolve(), a.work.resolve()
    project = Path(__file__).resolve().parent.parent
    if not (source / 'Makefile').is_file(): p.error('kernel source missing')
    if work == source or source in work.parents: p.error('work must be outside the source tree')
    work.mkdir(parents=True, exist_ok=True)
    marker = work / '.async-net-kernel-lab'
    if not marker.exists() and any(work.iterdir()): p.error('work is nonempty and not owned by this helper')
    marker.touch()
    for tool in ('make', 'gcc', 'flex', 'bison', 'bc', 'cpio', 'nm', 'readelf', 'busybox', 'cargo'):
        if not shutil.which(tool): p.error(f'missing prerequisite: {tool}')
    busybox = Path(shutil.which('busybox')).resolve()
    if 'INTERP' in output(['readelf', '-l', busybox]): p.error('install busybox-static first')
    if (source / '.config').exists(): p.error('source has an in-tree .config; use a clean source checkout for O= builds')
    build = work / 'build'
    build.mkdir(exist_ok=True)
    make = ['make', '-C', source, f'O={build}', 'ARCH=arm64']
    env = os.environ.copy()
    env['KCONFIG_ALLCONFIG'] = str(project / 'scripts/kernel-lab.config')
    run(make + ['allnoconfig'], env=env)
    config = (build / '.config').read_text()
    required = ('ARM64', 'ARM64_4K_PAGES', 'SMP', 'ARM_GIC_V3', 'PCI_HOST_GENERIC',
                'PCI_MSI', 'SERIAL_AMBA_PL011_CONSOLE', 'BLK_DEV_INITRD', 'RD_GZIP',
                'BINFMT_ELF', 'BINFMT_SCRIPT', 'DEVTMPFS', 'PROC_FS', 'SYSFS',
                'FUTEX', 'MULTIUSER', 'BPF_SYSCALL', 'BPF_JIT', 'XDP_SOCKETS',
                'VIRTIO_PCI', 'VIRTIO_NET', 'PROC_PAGE_MONITOR', 'DEBUG_INFO',
                'DEBUG_INFO_DWARF4', 'GDB_SCRIPTS', 'KALLSYMS')
    missing = [s for s in required if f'CONFIG_{s}=y\n' not in config]
    if missing: raise RuntimeError(f'Kconfig did not enable required options: {missing}')
    for s in ('RANDOMIZE_BASE', 'MODULES', 'DEBUG_INFO_REDUCED', 'DEBUG_INFO_SPLIT'):
        if f'CONFIG_{s}=y\n' in config: raise RuntimeError(f'unexpected CONFIG_{s}=y')
    # Build the guest receiver first: it takes seconds, so a manifest or toolchain
    # problem is reported before committing to a long kernel compile.
    cargo_env = os.environ.copy()
    cargo_env['CARGO_TARGET_DIR'] = str(work / 'cargo-target')
    run(['cargo', 'build', '--locked', '--release', '--features', 'xdp',
         '--example', 'xdp_vm_rx'], cwd=project, env=cargo_env)
    run(make + [f'-j{a.jobs}', 'Image', 'scripts_gdb'])
    release = (build / 'include/config/kernel.release').read_text().strip()
    # Publish only after all build/package steps succeed; retain older bundles.
    # The name carries the build time and kernel release because the work
    # directory is sometimes copied out wholesale: "bundle-j2xkot6c" says nothing
    # about which kernel it holds, and several builds accumulate side by side.
    stamp = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
    safe = ''.join(c if c.isalnum() or c in '._+' else '-' for c in release)
    bundle = work / f'bundle-{stamp}-{safe}'
    for attempt in range(2, 100):
        if not bundle.exists(): break
        bundle = work / f'bundle-{stamp}-{safe}-{attempt}'
    bundle.mkdir()
    for src, name in [(build/'arch/arm64/boot/Image', 'Image'), (build/'vmlinux', 'vmlinux'),
                      (build/'System.map', 'System.map'), (build/'.config', 'config')]:
        shutil.copy2(src, bundle/name)
    # Keep source for the early architecture path with the exported symbols.
    for rel in ('arch/arm64', 'init', 'include', 'scripts/gdb'):
        shutil.copytree(source/rel, bundle/'source'/rel,
                        ignore=shutil.ignore_patterns('*.o', '*.a', '*.cmd', '__pycache__'))
    run(['readelf', '-S', build/'vmlinux'], stdout=(bundle/'elf-sections.txt').open('w'))
    with tempfile.TemporaryDirectory(prefix='initramfs-', dir=work) as name:
        root = Path(name)
        for d in ('bin', 'sbin', 'dev', 'proc', 'sys', 'tmp', 'run', 'etc'):
            (root/d).mkdir()
        shutil.copy2(busybox, root/'bin/busybox')
        for applet in output([busybox, '--list']).splitlines():
            if applet != 'busybox': (root/'bin'/applet).symlink_to('busybox')
        (root/'sbin/poweroff').symlink_to('/bin/busybox')
        receiver = work/'cargo-target/release/examples/xdp_vm_rx'
        shutil.copy2(receiver, root/'bin/xdp_vm_rx')
        # ldd is used only on our freshly built executable. Preserve loader paths.
        libs = output(['ldd', receiver])
        for line in libs.splitlines():
            for word in line.split():
                if word.startswith('/'):
                    lib = Path(word)
                    dest = root/str(lib).lstrip('/')
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(lib.resolve(), dest)
        shutil.copy2(project/'scripts/kernel-lab-init.sh', root/'init')
        (root/'init').chmod(0o755)
        names = b'\0'.join(str(f.relative_to(root)).encode() for f in sorted(root.rglob('*'))) + b'\0'
        archive = subprocess.run(['cpio', '--null', '-o', '--format=newc', '--owner=0:0'],
                                 input=names, cwd=root, stdout=subprocess.PIPE, check=True).stdout
        with (bundle/'initramfs.cpio.gz').open('wb') as f:
            with gzip.GzipFile(fileobj=f, mode='wb', mtime=0) as z: z.write(archive)
    symbols = {}
    for line in output(['nm', '-n', build/'vmlinux']).splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[2] in EARLY_SYMBOLS:
            symbols[parts[2]] = '0x' + parts[0]
    manifest = dict(kernel_release=release, source=str(source), build=str(build),
                    source_commit=output(['git', '-C', source, 'rev-parse', 'HEAD']),
                    source_status=output(['git', '-C', source, 'status', '--short']),
                    compiler=output(['gcc', '--version']).splitlines()[0], symbols=symbols,
                    cmdline='console=ttyAMA0 earlycon=pl011,0x09000000 rdinit=/init nokaslr loglevel=8 panic=0',
                    sha256={f.name: hashlib.sha256(f.read_bytes()).hexdigest()
                            for f in bundle.iterdir() if f.is_file()})
    (bundle/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    (bundle/'source-changes.patch').write_text(output(['git', '-C', source, 'diff', 'HEAD']))
    (work/'latest-bundle.txt').write_text(str(bundle)+'\n')
    print(f'KERNEL_LAB_BUNDLE={bundle}', flush=True)


if __name__ == '__main__': main()
