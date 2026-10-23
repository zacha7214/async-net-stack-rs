//! Identity-only probe over a board-described window of Virtio MMIO slots.
//! No status writes, feature negotiation, queue activation or DMA occur here.
use core::ptr::read_volatile;

/// Evenly spaced MMIO transport slots, e.g. QEMU virt's 32 slots of 0x200.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub base: usize,
    pub stride: usize,
    pub slots: usize,
}

#[derive(Debug)]
pub struct Device {
    pub base: usize,
    pub version: u32,
}

#[derive(Clone, Copy)]
#[repr(u32)]
pub enum Kind {
    Network = 1,
    Entropy = 4,
}

/// # Safety
/// Every slot in `window` must be mapped as device memory (or accessed with
/// MMU/caches off), with exclusive CPU access during probing.
pub unsafe fn find(window: Window, kind: Kind) -> Option<Device> {
    for slot in 0..window.slots {
        let base = window.base + slot * window.stride;
        let read = |offset| unsafe {
            read_volatile((base + offset) as *const u32)
        };
        
        if read(0) == 0x7472_6976 && read(8) == kind as u32 {
            return Some(Device {
                base,
                version: read(4),
            });
        }
    }

    None
}

