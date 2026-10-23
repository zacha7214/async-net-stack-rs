use super::*;
use crate::virtio::mmio::{Register, Registers};
use alloc::{boxed::Box, rc::Rc, vec, vec::Vec};
use core::{cell::RefCell, ptr::NonNull};

const BASE: u64 = 0x4000_0000;
#[derive(Clone, Copy, Default)]
struct Queue {
    size: u16,
    ready: u32,
    desc: u64,
    avail: u64,
    used: u64,
    taken: u16,
    completed: u16,
}
#[derive(Clone, Copy, Debug)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
}
struct Peer {
    pointer: *mut u8,
    status: u32,
    feature_sel: u32,
    selected: usize,
    queues: [Queue; 2],
    stuck: bool,
    fault_on_tx: bool,
}
impl Peer {
    fn ptr<T>(&self, address: u64) -> *mut T {
        let offset = address.checked_sub(BASE).unwrap() as usize;
        assert!(offset + core::mem::size_of::<T>() <= 65536);
        assert_eq!(offset % core::mem::align_of::<T>(), 0);
        unsafe { self.pointer.add(offset).cast() }
    }
    fn read16(&self, a: u64) -> u16 {
        unsafe { u16::from_le(self.ptr::<u16>(a).read_volatile()) }
    }
    fn read32(&self, a: u64) -> u32 {
        unsafe { u32::from_le(self.ptr::<u32>(a).read_volatile()) }
    }
    fn read64(&self, a: u64) -> u64 {
        unsafe { u64::from_le(self.ptr::<u64>(a).read_volatile()) }
    }
    fn write16(&self, a: u64, v: u16) {
        unsafe {
            self.ptr::<u16>(a).write_volatile(v.to_le());
        }
    }
    fn write32(&self, a: u64, v: u32) {
        unsafe {
            self.ptr::<u32>(a).write_volatile(v.to_le());
        }
    }
    fn bytes(&self, d: Descriptor) -> Vec<u8> {
        (0..d.length)
            .map(|i| unsafe { self.ptr::<u8>(d.address + i as u64).read_volatile() })
            .collect()
    }
    fn take(&mut self, index: usize) -> (u32, [Descriptor; 2]) {
        assert_eq!(self.status & 4, 4);
        let q = self.queues[index];
        assert_eq!(q.ready, 1);
        assert_ne!(self.read16(q.avail + 2), q.taken, "no published chain");
        let head = self.read16(q.avail + 4 + 2 * (q.taken % q.size) as u64);
        let entry = |id: u16| {
            let at = q.desc + 16 * id as u64;
            (
                Descriptor {
                    address: self.read64(at),
                    length: self.read32(at + 8),
                    flags: self.read16(at + 12),
                },
                self.read16(at + 14),
            )
        };
        let (first, next) = entry(head);
        let (second, _) = entry(next);
        assert_eq!(first.length, 12);
        assert_eq!(first.flags, if index == 0 { 3 } else { 1 });
        assert_eq!(second.flags, if index == 0 { 2 } else { 0 });
        self.queues[index].taken = q.taken.wrapping_add(1);
        (head as u32, [first, second])
    }
    fn complete(&mut self, index: usize, head: u32, length: u32) {
        let q = self.queues[index];
        let at = q.used + 4 + 8 * (q.completed % q.size) as u64;
        self.write32(at, head);
        self.write32(at + 4, length);
        self.write16(q.used + 2, q.completed.wrapping_add(1));
        self.queues[index].completed = q.completed.wrapping_add(1);
    }
    fn receive(&mut self, bytes: &[u8], header: [u8; 12], written: u32) -> u64 {
        let (head, ds) = self.take(0);
        assert!(bytes.len() <= ds[1].length as usize);
        // Synchronous, inert device: write only to published writable ranges,
        // relinquish all access before posting a used entry.
        for (d, data) in [(ds[0], &header[..]), (ds[1], bytes)] {
            for (i, b) in data.iter().enumerate() {
                unsafe {
                    self.ptr::<u8>(d.address + i as u64).write_volatile(*b);
                }
            }
        }
        self.complete(0, head, written);
        ds[1].address
    }
}
#[derive(Clone)]
struct Backend(Rc<RefCell<Peer>>);
// No physical/concurrent DMA. The test peer publishes actual ring memory and
// truthfully stops access before reporting completion/reset. Corruption cases
// alter metadata only; they never write outside published packet ranges.
unsafe impl Registers for Backend {
    unsafe fn read(&mut self, r: Register) -> u32 {
        let p = self.0.borrow();
        match r {
            Register::Magic => 0x74726976,
            Register::Version => 2,
            Register::DeviceId => 1,
            Register::DeviceFeatures => {
                ((mmio::VERSION_1 | mmio::NET_MAC | mmio::NET_STATUS) >> (32 * p.feature_sel))
                    as u32
            }
            Register::Status => p.status,
            Register::QueueNumMax => 64,
            Register::QueueReady => p.queues[p.selected].ready,
            Register::ConfigGeneration | Register::InterruptStatus => 0,
            _ => panic!("unexpected read {r:?}"),
        }
    }
    unsafe fn write(&mut self, r: Register, v: u32) {
        let mut p = self.0.borrow_mut();
        let selected = p.selected;
        let q = &mut p.queues[selected];
        match r {
            Register::Status if v == 0 => {
                if !p.stuck {
                    p.status = 0;
                    p.queues = [Queue::default(); 2];
                }
            }
            Register::Status => p.status = v,
            Register::DeviceFeaturesSel => p.feature_sel = v,
            Register::QueueSel => p.selected = v as usize,
            Register::QueueNum => q.size = v as u16,
            Register::QueueReady => q.ready = v,
            Register::QueueDescLow => q.desc = v as u64,
            Register::QueueDriverLow => q.avail = v as u64,
            Register::QueueDeviceLow => q.used = v as u64,
            Register::QueueDescHigh | Register::QueueDriverHigh | Register::QueueDeviceHigh => {
                assert_eq!(v, 0)
            }
            Register::QueueNotify => {
                if v == 1 && p.fault_on_tx {
                    p.status |= 64;
                }
            }
            Register::DriverFeaturesSel | Register::DriverFeatures | Register::InterruptAck => {}
            _ => panic!("unexpected write {r:?}"),
        }
    }
    unsafe fn config_u8(&mut self, i: usize) -> u8 {
        [0x52, 0x54, 0, 0x12, 0x34, 0x56][i]
    }
    unsafe fn config_u16(&mut self, _: usize) -> u16 {
        1
    }
}
#[repr(align(4096))]
struct Storage([u8; 65536]);
struct Harness {
    // Drop device before backing RAM; failed-stop tests explicitly leak RAM too.
    net: VirtioNet<Backend>,
    dma: DmaAllocator,
    peer: Rc<RefCell<Peer>>,
    storage: Box<Storage>,
}
fn harness(mut config: Config) -> Harness {
    config.reset_poll_budget = 4;
    let mut storage = Box::new(Storage([0; 65536]));
    let pointer = storage.0.as_mut_ptr();
    let dma =
        unsafe { DmaAllocator::new(NonNull::new(pointer).unwrap(), BASE as usize, 65536) }.unwrap();
    let peer = Rc::new(RefCell::new(Peer {
        pointer,
        status: 0,
        feature_sel: 0,
        selected: 0,
        queues: [Queue::default(); 2],
        stuck: false,
        fault_on_tx: false,
    }));
    let net = unsafe { VirtioNet::new(Backend(peer.clone()), dma.clone(), config) }.unwrap();
    Harness {
        net,
        dma,
        peer,
        storage,
    }
}
fn small() -> Config {
    Config {
        queue_size: 4,
        rx_buffers: 3,
        tx_buffers: 4,
        rx_headroom: 73,
        budget: 2,
        ..Config::default()
    }
}
fn packet(net: &mut VirtioNet<Backend>, byte: u8) -> PacketBuf {
    let mut p = net.alloc().unwrap();
    p.set_headroom(79);
    p.set_len(60);
    p.as_mut_packet().fill(byte);
    p
}

#[test]
fn tx_prefix_pressure_reordering_exact_mapping_and_foreign_rejection() {
    let mut h = harness(small());
    let mut packets = [
        packet(&mut h.net, 1),
        packet(&mut h.net, 2),
        packet(&mut h.net, 3),
    ];
    let first_ptr = packets[0].as_slice().as_ptr() as usize;
    assert_eq!(h.net.send(&mut packets).unwrap(), 2);
    assert_eq!(packets[0].capacity(), 0);
    assert_eq!(packets[1].capacity(), 0);
    assert_eq!(packets[2].as_slice(), &[3; 60]);
    assert_eq!(h.net.pending_tx(), 2);
    assert_eq!(h.net.stats().tx_completed, 0);
    let (a, ds) = h.peer.borrow_mut().take(1);
    assert_eq!(h.peer.borrow().bytes(ds[0]), [0; 12]);
    assert_eq!(h.peer.borrow().bytes(ds[1]), [1; 60]);
    assert_eq!(
        ds[1].address - BASE,
        (first_ptr - h.storage.0.as_ptr() as usize) as u64
    );
    let (b, _) = h.peer.borrow_mut().take(1);
    h.peer.borrow_mut().complete(1, b, u32::MAX); // not an RX length
    h.net.poll_at(Duration::ZERO).unwrap();
    assert_eq!(h.net.send(&mut packets[2..]).unwrap(), 1);
    let (c, _) = h.peer.borrow_mut().take(1);
    h.peer.borrow_mut().complete(1, c, 0);
    h.peer.borrow_mut().complete(1, a, 0);
    h.net.poll_at(Duration::ZERO).unwrap();
    assert_eq!(h.net.pending_tx(), 0);
    assert_eq!(h.net.stats().tx_completed, 3);
    let pool = PacketPool::new(1, 2048, 64);
    let mut foreign = pool.alloc().unwrap();
    foreign.set_len(60);
    foreign.as_mut_packet().fill(9);
    let mut batch = [packet(&mut h.net, 4), foreign];
    assert_eq!(h.net.send(&mut batch).unwrap(), 1);
    assert_eq!(batch[1].as_slice(), &[9; 60]);
    assert!(h.net.send(&mut batch[1..]).is_err());
    assert_eq!(batch[1].as_slice(), &[9; 60]);
    assert_eq!(h.net.stats().tx_submitted, 4);
}

#[test]
fn rx_bounds_pool_pressure_replenishment_and_zero_copy_echo() {
    let mut h = harness(small());
    for n in [1, 2] {
        h.peer.borrow_mut().receive(&[n; 60], [0; 12], 72);
    }
    h.net.poll_at(Duration::ZERO).unwrap();
    assert_eq!((h.net.posted_rx(), h.net.ready_rx()), (0, 2));
    let mut out = Vec::new();
    assert_eq!(h.net.recv(1, &mut out).unwrap(), 1);
    assert_eq!(out[0].data_offset(), 73);
    assert_eq!(out[0].as_slice(), &[1; 60]);
    let mut held = vec![out.pop().unwrap()];
    assert_eq!(h.net.posted_rx(), 1);
    let address = h.peer.borrow_mut().receive(&[3; 60], [0; 12], 72);
    assert_eq!(h.net.recv(2, &mut out).unwrap(), 2);
    held.append(&mut out);
    assert_eq!(h.net.posted_rx(), 0); // all three RX frames application-owned
    assert!(h.net.alloc().is_some()); // independent TX reserve
    assert_eq!(h.net.send(&mut held[2..]).unwrap(), 1);
    let (id, ds) = h.peer.borrow_mut().take(1);
    assert_eq!(ds[1].address, address); // same RX payload, no copy
    assert_eq!(h.peer.borrow().bytes(ds[1]), [3; 60]);
    h.peer.borrow_mut().complete(1, id, 0);
    h.net.poll_at(Duration::ZERO).unwrap();
    assert_eq!(h.net.posted_rx(), 1);
    drop(held);
    h.net.poll_at(Duration::ZERO).unwrap();
    assert_eq!(h.net.posted_rx(), 2);
    h.peer.borrow_mut().receive(&[4; 60], [0; 12], 72);
    assert_eq!(h.net.recv(1, &mut out).unwrap(), 1);
    assert_eq!(h.net.recv(0, &mut out).unwrap(), 0); // recycles prior output
    assert!(out.is_empty());
    assert_eq!(h.net.stats().rx_delivered, 4);
}

#[test]
fn malformed_headers_and_lengths_drop_only_quiescent_packets_then_refill() {
    let mut h = harness(small());
    let mut cases: Vec<([u8; 12], u32)> = vec![
        ([0; 12], 0),
        ([0; 12], 11),
        ([0; 12], 12),
        ([0; 12], 25),
        ([0; 12], 1527),
    ];
    for (offset, value) in [(0, 1), (0, 2), (0, 4), (1, 1), (10, 2)] {
        let mut header = [0; 12];
        header[offset] = value;
        cases.push((header, 72));
    }
    for (header, written) in cases {
        h.peer.borrow_mut().receive(
            &vec![7; written.saturating_sub(12) as usize],
            header,
            written,
        );
        assert_eq!(h.net.recv(8, &mut Vec::new()).unwrap(), 0);
        assert_eq!(h.net.posted_rx(), 2);
        assert!(h.net.fault().is_none());
    }
    assert_eq!(h.net.stats().rx_invalid, 10);
    let mut header = [0xff; 12];
    header[0] = 128;
    header[1] = 0;
    header[10] = 1;
    header[11] = 0;
    h.peer.borrow_mut().receive(&[8; 14], header, 26);
    assert_eq!(h.net.recv(1, &mut Vec::new()).unwrap(), 1); // unused fields ignored
}

#[test]
fn corruption_faults_both_directions_and_reset_retains_application_handles() {
    let mut h = harness(small());
    h.peer.borrow_mut().receive(&[5; 60], [0; 12], 72);
    let mut out = Vec::new();
    h.net.recv(1, &mut out).unwrap();
    let mut tx = [packet(&mut h.net, 9)];
    h.net.send(&mut tx).unwrap();
    // Oversized reported RX length: ring keeps the suspicious owner, even though
    // our inert test peer has stopped writing. This requires device-wide reset.
    let (head, ds) = h.peer.borrow_mut().take(0);
    h.peer.borrow_mut().complete(0, head, ds[1].length + 13);
    assert!(h.net.poll_at(Duration::ZERO).is_err());
    assert_eq!(h.net.pending_tx(), 1);
    assert!(h.net.alloc().is_none());
    assert!(h.net.fault().is_some());
    let occupied = h.dma.free_pages();
    h.peer.borrow_mut().stuck = true;
    assert_eq!(h.net.shutdown(), Err(mmio::Error::ResetTimeout));
    assert_eq!(h.dma.free_pages(), occupied);
    assert_eq!(h.net.stats().tx_cancelled, 0);
    h.peer.borrow_mut().stuck = false;
    h.net.shutdown().unwrap();
    h.net.shutdown().unwrap();
    assert_eq!(h.net.stats().tx_cancelled, 1);
    assert_eq!(h.net.stats().rx_cancelled, 2);
    drop(h.net);
    assert_eq!(out[0].as_slice(), &[5; 60]);
    assert_eq!(h.dma.free_pages(), 14); // RX arena retained by application handle
    drop(out);
    assert_eq!(h.dma.free_pages(), 16);
}

#[test]
fn prepublication_fault_rolls_back_but_postpublication_fault_keeps_prefix() {
    let mut h = harness(small());
    let mut tx = [packet(&mut h.net, 1), packet(&mut h.net, 2)];
    h.peer.borrow_mut().fault_on_tx = true;
    assert_eq!(h.net.send(&mut tx).unwrap(), 1);
    assert_eq!(tx[0].capacity(), 0);
    assert_eq!(tx[1].as_slice(), &[2; 60]);
    assert_eq!(h.net.fault(), Some(mmio::Error::DeviceNeedsReset));
    assert_eq!(h.net.pending_tx(), 1);
    assert!(h.net.send(&mut tx[1..]).is_err());
    h.net.shutdown().unwrap();
    assert_eq!(h.net.stats().tx_cancelled, 1);
}

#[test]
fn failed_drop_quarantines_both_rings_payloads_and_headers() {
    let mut h = harness(small());
    h.net.send(&mut [packet_from_pool(&h.net.tx_pool)]).unwrap();
    let occupied = h.dma.free_pages();
    h.peer.borrow_mut().stuck = true;
    // Retain backing RAM forever too, honoring DmaAllocator's lifetime contract.
    Box::leak(h.storage);
    drop(h.net);
    assert_eq!(h.dma.free_pages(), occupied);
}
fn packet_from_pool(pool: &PacketPool) -> PacketBuf {
    let mut p = pool.alloc().unwrap();
    p.set_len(60);
    p
}

#[test]
fn budget_clock_empty_batch_and_terminal_stop() {
    let mut config = small();
    config.budget = 1;
    let mut h = harness(config);
    assert_eq!(h.net.posted_rx(), 1);
    h.net.poll_at(Duration::from_secs(1)).unwrap();
    assert_eq!(h.net.posted_rx(), 2);
    assert!(h.net.poll_at(Duration::ZERO).is_err());
    for _ in 0..2 {
        h.peer.borrow_mut().receive(&[0; 60], [0; 12], 72);
    }
    h.net.poll_at(Duration::from_secs(1)).unwrap();
    assert_eq!(h.net.stats().rx_completed, 1);
    assert_eq!(h.net.ready_rx(), 1);
    h.net.shutdown().unwrap();
    assert!(h.net.alloc().is_none());
    assert!(h.net.recv(1, &mut Vec::new()).is_err());
    assert_eq!(h.net.send(&mut []).unwrap(), 0);
}

#[test]
fn invalid_configuration_and_pool_allocation_failure_reclaim_setup() {
    let mut h = harness(small());
    h.net.shutdown().unwrap();
    drop(h.net);
    assert_eq!(h.dma.free_pages(), 16);
    let mut configs = [small(); 7];
    configs[0].queue_size = 1;
    configs[1].rx_buffers = 0;
    configs[2].tx_buffers = 0;
    configs[3].rx_headroom = 2048;
    configs[4].frame_size = 1514;
    configs[5].budget = 0;
    configs[6].reset_poll_budget = 0;
    for config in configs {
        assert!(matches!(
            unsafe { VirtioNet::new(Backend(h.peer.clone()), h.dma.clone(), config) },
            Err(Error::InvalidConfig)
        ));
        assert_eq!(h.dma.free_pages(), 16);
    }
    // Ring setup succeeds, but the default packet pools exceed this test arena.
    assert!(matches!(
        unsafe { VirtioNet::new(Backend(h.peer.clone()), h.dma.clone(), Config::default()) },
        Err(Error::Allocation(_))
    ));
    assert_eq!(h.dma.free_pages(), 16);
    assert_eq!(h.peer.borrow().status, 0);
    let net = unsafe { VirtioNet::new(Backend(h.peer.clone()), h.dma.clone(), small()) }.unwrap();
    drop(net);
    assert_eq!(h.dma.free_pages(), 16);
}

#[test]
fn invalid_tx_lengths_and_token_exhaustion_never_consume_or_publish() {
    let mut h = harness(small());
    let mut tx = [packet(&mut h.net, 7)];
    for length in [0, 13, 1515] {
        tx[0].set_len(length);
        assert!(h.net.send(&mut tx).is_err());
        assert_eq!(tx[0].len(), length);
        assert_eq!(tx[0].capacity(), 2048);
        assert_eq!(h.net.pending_tx(), 0);
        assert_eq!(h.net.headers.available(), 2);
    }
    tx[0].set_len(60);
    h.net.tokens[1] = u64::MAX;
    assert!(h.net.send(&mut tx).is_err());
    assert_eq!(tx[0].as_slice(), &[7; 60]);
    assert_eq!(h.net.stats().tx_submitted, 0);
}
