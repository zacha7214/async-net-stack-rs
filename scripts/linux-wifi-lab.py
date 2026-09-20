#!/usr/bin/env python3
"""Isolated hwsim AP/station lab: WPA2, static IP, UDP discovery and TCP echo.

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


def emit(event, **fields):
    print(json.dumps(dict(event=event, **fields)), flush=True)


def firmware():
    """Small single-client echo service; bounded socket timeouts permit retries."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp, \
            socket.socket(socket.AF_INET, socket.SOCK_STREAM) as tcp, \
            selectors.DefaultSelector() as poll:
        udp.bind(('', 9000))
        tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        tcp.bind((AP_IP, 9001))
        tcp.listen(8)
        poll.register(udp, selectors.EVENT_READ)
        poll.register(tcp, selectors.EVENT_READ)
        emit('firmware_ready', udp=9000, tcp=9001)
        while True:
            for key, _ in poll.select():
                if key.fileobj is udp:
                    data, peer = udp.recvfrom(2048)
                    if data == QUERY:
                        udp.sendto(ADVERT, peer)
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
    p.add_argument('--role', choices=['firmware', 'probe'], help=argparse.SUPPRESS)
    a = p.parse_args()
    if a.role:
        return firmware() if a.role == 'firmware' else probe()
    if sys.platform != 'linux' or os.geteuid() != 0:
        p.error('run as root inside Linux; no host Wi-Fi hardware is used')
    if not 1 <= a.timeout <= 300:
        p.error('--timeout must be 1..300')
    for tool in ('ip', 'iw', 'hostapd', 'wpa_supplicant', 'wpa_cli'):
        if not shutil.which(tool):
            p.error(f'missing {tool}; install iproute2 iw hostapd wpasupplicant')
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
    children, logs, namespaces, moved = [], [], [], []
    script = str(Path(__file__).resolve())

    def spawn(ns, label, *argv):
        log = (run/f'{label}.log').open('w')
        logs.append(log)
        child = subprocess.Popen(['ip', 'netns', 'exec', ns, *map(str, argv)],
                                 stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        children.append(child)
        return child

    def alive():
        if any(c.poll() is not None for c in children):
            raise RuntimeError(f'a lab process exited; inspect {run}')

    # SIGTERM and Ctrl-C both pass through the cleanup below.
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    emit('wifi_lab_start', logs=str(run), ap_namespace=ap_ns, station_namespace=sta_ns)
    try:
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
        ap_conf.write_text(f'interface={ap_if}\ndriver=nl80211\nssid=async-net-lab\n'
                           'hw_mode=g\nchannel=1\nwpa=2\nwpa_key_mgmt=WPA-PSK\n'
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
