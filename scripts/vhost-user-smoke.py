#!/usr/bin/env python3
"""Exercise the real backend with a small vhost-user frontend, no QEMU or root.

Uses actual SCM_RIGHTS, file mappings, pipes/eventfds, IOTLB messages, split
descriptor chains and C11 atomic indices. This is not a Linux guest driver test.
"""
import argparse
import array
import collections
import ctypes
import json
import mmap
import os
import select
import socket
import struct
import subprocess
import tempfile
import time
from pathlib import Path

GPA = 0x40000000
USER = 0x70000000
IOVA = 0x90000000
RAM = 0x40000
FEATURES = (1 << 27) | (1 << 30) | (1 << 32) | (1 << 33) | (1 << 40)
PROTOCOL = (1 << 3) | (1 << 5) | (1 << 13)


class Frontend:
    def __init__(self, binary, directory, atomics, iommu, split, eventfd=False):
        self.iommu, self.split, self.eventfd = iommu, split, eventfd
        self.atomics = atomics
        self.log = tempfile.TemporaryFile(mode="w+")
        self.error = tempfile.TemporaryFile(mode="w+")
        self.path = str(Path(directory) / f"socket-{iommu}-{split}-{eventfd}")
        self.proc = subprocess.Popen([str(binary), "--socket", self.path, "--pps", "0",
                                      "--size", "1500", "--batch", "3", "--trace-control"],
                                     stdout=self.log, stderr=self.error)
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(5)
        deadline = time.monotonic() + 5
        while not Path(self.path).exists():
            if self.proc.poll() is not None or time.monotonic() > deadline:
                self.error.seek(0)
                raise RuntimeError(self.error.read())
            time.sleep(0.005)
        self.sock.connect(self.path)
        self.backend, peer = socket.socketpair()
        self.backend.settimeout(5)
        self.sent = collections.Counter()
        self.replied = collections.Counter()
        self.replies = {}
        self.misses = 0
        self.owned = []
        self.ramfile = tempfile.TemporaryFile()
        # Non-page-aligned memory-region offset exercises mmap's delta handling.
        self.file_offset = mmap.PAGESIZE + 128
        self.ramfile.truncate(self.file_offset + RAM)
        self.mm = mmap.mmap(self.ramfile.fileno(), self.file_offset + RAM)
        self.address = ctypes.addressof(ctypes.c_char.from_buffer(self.mm)) + self.file_offset
        self.q = {}
        assert struct.unpack("=Q", self.call(1))[0] == FEATURES
        assert struct.unpack("=Q", self.call(15))[0] == PROTOCOL
        self.call(16, struct.pack("=Q", PROTOCOL))
        self.call(3)
        self.call(21, fds=[peer.fileno()], fragment=True)
        peer.close()
        self.call(2, struct.pack("=Q", FEATURES if iommu else FEATURES & ~(1 << 33)))
        table = struct.pack("=IIQQQQ", 1, 0, GPA, RAM, USER, self.file_offset)
        self.call(5, table, [self.ramfile.fileno()], fragment=True)
        self.setup(0, 65530)
        self.setup(1, 0)

    @staticmethod
    def read_message(sock):
        def exact(n):
            b = b""
            while len(b) != n:
                part = sock.recv(n - len(b))
                if not part:
                    raise EOFError("backend closed control channel")
                b += part
            return b
        request, flags, size = struct.unpack("=III", exact(12))
        return request, flags, exact(size)

    def send(self, request, body=b"", fds=(), fragment=False):
        self.sent[request] += 1
        packet = struct.pack("=III", request, 9, len(body)) + body
        ancillary = [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", fds))] if fds else []
        first = packet[:1] if fragment else packet
        n = self.sock.sendmsg([first], ancillary)
        self.sock.sendall(packet[n:])
        return self.sent[request]

    def service(self, timeout=0.05):
        ready, _, _ = select.select([self.sock, self.backend], [], [], timeout)
        for stream in ready:
            request, flags, body = self.read_message(stream)
            if stream is self.sock:
                assert flags == 5, (request, flags)
                self.replied[request] += 1
                self.replies[(request, self.replied[request])] = body
            else:
                assert request == 1 and flags == 1 and len(body) == 32
                address, _, _ = struct.unpack_from("=QQQ", body)
                permission, kind = body[24:26]
                assert kind == 1 and permission in (1, 2, 3), body
                assert IOVA <= address < IOVA + RAM, hex(address)
                page = address & ~4095
                update = struct.pack("=QQQBB6x", page, 4096, USER + page - IOVA, permission, 2)
                self.send(22, update)
                self.misses += 1

    def wait(self, predicate, timeout=5):
        end = time.monotonic() + timeout
        while not predicate():
            if time.monotonic() >= end:
                self.error.seek(0)
                raise TimeoutError(self.error.read())
            self.service(0.001)

    def call(self, request, body=b"", fds=(), fragment=False):
        ticket = self.send(request, body, fds, fragment)
        self.wait(lambda: self.replied[request] >= ticket)
        result = self.replies.pop((request, ticket))
        if request not in (1, 15, 17, 11):
            assert result == bytes(8), (request, result)
        return result

    def pack(self, offset, fmt, *values):
        struct.pack_into(fmt, self.mm, self.file_offset + offset, *values)

    def store(self, offset, value):
        self.atomics.ring_store(self.address + offset, value & 65535)

    def load(self, offset):
        return self.atomics.ring_load(self.address + offset)

    def data_address(self, offset):
        return (IOVA if self.iommu else GPA) + offset

    def ring_address(self, offset):
        return (IOVA if self.iommu else USER) + offset

    def setup(self, index, base, shift=0):
        n = 16 if index == 0 and self.split else 8
        offsets = [0x1000, 0x2000, 0x3000] if index == 0 else [0x4000, 0x5000, 0x6000]
        desc, avail, used = [x + shift for x in offsets]
        self.mm[self.file_offset+desc:self.file_offset+desc+n*16] = bytes(n*16)
        self.mm[self.file_offset+avail:self.file_offset+avail+4+n*2] = bytes(4+n*2)
        self.mm[self.file_offset+used:self.file_offset+used+4+n*8] = bytes(4+n*8)
        self.store(avail+2, base)
        self.store(used+2, base)
        self.call(8, struct.pack("=II", index, n))
        self.call(10, struct.pack("=II", index, base))
        self.call(9, struct.pack("=IIQQQQ", index, 0, self.ring_address(desc),
                               self.ring_address(used), self.ring_address(avail), 0))
        if self.eventfd:
            kick_read = kick_write = os.eventfd(0, os.EFD_NONBLOCK)
            call_read = call_write = os.eventfd(0, os.EFD_NONBLOCK)
            self.owned.extend([kick_read, call_read])
        else:
            kick_read, kick_write = os.pipe()
            call_read, call_write = os.pipe()
            self.owned.extend([kick_read, kick_write, call_read, call_write])
            os.set_blocking(call_read, False)
        self.call(13, struct.pack("=Q", index), [call_write])
        self.call(12, struct.pack("=Q", index), [kick_read])
        self.call(18, struct.pack("=II", index, 1))
        self.q[index] = dict(n=n, desc=desc, avail=avail, used=used, posted=base,
                             consumed=base, kick=kick_write, call=call_read)

    def post(self, index, heads):
        q = self.q[index]
        for head in heads:
            self.pack(q["avail"] + 4 + (q["posted"] % q["n"]) * 2, "<H", head)
            q["posted"] = (q["posted"] + 1) & 65535
        self.store(q["avail"] + 2, q["posted"])
        os.write(q["kick"], struct.pack("=Q", 1))

    def prepare_rx(self, malformed=False):
        q = self.q[0]
        heads = []
        for slot in range(8):
            buf = 0x10000 + slot * 2048
            head = slot * 2 if self.split else slot
            if self.split:
                self.pack(q["desc"] + head * 16, "<QIHH", self.data_address(0x28000+slot*16), 12, 3, head+1)
                self.pack(q["desc"] + (head+1)*16, "<QIHH", self.data_address(buf), 2048, 2, 0)
            else:
                self.pack(q["desc"] + head*16, "<QIHH", self.data_address(buf), 2048,
                          3 if malformed and slot == 0 else 2, head)
            heads.append(head)
        self.post(0, heads)

    def start(self, session, count):
        q = self.q[1]
        header = bytes.fromhex("02000000000202000000000188b55653") + struct.pack("<QQIIQ", 0, count, 64, 0, session)
        data = bytes(12) + header + bytes(64-len(header))
        self.mm[self.file_offset+0x20000:self.file_offset+0x20000+len(data)] = data
        self.pack(q["desc"], "<QIHH", self.data_address(0x20000), len(data), 0, 0)
        self.post(1, [0])
        self.wait(lambda: self.load(q["used"]+2) == q["posted"])

    def receive(self, session, count):
        q = self.q[0]
        received = 0
        while received < count:
            self.wait(lambda: self.load(q["used"]+2) != q["consumed"])
            available = self.load(q["used"]+2)
            heads = []
            while q["consumed"] != available and received < count:
                head, length = struct.unpack_from("<II", self.mm, self.file_offset + q["used"] + 4 + (q["consumed"] % q["n"])*8)
                assert length == 1512
                slot = head//2 if self.split else head
                offset = 0x10000 + slot*2048 + (0 if self.split else 12)
                packet = self.mm[self.file_offset+offset:self.file_offset+offset+1500]
                assert packet[:16] == bytes.fromhex("02000000000202000000000188b55644")
                seq, gpa, size, reserved, sid = struct.unpack_from("<QQIIQ", packet, 16)
                assert (seq, gpa, size, reserved, sid) == (received, GPA+offset, 1500, 0, session)
                expected = bytes(((seq >> ((i & 7)*8)) & 255) ^ ((i//8) & 255) for i in range(1452))
                assert packet[48:] == expected
                received += 1
                q["consumed"] = (q["consumed"] + 1) & 65535
                heads.append(head)
            self.post(0, heads)
        return received

    def close(self, success=True):
        self.sock.close()
        try:
            code = self.proc.wait(timeout=5)
        finally:
            if self.proc.poll() is None:
                self.proc.kill()
                self.proc.wait()
            self.backend.close()
            for fd in self.owned: os.close(fd)
            self.mm.close()
            self.ramfile.close()
        self.log.seek(0)
        events = [json.loads(line) for line in self.log if line.strip()]
        self.error.seek(0)
        errors = self.error.read()
        self.log.close()
        self.error.close()
        assert (code == 0) == success, errors
        return events, errors


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--binary", type=Path, default=Path("target/release/examples/vhost_user_net"))
    a = p.parse_args()
    binary = a.binary.resolve()
    with tempfile.TemporaryDirectory(prefix="vhost-smoke-") as directory:
        library = str(Path(directory) / "atomics.so")
        subprocess.run(["cc", "-std=c11", "-shared", "-fPIC", "-O2",
                        str(Path(__file__).with_name("vhost-ring-atomics.c")), "-o", library], check=True)
        atomics = ctypes.CDLL(library)
        atomics.ring_load.argtypes, atomics.ring_load.restype = [ctypes.c_void_p], ctypes.c_uint16
        atomics.ring_store.argtypes = [ctypes.c_void_p, ctypes.c_uint16]
        event_modes = [False, True] if hasattr(os, "eventfd") else [False]
        cases = 0
        for iommu in [False, True]:
            for split in [False, True]:
                for eventfd in event_modes:
                    f = Frontend(binary, directory, atomics, iommu, split, eventfd)
                    try:
                        # Pool setup resets both queues before START, as virtio-net does.
                        for index in [0, 1]:
                            reply = f.call(11, struct.pack("=II", index, 0))
                            assert struct.unpack("=II", reply)[0] == index
                            f.setup(index, 65530 if index == 0 else 0)
                        if iommu:
                            f.call(22, struct.pack("=QQQBB6x", 0, 2**64-1, 0, 0, 3))
                        # Disabled TX must complete and discard, without starting RX.
                        f.prepare_rx()
                        f.call(18, struct.pack("=II", 1, 0))
                        f.start(41, 67)
                        f.service(0.02)
                        assert f.load(f.q[0]["used"]+2) == 65530
                        f.call(18, struct.pack("=II", 1, 1))
                        f.start(42, 67)
                        assert f.receive(42, 67) == 67
                        # Stop and reconfigure RX at different addresses; no old-ring reuse.
                        old_used = f.q[0]["used"]
                        reply = f.call(11, struct.pack("=II", 0, 0))
                        assert struct.unpack("=II", reply)[1] == f.load(old_used+2)
                        f.setup(0, 0, shift=0x6ff0)
                        f.prepare_rx()
                        f.start(43, 13)
                        assert f.receive(43, 13) == 13
                        if iommu: assert f.misses > 0
                    except BaseException:
                        f.proc.kill()
                        f.error.seek(0)
                        print(f.error.read())
                        f.close(success=False)
                        raise
                    events, _ = f.close()
                    summary = events[-1]
                    assert summary["rx_completed"] == 80 and summary["queue_stops"] == 3
                    assert summary["iotlb_updates"] > 0 if iommu else summary["iotlb_updates"] == 0
                    cases += 1
        # A descriptor loop must terminate the backend, not hang or touch arbitrary RAM.
        f = Frontend(binary, directory, atomics, False, False)
        try:
            f.prepare_rx(malformed=True)
            f.start(99, 1)
            f.proc.wait(timeout=5)
        except EOFError:
            pass
        _, error = f.close(success=False)
        assert "cycle" in error, error
        print(f"passed {cases} shared-memory/IOTLB/reset cases plus malformed descriptor rejection")


if __name__ == "__main__":
    main()
