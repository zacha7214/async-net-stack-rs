// Adapted from Zach Allen's bare-metal-utils; see docs/virtio-net-plan.md.
use super::*;

// These tests exercise address bookkeeping only, on synthetic physical maps.
// No address is dereferenced. QEMU separately verifies actual RAM access.
fn pool<const W: usize>(base: usize, pages: usize) -> PageAllocator<W> {
    unsafe { PageAllocator::new(base * PAGE_SIZE, (base + pages) * PAGE_SIZE) }.unwrap()
}

#[test]
fn validates_regions_and_metadata() {
    assert!(matches!(
        unsafe { PageAllocator::<1>::new(0, 0) },
        Err(InitError::EmptyOrReversed)
    ));
    assert!(matches!(
        unsafe { PageAllocator::<1>::new(PAGE_SIZE, 0) },
        Err(InitError::EmptyOrReversed)
    ));
    assert!(matches!(
        unsafe { PageAllocator::<1>::new(1, PAGE_SIZE) },
        Err(InitError::Unaligned)
    ));
    assert!(matches!(
        unsafe { PageAllocator::<1>::new(0, PAGE_SIZE + 1) },
        Err(InitError::Unaligned)
    ));
    assert!(matches!(
        unsafe { PageAllocator::<1>::new(0, 65 * PAGE_SIZE) },
        Err(InitError::InsufficientMetadata)
    ));
    assert!(matches!(
        unsafe { PageAllocator::<0>::new(0, PAGE_SIZE) },
        Err(InitError::InsufficientMetadata)
    ));
}

#[test]
fn exhaustion_partial_bitmap_word_and_reuse() {
    let mut allocator = pool::<2>(0x100, 65);
    let mut frames = std::vec::Vec::new();
    for index in 0..65 {
        let frame = allocator.allocate_page().unwrap();
        assert_eq!(frame.physical_start(), (0x100 + index) * PAGE_SIZE);
        frames.push(frame);
    }
    assert_eq!(allocator.free_pages(), 0);
    assert_eq!(allocator.allocate_page(), Err(AllocError::OutOfMemory));
    unsafe { allocator.release(frames[64]) }.unwrap();
    assert_eq!(allocator.allocate_page().unwrap(), frames[64]);
    for frame in frames {
        unsafe { allocator.release(frame) }.unwrap();
    }
    assert_eq!(allocator.free_pages(), 65);
}

#[test]
fn alignment_is_physical_not_pool_relative() {
    let mut allocator = pool::<1>(3, 16);
    let run = allocator.allocate(4, 4).unwrap();
    assert_eq!(run.physical_start(), 4 * PAGE_SIZE);
    assert_eq!(run.byte_len(), 4 * PAGE_SIZE);
    assert_eq!(
        allocator.allocate_page().unwrap().physical_start(),
        3 * PAGE_SIZE
    );
}

#[test]
fn invalid_requests_do_not_consume_frames() {
    let mut allocator = pool::<1>(0, 8);
    for (pages, align) in [(0, 1), (1, 0), (1, 3), (1, 1 << (usize::BITS - 1))] {
        assert_eq!(
            allocator.allocate(pages, align),
            Err(AllocError::InvalidRequest)
        );
    }
    assert_eq!(
        allocator.allocate(usize::MAX, 1),
        Err(AllocError::OutOfMemory)
    );
    assert_eq!(allocator.free_pages(), 8);
}

#[test]
fn fragmented_space_is_not_contiguous_space() {
    let mut allocator = pool::<1>(0, 8);
    let frames: std::vec::Vec<_> = (0..8).map(|_| allocator.allocate_page().unwrap()).collect();
    for index in [0, 2, 4, 6] {
        unsafe { allocator.release(frames[index]) }.unwrap();
    }
    assert_eq!(allocator.free_pages(), 4);
    assert_eq!(allocator.allocate(2, 1), Err(AllocError::OutOfMemory));
    unsafe { allocator.release(frames[1]) }.unwrap();
    assert_eq!(allocator.allocate(3, 1).unwrap().physical_start(), 0);
}

#[test]
fn defensive_free_checks_do_not_partially_change_bitmap() {
    let mut allocator = pool::<1>(10, 8);
    let run = allocator.allocate(2, 1).unwrap();
    // Deliberately malformed descriptors test defenses, not supported API use.
    for bad in [
        PageRun { start: 0, pages: 1 },
        PageRun {
            start: run.start + 1,
            pages: 1,
        },
        PageRun {
            start: run.start,
            pages: usize::MAX,
        },
    ] {
        assert_eq!(
            unsafe { allocator.release(bad) },
            Err(FreeError::OutsidePool)
        );
    }
    assert_eq!(
        unsafe {
            allocator.release(PageRun {
                start: run.start,
                pages: 3,
            })
        },
        Err(FreeError::AlreadyFree)
    );
    assert_eq!(allocator.free_pages(), 6);
    unsafe { allocator.release(run) }.unwrap();
    assert_eq!(
        unsafe { allocator.release(run) },
        Err(FreeError::AlreadyFree)
    );
    assert_eq!(allocator.free_pages(), 8);
}

#[test]
fn high_addresses_do_not_wrap() {
    let end = usize::MAX & !(PAGE_SIZE - 1);
    let mut allocator = unsafe { PageAllocator::<1>::new(end - 4 * PAGE_SIZE, end) }.unwrap();
    assert_eq!(
        allocator.allocate(4, 1).unwrap().physical_start(),
        end - 4 * PAGE_SIZE
    );
    assert_eq!(allocator.allocate_page(), Err(AllocError::OutOfMemory));
}

#[test]
fn mixed_requests_match_independent_occupancy_model() {
    let mut allocator = pool::<2>(3, 97);
    let mut occupied = [false; 97];
    let mut live = std::vec::Vec::<PageRun>::new();
    let mut rng = 0x1234_u64;
    for step in 0..4000 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        if step % 3 == 0 && !live.is_empty() {
            let run = live.swap_remove((rng as usize) % live.len());
            let first = run.start / PAGE_SIZE - 3;
            occupied[first..first + run.pages].fill(false);
            unsafe { allocator.release(run) }.unwrap();
        } else {
            let pages = ((rng >> 32) as usize % 8) + 1;
            let align = 1 << ((rng >> 48) % 5);
            let expected = (0..=97 - pages)
                .find(|&i| (i + 3) % align == 0 && occupied[i..i + pages].iter().all(|v| !v));
            match (expected, allocator.allocate(pages, align)) {
                (Some(first), Ok(run)) => {
                    assert_eq!(run.start, (first + 3) * PAGE_SIZE);
                    occupied[first..first + pages].fill(true);
                    live.push(run);
                }
                (None, Err(AllocError::OutOfMemory)) => {}
                other => panic!("model mismatch: {other:?}"),
            }
        }
        assert_eq!(
            allocator.free_pages(),
            occupied.iter().filter(|&&v| !v).count()
        );
    }
    for run in live {
        unsafe { allocator.release(run) }.unwrap();
    }
    assert_eq!(allocator.free_pages(), 97);
}

