//! XSKMAP + a hand-written default XDP program, loaded via raw `bpf(2)` and
//! attached with plain netlink (`RTM_SETLINK` + `IFLA_XDP`).
//!
//! The default program mirrors libxdp's `xsk_def_xdp_prog.o` (verified
//! byte-for-byte against clang's output for the same C source):
//!
//! ```text
//!   r0 = 2                          ; XDP_PASS unless we redirect
//!   r2 = &refcnt_map                ; ldimm64 (BPF_PSEUDO_MAP_FD)
//!   r2 = *(u32 *)(r2 + 0)           ; sockets installed?
//!   if r2 == 0 goto exit            ;   no -> pass everything
//!   r2 = *(u32 *)(r1 + 16)          ; ctx->rx_queue_index
//!   r1 = &xsks_map                  ; ldimm64 (BPF_PSEUDO_MAP_FD)
//!   r3 = 2                          ; flags on redirect failure
//!   call bpf_redirect_map
//!   exit
//! ```
//!
//! Custom match/drop policies can later be loaded the same way (or as ELF
//! objects); this module is deliberately dependency-free so it doubles as
//! the reference for the netlink attach path.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU32, Ordering};

use super::sys::*;

// netlink constants (linux/rtnetlink.h, linux/if_link.h)
const AF_NETLINK: libc::c_int = 16;
const NETLINK_ROUTE: libc::c_int = 0;
const RTM_SETLINK: u16 = 19;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLMSG_ERROR: u16 = 2;
const IFLA_XDP: u16 = 43;
const IFLA_XDP_FD: u16 = 1;
const IFLA_XDP_FLAGS: u16 = 3;
const IFLA_XDP_EXPECTED_FD: u16 = 6;
/// Netlink fallback only (bpf_link attaches are exclusive by nature).
const XDP_FLAGS_UPDATE_IF_NOEXIST: u32 = 1;

static NL_SEQ: AtomicU32 = AtomicU32::new(1);

/// How the default program ended up attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachMode {
    /// Not attached (`XdpConfig::attach == false`).
    None,
    /// Native driver mode (`XDP_FLAGS_DRV_MODE`).
    Driver,
    /// Generic/skb mode (`XDP_FLAGS_SKB_MODE`) — e.g. veth, no native XDP.
    Generic,
    /// Something else was already attached; we left it alone.
    AlreadyAttached,
}

/// XSKMAP + loaded default program; attaches/detaches over netlink.
pub(crate) struct XskProgram {
    map_fd: OwnedFd,
    /// "Any sockets installed?" guard map (see [`XskProgram::new`]).
    refcnt_fd: OwnedFd,
    prog_fd: OwnedFd,
    /// bpf_link fd (kernel >= 5.7); closing it detaches the program.
    link_fd: Option<OwnedFd>,
    /// Attach mode to undo on drop for the netlink fallback path.
    detach_mode: Option<u32>,
    ifindex: u32,
}

impl XskProgram {
    pub(crate) fn new(ifindex: u32, max_queues: u32) -> io::Result<Self> {
        let map_fd = create_xskmap(max_queues)?;
        let map_fd = unsafe { OwnedFd::from_raw_fd(map_fd as RawFd) };

        // "No sockets in the map yet" refcount guard, mirroring libxdp's
        // xsk_def_xdp_prog: pass traffic through until the first socket is
        // installed (avoids blackholing the interface on attach).
        let refcnt_fd = create_map(BPF_MAP_TYPE_ARRAY, 4, 4, 1)?;
        let refcnt_fd = unsafe { OwnedFd::from_raw_fd(refcnt_fd as RawFd) };

        // Byte-for-byte equivalent of what clang -O2 -target bpf emits for
        // libxdp's xsk_def_xdp_prog.c (verified against the objdump):
        //
        //   r0 = 2                          ; XDP_PASS unless we redirect
        //   r2 = &refcnt_map[0]             ; ldimm64 (BPF_PSEUDO_MAP_VALUE)
        //   r2 = *(u32 *)(r2 + 0)           ; sockets installed?
        //   if r2 == 0 goto +5              ;   no -> pass
        //   r2 = *(u32 *)(r1 + 16)          ; ctx->rx_queue_index
        //   r1 = &xsks_map                  ; ldimm64 (pseudo map fd)
        //   r3 = 2                          ; flags on redirect failure
        //   call bpf_redirect_map           ; -> xsks_map[rx_queue_index]
        //   exit
        let mut insns = Vec::with_capacity(11);
        insns.push(BpfInsn::mov64_imm(0, 2));
        insns.extend_from_slice(&BpfInsn::ld_map_value(2, refcnt_fd.as_raw_fd() as u32, 0));
        insns.push(BpfInsn::ldx_w(2, 2, 0));
        insns.push(BpfInsn::jeq_imm(2, 0, 5));
        insns.push(BpfInsn::ldx_w(2, 1, 16));
        insns.extend_from_slice(&BpfInsn::ld_map_fd(1, map_fd.as_raw_fd() as u32));
        insns.push(BpfInsn::mov64_imm(3, 2));
        insns.push(BpfInsn::call(BPF_REDIRECT_MAP));
        insns.push(BpfInsn::exit());

        let prog_fd = load_prog(&insns, b"xdp_def_prog")?;
        let prog_fd = unsafe { OwnedFd::from_raw_fd(prog_fd as RawFd) };

        Ok(Self {
            map_fd,
            refcnt_fd,
            prog_fd,
            link_fd: None,
            detach_mode: None,
            ifindex,
        })
    }

    pub(crate) fn set_socket(&self, queue_id: u32, socket_fd: RawFd) -> io::Result<()> {
        xskmap_set(self.map_fd.as_raw_fd(), queue_id, socket_fd)?;
        // A socket is installed: enable the redirect branch (refcnt[0] = 1).
        map_set_u32(self.refcnt_fd.as_raw_fd(), 0, 1)
    }

    #[cfg(test)]
    pub(crate) fn map_fd(&self) -> RawFd {
        self.map_fd.as_raw_fd()
    }

    /// Attach the program. `prefer_generic` forces generic/skb mode (needed
    /// on veth-style devices whose native XDP path does not deliver XSK
    /// redirects); otherwise native driver mode is tried first with a
    /// generic fallback. Returns the resulting mode; `AlreadyAttached`
    /// means another program is installed and we did not touch it.
    ///
    /// Uses `bpf_link_create` (exclusive attach, auto-detach on close);
    /// falls back to netlink `IFLA_XDP` on older kernels.
    pub(crate) fn attach(&mut self, prefer_generic: bool) -> io::Result<AttachMode> {
        let attempts: &[u32] = if prefer_generic {
            &[XDP_FLAGS_SKB_MODE]
        } else {
            &[XDP_FLAGS_DRV_MODE, XDP_FLAGS_SKB_MODE]
        };

        let mut last_err: Option<io::Error> = None;
        for &mode in attempts {
            match bpf_link_xdp(self.prog_fd.as_raw_fd(), self.ifindex, mode) {
                Ok(link_fd) => {
                    // SAFETY: freshly created fd from the kernel.
                    self.link_fd = Some(unsafe { OwnedFd::from_raw_fd(link_fd as RawFd) });
                    return Ok(if mode == XDP_FLAGS_SKB_MODE {
                        AttachMode::Generic
                    } else {
                        AttachMode::Driver
                    });
                }
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
                    return Ok(AttachMode::AlreadyAttached);
                }
                Err(e)
                    if e.raw_os_error() == Some(libc::EOPNOTSUPP)
                        || e.raw_os_error() == Some(libc::EINVAL) =>
                {
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }

        // Pre-5.7 kernels (or cases where bpf_link is unavailable): netlink.
        for &mode in attempts {
            match set_xdp(
                self.ifindex,
                self.prog_fd.as_raw_fd(),
                mode | XDP_FLAGS_UPDATE_IF_NOEXIST,
            ) {
                Ok(()) => {
                    self.detach_mode = Some(mode);
                    return Ok(if mode == XDP_FLAGS_SKB_MODE {
                        AttachMode::Generic
                    } else {
                        AttachMode::Driver
                    });
                }
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
                    return Ok(AttachMode::AlreadyAttached);
                }
                Err(e) => last_err = Some(e),
            }
        }

        Err(last_err.unwrap_or_else(|| io::Error::other("XDP attach unsupported")))
    }

    pub(crate) fn detach(&mut self) {
        if let Some(mode) = self.detach_mode.take() {
            // Netlink fallback path: explicit detach. The bpf_link path
            // detaches when `link_fd` closes (Drop).
            let _ = set_xdp(self.ifindex, -1, mode);
        }
    }
}

impl Drop for XskProgram {
    fn drop(&mut self) {
        self.detach();
    }
}

// ---------------------------------------------------------------------------
// Netlink helpers
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct IfInfomsg {
    family: u8,
    pad: u8,
    itype: u16,
    index: i32,
    flags: u32,
    change: u32,
}

#[inline]
fn align4(n: usize) -> usize {
    (n + 3) & !3
}

#[inline]
fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_ne_bytes());
}

#[inline]
fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

#[inline]
fn put_i32(buf: &mut [u8], off: usize, v: i32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

/// `RTM_SETLINK` + `IFLA_XDP{IFLA_XDP_FD, IFLA_XDP_FLAGS}`.
///
/// `prog_fd == -1` detaches. With `XDP_FLAGS_UPDATE_IF_NOEXIST` the kernel
/// atomically fails with `EEXIST` if a program is already attached
/// (`IFLA_XDP_EXPECTED_FD == 0`, kernel >= 5.4).
fn set_xdp(ifindex: u32, prog_fd: RawFd, xdp_flags: u32) -> io::Result<()> {
    let mode_flags = xdp_flags & (XDP_FLAGS_SKB_MODE | XDP_FLAGS_DRV_MODE);
    let noexist = xdp_flags & XDP_FLAGS_UPDATE_IF_NOEXIST != 0 && prog_fd >= 0;

    // nlmsghdr + ifinfomsg + IFLA_XDP nest {FD, FLAGS, EXPECTED_FD}
    let mut buf = vec![0u8; 64];
    // nlmsghdr
    put_u32(&mut buf, 0, 0); // nlmsg_len, patched below
    put_u16(&mut buf, 4, RTM_SETLINK);
    put_u16(&mut buf, 6, NLM_F_REQUEST | NLM_F_ACK);
    put_u32(&mut buf, 8, NL_SEQ.fetch_add(1, Ordering::Relaxed));
    put_u32(&mut buf, 12, 0); // nlmsg_pid
    // ifinfomsg
    let ifi = IfInfomsg { family: 0, pad: 0, itype: 0, index: ifindex as i32, flags: 0, change: 0xFFFF_FFFF };
    buf[16] = ifi.family;
    buf[17] = ifi.pad;
    put_u16(&mut buf, 18, ifi.itype);
    put_i32(&mut buf, 20, ifi.index);
    put_u32(&mut buf, 24, ifi.flags);
    put_u32(&mut buf, 28, ifi.change);

    let mut off = 32usize;
    let nest_start = off;
    put_u16(&mut buf, off, 0); // rta_len, patched below
    put_u16(&mut buf, off + 2, IFLA_XDP);
    off += 4;
    put_u16(&mut buf, off, 8);
    put_u16(&mut buf, off + 2, IFLA_XDP_FD);
    put_u32(&mut buf, off + 4, prog_fd as u32);
    off += 8;
    if mode_flags != 0 {
        put_u16(&mut buf, off, 8);
        put_u16(&mut buf, off + 2, IFLA_XDP_FLAGS);
        put_u32(&mut buf, off + 4, mode_flags);
        off += 8;
    }
    if noexist {
        put_u16(&mut buf, off, 8);
        put_u16(&mut buf, off + 2, IFLA_XDP_EXPECTED_FD);
        put_u32(&mut buf, off + 4, 0);
        off += 8;
    }
    put_u16(&mut buf, nest_start, (off - nest_start) as u16); // nest rta_len
    put_u32(&mut buf, 0, align4(off) as u32); // nlmsg_len
    buf.truncate(align4(off));

    let fd = unsafe { libc::socket(AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, NETLINK_ROUTE) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut sa: libc::sockaddr_nl = unsafe { mem::zeroed() };
    sa.nl_family = AF_NETLINK as libc::sa_family_t;
    let ret = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &sa as *const libc::sockaddr_nl as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    let seq = read_u32(&buf, 8);
    let ret = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            buf.as_ptr() as *const libc::c_void,
            buf.len(),
            0,
            &sa as *const libc::sockaddr_nl as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    // Wait for the ACK (NLMSG_ERROR) for our sequence number.
    let mut rx = vec![0u8; 4096];
    loop {
        let n = unsafe { libc::recv(fd.as_raw_fd(), rx.as_mut_ptr() as *mut libc::c_void, rx.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        let mut off = 0usize;
        while off + 16 <= n {
            let len = read_u32(&rx, off) as usize;
            if len < 16 {
                break;
            }
            let mtype = read_u16(&rx, off + 4);
            let mseq = read_u32(&rx, off + 8);
            if mtype == NLMSG_ERROR && mseq == seq {
                let error = read_i32(&rx, off + 16);
                if error == 0 {
                    return Ok(());
                }
                return Err(io::Error::from_raw_os_error(-error));
            }
            off += align4(len);
        }
    }
}

#[inline]
fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}

#[inline]
fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_ne_bytes(buf[off..off + 2].try_into().unwrap())
}

#[inline]
fn read_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}
