#!/usr/bin/env python3
"""Isolated hwsim AP/station lab with optional wmediumd and netem impairments.

Run as root in a disposable Linux guest with two unused hwsim radios. This
supervises only its own processes and namespaces; it never loads/unloads modules.
"""
import argparse
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

QUERY = b'ANSP\x01\x00' + struct.pack('!I', 7)
ADVERT = b'ANSP\x01\x01' + struct.pack('!I', 7)
AP_IP = '10.78.0.1'
STA_IP = '10.78.0.2'
NETEM = {
    'clean': ['limit', '1000'],
    'delay': ['limit', '100', 'delay', '40ms', '10ms'],
    'lossy': ['limit', '100', 'delay', '40ms', '10ms', 'loss', '5%'],
    'congested': ['limit', '32', 'delay', '40ms', '10ms', 'rate', '1mbit'],
    'outage': ['limit', '100', 'loss', '100%'],
}


def emit(event, **fields):
    print(json.dumps(dict(event=event, **fields)), flush=True)


def firmware():
    """Small single-client echo service; bounded socket timeouts permit retries."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp, \
            socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as echo, \
            socket.socket(socket.AF_INET, socket.SOCK_STREAM) as tcp, \
            selectors.DefaultSelector() as poll:
        udp.bind(('', 9000))
        echo.bind((AP_IP, 9002))
        tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        tcp.bind((AP_IP, 9001))
        tcp.listen(8)
        poll.register(udp, selectors.EVENT_READ)
        poll.register(echo, selectors.EVENT_READ)
        poll.register(tcp, selectors.EVENT_READ)
        emit('firmware_ready', udp=9000, tcp=9001)
        while True:
            for key, _ in poll.select():
                if key.fileobj is udp:
                    data, peer = udp.recvfrom(2048)
                    if data == QUERY:
                        udp.sendto(ADVERT, peer)
                elif key.fileobj is echo:
                    data, peer = echo.recvfrom(2048)
                    try:
                        echo.sendto(data, peer)
                    except OSError:
                        # Impairment is an observation; keep serving after a failed send.
                        pass
                else:
                    conn, peer = tcp.accept()
                    with conn:
                        conn.settimeout(2)
                        try:
                            while data := conn.recv(4096):
                                conn.sendall(data)
                        except (TimeoutError, ConnectionError):
                            pass


def probe():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        udp.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        udp.settimeout(1)
        udp.bind((STA_IP, 0))
        udp.sendto(QUERY, ('255.255.255.255', 9000))
        data, peer = udp.recvfrom(2048)
        if data != ADVERT or peer != (AP_IP, 9000):
            raise RuntimeError(f'unexpected discovery response from {peer}')
    payload = b'hwsim TCP echo\n'
    with socket.create_connection((AP_IP, 9001), timeout=2) as tcp:
        tcp.sendall(payload)
        reply = b''
        while len(reply) < len(payload):
            part = tcp.recv(len(payload) - len(reply))
            if not part:
                raise RuntimeError('TCP closed before completing echo')
            reply += part
        if reply != payload:
            raise RuntimeError('TCP echo mismatch')
    emit('wifi_probe_ok', discovery=peer[0], tcp_bytes=len(reply))


def command(*args, ns=None, check=True):
    argv = (['ip', 'netns', 'exec', ns] if ns else []) + list(map(str, args))
    return subprocess.run(argv, check=check, text=True, capture_output=True, timeout=10)


def interfaces(phy):
    return [p.name for p in Path('/sys/class/net').iterdir()
            if (p/'phy80211').exists() and (p/'phy80211').resolve().name == phy]


def hwsim(phy):
    driver = Path('/sys/class/ieee80211')/phy/'device/driver'
    return driver.exists() and driver.resolve().name == 'mac80211_hwsim'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--ap-phy', help='explicit unused hwsim PHY, e.g. phy0')
    p.add_argument('--station-phy', help='explicit unused hwsim PHY, e.g. phy1')
    p.add_argument('--guest-auto', action='store_true',
                   help='dedicated guest only: select exactly two unused hwsim PHYs')
    p.add_argument('--hold', action='store_true', help='keep the lab alive after probes until Ctrl-C')
    p.add_argument('--timeout', type=float, default=30, help='association/probe deadline in seconds')
    p.add_argument('--medium', choices=['kernel', 'wmediumd'], default='kernel')
    p.add_argument('--ap-loss', type=float, default=0.0,
                   help='wmediumd frame loss probability AP -> station, 0..1')
    p.add_argument('--station-loss', type=float, default=0.0,
                   help='wmediumd frame loss probability station -> AP, 0..1')
    p.add_argument('--netem-ap', choices=NETEM, default='clean', help='AP egress profile')
    p.add_argument('--netem-station', choices=NETEM, default='clean', help='station egress profile')
    p.add_argument('--compare', action='store_true',
                   help='measure UDP policies through baseline, congestion, outage and recovery')
    p.add_argument('--role', choices=['firmware', 'probe'], help=argparse.SUPPRESS)
    a = p.parse_args()
    if a.role:
        return firmware() if a.role == 'firmware' else probe()
    if sys.platform != 'linux' or os.geteuid() != 0:
        p.error('run as root inside Linux; no host Wi-Fi hardware is used')
    if not 1 <= a.timeout <= 300:
        p.error('--timeout must be 1..300')
    if not (0 <= a.ap_loss <= 1 and 0 <= a.station_loss <= 1):
        p.error('radio loss probabilities must be 0..1')
    if a.medium == 'kernel' and (a.ap_loss or a.station_loss):
        p.error('radio loss requires --medium wmediumd')
    if a.compare and not Path(__file__).with_name('udp-policy-probe.py').is_file():
        p.error('comparison requires udp-policy-probe.py beside this script; rebuild the guest bundle')
    use_netem = a.compare or a.netem_ap != 'clean' or a.netem_station != 'clean'
    extra_tools = (['tc'] if use_netem else []) + (['wmediumd'] if a.medium == 'wmediumd' else [])
    for tool in ['ip', 'iw', 'hostapd', 'wpa_supplicant', 'wpa_cli'] + extra_tools:
        if not shutil.which(tool):
            p.error(f'missing {tool}; install iproute2 iw hostapd wpasupplicant wmediumd')
    if a.medium == 'wmediumd':
        # Registration affects the radios' creation netgroup, even after moving
        # their PHYs. Require the dedicated two-radio environment for this mode.
        phys = sorted(x.name for x in Path('/sys/class/ieee80211').glob('phy*') if hwsim(x.name))
        if len(phys) != 2:
            p.error('wmediumd requires a dedicated guest with exactly two hwsim radios')
        for proc in Path('/proc').glob('[0-9]*/comm'):
            try:
                if proc.read_text().strip() == 'wmediumd':
                    p.error('an existing wmediumd is running; stop it before starting this lab')
            except (FileNotFoundError, ProcessLookupError):
                pass
    if a.guest_auto:
        if a.ap_phy or a.station_phy:
            p.error('choose --guest-auto OR the two explicit PHYs')
        phys = sorted(x.name for x in Path('/sys/class/ieee80211').glob('phy*') if hwsim(x.name))
        if len(phys) != 2:
            p.error(f'expected exactly two hwsim radios, found {phys}; boot the Wi-Fi kernel bundle')
        a.ap_phy, a.station_phy = phys
    if not a.ap_phy or not a.station_phy or a.ap_phy == a.station_phy:
        p.error('supply distinct --ap-phy and --station-phy, or --guest-auto in a dedicated guest')
    selected = []
    for phy in (a.ap_phy, a.station_phy):
        if not hwsim(phy):
            p.error(f'{phy} is not a mac80211_hwsim radio')
        names = interfaces(phy)
        if len(names) != 1:
            p.error(f'{phy} must have exactly one unused interface, found {names}')
        name = names[0]
        info = json.loads(command('ip', '-j', 'address', 'show', 'dev', name).stdout)[0]
        if 'UP' in info['flags'] or info.get('addr_info'):
            p.error(f'{name} is configured/in use; choose unused hwsim radios')
        if 'type managed' not in command('iw', 'dev', name, 'info').stdout:
            p.error(f'{name} must be an unused managed-mode interface')
        selected.append((phy, name))

    run = Path(tempfile.mkdtemp(prefix='wifi-lab-'))
    # Keep control socket paths short enough for sockaddr_un.
    ap_ns, sta_ns = f'wifi-ap-{os.getpid()}', f'wifi-sta-{os.getpid()}'
    children, logs, namespaces, moved, shaped = [], [], [], [], []
    script = str(Path(__file__).resolve())

    def spawn(ns, label, *argv):
        log = (run/f'{label}.log').open('w')
        logs.append(log)
        prefix = ['ip', 'netns', 'exec', ns] if ns else []
        child = subprocess.Popen(prefix + list(map(str, argv)),
                                 stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        children.append(child)
        return child

    def alive():
        if any(c.poll() is not None for c in children):
            raise RuntimeError(f'a lab process exited; inspect {run}')

    def shape(ns, iface, profile):
        command('tc', 'qdisc', 'replace', 'dev', iface, 'root', 'handle', '1:',
                'netem', *NETEM[profile], ns=ns)
        if (ns, iface) not in shaped:
            shaped.append((ns, iface))
        emit('wifi_netem', namespace=ns, interface=iface, profile=profile)

    def snapshot(label):
        state = {}
        for ns, (_, iface) in zip((ap_ns, sta_ns), selected):
            state[ns] = {}
            commands = [('addresses', ['ip', '-j', 'address', 'show']),
                        ('stations', ['iw', 'dev', iface, 'station', 'dump'])]
            if use_netem:
                commands.append(('qdisc', ['tc', '-s', '-j', 'qdisc', 'show', 'dev', iface]))
            for key, argv in commands:
                result = command(*argv, ns=ns, check=False)
                state[ns][key] = dict(stdout=result.stdout, stderr=result.stderr,
                                      returncode=result.returncode)
        (run/f'{label}.json').write_text(json.dumps(state, indent=2)+'\n')

    # SIGTERM and Ctrl-C both pass through the cleanup below.
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    emit('wifi_lab_start', logs=str(run), ap_namespace=ap_ns, station_namespace=sta_ns)
    try:
        if a.medium == 'wmediumd':
            macs = [json.loads(command('ip', '-j', 'link', 'show', 'dev', iface).stdout)[0]['address']
                    for _, iface in selected]
            conf = run/'wmediumd.cfg'
            conf.write_text('ifaces: { ids = ["'+macs[0]+'", "'+macs[1]+'"]; };\n'
                            'model: { type = "prob"; default_prob = 1.0; links = ('
                            f'(0, 1, {a.ap_loss:.8f}), (1, 0, {a.station_loss:.8f})'
                            '); };\n')
            spawn(None, 'wmediumd', 'wmediumd', '-l', '5', '-c', conf)
            # Start in the radios' original namespace before association. Startup
            # output is a bounded handshake, not proof of successful RF delivery.
            ready_by = time.monotonic() + a.timeout
            while 'REGISTER SENT!' not in (run/'wmediumd.log').read_text():
                alive()
                if time.monotonic() >= ready_by:
                    raise RuntimeError('wmediumd did not report registration; inspect its log')
                time.sleep(0.1)
        versions = {}
        for tool, flag in [('ip', '-V'), ('iw', '--version')] + \
                ([('tc', '-V')] if use_netem else []) + \
                ([('wmediumd', '-V')] if a.medium == 'wmediumd' else []):
            result = command(tool, flag, check=False)
            versions[tool] = result.stdout + result.stderr
        (run/'environment.json').write_text(json.dumps(dict(
            kernel=os.uname().release, arguments=vars(a), radios=selected,
            tool_versions=versions, netem_profiles=NETEM,
            ap_namespace=ap_ns, station_namespace=sta_ns), indent=2)+'\n')
        for ns, (phy, iface), address in zip((ap_ns, sta_ns), selected, (AP_IP, STA_IP)):
            command('ip', 'netns', 'add', ns)
            namespaces.append(ns)
            command('iw', 'phy', phy, 'set', 'netns', 'name', ns)
            moved.append((ns, phy, iface))
            command('ip', 'link', 'set', 'lo', 'up', ns=ns)
            command('ip', 'address', 'add', address+'/24', 'dev', iface, ns=ns)
            command('ip', 'link', 'set', iface, 'up', ns=ns)
        ap_if, sta_if = selected[0][1], selected[1][1]
        ap_conf, sta_conf = run/'hostapd.conf', run/'station.conf'
        # OFDM rates avoid wmediumd's CCK rate-table mismatch on 2.4 GHz.
        rates = 'supported_rates=60 90 120 180 240 360 480 540\nbasic_rates=60 120 240\n' if a.medium == 'wmediumd' else ''
        ap_conf.write_text(f'interface={ap_if}\ndriver=nl80211\nssid=async-net-lab\n'
                           'hw_mode=g\nchannel=1\n'+rates+'wpa=2\nwpa_key_mgmt=WPA-PSK\n'
                           'rsn_pairwise=CCMP\nwpa_passphrase=lab-password-only\n')
        sta_conf.write_text(f'ctrl_interface={run}/ctrl\np2p_disabled=1\nnetwork={{\n'
                            ' ssid="async-net-lab"\n psk="lab-password-only"\n'
                            ' key_mgmt=WPA-PSK\n proto=RSN\n pairwise=CCMP\n}\n')
        spawn(ap_ns, 'hostapd', 'hostapd', '-dd', ap_conf)
        spawn(sta_ns, 'station', 'wpa_supplicant', '-Dnl80211', '-i', sta_if, '-c', sta_conf)
        spawn(ap_ns, 'firmware', sys.executable, script, '--role', 'firmware')
        deadline = time.monotonic() + a.timeout
        while True:
            alive()
            status = command('wpa_cli', '-p', run/'ctrl', '-i', sta_if, 'status', ns=sta_ns, check=False)
            if 'wpa_state=COMPLETED' in status.stdout:
                emit('wifi_associated', interface=sta_if)
                break
            if time.monotonic() >= deadline:
                raise RuntimeError('association timed out')
            time.sleep(0.2)
        # Limited broadcast needs a link route; it is not forwarded through TUN.
        command('ip', 'route', 'add', '255.255.255.255/32', 'dev', sta_if, ns=sta_ns)
        while True:
            alive()
            result = command(sys.executable, script, '--role', 'probe', ns=sta_ns, check=False)
            if result.returncode == 0:
                print(result.stdout, end='', flush=True)
                break
            if time.monotonic() >= deadline:
                raise RuntimeError('discovery/TCP probe timed out: '+result.stderr)
            time.sleep(0.2)
        emit('wifi_lab_pass', logs=str(run))
        # Apply IP egress impairment after the initial association/smoke probe.
        # Radio impairment above also affects authentication/association frames.
        if use_netem:
            shape(ap_ns, ap_if, a.netem_ap)
            shape(sta_ns, sta_if, a.netem_station)
        snapshot('initial')
        if a.compare:
            probe_script = Path(script).with_name('udp-policy-probe.py')
            rows = []
            phases = [('baseline', a.netem_ap, a.netem_station),
                      ('congested', 'congested', 'congested'),
                      ('outage', 'clean', 'outage'),
                      ('recovered', a.netem_ap, a.netem_station)]
            for repeat in range(1, 4):
                for phase, ap_profile, sta_profile in phases:
                    shape(ap_ns, ap_if, ap_profile)
                    shape(sta_ns, sta_if, sta_profile)
                    # Let previously queued packets drain; replacing a qdisc may
                    # discard packets, so changes happen between measurements.
                    time.sleep(1)
                    policies = ('burst', 'paced') if repeat % 2 else ('paced', 'burst')
                    for policy in policies:
                        alive()
                        # Bound each subprocess and continue supervising daemons.
                        child = spawn(sta_ns, f'{repeat}-{phase}-{policy}', sys.executable,
                                      probe_script, '--target', AP_IP, '--port', '9002',
                                      '--count', '300', '--policy', policy)
                        until = time.monotonic() + 30
                        while child.poll() is None:
                            if any(c is not child and c.poll() is not None for c in children):
                                raise RuntimeError('a lab daemon exited during comparison')
                            if time.monotonic() >= until:
                                raise RuntimeError('UDP comparison timed out')
                            time.sleep(0.1)
                        if child.returncode:
                            raise RuntimeError(f'UDP comparison failed; inspect {run}')
                        children.remove(child)
                        alive()
                        row = json.loads((run/f'{repeat}-{phase}-{policy}.log').read_text())
                        row.update(repeat=repeat, profile=phase, medium=a.medium,
                                   ap_loss=a.ap_loss, station_loss=a.station_loss,
                                   netem_ap=ap_profile, netem_station=sta_profile)
                        rows.append(row)
                        (run/'comparison.json').write_text(json.dumps(rows, indent=2)+'\n')
                        emit('wifi_udp_result', **row)
                    snapshot(f'{repeat}-{phase}')
            result = command(sys.executable, probe_script, '--report', run/'comparison.json')
            emit('wifi_comparison_complete', report=result.stdout.strip())
        alive()
        if a.hold:
            print(f'Lab remains active. Probe again: ip netns exec {sta_ns} '
                  f'{sys.executable} {script} --role probe', flush=True)
            while True:
                alive()
                time.sleep(1)
    finally:
        # Do not let a second cancellation interrupt resource cleanup.
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        for child in reversed(children):
            if child.poll() is None:
                try:
                    os.killpg(child.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
        for log in logs:
            log.close()
        cleanup_errors = []
        for ns, iface in reversed(shaped):
            try:
                command('tc', 'qdisc', 'del', 'dev', iface, 'root', ns=ns)
            except (OSError, subprocess.SubprocessError) as error:
                cleanup_errors.append(str(error))
        for ns, phy, iface in reversed(moved):
            for args in [('ip', 'address', 'flush', 'dev', iface),
                         ('ip', 'link', 'set', iface, 'down'),
                         ('iw', 'dev', iface, 'set', 'type', 'managed'),
                         ('iw', 'phy', phy, 'set', 'netns', str(os.getpid()))]:
                try:
                    command(*args, ns=ns)
                except (OSError, subprocess.SubprocessError) as error:
                    cleanup_errors.append(str(error))
        for ns in reversed(namespaces):
            try:
                command('ip', 'netns', 'delete', ns)
            except (OSError, subprocess.SubprocessError) as error:
                cleanup_errors.append(str(error))
        if cleanup_errors:
            emit('wifi_cleanup_failed', errors=cleanup_errors)
            raise RuntimeError('incomplete namespace/radio cleanup')
        emit('wifi_lab_cleaned', logs=str(run))


if __name__ == '__main__':
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f'Wi-Fi lab failed: {error}', file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError):
            print(error.stderr, file=sys.stderr)
        sys.exit(1)
