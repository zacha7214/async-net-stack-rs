use super::*;
use crate::virtio::mmio::{Register, Registers};
use alloc::boxed::Box;
use async_net_stack_core::{
    api::tcp::{TcpConfig, TcpPool, TcpState},
    device::LoopbackDevice,
    platform::net::{Ipv4Addr, SocketAddrV4},
};
use core::ptr::NonNull;

const BASE: u64 = 0x4000_0000;
#[repr(align(4096))]
struct Storage([u8; 16384]);
struct Peer {
    pointer: *mut u8,
    device: u32,
    version: u32,
    status: u32,
    ready: u32,
    feature_sel: u32,
    driver_sel: u32,
    features: u64,
    desc: u64,
    avail: u64,
    used: u64,
    consumed: u16,
    calls: usize,
    stuck: bool,
    fault_on_notify: bool,
}
impl Peer {
    fn pointer<T>(&self, address: u64) -> *mut T {
        let offset = address.checked_sub(BASE).unwrap() as usize;
        assert!(offset + core::mem::size_of::<T>() <= 16384);
        assert_eq!(offset % core::mem::align_of::<T>(), 0);
        unsafe { self.pointer.add(offset).cast() }
    }
    fn read16(&self, address: u64) -> u16 {
        unsafe { u16::from_le(self.pointer::<u16>(address).read_volatile()) }
    }
    fn read32(&self, address: u64) -> u32 {
        unsafe { u32::from_le(self.pointer::<u32>(address).read_volatile()) }
    }
    fn read64(&self, address: u64) -> u64 {
        unsafe { u64::from_le(self.pointer::<u64>(address).read_volatile()) }
    }
    fn write16(&self, address: u64, value: u16) {
        unsafe {
            self.pointer::<u16>(address).write_volatile(value.to_le());
        }
    }
    fn write32(&self, address: u64, value: u32) {
        unsafe {
            self.pointer::<u32>(address).write_volatile(value.to_le());
        }
    }
    fn complete(&mut self, bytes: &[u8], reported: u32) {
        assert_eq!(self.status & 4, 4);
        assert_eq!(self.ready, 1);
        assert_eq!(self.read16(self.avail + 2), self.consumed.wrapping_add(1));
        assert_eq!(self.read16(self.avail + 4), 0);
        assert_eq!(self.read16(self.desc + 12), 2); // one writable descriptor
        assert!(bytes.len() <= self.read32(self.desc + 8) as usize);
        let address = self.read64(self.desc);
        for (i, &byte) in bytes.iter().enumerate() {
            unsafe {
                self.pointer::<u8>(address + i as u64).write_volatile(byte);
            }
        }
        self.write32(self.used + 4, 0);
        self.write32(self.used + 8, reported);
        self.consumed = self.consumed.wrapping_add(1);
        self.write16(self.used + 2, self.consumed);
    }
}
#[derive(Clone)]
struct Backend(Rc<RefCell<Peer>>);
// Synchronous test peer, no physical DMA. The actual ring memory is used and
// access ends before completion/reset. Fault injection changes metadata/status,
// never writes outside the descriptor. A stuck reset conservatively denies stop.
unsafe impl Registers for Backend {
    unsafe fn read(&mut self, r: Register) -> u32 {
        let mut p = self.0.borrow_mut();
        p.calls += 1;
        match r {
            Register::Magic => 0x74726976,
            Register::Version => p.version,
            Register::DeviceId => p.device,
            Register::DeviceFeatures => (u64::MAX >> (32 * p.feature_sel)) as u32,
            Register::Status => p.status,
            Register::QueueNumMax => 8,
            Register::QueueReady => p.ready,
            Register::InterruptStatus => 3,
            _ => panic!("unexpected entropy register read {r:?}"),
        }
    }
    unsafe fn write(&mut self, r: Register, v: u32) {
        let mut p = self.0.borrow_mut();
        p.calls += 1;
        match r {
            Register::Status if v == 0 => {
                if !p.stuck {
                    p.status = 0;
                    p.ready = 0;
                    p.consumed = 0;
                }
            }
            Register::Status => p.status = v,
            Register::DeviceFeaturesSel => p.feature_sel = v,
            Register::DriverFeaturesSel => p.driver_sel = v,
            Register::DriverFeatures => {
                let shift = 32 * p.driver_sel;
                p.features = (p.features & !(0xffff_ffff << shift)) | ((v as u64) << shift);
            }
            Register::QueueSel => assert_eq!(v, 0),
            Register::QueueNum => assert_eq!(v, 1),
            Register::QueueReady => p.ready = v,
            Register::QueueDescLow => p.desc = v as u64,
            Register::QueueDriverLow => p.avail = v as u64,
            Register::QueueDeviceLow => p.used = v as u64,
            Register::QueueDescHigh | Register::QueueDriverHigh | Register::QueueDeviceHigh => {
                assert_eq!(v, 0)
            }
            Register::QueueNotify => {
                assert_eq!(v, 0);
                if p.fault_on_notify {
                    p.status |= 64;
                }
            }
            Register::InterruptAck => assert_eq!(v, 3),
            _ => panic!("unexpected entropy register write {r:?}"),
        }
    }
    unsafe fn config_u8(&mut self, _: usize) -> u8 {
        panic!("RNG has no configuration fields")
    }
    unsafe fn config_u16(&mut self, _: usize) -> u16 {
        panic!("RNG has no configuration fields")
    }
}
struct Harness {
    rng: VirtioRng<Backend>,
    dma: DmaAllocator,
    peer: Rc<RefCell<Peer>>,
    storage: Box<Storage>,
}
fn config() -> Config {
    Config {
        request_bytes: 8,
        cache_bytes: 16,
        reset_poll_budget: 4,
    }
}
fn harness() -> Harness {
    let mut storage = Box::new(Storage([0; 16384]));
    let pointer = storage.0.as_mut_ptr();
    let dma =
        unsafe { DmaAllocator::new(NonNull::new(pointer).unwrap(), BASE as usize, 16384) }.unwrap();
    let peer = Rc::new(RefCell::new(Peer {
        pointer,
        device: 4,
        version: 2,
        status: 0,
        ready: 0,
        feature_sel: 0,
        driver_sel: 0,
        features: 0,
        desc: 0,
        avail: 0,
        used: 0,
        consumed: 0,
        calls: 0,
        stuck: false,
        fault_on_notify: false,
    }));
    let rng = unsafe { VirtioRng::new(Backend(peer.clone()), dma.clone(), config()) }.unwrap();
    Harness {
        rng,
        dma,
        peer,
        storage,
    }
}
fn reply(h: &Harness, bytes: &[u8]) {
    h.peer.borrow_mut().complete(bytes, bytes.len() as u32);
    h.rng.poll().unwrap();
}

#[test]
fn short_reads_accumulate_and_consumers_never_touch_mmio_or_partial_output() {
    let h = harness();
    assert_eq!(h.peer.borrow().features, mmio::VERSION_1);
    assert_eq!(h.dma.free_pages(), 2);
    assert!(h.rng.tcp_sequence_source().is_err());
    let mut out = [0xaa; 4];
    assert_eq!(
        h.rng.try_fill(&mut out).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(out, [0xaa; 4]);
    for bytes in [&[0x11][..], &[0x22, 0x33][..], &[0x44][..]] {
        reply(&h, bytes);
    }
    assert_eq!(h.rng.available(), 4);
    let calls = h.peer.borrow().calls;
    let mut source = h.rng.tcp_sequence_source().unwrap();
    assert_eq!(source().unwrap(), 0x44332211);
    assert_eq!(source().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(h.peer.borrow().calls, calls);
    assert_eq!(h.rng.stats().short_reads, 3);
    assert_eq!(h.rng.stats().bytes_received, 4);
    assert_eq!(h.rng.stats().bytes_delivered, 4);
    reply(&h, &[9; 8]);
    let mut large = [0xaa; 9];
    assert_eq!(
        h.rng.try_fill(&mut large).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(large, [0xaa; 9]);
    assert_eq!(h.rng.available(), 8);
    assert_eq!(
        h.rng.try_fill(&mut [0; 17]).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    h.rng.try_fill(&mut []).unwrap();
    h.rng.shutdown().unwrap();
    assert_eq!(source().unwrap_err().kind(), io::ErrorKind::NotConnected);
    assert_eq!(h.dma.free_pages(), 4);
}

#[test]
fn cache_capacity_reserves_whole_request_and_wraps_without_reusing_bytes() {
    let h = harness();
    reply(&h, &[0, 1, 2, 3, 4, 5, 6, 7]);
    reply(&h, &[8, 9, 10, 11, 12, 13, 14, 15]);
    assert_eq!(h.rng.available(), 16);
    assert!(!h.rng.pending());
    h.rng.poll().unwrap();
    assert_eq!(h.rng.stats().submitted, 2);
    let mut out = [0; 6];
    h.rng.try_fill(&mut out).unwrap();
    assert_eq!(out, [0, 1, 2, 3, 4, 5]);
    h.rng.poll().unwrap();
    assert!(!h.rng.pending()); // only six bytes free
    let mut out = [0; 3];
    h.rng.try_fill(&mut out).unwrap();
    assert_eq!(out, [6, 7, 8]);
    h.rng.poll().unwrap();
    assert!(h.rng.pending());
    reply(&h, &[16, 17, 18, 19, 20, 21, 22, 23]);
    let mut out = [0; 15];
    h.rng.try_fill(&mut out).unwrap();
    assert_eq!(
        out,
        [9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23]
    );
    assert_eq!(h.rng.stats().bytes_delivered, 24);
}

#[test]
fn empty_completion_faults_without_returning_stale_entropy() {
    let h = harness();
    reply(&h, &[5; 8]);
    h.peer.borrow_mut().complete(&[], 0);
    assert_eq!(h.rng.poll(), Err(Error::EmptyCompletion));
    assert_eq!(h.rng.available(), 0);
    let mut out = [0xaa; 4];
    assert_eq!(
        h.rng.try_fill(&mut out).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(out, [0xaa; 4]);
    assert_eq!(h.rng.poll(), Err(Error::EmptyCompletion));
    h.rng.shutdown().unwrap();
    assert_eq!(h.dma.free_pages(), 4);
}

#[test]
fn corrupt_length_retains_dma_until_confirmed_reset_retry() {
    let h = harness();
    reply(&h, &[1; 8]);
    h.peer.borrow_mut().complete(&[2; 8], 9);
    assert_eq!(
        h.rng.poll(),
        Err(Error::Transport(mmio::Error::Ring(
            super::super::split::Error::UsedLength
        )))
    );
    assert!(h.rng.pending());
    assert_eq!(h.rng.available(), 0);
    h.peer.borrow_mut().stuck = true;
    assert_eq!(
        h.rng.shutdown(),
        Err(Error::Transport(mmio::Error::ResetTimeout))
    );
    assert_eq!(h.dma.free_pages(), 2);
    assert_eq!(h.rng.stats().cancelled, 0);
    h.peer.borrow_mut().stuck = false;
    h.rng.shutdown().unwrap();
    h.rng.shutdown().unwrap();
    assert_eq!(h.dma.free_pages(), 4);
    assert_eq!(h.rng.stats().cancelled, 1);
}

#[test]
fn no_completion_and_notification_fault_never_allow_speculative_reuse() {
    let h = harness();
    for _ in 0..100 {
        h.rng.poll().unwrap();
    }
    assert_eq!(h.rng.stats().submitted, 1);
    assert_eq!(h.rng.available(), 0);
    h.peer.borrow_mut().fault_on_notify = true;
    reply(&h, &[1; 8]); // refill committed before the device requests reset
    assert_eq!(h.rng.stats().submitted, 2);
    assert!(h.rng.pending());
    assert_eq!(
        h.rng.poll(),
        Err(Error::Transport(mmio::Error::DeviceNeedsReset))
    );
    assert_eq!(h.rng.available(), 0);
    h.peer.borrow_mut().stuck = true;
    Box::leak(h.storage); // honor quarantined owners' lifetime contract too
    drop(h.rng);
    assert_eq!(h.dma.free_pages(), 2);
}

#[test]
fn missing_legacy_and_invalid_setup_have_no_entropy_fallback_or_page_leaks() {
    let h = harness();
    h.rng.shutdown().unwrap();
    drop(h.rng);
    for (device, version, expected) in [
        (0, 2, mmio::Error::WrongDevice),
        (1, 2, mmio::Error::WrongDevice),
        (4, 1, mmio::Error::LegacyOrUnknownVersion),
    ] {
        h.peer.borrow_mut().device = device;
        h.peer.borrow_mut().version = version;
        assert!(
            matches!(unsafe { VirtioRng::new(Backend(h.peer.clone()), h.dma.clone(), config()) }, Err(Error::Transport(e)) if e == expected)
        );
        assert_eq!(h.dma.free_pages(), 4);
    }
    h.peer.borrow_mut().device = 4;
    h.peer.borrow_mut().version = 2;
    for (request, cache) in [(0, 16), (17, 16), (8, 4097), (4097, 4097)] {
        assert!(matches!(
            unsafe {
                VirtioRng::new(
                    Backend(h.peer.clone()),
                    h.dma.clone(),
                    Config {
                        request_bytes: request,
                        cache_bytes: cache,
                        ..config()
                    },
                )
            },
            Err(Error::InvalidConfig)
        ));
    }
    let held = h.dma.allocate(3 * 4096, 4096).unwrap();
    assert!(matches!(
        unsafe { VirtioRng::new(Backend(h.peer.clone()), h.dma.clone(), config()) },
        Err(Error::Allocation)
    ));
    drop(held);
    assert_eq!(h.dma.free_pages(), 4);
    let rng = unsafe { VirtioRng::new(Backend(h.peer.clone()), h.dma.clone(), config()) }.unwrap();
    assert!(rng.tcp_sequence_source().is_err());
    assert!(
        matches!(TcpPool::new(LoopbackDevice::new(), TcpConfig::default()), Err(e) if e.kind() == io::ErrorKind::Unsupported)
    );
}

#[test]
fn tcp_source_handles_not_ready_exhaustion_refill_and_last_owner_shutdown() {
    let h = harness();
    // The setup expression itself fails before constructing TCP without entropy.
    let make_tcp = || {
        TcpPool::with_sequence_source(
            LoopbackDevice::new(),
            TcpConfig::default(),
            h.rng.tcp_sequence_source()?,
        )
    };
    assert!(matches!(make_tcp(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    reply(&h, &[0x12, 0x34, 0x56, 0x78]);
    let mut tcp = make_tcp().unwrap();
    let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 15), 30000);
    let remote = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), 9001);
    let first = tcp.connect(local, remote).unwrap();
    assert_eq!(tcp.status(first).unwrap().state, TcpState::SynSent);
    assert_eq!(h.rng.stats().bytes_delivered, 4);
    let second_local = SocketAddrV4::new(*local.ip(), 30001);
    assert_eq!(
        tcp.connect(second_local, remote).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    reply(&h, &[0; 4]); // zero is a valid random value; no probabilistic tests
    let second = tcp.connect(second_local, remote).unwrap();
    assert_eq!(second.as_u64(), first.as_u64() + 1);
    assert_eq!(h.rng.stats().bytes_delivered, 8);
    drop(h.rng); // callback still owns the service and pending region
    assert_eq!(h.dma.free_pages(), 2);
    drop(tcp);
    assert_eq!(h.peer.borrow().status, 0);
    assert_eq!(h.dma.free_pages(), 4);
}

#[test]
fn token_exhaustion_and_reentrant_consumers_fail_without_modifying_output() {
    let h = harness();
    let mut out = [0xaa; 4];
    let borrow = h.rng.0.borrow_mut();
    assert_eq!(
        h.rng.try_fill(&mut out).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(h.rng.poll(), Err(Error::Busy));
    assert_eq!(out, [0xaa; 4]);
    drop(borrow);
    h.rng.0.borrow_mut().token = u64::MAX;
    h.peer.borrow_mut().complete(&[3; 8], 8);
    assert_eq!(h.rng.poll(), Err(Error::TokensExhausted));
    assert_eq!(h.rng.available(), 0);
    h.rng.shutdown().unwrap();
    assert_eq!(h.dma.free_pages(), 4);
}

#[test]
fn entropy_cache_smaller_than_a_sequence_is_valid_rng_but_invalid_tcp_source() {
    let h = harness();
    h.rng.shutdown().unwrap();
    drop(h.rng);
    let rng = unsafe {
        VirtioRng::new(
            Backend(h.peer.clone()),
            h.dma.clone(),
            Config {
                request_bytes: 1,
                cache_bytes: 1,
                ..config()
            },
        )
    }
    .unwrap();
    assert!(matches!(rng.tcp_sequence_source(), Err(e) if e.kind() == io::ErrorKind::InvalidInput));
    h.peer.borrow_mut().complete(&[0], 1);
    rng.poll().unwrap();
    let mut byte = [0xff];
    rng.try_fill(&mut byte).unwrap();
    assert_eq!(byte, [0]);
    rng.shutdown().unwrap();
    assert_eq!(h.dma.free_pages(), 4);
}

#[test]
fn passive_tcp_retries_after_entropy_refill_and_encodes_the_supplied_sequence() {
    use alloc::vec::Vec;
    use async_net_stack_core::{
        device::Device,
        transport::tcp::{self, Segment, ACK, SYN},
    };
    use core::time::Duration;
    let h = harness();
    reply(&h, &[1, 2, 3, 4]);
    let mut tcp = TcpPool::with_sequence_source(
        LoopbackDevice::with_capacity(2, 2),
        TcpConfig::default(),
        h.rng.tcp_sequence_source().unwrap(),
    )
    .unwrap();
    h.rng.try_fill(&mut [0; 4]).unwrap(); // another consumer exhausts the shared cache
    let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 15), 9001);
    let remote = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), 30000);
    tcp.listen(local).unwrap();
    let inject = |pool: &mut TcpPool<LoopbackDevice>| {
        let mut frame = pool.device_mut().alloc().unwrap();
        tcp::build_ipv4(
            &mut frame,
            &Segment {
                source: remote,
                destination: local,
                sequence: 100,
                acknowledgment: 0,
                flags: SYN,
                window: 4096,
                mss: Some(536),
                payload: &[],
            },
        )
        .unwrap();
        assert_eq!(pool.device_mut().send(&mut [frame]).unwrap(), 1);
    };
    inject(&mut tcp);
    assert_eq!(
        tcp.poll(Duration::ZERO, 2).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(tcp.connections().count(), 0);
    assert_eq!(tcp.device_mut().queued(), 0);
    reply(&h, &[0x78, 0x56, 0x34, 0x12]);
    inject(&mut tcp); // remote retransmits its SYN after refill
    assert_eq!(tcp.poll(Duration::ZERO, 2).unwrap(), 1);
    let connection = tcp.connections().next().unwrap();
    assert_eq!(tcp.status(connection).unwrap().state, TcpState::SynReceived);
    let mut out = Vec::new();
    assert_eq!(tcp.device_mut().recv(1, &mut out).unwrap(), 1);
    let synack = tcp::parse_ipv4(out[0].as_slice()).unwrap();
    assert_eq!(synack.flags, SYN | ACK);
    assert_eq!(synack.sequence, 0x12345678);
    assert_eq!(synack.acknowledgment, 101);
    assert_eq!(h.rng.stats().bytes_delivered, 8);
}

