//! Minimal io_uring ABI (64-byte SQEs, 16-byte CQEs, no SQPOLL).
//! Only the owning backend can enqueue requests; it retains their buffers
//! until completion or successful synchronous cancellation.
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

#[repr(C)]
#[derive(Default)]
struct Offsets {
    head: u32,
    tail: u32,
    mask: u32,
    entries: u32,
    flags_or_overflow: u32,
    dropped_or_cqes: u32,
    array_or_flags: u32,
    reserved: u32,
    address: u64,
}
#[repr(C)]
#[derive(Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    cpu: u32,
    idle: u32,
    features: u32,
    wq_fd: u32,
    reserved: [u32; 3],
    sq: Offsets,
    cq: Offsets,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub(super) struct Sqe {
    pub opcode: u8,
    pub flags: u8,
    pub priority: u16,
    pub fd: i32,
    pub offset: u64,
    pub address: u64,
    pub len: u32,
    pub rw_flags: u32,
    pub user_data: u64,
    pub buf_index: u16,
    pub personality: u16,
    pub splice_fd: i32,
    pub padding: [u64; 2],
}
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Cqe {
    pub user_data: u64,
    pub result: i32,
    pub flags: u32,
}
#[repr(C)]
struct Cancel {
    address: u64,
    fd: i32,
    flags: u32,
    seconds: i64,
    nanoseconds: i64,
    padding: [u64; 4],
}

struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}
impl Mapping {
    fn new(fd: RawFd, len: usize, offset: libc::off_t) -> io::Result<Self> {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                offset,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: NonNull::new(p.cast()).expect("mmap address"),
            len,
        })
    }
    fn at<T>(&self, offset: usize) -> *mut T {
        assert!(offset + mem::size_of::<T>() <= self.len);
        let p = unsafe { self.ptr.as_ptr().add(offset).cast::<T>() };
        assert_eq!(p as usize % mem::align_of::<T>(), 0);
        p
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

pub(super) struct Ring {
    fd: OwnedFd,
    sq: Mapping,
    cq: Mapping,
    sqes: Mapping,
    params: Params,
    sq_tail: u32,
    cq_head: u32,
}
impl Ring {
    pub fn new(entries: u32, device_fd: RawFd) -> io::Result<Self> {
        let mut params = Params::default();
        let fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, entries, &mut params) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        if params.features & (1 << 1) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring needs IORING_FEAT_NODROP",
            ));
        }
        let sq_len = params.sq.array_or_flags as usize + params.sq_entries as usize * 4;
        let cq_len = params.cq.dropped_or_cqes as usize + params.cq_entries as usize * 16;
        // Separate mappings work with SINGLE_MMAP kernels too; both refer to
        // the same kernel allocation, and each mapping has its own VM ref.
        let sq = Mapping::new(fd.as_raw_fd(), sq_len, 0)?;
        let cq = Mapping::new(fd.as_raw_fd(), cq_len, 0x0800_0000)?;
        let sqes = Mapping::new(fd.as_raw_fd(), params.sq_entries as usize * 64, 0x1000_0000)?;
        let ring = Self {
            fd,
            sq,
            cq,
            sqes,
            params,
            sq_tail: 0,
            cq_head: 0,
        };
        // Detect synchronous cancellation support before queuing any buffers.
        // This is required for safe teardown (Linux >= 6.0).
        ring.cancel_all()?;
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                ring.fd.as_raw_fd(),
                2u32,
                &device_fd as *const RawFd,
                1u32,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ring)
    }
    pub fn entries(&self) -> usize {
        self.params.sq_entries as usize
    }
    fn load(map: &Mapping, offset: u32) -> u32 {
        unsafe { (&*map.at::<AtomicU32>(offset as usize)).load(Ordering::Acquire) }
    }
    fn store(map: &Mapping, offset: u32, value: u32) {
        unsafe {
            (&*map.at::<AtomicU32>(offset as usize)).store(value, Ordering::Release);
        }
    }
    pub fn free_slots(&self) -> usize {
        self.entries()
            - self
                .sq_tail
                .wrapping_sub(Self::load(&self.sq, self.params.sq.head)) as usize
    }
    /// Caller must have already stored the owned buffer in its pending table.
    pub fn push(&mut self, sqe: Sqe) {
        assert!(self.free_slots() > 0);
        let index = self.sq_tail & (self.params.sq_entries - 1);
        unsafe {
            *self.sqes.at::<Sqe>(index as usize * 64) = sqe;
            *self
                .sq
                .at::<u32>(self.params.sq.array_or_flags as usize + index as usize * 4) = index;
        }
        self.sq_tail = self.sq_tail.wrapping_add(1);
    }
    /// Publish the whole batch and service task work in one non-waiting enter.
    /// Partial submissions remain on the SQ and are retried next progress call.
    pub fn submit(&mut self) -> io::Result<()> {
        Self::store(&self.sq, self.params.sq.tail, self.sq_tail);
        let pending = self
            .sq_tail
            .wrapping_sub(Self::load(&self.sq, self.params.sq.head));
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_uring_enter,
                self.fd.as_raw_fd(),
                pending,
                0u32,
                1u32,
                std::ptr::null::<libc::sigset_t>(),
                0usize,
            )
        };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if matches!(
                err.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }
    pub fn pop(&mut self) -> Option<Cqe> {
        if self.cq_head == Self::load(&self.cq, self.params.cq.tail) {
            return None;
        }
        let index = self.cq_head & (self.params.cq_entries - 1);
        let cqe = unsafe {
            *self
                .cq
                .at::<Cqe>(self.params.cq.dropped_or_cqes as usize + index as usize * 16)
        };
        self.cq_head = self.cq_head.wrapping_add(1);
        Some(cqe)
    }
    pub fn release_completions(&self) {
        Self::store(&self.cq, self.params.cq.head, self.cq_head);
    }
    /// All submitted operations are finished/cancelled when this returns Ok.
    /// Never submit again between this call and releasing the pending buffers.
    pub fn cancel_all(&self) -> io::Result<()> {
        let arg = Cancel {
            address: 0,
            fd: -1,
            flags: (1 << 0) | (1 << 2),
            seconds: 1,
            nanoseconds: 0,
            padding: [0; 4],
        };
        loop {
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_io_uring_register,
                    self.fd.as_raw_fd(),
                    24u32,
                    &arg as *const Cancel,
                    1u32,
                )
            };
            if ret >= 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abi_sizes() {
        assert_eq!(mem::size_of::<Params>(), 120);
        assert_eq!(mem::size_of::<Offsets>(), 40);
        assert_eq!(mem::size_of::<Sqe>(), 64);
        assert_eq!(mem::size_of::<Cqe>(), 16);
        assert_eq!(mem::size_of::<Cancel>(), 64);
    }
}
