//! Board-independent boot checks over the runtime's heap, clock and memory.
//! Each writes one `PASS ...` line; failures panic through the board handler.
use crate::{
    arch::aarch64::timer,
    memory::{
        self,
        dma::DmaAllocator,
        heap::LockedHeap,
        page::{AllocError, PAGE_SIZE},
    },
};
use async_net_stack_core::device::{
    dma::{DmaMemory, RxDmaMemory},
    PacketPool,
};
use core::{
    alloc::{GlobalAlloc, Layout},
    fmt::Write,
};

/// Exercise the global allocator, including alignment and full reuse.
/// Run immediately after `heap` was initialized over `memory::heap_region()`
/// and installed as `#[global_allocator]`, before anything else allocates.
pub unsafe fn heap<const WORDS: usize>(heap: &LockedHeap<WORDS>, out: &mut impl Write) {
    let (heap_start, heap_bytes) = memory::heap_region();
    for _ in 0..16 {
        let data: alloc::vec::Vec<u64> = (0..512).collect();
        assert_eq!(data.iter().sum::<u64>(), 511 * 512 / 2);
    }

    let all = Layout::from_size_align(heap_bytes, PAGE_SIZE).unwrap();
    let pointer = unsafe { heap.alloc(all) };
    assert_eq!(pointer, heap_start);
    assert!(unsafe { heap.alloc(Layout::new::<u64>()) }.is_null());
    unsafe {
        heap.dealloc(pointer, all);
    }

    let mut huge = alloc::vec::Vec::<u8>::new();
    assert!(huge.try_reserve(heap_bytes + 1).is_err());

    let pool = PacketPool::new(2, 2048, 64);
    let mut packet = pool.alloc().unwrap();
    packet.set_len(4);
    packet.as_mut_packet().copy_from_slice(b"heap");
    drop(pool);
    assert_eq!(packet.as_slice(), b"heap");
    drop(packet);

    writeln!(
        out,
        "PASS heap: portable PacketPool, alignment, exhaustion, reclamation"
    )
    .unwrap();
}

pub fn clock(out: &mut impl Write) {
    let frequency = timer::frequency_hz();
    assert!(frequency > 0);

    let before = timer::ticks();
    timer::delay_ms(2);

    assert!(timer::ticks().wrapping_sub(before) >= frequency / 500);
    writeln!(out, "PASS clock: {frequency} Hz").unwrap();
}

/// Allocate, touch and fully reclaim the linker page pool as raw pages.
///
/// # Safety
/// Nothing else may own the page pool (in particular, no `DmaAllocator`
/// may exist yet). All pages are returned before this returns.
pub unsafe fn pages<const WORDS: usize>(out: &mut impl Write) {
    let mut pages = unsafe { memory::linker_page_pool::<WORDS>() }.unwrap();
    let total = pages.total_pages();
    let run = pages.allocate(4, 4).unwrap();
    assert_eq!(run.page_count(), 4);
    assert_eq!(run.physical_start() % 16384, 0);
    // Exclusively owned RAM, identity accessible; device has no descriptors.
    unsafe {
        core::ptr::write_bytes(run.physical_start() as *mut u8, 0, run.byte_len());
        let last = (run.physical_start() + run.byte_len() - 8) as *mut u64;
        core::ptr::write_volatile(last, 0xfeed_face);
        assert_eq!(core::ptr::read_volatile(last), 0xfeed_face);
        pages.release(run).unwrap();
    }
    let all = pages.allocate(total, 1).unwrap();
    assert_eq!(pages.allocate_page(), Err(AllocError::OutOfMemory));

    unsafe { pages.release(all).unwrap() };
    assert_eq!(pages.free_pages(), total);
    writeln!(
        out,
        "PASS pages: aligned RAM, exhaustion, reclamation ({total} pages)"
    )
    .unwrap();
}

/// Packet storage, headroom and mapping ranges over `dma`; CPU-only, no device.
/// Leaves `dma` with exactly the free pages it started with.
pub fn dma(dma: &DmaAllocator, out: &mut impl Write) {
    let original_free = dma.free_pages();
    let region = dma.allocate(4096, 4096).unwrap();
    let base = region.device_address();
    let (pool, mut mapper) = region.into_packet_pool(2, 2048).unwrap();

    let mut tx = pool.alloc().unwrap();
    tx.set_headroom(73);
    tx.set_len(4);
    tx.as_mut_packet().copy_from_slice(b"zero");
    let mut mapping = unsafe { DmaMemory::map(&mut mapper, &tx) }.unwrap();
    assert_eq!(DmaMemory::address(&mapper, &mapping).0, base.0 + 73);
    assert_eq!(mapping.length(), 4);

    unsafe {
        DmaMemory::sync_for_device(&mut mapper, &mut mapping);
        DmaMemory::sync_for_cpu(&mut mapper, &mut mapping);
        DmaMemory::unmap(&mut mapper, mapping);
    }

    let mut rx = pool.alloc().unwrap();
    rx.set_headroom(91);
    let mut mapping = unsafe { RxDmaMemory::map(&mut mapper, &mut rx) }.unwrap();
    assert_eq!(
        RxDmaMemory::address(&mapper, &mapping).0,
        base.0 + 2048 + 91
    );
    assert_eq!(mapping.length(), 2048 - 91);

    unsafe {
        RxDmaMemory::sync_for_device(&mut mapper, &mut mapping);
        RxDmaMemory::sync_for_cpu(&mut mapper, &mut mapping);
        RxDmaMemory::unmap(&mut mapper, mapping);
    }

    let ordinary = PacketPool::new(1, 256, 64);
    let mut foreign = ordinary.alloc().unwrap();
    assert!(unsafe { RxDmaMemory::map(&mut mapper, &mut foreign) }.is_err());
    drop(pool);
    drop(mapper);

    assert_eq!(dma.free_pages(), original_free - 1);
    assert_eq!(tx.as_slice(), b"zero");

    drop(tx);
    drop(rx);
    assert_eq!(dma.free_pages(), original_free);
    writeln!(
        out,
        "PASS DMA: direct packet storage, headroom, exact RX range, lifetime, foreign rejection"
    )
    .unwrap();
}

