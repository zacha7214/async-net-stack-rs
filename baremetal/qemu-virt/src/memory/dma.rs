//! Owned, contiguous DMA regions for the caches-off, no-IOMMU QEMU domain.
//! Mapping performs validation and retains storage; it never starts a device.
use super::page::{PageAllocator, PageRun, PAGE_SIZE};
use alloc::rc::Rc;
use async_net_stack_core::{
    device::{
        dma::{DmaAddress, DmaMemory, RxDmaMemory},
        PacketBuf, PacketPool,
    },
    platform::io,
};

use core::{cell::RefCell, ptr::NonNull};

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidRegion,
    InvalidLayout,
    Exhausted,
}

struct AllocatorInner {
    cpu: NonNull<u8>,
    device_start: usize,
    pages: RefCell<PageAllocator<16>>,
}

#[derive(Clone)]
pub struct DmaAllocator(Rc<AllocatorInner>);
impl DmaAllocator {
    /// # Safety
    /// Transfer exclusive ownership of this stable, writable CPU allocation for
    /// the lifetime of ALL allocator/region/packet/mapping handles (or forever
    /// if any are leaked). It must map contiguously to `device_start` in the
    /// no-IOMMU device domain, with caches disabled throughout. Exclude kernel,
    /// heap, stacks and firmware. CPU base and device base must be page aligned.
    /// On the host, use only with an inert test device, not physical DMA.
    pub unsafe fn new(cpu: NonNull<u8>, device_start: usize, bytes: usize) -> Result<Self, Error> {
        if cpu.as_ptr() as usize % PAGE_SIZE != 0
            || bytes == 0
            || bytes > isize::MAX as usize
            || (cpu.as_ptr() as usize).checked_add(bytes).is_none()
        {
            return Err(Error::InvalidRegion);
        }

        let end = device_start
            .checked_add(bytes)
            .ok_or(Error::InvalidRegion)?;

        if end as u64 > u32::MAX as u64 + 1 {
            return Err(Error::InvalidRegion);
        }

        let pages =
            unsafe { PageAllocator::new(device_start, end) }.map_err(|_| Error::InvalidRegion)?;

        Ok(Self(Rc::new(AllocatorInner {
            cpu,
            device_start,
            pages: RefCell::new(pages),
        })))
    }

    pub fn free_pages(&self) -> usize {
        self.0.pages.borrow().free_pages()
    }

    /// Zeroes once on allocation, including unused trailing bytes. No device can
    /// access this region until an unsafe driver publishes its address.
    pub fn allocate(&self, bytes: usize, alignment: usize) -> Result<OwnedDmaRegion, Error> {
        if bytes == 0 || !alignment.is_power_of_two() {
            return Err(Error::InvalidLayout);
        }

        let count = bytes
            .checked_add(PAGE_SIZE - 1)
            .ok_or(Error::InvalidLayout)?
            / PAGE_SIZE;

        let alignment_pages = alignment.max(PAGE_SIZE) / PAGE_SIZE;
        let run = self
            .0
            .pages
            .borrow_mut()
            .allocate(count, alignment_pages)
            .map_err(|_| Error::Exhausted)?;
        let offset = run.physical_start() - self.0.device_start;
        let cpu = unsafe {
            NonNull::new_unchecked(self.0.cpu.as_ptr().add(offset))
        };

        // Both CPU and device alignment must hold, even in translated host tests.
        if cpu.as_ptr() as usize % alignment != 0 {
            unsafe {
                self.0.pages.borrow_mut().release(run).unwrap();
            }

            return Err(Error::InvalidLayout);
        }

        unsafe {
            core::ptr::write_bytes(cpu.as_ptr(), 0, run.byte_len());
        }

        Ok(OwnedDmaRegion {
            allocator: self.0.clone(),
            cpu,
            run,
        })
    }
}

/// Non-Copy exclusive CPU owner. Once converted to a packet arena, CPU access
/// is available only through PacketBuf and mappings hold private retention refs.
pub struct OwnedDmaRegion {
    allocator: Rc<AllocatorInner>,
    cpu: NonNull<u8>,
    run: PageRun,
}

impl OwnedDmaRegion {
    pub fn len(&self) -> usize {
        self.run.byte_len()
    }
    pub fn is_empty(&self) -> bool {
        false
    }
    pub fn device_address(&self) -> DmaAddress {
        DmaAddress(self.run.physical_start() as u64)
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(self.cpu.as_ptr(), self.len())
        }
    }

    pub fn into_packet_pool(
        self,
        frames: usize,
        frame_size: usize,
    ) -> Result<(PacketPool, PacketDmaMemory), Error> {
        // 64-byte isolated frame boundaries for this Cortex-A53 target. Actual
        // noncoherent cache maintenance is deliberately not implemented here.
        if frames == 0
            || frame_size == 0
            || frame_size % 64 != 0
            || frames
            .checked_mul(frame_size)
            .filter(|&n| n <= self.len())
            .is_none()
        {
            return Err(Error::InvalidLayout);
        }

        let owner = Rc::new(self);
        let pool = unsafe {
            PacketPool::from_owned_region(
                owner.cpu.as_ptr(),
                owner.len(),
                frames,
                frame_size,
                owner.clone(),
            )
        };

        Ok((pool, PacketDmaMemory { region: owner }))
    }
}

impl Drop for OwnedDmaRegion {
    fn drop(&mut self) {
        // No public PageRun copies or allocator borrows escape this module.
        // Last owner only: all packets/mappings are gone, hence no DMA may remain.
        unsafe {
            self.allocator
                .pages
                .borrow_mut()
                .release(self.run)
                .expect("owned DMA run invariant");
        }
    }
}

#[derive(Clone)]
pub struct PacketDmaMemory {
    region: Rc<OwnedDmaRegion>,
}

/// Deliberately not Clone/Copy; holds the arena but exposes no CPU payload view.
pub struct Mapping {
    address: DmaAddress,
    length: usize,
    _region: Rc<OwnedDmaRegion>,
}

impl Mapping {
    pub fn length(&self) -> usize {
        self.length
    }
}

impl PacketDmaMemory {
    fn map_range(&self, pointer: *const u8, length: usize) -> io::Result<Mapping> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "packet outside DMA arena or invalid length",
            )
        };

        if length == 0 || length > u32::MAX as usize {
            return Err(invalid());
        }

        let offset = (pointer as usize)
            .checked_sub(self.region.cpu.as_ptr() as usize)
            .ok_or_else(invalid)?;

        let end = offset.checked_add(length).ok_or_else(invalid)?;
        if end > self.region.len() {
            return Err(invalid());
        }

        let address = self
            .region
            .device_address()
            .0
            .checked_add(offset as u64)
            .ok_or_else(invalid)?;

        if address
            .checked_add(length as u64 - 1)
            .filter(|&a| a <= u32::MAX as u64)
            .is_none()
        {
            return Err(invalid());
        }

        Ok(Mapping {
            address: DmaAddress(address),
            length,
            _region: self.region.clone(),
        })
    }
}

// This memory domain is explicitly caches-off for its entire lifetime. No cache
// operation is required; ring/MMIO barriers belong to the future engine, not here.
unsafe impl DmaMemory for PacketDmaMemory {
    type Mapping = Mapping;
    unsafe fn map(&mut self, packet: &PacketBuf) -> io::Result<Mapping> {
        self.map_range(packet.as_slice().as_ptr(), packet.len())
    }

    fn address(&self, mapping: &Mapping) -> DmaAddress {
        mapping.address
    }

    unsafe fn sync_for_device(&mut self, _: &mut Mapping) {}
    unsafe fn sync_for_cpu(&mut self, _: &mut Mapping) {}
    unsafe fn unmap(&mut self, _: Mapping) {}
}

unsafe impl RxDmaMemory for PacketDmaMemory {
    type Mapping = Mapping;
    unsafe fn map(&mut self, packet: &mut PacketBuf) -> io::Result<Mapping> {
        if !packet.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RX packet must be empty",
            ));
        }
        let offset = packet.data_offset();
        let capacity = packet.tail_capacity();

        // Obtain writable provenance from the mutable whole-frame view; do not
        // derive a writable DMA address from an immutable packet slice.
        let ptr = unsafe { packet.as_mut_slice().as_mut_ptr().add(offset) };
        self.map_range(ptr, capacity)
    }

    fn address(&self, mapping: &Mapping) -> DmaAddress {
        mapping.address
    }

    unsafe fn sync_for_device(&mut self, _: &mut Mapping) {}
    unsafe fn sync_for_cpu(&mut self, _: &mut Mapping) {}
    unsafe fn unmap(&mut self, _: Mapping) {}
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use super::*;
    #[repr(align(16384))]
    struct Storage([u8; 32768]);

    fn allocator(storage: &mut Storage) -> DmaAllocator {
        unsafe {
            DmaAllocator::new(
                NonNull::new(storage.0.as_mut_ptr()).unwrap(),
                0x4000_0000,
                storage.0.len(),
            )
                .unwrap()
        }
    }

    #[test]
    fn regions_zero_align_exhaust_and_reclaim() {
        let mut storage = Box::new(Storage([0xab; 32768]));
        let allocator = allocator(&mut storage);
        let mut region = allocator.allocate(5000, 16384).unwrap();
        assert_eq!(region.device_address().0 % 16384, 0);
        assert_eq!(region.as_mut_slice().as_ptr() as usize % 16384, 0);
        assert!(region.as_mut_slice().iter().all(|&b| b == 0));
        region.as_mut_slice().fill(0xff);
        assert_eq!(allocator.free_pages(), 6);
        assert!(allocator.allocate(32768, 1).is_err());
        drop(region);
        assert_eq!(allocator.free_pages(), 8);
        assert!(allocator
            .allocate(5000, 1)
            .unwrap()
            .as_mut_slice()
            .iter()
            .all(|&b| b == 0));

        assert!(allocator.allocate(usize::MAX, 1).is_err());
        assert!(allocator.allocate(1, 3).is_err());
        assert_eq!(allocator.free_pages(), 8);
    }

    #[test]
    fn packet_and_mapping_retain_arena_without_copying() {
        let mut storage = Box::new(Storage([0; 32768]));
        let allocator = allocator(&mut storage);
        let region = allocator.allocate(4096, 4096).unwrap();
        let original = region.cpu.as_ptr();
        let (pool, mut mapper) = region.into_packet_pool(2, 2048).unwrap();
        let mut packet = pool.alloc().unwrap();
        packet.set_headroom(71);
        packet.set_len(3);
        packet.as_mut_packet().copy_from_slice(b"dma");
        assert_eq!(packet.as_slice().as_ptr(), unsafe { original.add(71) });

        let map = unsafe { DmaMemory::map(&mut mapper, &packet) }.unwrap();
        assert_eq!(
            DmaMemory::address(&mapper, &map),
            DmaAddress(0x4000_0000 + 71)
        );

        assert_eq!(map.length(), 3);
        drop(pool);
        drop(mapper);

        assert_eq!(allocator.free_pages(), 7);

        // There is no active device in this test; accesses are already quiescent.
        drop(packet);
        assert_eq!(allocator.free_pages(), 7);

        drop(map);
        assert_eq!(allocator.free_pages(), 8);
    }

    #[test]
    fn rx_maps_exact_tail_and_rejects_foreign_or_empty_ranges() {
        let mut storage = Box::new(Storage([0; 32768]));
        let allocator = allocator(&mut storage);
        let (pool, mut mapper) = allocator
            .allocate(4096, 4096)
            .unwrap()
            .into_packet_pool(2, 2048)
            .unwrap();

        let mut packet = pool.alloc().unwrap();
        packet.set_headroom(127);
        let mut map = unsafe { RxDmaMemory::map(&mut mapper, &mut packet) }.unwrap();
        assert_eq!(map.length(), 2048 - 127);
        assert_eq!(RxDmaMemory::address(&mapper, &map).0, 0x4000_0000 + 127);
        unsafe {
            RxDmaMemory::sync_for_device(&mut mapper, &mut map);
            RxDmaMemory::sync_for_cpu(&mut mapper, &mut map);
            RxDmaMemory::unmap(&mut mapper, map);
        }

        assert!(unsafe { DmaMemory::map(&mut mapper, &packet) }.is_err());
        packet.set_headroom(2048);
        assert!(unsafe { RxDmaMemory::map(&mut mapper, &mut packet) }.is_err());

        let foreign = PacketPool::new(1, 256, 64);
        let mut other = foreign.alloc().unwrap();
        assert!(unsafe { RxDmaMemory::map(&mut mapper, &mut other) }.is_err());

        other.set_len(1);
        assert!(unsafe { DmaMemory::map(&mut mapper, &other) }.is_err());
        assert_eq!(allocator.free_pages(), 7);
        drop(packet);
        drop(pool);
        drop(mapper);

        assert_eq!(allocator.free_pages(), 8);
    }

    #[test]
    fn invalid_pool_and_domain_preserve_resources() {
        let mut storage = Box::new(Storage([0; 32768]));
        let allocator = allocator(&mut storage);
        for (frames, size) in [(0, 64), (1, 63), (usize::MAX, 64), (100, 4096)] {
            assert!(allocator
                .allocate(4096, 4096)
                .unwrap()
                .into_packet_pool(frames, size)
                .is_err());
            assert_eq!(allocator.free_pages(), 8);
        }

        assert!(unsafe {
            DmaAllocator::new(
                NonNull::new(storage.0.as_mut_ptr()).unwrap(),
                u32::MAX as usize - 4095,
                8192,
            )
        }
            .is_err());
    }

    #[test]
    fn tx_queue_retains_dma_storage_until_confirmed_stop() {
        use async_net_stack_core::device::{
            dma::{Completion, Submission, TxConfig, TxEngine, TxQueue},
            Device,
        };

        use core::{cell::Cell, time::Duration};

        struct Held(Rc<Cell<bool>>);

        // Inert device: publishes no physical access and withholds completion.
        // False conservatively refuses stop; true is genuinely quiescent.
        unsafe impl TxEngine for Held {
            unsafe fn submit(&mut self, _: Submission, _: Duration) -> io::Result<()> {
                Ok(())
            }
            fn completion(&mut self, _: Duration) -> Option<Completion> {
                None
            }
            fn stop(&mut self) -> bool {
                self.0.get()
            }
        }

        let mut storage = Box::new(Storage([0; 32768]));
        let allocator = allocator(&mut storage);
        let (pool, mapper) = allocator
            .allocate(4096, 4096)
            .unwrap()
            .into_packet_pool(1, 4096)
            .unwrap();

        let mut packet = pool.alloc().unwrap();

        packet.set_len(3);
        packet.as_mut_packet().copy_from_slice(b"dma");

        let stop = Rc::new(Cell::new(false));
        let mut queue =
            unsafe {
                TxQueue::new(pool, mapper, Held(stop.clone()), TxConfig::default())
            }.unwrap();

        let mut packets = [packet];
        assert_eq!(queue.send(&mut packets).unwrap(), 1);
        assert_eq!(packets[0].capacity(), 0);
        assert!(!queue.shutdown());
        assert_eq!(allocator.free_pages(), 7);
        assert_eq!(queue.pending(), 1);

        stop.set(true);
        assert!(queue.shutdown());
        assert_eq!(queue.pending(), 0);
        drop(queue);

        assert_eq!(allocator.free_pages(), 8);
    }
}
