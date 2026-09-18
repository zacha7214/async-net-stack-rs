#!/usr/bin/env python3
"""Where the kernel lab build runs: this machine, an SSH host, a container or a VM.

The driver needs exactly two things from a builder — run a command, optionally
feeding it stdin, and copy a finished directory back here — because the project
payload, the work-directory marker and `latest-bundle.txt` are already expressed
in terms of those two. Everything specific to Multipass, SSH or Docker lives in
this module, so adding a backend does not touch the build logic.

Builders are context managers. Docker creates a container on entry and removes it
on exit; the others have nothing to set up.
"""
from pathlib import Path
import platform
import os
import shlex
import shutil
import subprocess

KINDS = ('multipass', 'ssh', 'docker', 'local')
DEFAULT_IMAGE = 'async-net-kernel-lab:24.04'
DEFAULT_VOLUME = 'async-net-kernel-lab-work'
CONTAINER_SOURCE = '/src'
# A pair of kernel files whose names differ only in case. If they are the same
# file, the checkout sits on a case-insensitive filesystem and is missing content.
CASE_PROBE = ('net/netfilter/xt_RATEEST.c', 'net/netfilter/xt_rateest.c')
CONTAINER_WORK = '/work'
# The lab kernel is ARM64 today. See "Other architectures" in docs/kernel-lab.md
# for what makes this a derived value rather than a constant.
TARGET_PLATFORM = 'linux/arm64'


def _run(argv, stdin=None, capture=False, check=False, timeout=None):
    """Run argv here. Returns (exit code, stdout, stderr); the texts are '' unless captured.

    stdout and stderr stay separate on purpose. A transport writes its own
    diagnostics to stderr -- ssh announces host keys, docker prints daemon
    warnings -- and merging those into the output would corrupt every value the
    callers parse out of it, from nproc to the bundle path.
    """
    argv = [str(x) for x in argv]
    try:
        if capture:
            r = subprocess.run(argv, stdin=stdin, capture_output=True, text=True, timeout=timeout)
            out, err = r.stdout, r.stderr
        else:
            r = subprocess.run(argv, stdin=stdin, timeout=timeout)
            out = err = ''
    except (OSError, subprocess.SubprocessError) as error:
        if check:
            raise
        return 127, '', str(error)
    if check and r.returncode:
        raise subprocess.CalledProcessError(r.returncode, argv, output=out, stderr=err)
    return r.returncode, out, err


def case_collapsed(source):
    """True when a kernel tree has lost files to a case-insensitive filesystem."""
    first, second = (Path(source)/name for name in CASE_PROBE)
    try:
        return first.is_file() and second.is_file() and os.path.samefile(first, second)
    except OSError:
        return False


def _detail(out, err, fallback):
    """The last meaningful line of a failed command, for an error message."""
    for text in (err, out):
        lines = [line for line in text.strip().splitlines() if line.strip()]
        if lines:
            return lines[-1]
    return fallback


class Builder:
    """Two primitives, plus the path translation a container needs."""

    kind = 'builder'

    def describe(self):
        return self.kind

    def check(self):
        """'' when this builder is usable, otherwise the reason and its fix."""
        return ''

    def paths(self, source, work):
        """The (source, work) paths as the builder itself sees them."""
        return source, work

    def run(self, argv, stdin=None, capture=False, check=False):
        """Returns (exit code, stdout). Transport chatter on stderr is not mixed in."""
        raise NotImplementedError

    def pull(self, remote_dir, local_parent):
        """Copy remote_dir to local_parent/<basename>, which must not exist yet."""
        raise NotImplementedError

    def capture(self, argv):
        return self.run(argv, capture=True)[1].strip()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


class LocalBuilder(Builder):
    kind = 'local'

    def describe(self):
        return f'this machine ({platform.system()} {platform.machine()})'

    def check(self):
        if platform.system() != 'Linux':
            return (f'the local builder needs Linux, and this is {platform.system()}; '
                    'use --builder docker, ssh or multipass')
        return ''

    def run(self, argv, stdin=None, capture=False, check=False):
        code, out, _ = _run(argv, stdin, capture, check)
        return code, out

    def pull(self, remote_dir, local_parent):
        shutil.copytree(remote_dir, Path(local_parent)/Path(remote_dir).name, symlinks=True)


class SshBuilder(Builder):
    """Any host reachable over SSH: UTM, Parallels, VMware, Lima, a Pi, a cloud instance."""

    kind = 'ssh'

    def __init__(self, target, options=()):
        self.target = target
        extra = [word for option in options for word in ('-o', option)]
        # BatchMode keeps a misconfigured host from blocking on a password prompt.
        self.prefix = ['ssh', '-o', 'BatchMode=yes', *extra, target]

    def describe(self):
        return f'ssh {self.target}'

    def check(self):
        if not shutil.which('ssh'):
            return 'ssh is not on PATH'
        code, out, err = _run(self.prefix + ['true'], capture=True, timeout=60)
        if code:
            return (f'cannot run commands on {self.target}: {_detail(out, err, "ssh failed")}; '
                    'check ~/.ssh/config and that key authentication succeeds without a prompt')
        return ''

    def run(self, argv, stdin=None, capture=False, check=False):
        # ssh concatenates its command arguments and hands the result to the
        # remote shell, so the command has to survive one round of shell quoting.
        code, out, _ = _run(self.prefix + [shlex.join(str(x) for x in argv)], stdin, capture, check)
        return code, out

    def pull(self, remote_dir, local_parent):
        remote_dir = str(remote_dir)
        command = shlex.join(['tar', '-C', str(Path(remote_dir).parent), '-cf', '-',
                              Path(remote_dir).name])
        # tar over the connection: one round trip, and modes and symlinks survive.
        source = subprocess.Popen(self.prefix + [command], stdout=subprocess.PIPE)
        try:
            extract = subprocess.run(['tar', '-xf', '-', '-C', str(local_parent)],
                                     stdin=source.stdout)
        finally:
            source.stdout.close()
            sent = source.wait()
        if sent or extract.returncode:
            raise RuntimeError(f'copying {remote_dir} from {self.target} failed')


class MultipassBuilder(Builder):
    kind = 'multipass'

    def __init__(self, instance):
        self.instance = instance

    def describe(self):
        return f'multipass instance {self.instance}'

    def check(self):
        if not shutil.which('multipass'):
            return 'multipass is not on PATH; use --builder docker, ssh or local'
        code, out, _ = _run(['multipass', 'info', self.instance, '--format', 'csv'],
                            capture=True, timeout=60)
        if code:
            return (f'instance {self.instance!r} not found; '
                    f'multipass launch --name {self.instance} 24.04')
        if 'Running' not in out:
            return f'instance {self.instance!r} is not running; multipass start {self.instance}'
        return ''

    def run(self, argv, stdin=None, capture=False, check=False):
        code, out, _ = _run(['multipass', 'exec', self.instance, '--', *argv],
                            stdin, capture, check)
        return code, out

    def pull(self, remote_dir, local_parent):
        _run(['multipass', 'transfer', '--recursive', f'{self.instance}:{remote_dir}',
              str(local_parent)], check=True)


class DockerBuilder(Builder):
    """A throwaway container on whatever container runtime is already here.

    On macOS the runtime (Docker Desktop, Colima, Podman machine, Lima) still runs
    a Linux VM, but it is one shared VM nobody has to name or maintain, and the
    build environment inside it is disposable. The kernel tree is bind-mounted
    from this machine; the work directory is a named volume, so `build/` and
    `cargo-target/` survive between runs and rebuilds stay incremental.
    """

    kind = 'docker'

    def __init__(self, engine, source=None, image=DEFAULT_IMAGE, volume=DEFAULT_VOLUME,
                 target_platform=TARGET_PLATFORM, keep=False, source_volume=None):
        self.engine = engine
        # No source is needed to read a finished bundle out of the work volume.
        self.source = Path(source).expanduser().resolve() if source and not source_volume else None
        self.source_volume = source_volume
        self.image, self.volume, self.platform, self.keep = image, volume, target_platform, keep
        self.container = None
        self.dockerfile = Path(__file__).resolve().with_name('kernel-lab.Dockerfile')

    def describe(self):
        return f'{self.engine} container from {self.image} ({self.platform})'

    def check(self):
        if not shutil.which(self.engine):
            return f'{self.engine} is not on PATH'
        code, out, err = _run([self.engine, 'info', '--format', '{{.ServerVersion}}'],
                              capture=True, timeout=60)
        if code:
            return (f'no {self.engine} daemon: {_detail(out, err, "daemon unreachable")}; '
                    'start one, for example '
                    'colima start --arch aarch64 --cpu 4 --memory 6 --disk 60')
        if self.source is not None:
            if not self.source.is_dir():
                return f'--source {self.source} is not a directory on this machine'
            if not (self.source/'Makefile').is_file():
                return (f'{self.source} has no Makefile; with --builder docker, --source is a '
                        'kernel tree on THIS machine, bind-mounted into the container')
        if not self.dockerfile.is_file():
            return f'missing {self.dockerfile}'
        return ''

    def paths(self, source, work):
        # The container sees the bind-mounted tree and its own work volume.
        return CONTAINER_SOURCE, CONTAINER_WORK

    def __enter__(self):
        if _run([self.engine, 'image', 'inspect', self.image], capture=True)[0]:
            print(f'Building the builder image {self.image} (first run only)', flush=True)
            _run([self.engine, 'build', '--platform', self.platform, '--tag', self.image,
                  '--file', str(self.dockerfile), str(self.dockerfile.parent)], check=True)
        # The tree is mounted read-write: an O= build writes only to the work
        # volume, but git refreshes its index while reporting the source commit.
        mounts = ['--volume', f'{self.volume}:{CONTAINER_WORK}']
        if self.source_volume:
            mounts += ['--volume', f'{self.source_volume}:{CONTAINER_SOURCE}']
        elif self.source is not None:
            mounts += ['--volume', f'{self.source}:{CONTAINER_SOURCE}']
        code, out, err = _run([self.engine, 'run', '--detach', '--platform', self.platform,
                               *mounts, self.image, 'sleep', 'infinity'], capture=True)
        if code:
            raise RuntimeError('could not start the builder container: '
                               + _detail(out, err, 'unknown error'))
        self.container = out.strip().splitlines()[-1]
        print(f'Builder container {self.container[:12]} from {self.image}', flush=True)
        if self.source is not None and self.run(['test', '-f', CONTAINER_SOURCE + '/Makefile'])[0]:
            # A path the runtime does not share is mounted as an empty directory
            # rather than refused, so the build would fail much further along.
            self.__exit__()
            raise RuntimeError(
                f'{self.source} is empty inside the container. The container runtime only '
                'shares some of this machine; on macOS that is a fixed set of paths. Move the '
                'kernel tree under a shared path (your home directory, for Colima and Lima) '
                'or add its location to the runtime\'s file sharing settings.')
        return self

    def __exit__(self, *exc):
        if self.container and self.keep:
            print(f'Container kept: {self.engine} exec -it {self.container[:12]} bash', flush=True)
        elif self.container:
            _run([self.engine, 'rm', '--force', self.container], capture=True)
        self.container = None
        return False

    def run(self, argv, stdin=None, capture=False, check=False):
        if self.container is None:
            raise RuntimeError('use the docker builder as a context manager')
        flags = ['--interactive'] if stdin is not None else []
        code, out, _ = _run([self.engine, 'exec', *flags, self.container, *argv],
                            stdin, capture, check)
        return code, out

    def pull(self, remote_dir, local_parent):
        _run([self.engine, 'cp', f'{self.container}:{remote_dir}', str(local_parent)], check=True)


def add_arguments(parser, default_instance='tito-burrito'):
    """The builder selection flags, shared by every script that needs one."""
    group = parser.add_argument_group('builder')
    group.add_argument('--builder', choices=KINDS, default='multipass',
                       help='where the kernel is built (default: %(default)s)')
    group.add_argument('--instance', default=default_instance,
                       help='--builder multipass: instance name (default: %(default)s)')
    group.add_argument('--ssh', metavar='[USER@]HOST',
                       help='--builder ssh: target, as ssh would take it')
    group.add_argument('--ssh-option', action='append', default=[], metavar='OPT',
                       help='--builder ssh: extra ssh -o option, repeatable '
                            '(e.g. IdentityFile=~/.ssh/lab)')
    group.add_argument('--engine', default='docker', choices=('docker', 'podman'),
                       help='--builder docker: container runtime (default: %(default)s)')
    group.add_argument('--image', default=DEFAULT_IMAGE,
                       help='--builder docker: builder image (default: %(default)s)')
    group.add_argument('--volume', default=DEFAULT_VOLUME,
                       help='--builder docker: named volume holding the work directory')
    group.add_argument('--source-volume', metavar='NAME',
                       help='--builder docker: keep the kernel tree in this named volume '
                            'instead of bind-mounting --source. Required on macOS, whose '
                            'filesystem cannot hold a kernel tree (see --clone)')
    group.add_argument('--platform', default=TARGET_PLATFORM,
                       help='--builder docker: container platform (default: %(default)s)')
    group.add_argument('--keep-container', action='store_true',
                       help='--builder docker: leave the container running to inspect it')
    return group


def from_args(a, source=None, need_source=False):
    """Construct the selected builder, or exit with what is missing.

    need_source is for callers that are about to build: reading a finished bundle
    back out of a container's work volume does not need a kernel tree at all.
    """
    if a.builder == 'local':
        return LocalBuilder()
    if a.builder == 'ssh':
        if not a.ssh:
            raise SystemExit('--builder ssh needs --ssh [USER@]HOST')
        return SshBuilder(a.ssh, a.ssh_option)
    if a.builder == 'docker':
        if need_source and not source and not getattr(a, 'source_volume', None):
            raise SystemExit('--builder docker needs --source pointing at a kernel tree here, '
                             'or --source-volume naming a volume that holds one')
        return DockerBuilder(a.engine, source, a.image, a.volume, a.platform,
                             a.keep_container, getattr(a, 'source_volume', None))
    return MultipassBuilder(a.instance)


def forward(a):
    """The builder flags to hand to another lab script, so selection propagates."""
    argv = ['--builder', a.builder]
    if a.builder == 'multipass':
        argv += ['--instance', a.instance]
    elif a.builder == 'ssh':
        argv += ['--ssh', a.ssh]
        for option in a.ssh_option:
            argv += ['--ssh-option', option]
    elif a.builder == 'docker':
        argv += ['--engine', a.engine, '--image', a.image,
                 '--volume', a.volume, '--platform', a.platform]
        if getattr(a, 'source_volume', None):
            argv += ['--source-volume', a.source_volume]
        if a.keep_container:
            argv.append('--keep-container')
    return argv


def default_work(kind):
    """A work directory that exists on the kind of builder actually chosen."""
    if kind == 'docker':
        return CONTAINER_WORK
    if kind == 'local':
        return str(Path.home()/'async-net-kernel-lab')
    return '/home/ubuntu/async-net-kernel-lab'
