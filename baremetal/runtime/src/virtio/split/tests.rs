use super::*;
use alloc::{rc::Rc, vec, vec::Vec};
use core::cell::{Cell, RefCell};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    R16(usize),
    R32(usize),
    W16(usize, u16),
    W32(usize, u32),
    W64(usize, u64),
    Fence(Barrier),
}
struct Image {
    bytes: Vec<u8>,
    log: Vec<Event>,
    record: bool,
}
impl Image {
    fn u16(&self, o: usize) -> u16 {
        u16::from_le_bytes(self.bytes[o..o + 2].try_into().unwrap())
    }
    fn u32(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.bytes[o..o + 4].try_into().unwrap())
    }
    fn u64(&self, o: usize) -> u64 {
        u64::from_le_bytes(self.bytes[o..o + 8].try_into().unwrap())
    }
    fn put16(&mut self, o: usize, v: u16) {
        self.bytes[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(&mut self, o: usize, v: u32) {
        self.bytes[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn event(&mut self, e: Event) {
        if self.record {
            self.log.push(e);
        }
    }
    fn complete(&mut self, layout: Layout, size: u16, id: u32, len: u32) {
        let idx = self.u16(layout.used + 2);
        let offset = layout.used + 4 + 8 * (idx % size) as usize;
        self.put32(offset, id);
        self.put32(offset + 4, len);
        self.put16(layout.used + 2, idx.wrapping_add(1));
    }
}
struct Memory(Rc<RefCell<Image>>);
// Synchronous inert device image; tests never dereference descriptor addresses.
unsafe impl RingMemory for Memory {
    fn len(&self) -> usize {
        self.0.borrow().bytes.len()
    }
    fn device_address(&self) -> u64 {
        0x4000_0000
    }
    unsafe fn read16(&mut self, o: usize) -> u16 {
        assert_eq!(o % 2, 0);
        let mut m = self.0.borrow_mut();
        m.event(Event::R16(o));
        m.u16(o)
    }
    unsafe fn read32(&mut self, o: usize) -> u32 {
        assert_eq!(o % 4, 0);
        let mut m = self.0.borrow_mut();
        m.event(Event::R32(o));
        m.u32(o)
    }
    unsafe fn write16(&mut self, o: usize, v: u16) {
        assert_eq!(o % 2, 0);
        let mut m = self.0.borrow_mut();
        m.event(Event::W16(o, v));
        m.put16(o, v);
    }
    unsafe fn write32(&mut self, o: usize, v: u32) {
        assert_eq!(o % 4, 0);
        let mut m = self.0.borrow_mut();
        m.event(Event::W32(o, v));
        m.put32(o, v);
    }
    unsafe fn write64(&mut self, o: usize, v: u64) {
        assert_eq!(o % 8, 0);
        let mut m = self.0.borrow_mut();
        m.event(Event::W64(o, v));
        m.bytes[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn barrier(&mut self, b: Barrier) {
        self.0.borrow_mut().event(Event::Fence(b));
    }
}
#[derive(Debug)]
struct Owner(Rc<Cell<usize>>);
impl Drop for Owner {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}
fn owner() -> (Owner, Rc<Cell<usize>>) {
    let c = Rc::new(Cell::new(0));
    (Owner(c.clone()), c)
}
fn ring<T>(size: u16) -> (SplitRing<Memory, T>, Rc<RefCell<Image>>) {
    let layout = Layout::new(size).unwrap();
    let image = Rc::new(RefCell::new(Image {
        bytes: vec![0xaa; layout.bytes],
        log: Vec::new(),
        record: true,
    }));
    let mut ring = SplitRing::new(Memory(image.clone()), size).unwrap();
    unsafe {
        ring.activate();
    }
    image.borrow_mut().log.clear();
    (ring, image)
}
fn buf(address: u64, writable: bool) -> Buffer {
    Buffer {
        address,
        length: 12,
        writable,
    }
}

#[test]
fn direct_chain_publication_and_acquisition_order() {
    let (mut q, image) = ring(4);
    let (ownership, dropped) = owner();
    let layout = q.layout;
    let p = unsafe { q.submit(17, &[buf(0x8000, false), buf(0x9000, true)], ownership) }.unwrap();
    assert_eq!(
        p,
        Published {
            head: 0,
            notify: true
        }
    );
    let m = image.borrow();
    assert_eq!(m.u64(0), 0x8000);
    assert_eq!(m.u32(8), 12);
    assert_eq!(m.u16(12), 1);
    assert_eq!(m.u16(14), 1);
    assert_eq!(m.u64(16), 0x9000);
    assert_eq!(m.u16(28), 2);
    assert_eq!(m.u16(30), 0);
    assert_eq!(
        &m.log[8..],
        &[
            Event::W16(layout.available + 4, 0),
            Event::Fence(Barrier::Publish),
            Event::W16(layout.available + 2, 1),
            Event::Fence(Barrier::Notify),
            Event::R16(layout.used),
        ]
    );
    drop(m);
    assert_eq!(q.free_descriptors(), 2);
    assert_eq!(dropped.get(), 0);
    assert!(q.poll().unwrap().is_none());
    image.borrow_mut().complete(layout, 4, 0, 7);
    image.borrow_mut().log.clear();
    let completion = q.poll().unwrap().unwrap();
    assert_eq!((completion.token, completion.written), (17, 7));
    assert_eq!(
        image.borrow().log,
        [
            Event::R16(layout.used + 2),
            Event::Fence(Barrier::Acquire),
            Event::R32(layout.used + 4),
            Event::R32(layout.used + 8)
        ]
    );
    assert_eq!(q.free_descriptors(), 4);
    assert_eq!(dropped.get(), 0);
    drop(completion);
    assert_eq!(dropped.get(), 1);
    unsafe {
        q.quiesce();
    }
}

#[test]
fn full_queue_returns_owner_and_reordered_completion_reuses_only_its_chain() {
    let (mut q, image) = ring(4);
    let buffers = [buf(0x8000, false), buf(0x9000, false)];
    let a = unsafe { q.submit(1, &buffers, 11u64) }.unwrap();
    let b = unsafe { q.submit(2, &buffers, 22u64) }.unwrap();
    assert_eq!(q.free_descriptors(), 0);
    assert_eq!(unsafe { q.submit(3, &buffers, 33) }, Err((Error::Full, 33)));
    image
        .borrow_mut()
        .complete(q.layout, 4, b.head as u32, u32::MAX);
    let c = q.poll().unwrap().unwrap();
    assert_eq!((c.token, c.owner), (2, 22)); // TX length is not a byte count
    let c = unsafe { q.submit(3, &buffers, 33) }.unwrap();
    assert_eq!(c.head, b.head);
    image.borrow_mut().complete(q.layout, 4, a.head as u32, 0);
    image.borrow_mut().complete(q.layout, 4, c.head as u32, 0);
    assert_eq!(q.poll().unwrap().unwrap().owner, 11);
    assert_eq!(q.poll().unwrap().unwrap().owner, 33);
    assert_eq!(q.free_descriptors(), 4);
    unsafe {
        q.quiesce();
    }
}

#[test]
fn validation_and_token_failures_do_not_touch_ring_or_consume_owner() {
    let (mut q, image) = ring(4);
    for (buffers, error) in [
        (vec![], Error::EmptyChain),
        (vec![buf(1, false); 5], Error::ChainTooLong),
        (
            vec![Buffer {
                address: 0,
                length: 0,
                writable: false,
            }],
            Error::InvalidBuffer,
        ),
        (vec![buf(u64::MAX, false)], Error::InvalidBuffer),
        (vec![buf(1, true), buf(2, false)], Error::InvalidDirection),
        (
            vec![
                Buffer {
                    address: 0,
                    length: u32::MAX,
                    writable: true,
                },
                buf(2, true),
            ],
            Error::ChainTooLong,
        ),
    ] {
        assert_eq!(unsafe { q.submit(1, &buffers, 7) }, Err((error, 7)));
        assert!(image.borrow().log.is_empty());
        assert_eq!(q.free_descriptors(), 4);
    }
    unsafe { q.submit(u64::MAX, &[buf(1, false)], 7) }.unwrap();
    let log = image.borrow().log.len();
    assert_eq!(
        unsafe { q.submit(u64::MAX, &[buf(1, false)], 8) },
        Err((Error::TokenOrder, 8))
    );
    assert_eq!(
        unsafe { q.submit(0, &[buf(1, false)], 8) },
        Err((Error::TokenOrder, 8))
    );
    assert_eq!(image.borrow().log.len(), log);
    unsafe {
        q.quiesce();
    }
}

#[test]
fn malformed_progress_ids_and_lengths_retain_owners_until_reset() {
    for (id, len, index, error) in [
        (99, 0, 1, Error::UsedId),
        (1, 0, 1, Error::UsedId),
        (0, 25, 1, Error::UsedLength),
        (0, 0, 2, Error::UsedIndex),
        (0, 0, 65535, Error::UsedIndex),
    ] {
        let (mut q, image) = ring(4);
        let (ownership, dropped) = owner();
        unsafe { q.submit(1, &[buf(1, true), buf(2, true)], ownership) }.unwrap();
        image.borrow_mut().complete(q.layout, 4, id, len);
        image.borrow_mut().put16(q.layout.used + 2, index);
        assert!(matches!(q.poll(),Err(e) if e==error));
        assert_eq!(q.free_descriptors(), 2);
        assert_eq!(dropped.get(), 0);
        assert!(matches!(q.poll(), Err(Error::Faulted)));
        unsafe {
            q.quiesce();
        }
        drop(q);
        assert_eq!(dropped.get(), 1);
    }
}

#[test]
fn duplicate_completion_cannot_release_another_active_head() {
    let (mut q, image) = ring(4);
    unsafe { q.submit(1, &[buf(1, true)], 10) }.unwrap();
    unsafe { q.submit(2, &[buf(2, true)], 20) }.unwrap();
    image.borrow_mut().complete(q.layout, 4, 0, 1);
    image.borrow_mut().complete(q.layout, 4, 0, 1);
    assert_eq!(q.poll().unwrap().unwrap().owner, 10);
    assert!(matches!(q.poll(), Err(Error::UsedId)));
    assert_eq!(q.pending(), 1);
    unsafe {
        q.quiesce();
    }
}

#[test]
fn wraps_16_bit_indices_without_losing_capacity_or_tokens() {
    let (mut q, image) = ring(2);
    image.borrow_mut().record = false;
    for token in 0..65540u64 {
        let p = unsafe { q.submit(token, &[buf(0x8000, true), buf(0x9000, true)], token) }.unwrap();
        image.borrow_mut().complete(q.layout, 2, p.head as u32, 24);
        let c = q.poll().unwrap().unwrap();
        assert_eq!((c.token, c.owner), (token, token));
        assert_eq!(q.free_descriptors(), 2);
    }
    assert_eq!(image.borrow().u16(q.layout.available + 2), 4);
    assert_eq!(q.consumed, 4);
    unsafe {
        q.quiesce();
    }
}

#[test]
fn suppression_is_checked_after_publication_and_does_not_reject_ownership() {
    let (mut q, image) = ring(1);
    image.borrow_mut().put16(q.layout.used, 1);
    let p = unsafe { q.submit(1, &[buf(1, false)], 7) }.unwrap();
    assert!(!p.notify);
    assert_eq!(q.pending(), 1);
    assert_eq!(image.borrow().u16(q.layout.available + 2), 1);
    unsafe {
        q.quiesce();
    }
}

#[test]
fn live_drop_quarantines_ring_and_payload_owner() {
    let (mut q, image) = ring(1);
    let (ownership, dropped) = owner();
    unsafe { q.submit(1, &[buf(1, true)], ownership) }.unwrap();
    drop(q);
    assert_eq!(dropped.get(), 0);
    assert_eq!(Rc::strong_count(&image), 2);
}

#[test]
fn mixed_chain_sizes_and_reordering_match_independent_live_set() {
    let (mut q, image) = ring(16);
    image.borrow_mut().record = false;
    let mut live = Vec::<(u16, u64, usize)>::new();
    let mut seed = 11u64;
    for token in 1..4000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        if seed & 3 == 0 && !live.is_empty() {
            let (head, expected, count) = live.swap_remove(seed as usize % live.len());
            image
                .borrow_mut()
                .complete(q.layout, 16, head as u32, 12 * count as u32);
            let c = q.poll().unwrap().unwrap();
            assert_eq!((c.token, c.owner), (expected, expected));
        } else {
            let count = (seed as usize % 5) + 1;
            match unsafe { q.submit(token, &vec![buf(0x8000, true); count], token) } {
                Ok(p) => live.push((p.head, token, count)),
                Err((e, returned)) => {
                    assert_eq!(e, Error::Full);
                    assert_eq!(returned, token);
                }
            }
        }
        assert_eq!(
            q.free_descriptors() as usize,
            16 - live.iter().map(|v| v.2).sum::<usize>()
        );
        assert_eq!(q.pending() as usize, live.len());
    }
    unsafe {
        q.quiesce();
    }
}

#[test]
fn layout_limits_and_short_storage_fail_before_publication() {
    assert_eq!(Layout::new(0), Err(Error::InvalidSize));
    assert_eq!(Layout::new(3), Err(Error::InvalidSize));
    let largest = Layout::new(32768).unwrap();
    assert_eq!(largest.available, 16 * 32768);
    assert_eq!(largest.used % 4, 0);
    let image = Rc::new(RefCell::new(Image {
        bytes: vec![0; 4],
        log: Vec::new(),
        record: true,
    }));
    assert!(matches!(
        SplitRing::<_, ()>::new(Memory(image.clone()), 4),
        Err(Error::InvalidMemory)
    ));
    assert!(image.borrow().log.is_empty());
}

