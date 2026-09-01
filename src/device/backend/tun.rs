//! TUN (L3) backend: platform dispatch plus shared packet-I/O helpers.
//!
//! Both the macOS `utun` and Linux `/dev/net/tun` backends hand the kernel
//! datagrams at the IP layer (no Ethernet header, no packet-info header), so
//! their `read`/`write` fast paths are identical. The platform-specific bits
//! are only construction (`new`), `name()`, and `mtu()`.

use std::ffi::c_char;
use std::io;
use std::os::fd::RawFd;

use crate::device::backend::Error;
use crate::device::buffer_pool::PacketBuf;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::TunDevice as DefaultDevice;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::UtunDevice as DefaultDevice;

/// Default interface MTU the backends size their frame pool for.
pub(crate) const DEFAULT_MTU: usize = 1500;

/// Smallest frame stride that holds a datagram of `mtu` bytes plus the 4-byte
/// address-family prefix (macOS utun) and the frame pool's 128-byte headroom,
/// with a little margin, rounded up to a 512-byte boundary for cache
/// friendliness. This keeps a clean L3 payload *and* leaves room for jumbo
/// frames when constructed with a larger `mtu`.
pub(crate) fn frame_size_for_mtu(mtu: usize) -> usize {
    const FAMILY_PREFIX: usize = 4;
    const HEADROOM: usize = 128;
    const MARGIN: usize = 64;
    const STRIDE: usize = 512;
    (mtu + FAMILY_PREFIX + HEADROOM + MARGIN).div_ceil(STRIDE) * STRIDE
}

/// Copy `name` into `dst` and NUL-terminate it.
///
/// `dst` is the fixed-size `ifr_name`/`ctl_name` field the kernel expects. The
/// unused tail is already zero in the callers (they come from zeroed memory),
/// but we write the terminator explicitly so the helper is self-contained.
#[inline]
pub(crate) fn set_ifname(dst: &mut [c_char], name: &str) {
    let bytes = name.as_bytes();
    assert!(
        bytes.len() < dst.len(),
        "interface name `{name}` too long (max {} bytes)",
        dst.len() - 1
    );
    for (d, b) in dst.iter_mut().zip(bytes) {
        *d = *b as c_char;
    }
    dst[bytes.len()] = 0;
}

/// Put `fd` into non-blocking mode so `read`/`write` return [`io::ErrorKind::WouldBlock`]
/// instead of blocking the caller.
pub(crate) fn set_nonblocking(fd: RawFd) -> Result<(), Error> {
    // SAFETY: F_GETFL is valid for any open fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(Error::FCntl(io::Error::last_os_error()));
    }
    // SAFETY: `flags` came from F_GETFL; OR-ing O_NONBLOCK is valid for F_SETFL.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(Error::FCntl(io::Error::last_os_error()));
    }
    Ok(())
}

/// Read one datagram from `fd` into `buf`, writing the payload at the frame's
/// reserved data offset and leaving the headroom intact for later header
/// prepends.
///
/// Returns:
/// * `Ok(Some(n))` — one packet of `n` bytes was read;
/// * `Ok(None)` — no data right now (EWOULDBLOCK) or EOF/device closed;
/// * `Err(e)` — a real I/O error.
#[inline]
pub(crate) fn read_datagram(fd: RawFd, buf: &mut PacketBuf) -> io::Result<Option<usize>> {
    let off = buf.data_offset();
    let room = buf.capacity() - off;
    // SAFETY: `buf` exclusively owns its frame; we read into `[off, off + room)`.
    let n = unsafe {
        libc::read(
            fd,
            buf.as_mut_slice()[off..].as_mut_ptr() as *mut libc::c_void,
            room,
        )
    };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(e);
    }
    if n == 0 {
        // EOF / device closed.
        return Ok(None);
    }
    let n = n as usize;
    if n == room {
        // A datagram that fills the entire frame was (almost certainly)
        // truncated: the interface MTU exceeds the reserved frame capacity.
        // With `frame_size_for_mtu` the payload region always has margin, so a
        // well-sized frame never trips this.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "datagram truncated: frame smaller than interface MTU",
        ));
    }
    buf.set_len(n);
    Ok(Some(n))
}

/// Write one datagram from `buf` (payload starting at its data offset) to `fd`.
///
/// TUN/TAP datagrams are consumed atomically: the kernel takes the whole packet
/// or errors, so a short write is a programming error.
#[inline]
pub(crate) fn write_datagram(fd: RawFd, buf: &PacketBuf) -> io::Result<()> {
    let data = buf.as_slice();
    // SAFETY: `data` is a valid slice of `buf`'s frame.
    let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    debug_assert_eq!(n as usize, data.len(), "short datagram write");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::frame_size_for_mtu;

    #[test]
    fn frame_size_fits_default_and_jumbo_mtu() {
        assert_eq!(frame_size_for_mtu(1500), 2048);
        assert_eq!(frame_size_for_mtu(9000), 9216);

        // Always leave room for the 4-byte family prefix + 128-byte headroom.
        for mtu in [64usize, 1500, 9000, 9216] {
            assert!(frame_size_for_mtu(mtu) >= mtu + 4 + 128);
        }
    }
}
