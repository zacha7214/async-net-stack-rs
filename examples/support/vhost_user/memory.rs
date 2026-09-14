//! Guest-RAM mappings and IOTLB permissions. No raw host pointers on the wire.
use super::{bad, Result};
use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr::NonNull;

#[derive(Debug)]
pub enum Fault {
    Missing { address: u64, permission: u8 },
    Invalid(String),
}
pub type Access<T> = std::result::Result<T, Fault>;
pub fn invalid(reason: impl Into<String>) -> Fault {
    Fault::Invalid(reason.into())
}

#[derive(Clone, Copy)]
pub struct Span {
    pub ptr: *mut u8,
    pub len: usize,
    pub gpa: u64,
}
impl Span {
    pub fn slice(self, offset: usize, len: usize) -> Self {
        assert!(offset <= self.len && len <= self.len - offset);
        Self {
            ptr: unsafe { self.ptr.add(offset) },
            len,
            gpa: self.gpa + offset as u64,
        }
    }
    pub fn copy_in(self, bytes: &[u8]) {
        assert!(bytes.len() <= self.len);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr, bytes.len());
        }
    }
    pub fn copy_out(self, bytes: &mut [u8]) {
        assert!(bytes.len() <= self.len);
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr, bytes.as_mut_ptr(), bytes.len());
        }
    }
    pub fn overlaps(self, other: Self) -> bool {
        self.gpa < other.gpa + other.len as u64 && other.gpa < self.gpa + self.len as u64
    }
}
pub struct Region {
    pub guest: u64,
    pub user: u64,
    pub size: u64,
    base: NonNull<u8>,
    map_len: usize,
    delta: usize,
}
impl Region {
    pub fn new(fd: OwnedFd, guest: u64, user: u64, size: u64, offset: u64) -> Result<Self> {
        if size == 0 || guest.checked_add(size).is_none() || user.checked_add(size).is_none() {
            return Err(bad("invalid memory region range"));
        }
        let file = File::from(fd);
        if offset
            .checked_add(size)
            .filter(|end| *end <= file.metadata().map(|m| m.len()).unwrap_or(0))
            .is_none()
        {
            return Err(bad("memory region extends beyond its backing file"));
        }
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page <= 0 {
            return Err(bad("cannot query host page size"));
        }
        let start = offset / page as u64 * page as u64;
        let delta = (offset - start) as usize;
        let map_len = usize::try_from(size)?
            .checked_add(delta)
            .ok_or_else(|| bad("mapping length overflow"))?;
        let offset = libc::off_t::try_from(start)?;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                offset,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            guest,
            user,
            size,
            base: NonNull::new(ptr.cast()).ok_or_else(|| bad("null mapping"))?,
            map_len,
            delta,
        })
    }
    fn span(&self, offset: u64, len: usize) -> Span {
        Span {
            ptr: unsafe { self.base.as_ptr().add(self.delta + offset as usize) },
            len,
            gpa: self.guest + offset,
        }
    }
}
impl Drop for Region {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.map_len);
        }
    }
}
#[derive(Clone, Copy)]
struct Entry {
    iova: u64,
    size: u64,
    user: u64,
    permission: u8,
}
#[derive(Default)]
pub struct Memory {
    pub regions: Vec<Region>,
    entries: Vec<Entry>,
    pub iommu: bool,
}
impl Memory {
    pub fn replace(&mut self, regions: Vec<Region>) -> Result<()> {
        for (i, a) in regions.iter().enumerate() {
            for b in &regions[..i] {
                if overlap(a.guest, a.size, b.guest, b.size)
                    || overlap(a.user, a.size, b.user, b.size)
                {
                    return Err(bad("overlapping memory regions"));
                }
            }
        }
        self.regions = regions;
        self.entries.clear();
        Ok(())
    }
    pub fn invalidate(&mut self, address: u64, size: u64) -> Result<()> {
        if size == 0 {
            return Err(bad("zero-length IOTLB invalidation"));
        }
        // Invalidating the whole matching cache entry is conservative. A later
        // miss will refill any surviving part; stale translations never survive.
        self.entries
            .retain(|e| !overlap(address, size, e.iova, e.size));
        Ok(())
    }
    pub fn update(&mut self, address: u64, size: u64, user: u64, permission: u8) -> Result<()> {
        if size == 0 || address.checked_add(size).is_none() || !(1..=3).contains(&permission) {
            return Err(bad("invalid IOTLB update"));
        }
        self.by_user(user, usize::try_from(size)?)
            .map_err(|_| bad("IOTLB update is outside shared RAM"))?;
        self.invalidate(address, size)?;
        if self.entries.len() == 4096 {
            self.entries.drain(..1024);
        }
        self.entries.push(Entry {
            iova: address,
            size,
            user,
            permission,
        });
        Ok(())
    }
    pub fn by_user(&self, address: u64, len: usize) -> Access<Span> {
        self.region(address, len, false)
    }
    fn region(&self, address: u64, len: usize, guest: bool) -> Access<Span> {
        if len == 0 || address.checked_add(len as u64).is_none() {
            return Err(invalid("invalid RAM access range"));
        }
        for r in &self.regions {
            let base = if guest { r.guest } else { r.user };
            if address >= base && address - base < r.size && len as u64 <= r.size - (address - base)
            {
                return Ok(r.span(address - base, len));
            }
        }
        Err(invalid(format!(
            "unmapped {} address {address:#x}+{len}",
            if guest { "guest" } else { "QEMU user" }
        )))
    }
    pub fn translate(&self, address: u64, len: usize, permission: u8, ring: bool) -> Access<Span> {
        if !self.iommu {
            return self.region(address, len, !ring);
        }
        if len == 0 || address.checked_add(len as u64).is_none() {
            return Err(invalid("invalid IOVA range"));
        }
        let mut done = 0usize;
        let mut first_user = 0;
        while done < len {
            let current = address + done as u64;
            let e = self
                .entries
                .iter()
                .find(|e| {
                    current >= e.iova
                        && current - e.iova < e.size
                        && e.permission & permission == permission
                })
                .ok_or(Fault::Missing {
                    address: current,
                    permission,
                })?;
            let user = e.user + current - e.iova;
            if done == 0 {
                first_user = user;
            } else if first_user.checked_add(done as u64) != Some(user) {
                return Err(invalid("one descriptor crosses noncontiguous IOTLB mappings; use identity DMA for this lab"));
            }
            done += (e.size - (current - e.iova)).min((len - done) as u64) as usize;
        }
        self.by_user(first_user, len)
    }
}
fn overlap(a: u64, size_a: u64, b: u64, size_b: u64) -> bool {
    // Subtraction avoids overflow for an invalidate-all range.
    if a <= b {
        b - a < size_a
    } else {
        a - b < size_b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrong_iotlb_permission_requires_a_new_translation() {
        let mut m = Memory {
            iommu: true,
            ..Memory::default()
        };
        m.entries.push(Entry {
            iova: 0x1000,
            size: 4096,
            user: 0x8000,
            permission: 1,
        });
        assert!(matches!(
            m.translate(0x1100, 64, 2, false),
            Err(Fault::Missing {
                address: 0x1100,
                permission: 2
            })
        ));
        m.entries[0].permission = 2;
        assert!(matches!(
            m.translate(0x1100, 64, 1, false),
            Err(Fault::Missing {
                address: 0x1100,
                permission: 1
            })
        ));
    }
    #[test]
    fn invalidation_handles_maximum_range_without_wrapping() {
        let mut m = Memory::default();
        m.entries.push(Entry {
            iova: 0x1000,
            size: 4096,
            user: 0,
            permission: 3,
        });
        m.invalidate(0, u64::MAX).unwrap();
        assert!(m.entries.is_empty());
        assert!(overlap(u64::MAX - 4, 5, u64::MAX - 1, 1));
    }
}
