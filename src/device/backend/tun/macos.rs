//! macOS `utun` (TUN, L3) backend.
//!
//! A `utun` device is a kernel-control socket (`PF_SYSTEM` / `SOCK_DGRAM` /
//! `SYSPROTO_CONTROL`) bound to the `com.apple.net.utun_control` controller. It
//! carries raw IP packets prefixed with a 4-byte address-family header
//! (`AF_INET`/`AF_INET6`, host byte order). This backend strips that prefix on
//! receive and re-derives it on send, so callers see plain IP packets exactly
//! like the Linux `IFF_TUN | IFF_NO_PI` backend.

use std::ffi::{CStr, c_char, c_uchar, c_void};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::device::Device;
use crate::device::backend::Error;
use crate::device::buffer_pool::{FramePool, PacketBuf};
use crate::device::recycle_frames;

use super::{
    DEFAULT_MTU, frame_size_for_mtu, read_datagram, set_ifname, set_nonblocking, write_datagram,
};

/// Kernel-control name for utun devices.
const CTRL_NAME: &str = "com.apple.net.utun_control";

/// `_IOWR('N', 3, struct ctl_info)` — resolve a kernel-control name to an id.
/// Not exported by libc on Apple targets.
const CTLIOCGINFO: libc::c_ulong = 0x0000_0000_c064_4e03;

/// `_IOWR('i', 51, struct ifreq)` — get interface MTU. Not exported by libc.
const SIOCGIFMTU: libc::c_ulong = 0x0000_0000_c020_6933;

// Pool geometry: 256 frames, sized from the MTU (see `frame_size_for_mtu`),
// page-aligned so a future zero-copy/DMA backend can reuse the arena.
const FRAME_COUNT: usize = 256;
const FRAME_ALIGN: usize = 4096;

/// `struct ctl_info` from `<sys/kern_control.h>` (not exported by libc).
#[repr(C)]
struct ctl_info {
    ctl_id: u32,
    ctl_name: [c_char; 96],
}

/// A macOS utun device.
pub struct UtunDevice {
    fd: OwnedFd,
    pool: FramePool,
}

impl UtunDevice {
    /// Open a utun device sized for a 1500-byte MTU. See [`Self::new_with_mtu`].
    pub fn new(unit: u32) -> Result<Self, Error> {
        Self::new_with_mtu(unit, DEFAULT_MTU)
    }

    /// Open `utun{unit}` and size the frame pool for datagrams up to `mtu` bytes
    /// (pass 9000 for jumbo frames; the interface MTU itself is still configured
    /// separately via `ifconfig`).
    ///
    /// Requires elevated privileges; the kernel rejects the `connect` with
    /// `EPERM`/`EACCES` otherwise.
    pub fn new_with_mtu(unit: u32, mtu: usize) -> Result<Self, Error> {
        let fd = unsafe { libc::socket(libc::PF_SYSTEM, libc::SOCK_DGRAM, libc::SYSPROTO_CONTROL) };
        if fd < 0 {
            return Err(Error::CreateSocket(io::Error::last_os_error()));
        }
        // SAFETY: `fd >= 0` and freshly created; ownership moves to the guard so
        // every subsequent error path closes it.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };

        // Resolve the utun_control kernel-control id.
        let mut info = ctl_info {
            ctl_id: 0,
            ctl_name: [0; 96],
        };
        set_ifname(&mut info.ctl_name, CTRL_NAME);
        // SAFETY: `info` points to a correctly-sized `struct ctl_info`.
        if unsafe { libc::ioctl(sock.as_raw_fd(), CTLIOCGINFO, &mut info) } < 0 {
            return Err(Error::IoctlFailed(io::Error::last_os_error()));
        }

        // Attach this socket to utun{unit}.
        let addr = libc::sockaddr_ctl {
            sc_len: size_of::<libc::sockaddr_ctl>() as c_uchar,
            sc_family: libc::AF_SYSTEM as c_uchar,
            ss_sysaddr: libc::AF_SYS_CONTROL as u16,
            sc_id: info.ctl_id,
            sc_unit: unit,
            sc_reserved: [0; 5],
        };
        // SAFETY: `addr` is a valid `sockaddr_ctl` for `connect(2)`.
        if unsafe {
            libc::connect(
                sock.as_raw_fd(),
                &addr as *const libc::sockaddr_ctl as *const libc::sockaddr,
                size_of_val(&addr) as libc::socklen_t,
            )
        } < 0
        {
            let mut msg = io::Error::last_os_error().to_string();
            msg.push_str(" (requires elevated privileges — did you run with sudo?)");
            return Err(Error::ConnectFailed(msg));
        }

        set_nonblocking(sock.as_raw_fd())?;

        Ok(Self {
            fd: sock,
            pool: FramePool::new(FRAME_COUNT, frame_size_for_mtu(mtu), FRAME_ALIGN),
        })
    }

    /// The assigned interface name (e.g. `utun0`).
    pub fn name(&self) -> Result<String, Error> {
        let mut name = [0u8; 256];
        let mut len = name.len() as libc::socklen_t;
        // SAFETY: `name`/`len` point to a writable buffer; the kernel writes a
        // NUL-terminated name (at most IFNAMSIZ-1 bytes).
        if unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SYSPROTO_CONTROL,
                libc::UTUN_OPT_IFNAME,
                name.as_mut_ptr() as *mut c_void,
                &mut len,
            )
        } < 0
        {
            return Err(Error::GetSockOpt(io::Error::last_os_error()));
        }
        // SAFETY: `name` is zero-initialized, so it always contains a NUL within
        // its 256 bytes; the kernel's name is NUL-terminated.
        let cstr = unsafe { CStr::from_ptr(name.as_ptr() as *const c_char) };
        Ok(cstr.to_string_lossy().into_owned())
    }

    /// Current MTU of the interface.
    pub fn mtu(&self) -> Result<usize, Error> {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(Error::CreateSocket(io::Error::last_os_error()));
        }
        // SAFETY: `fd >= 0` and freshly created.
        let ctl = unsafe { OwnedFd::from_raw_fd(fd) };

        let mut ifr: libc::ifreq = unsafe { zeroed() };
        set_ifname(&mut ifr.ifr_name, &self.name()?);
        // SAFETY: `ifr` is a valid `struct ifreq`; SIOCGIFMTU fills `ifru_mtu`.
        if unsafe { libc::ioctl(ctl.as_raw_fd(), SIOCGIFMTU, &mut ifr) } < 0 {
            return Err(Error::IoctlFailed(io::Error::last_os_error()));
        }
        // SAFETY: kernel filled `ifru_mtu` on success.
        Ok(unsafe { ifr.ifr_ifru.ifru_mtu } as usize)
    }
}

/// Read a utun datagram and strip the 4-byte address-family prefix, so the
/// caller sees a clean IP packet. The family bytes stay in the frame's headroom
/// (immediately before the payload) where the send path can re-expose them.
fn read_packet(fd: RawFd, buf: &mut PacketBuf) -> io::Result<Option<usize>> {
    let Some(n) = read_datagram(fd, buf)? else {
        return Ok(None);
    };
    if n < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short utun datagram (missing address-family header)",
        ));
    }
    buf.pull_header(4);
    Ok(Some(n - 4))
}

/// Derive the `AF_INET`/`AF_INET6` family the kernel expects from the IP version
/// nibble (the high 4 bits of the first byte).
fn family_for(buf: &PacketBuf) -> io::Result<u32> {
    let Some(&first) = buf.as_slice().first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot send an empty datagram",
        ));
    };
    match first >> 4 {
        4 => Ok(libc::AF_INET as u32),
        6 => Ok(libc::AF_INET6 as u32),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported IP version",
        )),
    }
}

/// Prepend the 4-byte address-family prefix utun requires, then write the
/// datagram.
fn write_packet(fd: RawFd, buf: &mut PacketBuf) -> io::Result<()> {
    let family = family_for(buf)?;
    buf.push_header(&family.to_ne_bytes());
    write_datagram(fd, buf)
}

impl Device for UtunDevice {
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();

        for _ in 0..max {
            let Some(mut buf) = self.alloc() else {
                break; // pool exhausted
            };
            match read_packet(self.fd.as_raw_fd(), &mut buf) {
                Ok(Some(_)) => out.push(buf),
                // EWOULDBLOCK or EOF: `buf` is dropped here and recycled.
                Ok(None) => break,
                // `buf` is dropped here and recycled.
                Err(e) => return Err(e),
            }
        }

        Ok(out.len())
    }

    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        let mut sent = 0usize;
        let mut err = None;

        for frame in frames.iter_mut() {
            match write_packet(self.fd.as_raw_fd(), frame) {
                Ok(()) => sent += 1,
                Err(e) => {
                    if e.kind() != io::ErrorKind::WouldBlock {
                        err = Some(e);
                    }
                    break;
                }
            }
        }

        // The device has taken ownership of the whole slice; recycle every frame.
        recycle_frames(frames);

        match err {
            Some(e) => Err(e),
            None => Ok(sent),
        }
    }

    fn alloc(&mut self) -> Option<PacketBuf> {
        let idx = self.pool.alloc()?;
        Some(self.pool.packet_buf(idx, 0))
    }

    fn frame_size(&self) -> usize {
        self.pool.frame_size()
    }
}

#[cfg(test)]
mod tests {
    use super::family_for;
    use crate::device::buffer_pool::FramePool;

    #[test]
    fn family_for_detects_ip_version() {
        let pool = FramePool::new(1, 256, 4096);
        let idx = pool.alloc().unwrap();
        let mut buf = pool.packet_buf(idx, 0);
        let off = buf.data_offset();

        buf.as_mut_slice()[off] = 0x45; // IPv4: version nibble = 4
        buf.set_len(20);
        assert_eq!(family_for(&buf).unwrap(), libc::AF_INET as u32);

        buf.as_mut_slice()[off] = 0x60; // IPv6: version nibble = 6
        assert_eq!(family_for(&buf).unwrap(), libc::AF_INET6 as u32);
    }
}
