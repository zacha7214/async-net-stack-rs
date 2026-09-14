use super::memory::{invalid, Access, Memory, Span};
use super::{bad, Result};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{fence, AtomicU16, Ordering};

#[derive(Default)]
pub struct Queue {
    pub num: u16,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub configured: bool,
    pub enabled: bool,
    pub started: bool,
    pub last_avail: u16,
    pub last_used: Option<u16>,
    pub kick: Option<OwnedFd>,
    pub call: Option<OwnedFd>,
    pub error: Option<OwnedFd>,
    pub notifications: u64,
    pub notification_full: u64,
}
#[derive(Clone, Copy)]
pub struct Rings {
    desc: Span,
    avail: Span,
    used: Span,
}
impl Queue {
    pub fn running(&self) -> bool {
        self.configured && self.num != 0 && self.started
    }
    pub fn stop(&mut self) -> u16 {
        self.started = false;
        self.kick = None;
        // Keep the call registration until SET_VRING_CALL replaces it. QEMU's
        // individual queue restart can retain the same MSI-X vector. A stopped
        // queue never uses this fd; retaining it does not emit notifications.
        self.last_used = None;
        self.last_avail
    }
    pub fn map(&mut self, memory: &Memory) -> Access<Rings> {
        let n = self.num as usize;
        let r = Rings {
            desc: memory.translate(self.desc, n * 16, 1, true)?,
            avail: memory.translate(self.avail, 4 + n * 2, 1, true)?,
            used: memory.translate(self.used, 4 + n * 8, 3, true)?,
        };
        if (r.desc.ptr as usize & 15) != 0
            || (r.avail.ptr as usize & 1) != 0
            || (r.used.ptr as usize & 3) != 0
            || r.desc.overlaps(r.avail)
            || r.desc.overlaps(r.used)
            || r.avail.overlaps(r.used)
        {
            return Err(invalid("misaligned/overlapping virtqueue areas"));
        }
        if self.last_used.is_none() {
            let used = load_index(r.used);
            if used != self.last_avail {
                return Err(invalid(
                    "in-order backend cannot resume mismatched avail/used bases",
                ));
            }
            self.last_used = Some(used);
            // Request kicks; we do not negotiate EVENT_IDX.
            unsafe {
                r.used.ptr.cast::<u16>().write(0);
            }
        }
        Ok(r)
    }
    pub fn peek(
        &self,
        memory: &Memory,
        r: Rings,
        write: bool,
        spans: &mut Vec<Span>,
    ) -> Access<Option<u16>> {
        spans.clear();
        let pending = load_index(r.avail).wrapping_sub(self.last_avail);
        if pending == 0 {
            return Ok(None);
        }
        if pending > self.num {
            return Err(invalid("avail index advanced beyond queue capacity"));
        }
        let slot = self.last_avail as usize & (self.num as usize - 1);
        let head = u16::from_le(unsafe { r.avail.ptr.add(4 + slot * 2).cast::<u16>().read() });
        let mut index = head;
        let mut visited = [0u64; 16];
        let mut total = 0usize;
        loop {
            if index >= self.num || spans.len() >= 64 {
                return Err(invalid("descriptor index or chain length exceeds bounds"));
            }
            let word = index as usize / 64;
            let bit = 1u64 << (index & 63);
            if visited[word] & bit != 0 {
                return Err(invalid("descriptor chain cycle"));
            }
            visited[word] |= bit;
            let mut d = [0; 16];
            r.desc.slice(index as usize * 16, 16).copy_out(&mut d);
            let address = u64::from_le_bytes(d[..8].try_into().unwrap());
            let len = u32::from_le_bytes(d[8..12].try_into().unwrap()) as usize;
            let flags = u16::from_le_bytes(d[12..14].try_into().unwrap());
            if flags & !3 != 0 || (flags & 2 != 0) != write || len == 0 {
                return Err(invalid(
                    "unsupported descriptor flags/direction/zero length",
                ));
            }
            total = total
                .checked_add(len)
                .ok_or_else(|| invalid("descriptor length overflow"))?;
            if total > 65536 {
                return Err(invalid("packet chain exceeds 64 KiB"));
            }
            let span = memory.translate(address, len, if write { 2 } else { 1 }, false)?;
            if span.overlaps(r.desc)
                || span.overlaps(r.avail)
                || span.overlaps(r.used)
                || spans.iter().any(|&s| span.overlaps(s))
            {
                return Err(invalid("packet aliases a ring or another chain segment"));
            }
            spans.push(span);
            if flags & 1 == 0 {
                break;
            }
            index = u16::from_le_bytes(d[14..16].try_into().unwrap());
        }
        Ok(Some(head))
    }
    pub fn complete(&mut self, r: Rings, head: u16, len: u32) {
        let used = self.last_used.unwrap();
        let slot = used as usize & (self.num as usize - 1);
        let mut entry = [0; 8];
        entry[..4].copy_from_slice(&(head as u32).to_le_bytes());
        entry[4..].copy_from_slice(&len.to_le_bytes());
        r.used.slice(4 + slot * 8, 8).copy_in(&entry);
        self.last_used = Some(used.wrapping_add(1));
        self.last_avail = self.last_avail.wrapping_add(1);
    }
    pub fn publish(&mut self, r: Rings) -> Result<()> {
        // Release packet bytes and used entries together. The full barrier
        // before reading interrupt suppression pairs with the driver's re-arm.
        unsafe {
            (&*r.used.ptr.add(2).cast::<AtomicU16>())
                .store(self.last_used.unwrap().to_le(), Ordering::Release);
        }
        fence(Ordering::SeqCst);
        let flags = u16::from_le(unsafe { r.avail.ptr.cast::<u16>().read_volatile() });
        if flags & 1 == 0 {
            if let Some(fd) = &self.call {
                let token = 1u64;
                loop {
                    let n =
                        unsafe { libc::write(fd.as_raw_fd(), (&token as *const u64).cast(), 8) };
                    if n == 8 {
                        self.notifications += 1;
                        break;
                    }
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    if e.kind() == io::ErrorKind::WouldBlock {
                        self.notification_full += 1;
                        break;
                    }
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }
}
fn load_index(span: Span) -> u16 {
    u16::from_le(unsafe { (&*span.ptr.add(2).cast::<AtomicU16>()).load(Ordering::Acquire) })
}
pub fn nonblocking(fd: &OwnedFd) -> Result<()> {
    let old = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if old < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, old | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn drain_kick(fd: &OwnedFd) -> Result<()> {
    let mut token = 0u64;
    for _ in 0..64 {
        let n = unsafe { libc::read(fd.as_raw_fd(), (&mut token as *mut u64).cast(), 8) };
        if n == 0 {
            return Err(bad("kick pipe closed"));
        }
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if n != 8 {
            return Err(bad("short kick token"));
        }
    }
    Ok(())
}
pub fn read_scatter(spans: &[Span], mut skip: usize, out: &mut [u8]) -> bool {
    let mut done = 0;
    for &s in spans {
        if skip >= s.len {
            skip -= s.len;
            continue;
        }
        let n = (s.len - skip).min(out.len() - done);
        s.slice(skip, n).copy_out(&mut out[done..done + n]);
        done += n;
        skip = 0;
        if done == out.len() {
            return true;
        }
    }
    false
}
