#!/usr/bin/env python3
"""BusyBox udhcpc hook; invoked only inside this lab's client namespace."""
import ipaddress
import os
import json
import subprocess
import sys

# Refuse invocation in the host network namespace.
if os.stat('/proc/self/ns/net').st_ino == os.stat('/proc/1/ns/net').st_ino:
    raise SystemExit('refusing DHCP changes in the initial network namespace')
if os.environ.get('interface') != 'eth0':
    raise SystemExit('unexpected DHCP interface')
event = sys.argv[1]
if event in ('deconfig', 'bound', 'renew'):
    address = None
    if event != 'deconfig':
        address = ipaddress.IPv4Interface(os.environ['ip'] + '/' + os.environ.get('subnet', '255.255.255.0'))
        routers = [str(ipaddress.IPv4Address(x)) for x in os.environ.get('router', '').split()]
    current = json.loads(subprocess.check_output(['ip', '-j', '-4', 'addr', 'show', 'dev', 'eth0'], text=True))
    addresses = [f"{entry['local']}/{entry['prefixlen']}" for link in current
                 for entry in link['addr_info'] if entry.get('scope') == 'global']
    # A renewal of the same lease must not tear down the interface address.
    if address and str(address) not in addresses:
        subprocess.run(['ip', 'addr', 'add', str(address), 'dev', 'eth0'], check=True)
    for old in addresses:
        if address is None or old != str(address):
            subprocess.run(['ip', 'addr', 'del', old, 'dev', 'eth0'], check=True)
    if address and routers:
        subprocess.run(['ip', 'route', 'replace', 'default', 'via', routers[0], 'dev', 'eth0'], check=True)
    else:
        subprocess.run(['ip', 'route', 'del', 'default'], check=False)
# Never modify the host-shared resolv.conf. DNS is outside this UDP-address lab.
