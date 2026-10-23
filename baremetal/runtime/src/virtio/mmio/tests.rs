use super::*;
use alloc::{boxed::Box, rc::Rc, vec::Vec};
use core::{cell::RefCell, ptr::NonNull};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Access {
    Read(Register),
    Write(Register, u32),
    Byte(usize),
    Half(usize),
}
struct Fake {
    log: Vec<Access>,
    version: u32,
    device: u32,
    magic: u32,
    status: u32,
    offered: u64,
    selected: u64,
    feature_sel: u32,
    driver_sel: u32,
    queue: usize,
    ready: [u32; 2],
    maximum: [u32; 2],
    needs_reset_on_notify: bool,
    reject_features: bool,
    reject_ready: bool,
    stuck_reset: bool,
    reset_delay: usize,
    countdown: usize,
    generation: u32,
    changing: usize,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            log: Vec::new(),
            version: 2,
            device: 1,
            magic: 0x74726976,
            status: 0,
            offered: u64::MAX,
            selected: 0,
            feature_sel: 0,
            driver_sel: 0,
            queue: 0,
            ready: [0; 2],
            maximum: [256; 2],
            needs_reset_on_notify: false,
            reject_features: false,
            reject_ready: false,
            stuck_reset: false,
            reset_delay: 0,
            countdown: 0,
            generation: 0,
            changing: 0,
        }
    }
}
#[derive(Clone)]
struct Backend(Rc<RefCell<Fake>>);
unsafe impl Registers for Backend {
    unsafe fn read(&mut self, reg: Register) -> u32 {
        let mut f = self.0.borrow_mut();
        f.log.push(Access::Read(reg));
        match reg {
            Register::Magic => f.magic,
            Register::Version => f.version,
            Register::DeviceId => f.device,
            Register::Status => {
                if f.countdown != 0 {
                    f.countdown -= 1;
                    if f.countdown == 0 {
                        f.status = 0;
                        f.ready = [0; 2];
                    }
                }
                f.status
            }
            Register::DeviceFeatures => (f.offered >> (32 * f.feature_sel)) as u32,
            Register::QueueReady => f.ready[f.queue],
            Register::QueueNumMax => f.maximum[f.queue],
            Register::ConfigGeneration => {
                if f.changing != 0 {
                    f.changing -= 1;
                    f.generation += 1;
                }
                f.generation
            }
            Register::InterruptStatus => 0xffff_ffff,
            _ => panic!("read of non-readable or unexpected register: {reg:?}"),
        }
    }
    unsafe fn write(&mut self, reg: Register, value: u32) {
        let mut f = self.0.borrow_mut();
        f.log.push(Access::Write(reg, value));
        match reg {
            Register::Status if value == 0 => {
                if f.stuck_reset {
                    return;
                }
                f.countdown = f.reset_delay;
                if f.countdown == 0 {
                    f.status = 0;
                    f.ready = [0; 2];
                }
            }
            Register::Status => {
                f.status = if f.reject_features {
                    value & !FEATURES_OK
                } else {
                    value
                };
            }
            Register::DeviceFeaturesSel => {
                assert!(value < 2);
                f.feature_sel = value;
            }
            Register::DriverFeaturesSel => {
                assert!(value < 2);
                f.driver_sel = value;
            }
            Register::DriverFeatures => {
                let shift = 32 * f.driver_sel;
                f.selected = (f.selected & !(0xffff_ffff << shift)) | ((value as u64) << shift);
            }
            Register::QueueSel => {
                assert!(value < 2);
                f.queue = value as usize;
            }
            Register::QueueReady => {
                if !f.reject_ready {
                    let q = f.queue;
                    f.ready[q] = value;
                }
            }
            Register::QueueNum
            | Register::QueueDescLow
            | Register::QueueDescHigh
            | Register::QueueDriverLow
            | Register::QueueDriverHigh
            | Register::QueueDeviceLow
            | Register::QueueDeviceHigh => {
                assert_eq!(f.ready[f.queue], 0);
            }
            Register::QueueNotify => {
                assert!(f.status & DRIVER_OK != 0);
                assert_eq!(f.ready[value as usize], 1);
                if f.needs_reset_on_notify {
                    f.status |= NEEDS_RESET;
                }
            }
            Register::InterruptAck => assert!(value <= 3),
            _ => panic!("write of non-writable register: {reg:?}"),
        }
    }
    unsafe fn config_u8(&mut self, offset: usize) -> u8 {
        self.0.borrow_mut().log.push(Access::Byte(offset));
        [0x52, 0x54, 0, 0x12, 0x34, 0x56][offset]
    }
    unsafe fn config_u16(&mut self, offset: usize) -> u16 {
        assert_eq!(offset, 6);
        self.0.borrow_mut().log.push(Access::Half(offset));
        1
    }
}
#[repr(align(4096))]
struct Storage([u8; 65536]);
fn allocator(storage: &mut Storage) -> DmaAllocator {
    unsafe {
        DmaAllocator::new(
            NonNull::new(storage.0.as_mut_ptr()).unwrap(),
            0x4000_0000,
            65536,
        )
        .unwrap()
    }
}
fn transport(allocator: DmaAllocator, fake: &Rc<RefCell<Fake>>) -> NetTransport<Backend> {
    unsafe { NetTransport::new(Backend(fake.clone()), allocator, 4).unwrap() }
}

#[test]
fn initialization_order_and_feature_allowlist() {
    let mut storage = Box::new(Storage([0; 65536]));
    let fake = Rc::new(RefCell::new(Fake::default()));
    let mut net = transport(allocator(&mut storage), &fake);
    assert_eq!(net.initialize(), Ok(SUPPORTED));
    assert_eq!(fake.borrow().selected, SUPPORTED);
    assert_eq!(
        fake.borrow().log,
        [
            Access::Read(Register::Magic),
            Access::Read(Register::Version),
            Access::Read(Register::DeviceId),
            Access::Write(Register::Status, 0),
            Access::Read(Register::Status),
            Access::Write(Register::Status, 1),
            Access::Read(Register::Status),
            Access::Write(Register::Status, 3),
            Access::Read(Register::Status),
            Access::Write(Register::DeviceFeaturesSel, 0),
            Access::Read(Register::DeviceFeatures),
            Access::Write(Register::DeviceFeaturesSel, 1),
            Access::Read(Register::DeviceFeatures),
            Access::Write(Register::DriverFeaturesSel, 0),
            Access::Write(Register::DriverFeatures, (NET_MAC | NET_STATUS) as u32),
            Access::Write(Register::DriverFeaturesSel, 1),
            Access::Write(Register::DriverFeatures, 1),
            Access::Write(Register::Status, 11),
            Access::Read(Register::Status),
        ]
    );
    assert_eq!(net.notify(0), Err(Error::WrongState));
    assert_eq!(net.start(), Err(Error::QueueNotReady));
}

#[test]
fn identity_failures_do_not_write_or_read_past_identity() {
    let mut storage = Box::new(Storage([0; 65536]));
    let dma = allocator(&mut storage);
    for (magic, version, device, expected, reads) in [
        (0, 2, 1, Error::BadMagic, 1),
        (0x74726976, 1, 1, Error::LegacyOrUnknownVersion, 2),
        (0x74726976, 2, 0, Error::WrongDevice, 3),
    ] {
        let fake = Rc::new(RefCell::new(Fake {
            magic,
            version,
            device,
            ..Fake::default()
        }));
        assert!(
            matches!(unsafe { NetTransport::<_, ()>::new(Backend(fake.clone()), dma.clone(), 4) }, Err(e) if e == expected)
        );
        assert_eq!(fake.borrow().log.len(), reads);
        assert!(fake
            .borrow()
            .log
            .iter()
            .all(|a| matches!(a, Access::Read(_))));
    }
}

#[test]
fn missing_features_rejected_negotiation_and_device_errors() {
    for (offered, reject, expected) in [
        (NET_MAC, false, Error::MissingFeatures),
        (VERSION_1, false, Error::MissingFeatures),
        (SUPPORTED, true, Error::FeaturesRejected),
    ] {
        let mut storage = Box::new(Storage([0; 65536]));
        let fake = Rc::new(RefCell::new(Fake {
            offered,
            reject_features: reject,
            ..Fake::default()
        }));
        let mut net = transport(allocator(&mut storage), &fake);
        assert_eq!(net.initialize(), Err(expected));
        assert_ne!(fake.borrow().status & FAILED, 0);
        assert_eq!(net.configure_queue(0, 8), Err(Error::WrongState));
    }
    for (bit, expected) in [
        (NEEDS_RESET, Error::DeviceNeedsReset),
        (FAILED, Error::DeviceFailed),
    ] {
        let mut storage = Box::new(Storage([0; 65536]));
        let fake = Rc::new(RefCell::new(Fake::default()));
        let mut net = transport(allocator(&mut storage), &fake);
        net.initialize().unwrap();
        fake.borrow_mut().status |= bit;
        assert_eq!(net.read_config(), Err(expected));
        assert_eq!(net.configure_queue(0, 8), Err(Error::WrongState));
    }
}

#[test]
fn queue_layout_ready_start_notify_and_shared_reset() {
    let mut storage = Box::new(Storage([0; 65536]));
    let dma = allocator(&mut storage);
    let fake = Rc::new(RefCell::new(Fake::default()));
    let mut net = transport(dma.clone(), &fake);
    net.initialize().unwrap();
    assert_eq!(net.configure_queue(0, 70), Ok(64));
    assert_eq!(net.configure_queue(1, 64), Ok(64));
    assert_eq!(dma.free_pages(), 14);
    let log = &fake.borrow().log.clone();
    let writes: Vec<_> = log
        .iter()
        .filter_map(|a| {
            if let Access::Write(r, v) = a {
                Some((*r, *v))
            } else {
                None
            }
        })
        .collect();
    assert!(writes.windows(8).any(|w| w
        == [
            (Register::QueueNum, 64),
            (Register::QueueDescLow, 0x40000000),
            (Register::QueueDescHigh, 0),
            (Register::QueueDriverLow, 0x40000400),
            (Register::QueueDriverHigh, 0),
            (Register::QueueDeviceLow, 0x40000488),
            (Register::QueueDeviceHigh, 0),
            (Register::QueueReady, 1),
        ]));
    assert_eq!(net.configure_queue(0, 64), Err(Error::QueueAlreadyReady));
    net.start().unwrap();
    net.notify(0).unwrap();
    net.notify(1).unwrap();
    assert_eq!(net.pending_interrupts(), Ok(3));
    net.acknowledge_interrupts(0xffff).unwrap();
    assert_eq!(net.configure_queue(0, 8), Err(Error::WrongState));
    fake.borrow_mut().reset_delay = 3;
    net.reset().unwrap();
    assert_eq!(dma.free_pages(), 16);
    assert_eq!(fake.borrow().ready, [0, 0]);
    let count = fake.borrow().log.len();
    net.reset().unwrap();
    assert_eq!(fake.borrow().log.len(), count);
    net.initialize().unwrap();
    assert_eq!(net.configure_queue(0, 8), Ok(8));
}

#[test]
fn queue_errors_do_not_publish_unowned_memory() {
    let mut storage = Box::new(Storage([0; 65536]));
    let dma = allocator(&mut storage);
    let fake = Rc::new(RefCell::new(Fake::default()));
    let mut net = transport(dma.clone(), &fake);
    net.initialize().unwrap();
    assert_eq!(net.configure_queue(2, 8), Err(Error::InvalidQueue));
    assert_eq!(net.configure_queue(0, 0), Err(Error::InvalidQueue));
    fake.borrow_mut().maximum[0] = 0;
    assert_eq!(net.configure_queue(0, 8), Err(Error::QueueUnavailable));
    fake.borrow_mut().maximum[0] = 256;
    let all = dma.allocate(65536, 4096).unwrap();
    assert_eq!(net.configure_queue(0, 8), Err(Error::Allocation));
    drop(all);
    assert!(!fake
        .borrow()
        .log
        .iter()
        .any(|a| matches!(a, Access::Write(Register::QueueReady, 1))));
    fake.borrow_mut().reject_ready = true;
    assert_eq!(net.configure_queue(0, 8), Err(Error::QueueNotReady));
    assert_eq!(dma.free_pages(), 15); // ambiguous publication retained until reset
    net.reset().unwrap();
    assert_eq!(dma.free_pages(), 16);
}

#[test]
fn configuration_width_generation_retry_and_optional_status() {
    let mut storage = Box::new(Storage([0; 65536]));
    let fake = Rc::new(RefCell::new(Fake {
        offered: REQUIRED,
        ..Fake::default()
    }));
    let mut net = transport(allocator(&mut storage), &fake);
    net.initialize().unwrap();
    fake.borrow_mut().changing = 2;
    assert_eq!(
        net.read_config(),
        Ok(NetConfig {
            mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56],
            link_up: None
        })
    );
    assert!(!fake
        .borrow()
        .log
        .iter()
        .any(|a| matches!(a, Access::Half(_))));
    fake.borrow_mut().changing = 100;
    assert_eq!(net.read_config(), Err(Error::ConfigurationUnstable));
    fake.borrow_mut().changing = 0;
    fake.borrow_mut().offered = SUPPORTED;
    net.initialize().unwrap();
    assert_eq!(net.read_config().unwrap().link_up, Some(true));
}

#[test]
fn reset_timeout_retains_memory_and_retry_reclaims_both_queues() {
    let mut storage = Box::new(Storage([0; 65536]));
    let dma = allocator(&mut storage);
    let fake = Rc::new(RefCell::new(Fake::default()));
    let mut net = transport(dma.clone(), &fake);
    net.initialize().unwrap();
    net.configure_queue(0, 8).unwrap();
    net.configure_queue(1, 8).unwrap();
    net.start().unwrap();
    fake.borrow_mut().stuck_reset = true;
    let before = fake.borrow().log.len();
    assert_eq!(net.reset(), Err(Error::ResetTimeout));
    assert_eq!(fake.borrow().log.len() - before, 5);
    assert_eq!(dma.free_pages(), 14);
    assert_eq!(net.notify(0), Err(Error::WrongState));
    fake.borrow_mut().stuck_reset = false;
    net.reset().unwrap();
    assert_eq!(dma.free_pages(), 16);
}

#[test]
fn drop_without_confirmed_reset_quarantines_ring_pages() {
    // Intentionally permanent backing: quarantine must outlive all leaked refs.
    let storage = Box::leak(Box::new(Storage([0; 65536])));
    let dma = allocator(storage);
    let fake = Rc::new(RefCell::new(Fake::default()));
    let mut net = transport(dma.clone(), &fake);
    net.initialize().unwrap();
    net.configure_queue(0, 8).unwrap();
    fake.borrow_mut().stuck_reset = true;
    drop(net);
    assert_eq!(dma.free_pages(), 15);
}

#[test]
fn post_publication_device_fault_is_not_a_rejection_and_reset_retains_owner() {
    use core::cell::Cell;
    struct Payload {
        _region: crate::memory::dma::OwnedDmaRegion,
        drops: Rc<Cell<u32>>,
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    let mut storage = Box::new(Storage([0; 65536]));
    let dma = allocator(&mut storage);
    let fake = Rc::new(RefCell::new(Fake::default()));
    let mut net: NetTransport<_, Payload> =
        unsafe { NetTransport::new(Backend(fake.clone()), dma.clone(), 4) }.unwrap();
    net.initialize().unwrap();
    net.configure_queue(0, 8).unwrap();
    net.configure_queue(1, 8).unwrap();
    net.start().unwrap();
    let drops = Rc::new(Cell::new(0));
    let region = dma.allocate(12, 64).unwrap();
    let buffers = [Buffer {
        address: region.device_address().0,
        length: 12,
        writable: false,
    }];
    fake.borrow_mut().needs_reset_on_notify = true;
    assert!(unsafe {
        net.submit(
            1,
            1,
            &buffers,
            Payload {
                _region: region,
                drops: drops.clone(),
            },
        )
    }
    .is_ok());
    assert_eq!(drops.get(), 0);
    assert_eq!(dma.free_pages(), 13);
    assert!(matches!(net.completion(1), Err(Error::DeviceNeedsReset)));
    fake.borrow_mut().stuck_reset = true;
    assert_eq!(net.reset(), Err(Error::ResetTimeout));
    assert_eq!(drops.get(), 0);
    assert_eq!(dma.free_pages(), 13);
    fake.borrow_mut().stuck_reset = false;
    net.reset().unwrap();
    assert_eq!(drops.get(), 1);
    assert_eq!(dma.free_pages(), 16);
}

#[test]
fn malformed_ring_completion_faults_both_queues_without_returning_owner() {
    let mut storage = Box::new(Storage([0; 65536]));
    // Test device view captured before exclusive management passes to the driver.
    let device_view = storage.0.as_mut_ptr();
    let dma = allocator(&mut storage);
    let fake = Rc::new(RefCell::new(Fake::default()));
    let mut net: NetTransport<_, crate::memory::dma::OwnedDmaRegion> =
        unsafe { NetTransport::new(Backend(fake.clone()), dma.clone(), 4) }.unwrap();
    net.initialize().unwrap();
    net.configure_queue(0, 8).unwrap();
    net.configure_queue(1, 8).unwrap();
    net.start().unwrap();
    let region = dma.allocate(12, 64).unwrap();
    let buffers = [Buffer {
        address: region.device_address().0,
        length: 12,
        writable: true,
    }];
    assert!(unsafe { net.submit(0, 1, &buffers, region) }.is_ok());
    let layout = split::Layout::new(8).unwrap();
    // Inert fake writes an impossible head ID to the first queue's used ring.
    unsafe {
        core::ptr::write_volatile(
            device_view.add(layout.used + 4).cast::<u32>(),
            u32::MAX.to_le(),
        );
        core::ptr::write_volatile(device_view.add(layout.used + 8).cast::<u32>(), 1u32.to_le());
        core::ptr::write_volatile(device_view.add(layout.used + 2).cast::<u16>(), 1u16.to_le());
    }
    assert!(matches!(
        net.completion(0),
        Err(Error::Ring(split::Error::UsedId))
    ));
    assert_eq!(net.notify(1), Err(Error::WrongState));
    assert_eq!(dma.free_pages(), 13);
    net.reset().unwrap();
    assert_eq!(dma.free_pages(), 16);
}

#[test]
fn entropy_transport_uses_one_queue_and_only_modern_common_features() {
    let mut storage = Box::new(Storage([0; 65536]));
    let dma = allocator(&mut storage);
    let fake = Rc::new(RefCell::new(Fake {
        device: 4,
        offered: VERSION_1,
        ..Fake::default()
    }));
    let mut rng: RngTransport<_> =
        unsafe { RngTransport::new(Backend(fake.clone()), dma.clone(), 4) }.unwrap();
    assert_eq!(rng.initialize(), Ok(VERSION_1));
    assert_eq!(rng.start(), Err(Error::QueueNotReady));
    assert_eq!(rng.configure_queue(1, 8), Err(Error::InvalidQueue));
    assert_eq!(rng.configure_queue(0, 8), Ok(8));
    assert_eq!(rng.start(), Ok(()));
    assert_eq!(fake.borrow().selected, VERSION_1);
    assert_eq!(rng.notify(1), Err(Error::InvalidQueue));
    rng.notify(0).unwrap();
    assert!(!fake.borrow().log.iter().any(|a| matches!(
        a,
        Access::Byte(_) | Access::Half(_) | Access::Read(Register::ConfigGeneration)
    )));
    rng.reset().unwrap();
    assert_eq!(dma.free_pages(), 16);
}

