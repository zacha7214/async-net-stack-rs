#!/usr/bin/env python3
"""Linux namespace lab. Explicit up/down lifecycle; no host routes or NIC changes."""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parent.parent


def run(*args, capture=False, check=True):
    return subprocess.run([str(a) for a in args], check=check, text=True,
                          stdout=subprocess.PIPE if capture else None)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('command', choices=['up', 'down', 'status', 'profile', 'fault', 'stack', 'dhcp-server', 'dhcp-client', 'compare'])
    p.add_argument('--name', default='udplab')
    p.add_argument('--config', type=Path, default=ROOT / 'config/udp-lab.json')
    p.add_argument('--profile', default='lossy')
    p.add_argument('--direction', choices=['both', 'request', 'reply'], default='both')
    p.add_argument('--fault', choices=['link-down', 'route-blackhole', 'address-change', 'restore'], default='restore')
    p.add_argument('--binary', type=Path, default=ROOT / 'target/release/examples/tun_pool')
    p.add_argument('--output', type=Path, default=Path('udp-results.json'))
    p.add_argument('--repeats', type=int, default=3)
    a = p.parse_args()
    if sys.platform != 'linux' or os.geteuid() != 0:
        p.error('requires root on Linux; use a disposable VM')
    if not re.fullmatch(r'[a-z][a-z0-9-]{0,19}', a.name) or not 1 <= a.repeats <= 100:
        p.error('invalid lab name or repeats (1..100)')
    state = Path('/run/async-net-labs') / a.name
    ns = {role: f'{a.name}-{role}' for role in ['client', 'router', 'server']}

    def ip(role, *args, **kw):
        return run('ip', '-n', ns[role], *args, **kw)

    def execute(role, *args, **kw):
        return run('ip', 'netns', 'exec', ns[role], *args, **kw)

    def owned():
        if not (state / 'owner.json').is_file():
            raise SystemExit('lab has no ownership marker; run up first')
        if json.loads((state / 'owner.json').read_text()) != ns:
            raise SystemExit('lab ownership mismatch')

    def profile(name, direction='both'):
        profiles = json.loads(a.config.read_text())['profiles']
        opts = profiles[name]
        if not isinstance(opts, list) or not all(isinstance(x, str) for x in opts):
            raise ValueError('profile must be a list of tc arguments')
        for dev, selected in [('s0', 'request'), ('c0', 'reply')]:
            if direction in ['both', selected]:
                execute('router', 'tc', 'qdisc', 'replace', 'dev', dev, 'root', 'netem', *opts)

    if a.command == 'up':
        existing = {line.split()[0] for line in run('ip', 'netns', 'list', capture=True).stdout.splitlines()}
        if state.exists() or any(n in existing for n in ns.values()):
            raise SystemExit('lab already exists or names collide; inspect status/down')
        state.mkdir(parents=True, mode=0o700)
        created = []
        try:
            for name in ns.values():
                run('ip', 'netns', 'add', name)
                created.append(name)
            (state / 'owner.json').write_text(json.dumps(ns))
            for role in ns:
                ip(role, 'link', 'set', 'lo', 'up')
            for role, peer, subnet in [('client', 'c0', '1'), ('server', 's0', '2')]:
                ip('router', 'link', 'add', peer, 'type', 'veth', 'peer', 'name', 'eth0', 'netns', ns[role])
                ip('router', 'addr', 'add', f'10.80.{subnet}.1/24', 'dev', peer)
                ip('router', 'link', 'set', peer, 'up')
                ip(role, 'addr', 'add', f'10.80.{subnet}.2/24', 'dev', 'eth0')
                ip(role, 'link', 'set', 'eth0', 'up')
                ip(role, 'route', 'add', 'default', 'via', f'10.80.{subnet}.1')
            for role in ['router', 'server']:
                execute(role, 'sysctl', '-qw', 'net.ipv4.ip_forward=1')
            ip('router', 'route', 'add', '10.77.0.0/24', 'via', '10.80.2.2')
            execute('server', 'ip', 'tuntap', 'add', 'dev', 'labtun', 'mode', 'tun')
            ip('server', 'addr', 'add', '10.77.0.1/24', 'dev', 'labtun')
            ip('server', 'link', 'set', 'labtun', 'up')
            profile(a.profile)
        except BaseException:
            for name in reversed(created):
                run('ip', 'netns', 'del', name, check=False)
            for f in state.iterdir():
                f.unlink()
            state.rmdir()
            raise
        print(json.dumps(dict(namespaces=ns, target='10.77.0.2:9000', profile=a.profile)))
        return
    owned()
    if a.command == 'down':
        # Never silently orphan processes or kill unknown namespace users.
        for name in ns.values():
            if run('ip', 'netns', 'pids', name, capture=True).stdout.strip():
                raise SystemExit('namespace still has processes; stop stack/DHCP/probes first')
        for name in reversed(list(ns.values())):
            run('ip', 'netns', 'del', name)
        for f in state.iterdir():
            f.unlink()
        state.rmdir()
    elif a.command == 'status':
        for role in ns:
            print(role, flush=True)
            ip(role, '-br', 'addr')
            ip(role, 'route', 'show', 'table', 'all')
            ip(role, 'neigh', 'show')
            execute(role, 'tc', '-s', 'qdisc', 'show')
    elif a.command == 'profile':
        profile(a.profile, a.direction)
    elif a.command == 'fault':
        if a.fault == 'link-down':
            ip('client', 'link', 'set', 'eth0', 'down')
        elif a.fault == 'route-blackhole':
            ip('client', 'route', 'replace', 'blackhole', '10.77.0.0/24')
        elif a.fault == 'address-change':
            ip('client', 'addr', 'flush', 'dev', 'eth0', 'scope', 'global')
            ip('client', 'addr', 'add', '10.80.1.99/24', 'dev', 'eth0')
            ip('client', 'route', 'replace', 'default', 'via', '10.80.1.1')
        else:
            ip('client', 'link', 'set', 'eth0', 'up')
            ip('client', 'route', 'del', 'blackhole', '10.77.0.0/24', check=False)
            ip('client', 'addr', 'flush', 'dev', 'eth0', 'scope', 'global')
            ip('client', 'addr', 'add', '10.80.1.2/24', 'dev', 'eth0')
            ip('client', 'route', 'replace', 'default', 'via', '10.80.1.1')
            ip('client', 'neigh', 'flush', 'dev', 'eth0')
    elif a.command == 'stack':
        os.execvp('ip', ['ip', 'netns', 'exec', ns['server'], str(a.binary.resolve()), '--seconds', '0', '--workers', '1'])
    elif a.command == 'dhcp-server':
        os.execvp('ip', ['ip', 'netns', 'exec', ns['router'], 'dnsmasq', '--keep-in-foreground', '--conf-file=/dev/null',
            '--port=0', '--interface=c0', '--bind-interfaces', '--dhcp-authoritative',
            '--dhcp-range=10.80.1.50,10.80.1.80,255.255.255.0,2m', '--dhcp-option=3,10.80.1.1',
            f'--dhcp-leasefile={state}/leases', '--pid-file=', '--log-dhcp', '--log-facility=-', '--user=root'])
    elif a.command == 'dhcp-client':
        hook = ROOT / 'scripts/udp-dhcp-hook.py'
        os.execvp('ip', ['ip', 'netns', 'exec', ns['client'], 'udhcpc', '-f', '-i', 'eth0', '-s', str(hook)])
    elif a.command == 'compare':
        metadata = dict(config=json.loads(a.config.read_text()), namespaces=ns,
                        kernel=run('uname', '-a', capture=True).stdout.strip(),
                        tc=run('tc', '-V', capture=True).stdout.strip(), snapshots=[])
        rows = []
        try:
            for repeat in range(a.repeats):
                for scenario in ['clean', 'lossy', 'bufferbloat', 'burst-loss']:
                    profile(scenario)
                    # Drain prior qdisc traffic after replacement. Rotate order to
                    # reduce always-running-the-burst-first bias.
                    time.sleep(1)
                    for policy in (['burst', 'paced'] if repeat % 2 == 0 else ['paced', 'burst']):
                        result = execute('client', sys.executable, ROOT / 'scripts/udp-policy-probe.py',
                                         '--policy', policy, capture=True)
                        rows.append(dict(repeat=repeat, profile=scenario, **json.loads(result.stdout)))
                        metadata['snapshots'].append(dict(repeat=repeat, profile=scenario, policy=policy,
                            qdisc=execute('router', 'tc', '-s', 'qdisc', 'show', capture=True).stdout))
                        a.output.with_suffix('.environment.json').write_text(json.dumps(metadata, indent=2))
                        a.output.write_text(json.dumps(rows, indent=2))
                        time.sleep(1)
        finally:
            profile(a.profile)
        run(sys.executable, ROOT / 'scripts/udp-policy-probe.py', '--report', a.output)


if __name__ == '__main__':
    main()
