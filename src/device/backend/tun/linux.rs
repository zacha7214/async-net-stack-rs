//! Linux TUN (L3) backend via `/dev/net/tun`.
//!
//! The clone device is configured with `TUNSETIFF` using `IFF_TUN | IFF_NO_PI`,
//! so each `read`/`write` is a raw IP datagram with no packet-info header —
//! matching the macOS `utun` backend.

use std::ffi::CStr;
use std::io;
use std::mem::zeroed;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::device::Device;
use crate::device::backend::Error;
use crate::device::buffer_pool::{FramePool, PacketBuf};
use crate::device::recycle_frames;

use super::{
    DEFAULT_MTU, frame_size_for_mtu, read_datagram, set_ifname, set_nonblocking, write_datagram,
};

/// Path to the TUN clone device (must be NUL-terminated for `open`).
const TUN_CLONE_DEVICE: &[u8] = b"/dev/net/tun\0";

/// `SIOCGIFMTU` (get MTU) from `<linux/sockios.h>` — not exported by libc.
const SIOCGIFMTU: libc::c_ulong = 0x8921;

// Pool geometry: 256 frames, sized from the MTU (see `frame_size_for_mtu`),
// page-aligned so a future zero-copy/DMA backend can reuse the arena.
const FRAME_COUNT: usize = 256;
const FRAME_ALIGN: usize = 4096;

/// A Linux TUN device.
pub struct TunDevice {
    fd: OwnedFd,
    name: String,
    pool: FramePool,
}

impl TunDevice {
    /// Open (or create) a TUN device sized for a 1500-byte MTU. See
    /// [`Self::new_with_mtu`].
    pub fn new(name: &str) -> Result<Self, Error> {
        Self::new_with_mtu(name, DEFAULT_MTU)
    }

    /// Open (or create) a TUN device and size the frame pool for datagrams up to
    /// `mtu` bytes (pass 9000 for jumbo frames; the interface MTU itself is still
    /// configured separately via `ip link`).
    ///
    /// Pass an empty `name` to let the kernel auto-assign `tun%d`; otherwise the
    /// name is used verbatim (it must be shorter than `IFNAMSIZ`). Requires
    /// `CAP_NET_ADMIN` (typically root) and `/dev/net/tun` to exist.
    pub fn new_with_mtu(name: &str, mtu: usize) -> Result<Self, Error> {
        if name.len() >= libc::IFNAMSIZ {
            return Err(Error::InvalidTunnelName(name.to_owned()));
        }

        let fd = unsafe {
            libc::open(
                TUN_CLONE_DEVICE.as_ptr() as *const libc::c_char,
                libc::O_RDWR | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(Error::CreateSocket(io::Error::last_os_error()));
        }
        // SAFETY: `fd >= 0` and freshly opened; ownership moves to the guard so
        // every subsequent error path closes it.
        let tun = unsafe { OwnedFd::from_raw_fd(fd) };

        let mut ifr: libc::ifreq = unsafe { zeroed() };
        ifr.ifr_ifru.ifru_flags = (libc::IFF_TUN | libc::IFF_NO_PI) as libc::c_short;
        set_ifname(&mut ifr.ifr_name, name);
        // SAFETY: `ifr` is a valid `struct ifreq`; TUNSETIFF configures the
        // clone device and (on success) writes back the assigned name.
        if unsafe { libc::ioctl(tun.as_raw_fd(), libc::TUNSETIFF, &mut ifr) } < 0 {
            return Err(Error::IoctlFailed(io::Error::last_os_error()));
        }

        set_nonblocking(tun.as_raw_fd())?;

        // SAFETY: the kernel NUL-terminates `ifr_name` on success.
        let assigned = unsafe { CStr::from_ptr(ifr.ifr_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();

        Ok(Self {
            fd: tun,
            name: assigned,
            pool: FramePool::new(FRAME_COUNT, frame_size_for_mtu(mtu), FRAME_ALIGN),
        })
    }

    /// The assigned interface name (e.g. `tun0`). Infallible, but `Result` for a
    /// uniform cross-platform API with the macOS backend.
    pub fn name(&self) -> Result<String, Error> {
        Ok(self.name.clone())
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
        set_ifname(&mut ifr.ifr_name, &self.name);
        // SAFETY: `ifr` is a valid `struct ifreq`; SIOCGIFMTU fills `ifru_mtu`.
        if unsafe { libc::ioctl(ctl.as_raw_fd(), SIOCGIFMTU, &mut ifr) } < 0 {
            return Err(Error::IoctlFailed(io::Error::last_os_error()));
        }
        // SAFETY: kernel filled `ifru_mtu` on success.
        Ok(unsafe { ifr.ifr_ifru.ifru_mtu } as usize)
    }
}

impl Device for TunDevice {
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();

        for _ in 0..max {
            let Some(mut buf) = self.alloc() else {
                break; // pool exhausted
            };
            match read_datagram(self.fd.as_raw_fd(), &mut buf) {
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

        for frame in frames.iter() {
            match write_datagram(self.fd.as_raw_fd(), frame) {
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
