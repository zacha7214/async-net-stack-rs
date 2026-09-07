//! Raw AF_XDP / BPF kernel ABI: constants and structs, hand-written.
//!
//! Everything in here mirrors the kernel UAPI (`linux/if_xdp.h`,
//! `linux/bpf.h`) but is declared locally so the crate stays libc-only.
//! The ring offsets returned by `XDP_MMAP_OFFSETS` are used at runtime, so
//! the ring struct layout below is only a size reference, not an ABI
//! assumption.

use std::io;
use std::mem;
use std::os::fd::RawFd;

// ---------------------------------------------------------------------------
// AF_XDP socket layer
// ---------------------------------------------------------------------------

pub const AF_XDP: libc::c_int = 44;
pub const SOL_XDP: libc::c_int = 283;

/// `setsockopt`/`getsockopt` options on `SOL_XDP`
/// (linux/if_xdp.h "XDP socket options" — do not reorder).
pub(crate) const XDP_MMAP_OFFSETS: libc::c_int = 1;
pub(crate) const XDP_RX_RING: libc::c_int = 2;
pub(crate) const XDP_TX_RING: libc::c_int = 3;
pub(crate) const XDP_UMEM_REG: libc::c_int = 4;
pub(crate) const XDP_UMEM_FILL_RING: libc::c_int = 5;
pub(crate) const XDP_UMEM_COMPLETION_RING: libc::c_int = 6;
pub(crate) const XDP_STATISTICS: libc::c_int = 7;
pub(crate) const XDP_OPTIONS: libc::c_int = 8;

/// `sockaddr_xdp.sxdp_flags` bind flags.
pub const XDP_SHARED_UMEM: libc::c_ushort = 1 << 0;
pub const XDP_COPY: libc::c_ushort = 1 << 1;
pub const XDP_ZEROCOPY: libc::c_ushort = 1 << 2;
pub const XDP_USE_NEED_WAKEUP: libc::c_ushort = 1 << 3;

/// `xdp_umem_reg.flags`.
pub const XDP_UMEM_UNALIGNED_CHUNK_FLAG: u32 = 1 << 0;

/// `xdp_options.flags` (getsockopt `XDP_OPTIONS`).
pub(crate) const XDP_OPTIONS_ZEROCOPY: u32 = 1 << 0;

/// Shared ring header `flags` bits.
pub(crate) const XDP_RING_NEED_WAKEUP: u32 = 1 << 0;

/// `mmap(2)` offsets that select a ring (linux/if_xdp.h "Pgoff for mmaping
/// the rings"). These are the *file offsets* passed to mmap, not the
/// struct-relative offsets returned by `XDP_MMAP_OFFSETS`.
pub(crate) const XDP_PGOFF_RX_RING: libc::off_t = 0;
pub(crate) const XDP_PGOFF_TX_RING: libc::off_t = 0x80000000;
pub(crate) const XDP_UMEM_PGOFF_FILL_RING: libc::off_t = 0x100000000;
pub(crate) const XDP_UMEM_PGOFF_COMPLETION_RING: libc::off_t = 0x180000000;

/// Full UAPI shape of `struct xdp_umem_reg` (v3: includes
/// `tx_metadata_len`). Older kernels accept the shorter v1/v2 optlens and
/// simply read the prefix; passing the full 32 bytes with
/// `tx_metadata_len = 0` is safe everywhere.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct XdpUmemReg {
    pub addr: u64,
    pub len: u64,
    pub chunk_size: u32,
    pub headroom: u32,
    pub flags: u32,
    pub tx_metadata_len: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct XdpDesc {
    pub addr: u64,
    pub len: u32,
    pub options: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct XdpRingOffset {
    pub producer: u64,
    pub consumer: u64,
    pub desc: u64,
    pub flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct XdpMmapOffsets {
    pub rx: XdpRingOffset,
    pub tx: XdpRingOffset,
    pub fr: XdpRingOffset,
    pub cr: XdpRingOffset,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct XdpOptions {
    pub flags: u32,
}

/// `getsockopt(XDP_STATISTICS)`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct XdpStatistics {
    pub rx_dropped: u64,
    pub rx_invalid_descs: u64,
    pub tx_invalid_descs: u64,
    pub rx_ring_full: u64,
    pub rx_fill_ring_empty_descs: u64,
    pub tx_ring_empty_descs: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct SockAddrXdp {
    pub family: libc::c_ushort,
    pub flags: libc::c_ushort,
    pub ifindex: u32,
    pub queue_id: u32,
    pub shared_umem_fd: u32,
}

/// `setsockopt`/`getsockopt` on `SOL_XDP` with a typed value.
pub(crate) fn set_sockopt<T>(fd: RawFd, opt: libc::c_int, value: &T) -> io::Result<()> {
    let ret = unsafe {
        libc::setsockopt(
            fd,
            SOL_XDP,
            opt,
            value as *const T as *const libc::c_void,
            mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn get_sockopt<T>(fd: RawFd, opt: libc::c_int, value: &mut T) -> io::Result<()> {
    let mut len = mem::size_of::<T>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            SOL_XDP,
            opt,
            value as *mut T as *mut libc::c_void,
            &mut len,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Kick the kernel on an AF_XDP socket:
/// `sendto(fd, NULL, 0, MSG_DONTWAIT, NULL, 0)`.
///
/// Needed whenever a ring the kernel is waiting on goes from empty to
/// non-empty (fill ring after refills, TX ring after submissions) while
/// `XDP_USE_NEED_WAKEUP` is in effect — and unconditionally in copy mode
/// without need-wakeup. Errors are ignored (the kick is a hint; the
/// kernel re-arms itself on the next poll).
pub(crate) fn kick(fd: RawFd) {
    unsafe {
        libc::sendto(
            fd,
            std::ptr::null(),
            0,
            libc::MSG_DONTWAIT,
            std::ptr::null(),
            0,
        );
    }
}

// ---------------------------------------------------------------------------
// bpf(2): hand-written minimal `union bpf_attr` subset + instruction builder
// ---------------------------------------------------------------------------

pub(crate) const BPF_MAP_CREATE: libc::c_int = 0;
pub(crate) const BPF_MAP_UPDATE_ELEM: libc::c_int = 2;
pub(crate) const BPF_PROG_LOAD: libc::c_int = 5;
pub(crate) const BPF_LINK_CREATE: libc::c_int = 28;

/// `bpf_link_create` attach types / flags (linux/bpf.h).
pub(crate) const BPF_XDP: u32 = 37;
pub(crate) const XDP_FLAGS_SKB_MODE: u32 = 1 << 1;
pub(crate) const XDP_FLAGS_DRV_MODE: u32 = 1 << 2;

pub(crate) const BPF_MAP_TYPE_XSKMAP: u32 = 17;
pub(crate) const BPF_MAP_TYPE_ARRAY: u32 = 2;
pub(crate) const BPF_PROG_TYPE_XDP: u32 = 6;

/// `src_reg` pseudo values for `lddw` (linux/bpf.h).
pub(crate) const BPF_PSEUDO_MAP_FD: u8 = 1;
pub(crate) const BPF_PSEUDO_MAP_VALUE: u8 = 2;
/// `bpf_redirect_map` helper id.
pub(crate) const BPF_REDIRECT_MAP: i32 = 51;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BpfInsn {
    pub code: u8,
    /// `dst_reg` in the low nibble, `src_reg` in the high nibble.
    pub dst_src: u8,
    pub off: i16,
    pub imm: i32,
}

impl BpfInsn {
    pub(crate) const fn mov64_imm(dst: u8, imm: i32) -> Self {
        Self { code: 0xB7, dst_src: dst, off: 0, imm }
    }

    /// `if r{dst} == imm goto pc+off` (BPF_JMP | BPF_JEQ | BPF_K).
    pub(crate) const fn jeq_imm(dst: u8, imm: i32, off: i16) -> Self {
        Self { code: 0x15, dst_src: dst, off, imm }
    }

    pub(crate) const fn ldx_w(dst: u8, src: u8, off: i16) -> Self {
        Self { code: 0x61, dst_src: dst | (src << 4), off, imm: 0 }
    }

    /// `lddw dst, map_fd` with `BPF_PSEUDO_MAP_FD` — expands to two insns.
    pub(crate) const fn ld_map_fd(dst: u8, fd: u32) -> [Self; 2] {
        [
            Self { code: 0x18, dst_src: dst | (BPF_PSEUDO_MAP_FD << 4), off: 0, imm: fd as i32 },
            Self { code: 0x00, dst_src: 0, off: 0, imm: ((fd as u64) >> 32) as i32 },
        ]
    }

    /// `lddw dst, <address of map value>` with `BPF_PSEUDO_MAP_VALUE`
    /// (direct access to an ARRAY map element, e.g. `map[0]` in C; the
    /// verifier substitutes the real value address). `offset` is the byte
    /// offset of the element inside the map value area.
    pub(crate) const fn ld_map_value(dst: u8, fd: u32, offset: u32) -> [Self; 2] {
        [
            Self { code: 0x18, dst_src: dst | (BPF_PSEUDO_MAP_VALUE << 4), off: 0, imm: fd as i32 },
            Self { code: 0x00, dst_src: 0, off: 0, imm: offset as i32 },
        ]
    }

    pub(crate) const fn call(helper: i32) -> Self {
        Self { code: 0x85, dst_src: 0, off: 0, imm: helper }
    }

    pub(crate) const fn exit() -> Self {
        Self { code: 0x95, dst_src: 0, off: 0, imm: 0 }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BpfMapCreate {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BpfLinkCreate {
    prog_fd: u32,
    target_ifindex: u32,
    attach_type: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BpfMapElem {
    map_fd: u32,
    pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BpfProgLoad {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
    prog_name: [u8; 16],
    prog_ifindex: u32,
    expected_attach_type: u32,
}

/// Minimal `union bpf_attr`: every kernel sub-struct starts at offset 0.
#[repr(C)]
pub(crate) union BpfAttr {
    map_create: BpfMapCreate,
    prog_load: BpfProgLoad,
    map_elem: BpfMapElem,
    link_create: BpfLinkCreate,
}

pub(crate) fn bpf(cmd: libc::c_int, attr: BpfAttr) -> io::Result<libc::c_long> {
    let mut attr = attr;
    let ret = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            cmd,
            &mut attr as *mut BpfAttr,
            mem::size_of::<BpfAttr>(),
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// Create a BPF map; returns the map fd.
pub(crate) fn create_map(
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
) -> io::Result<libc::c_long> {
    // SAFETY: zeroed union of plain scalars is a valid value.
    let mut attr: BpfAttr = unsafe { mem::zeroed() };
    attr.map_create = BpfMapCreate {
        map_type,
        key_size,
        value_size,
        max_entries,
        map_flags: 0,
    };
    bpf(BPF_MAP_CREATE, attr)
}

/// Create an XSKMAP with `max_queues` entries; returns the map fd.
pub(crate) fn create_xskmap(max_queues: u32) -> io::Result<libc::c_long> {
    create_map(BPF_MAP_TYPE_XSKMAP, 4, 4, max_queues)
}

/// `bpf_map_update_elem` with a u32 key and value.
pub(crate) fn map_set_u32(map_fd: RawFd, key: u32, value: u32) -> io::Result<()> {
    let k = key as u64;
    let v = value as u64;
    // SAFETY: zeroed union of plain scalars is a valid value.
    let mut attr: BpfAttr = unsafe { mem::zeroed() };
    attr.map_elem = BpfMapElem {
        map_fd: map_fd as u32,
        pad: 0,
        key: &k as *const u64 as u64,
        value: &v as *const u64 as u64,
        flags: 0,
    };
    bpf(BPF_MAP_UPDATE_ELEM, attr)?;
    Ok(())
}

/// Insert `socket_fd` into an XSKMAP at key `queue_id`.
pub(crate) fn xskmap_set(map_fd: RawFd, queue_id: u32, socket_fd: RawFd) -> io::Result<()> {    let key = queue_id as u64;
    let value = socket_fd as u64;
    // SAFETY: zeroed union of plain scalars is a valid value.
    let mut attr: BpfAttr = unsafe { mem::zeroed() };
    attr.map_elem = BpfMapElem {
        map_fd: map_fd as u32,
        pad: 0,
        key: &key as *const u64 as u64,
        value: &value as *const u64 as u64,
        flags: 0,
    };
    bpf(BPF_MAP_UPDATE_ELEM, attr)?;
    Ok(())
}

/// Attach an XDP program with `bpf_link_create` (`BPF_XDP`,
/// kernel >= 5.7). The link detaches automatically when the returned fd
/// closes. Returns the link fd.
pub(crate) fn bpf_link_xdp(
    prog_fd: RawFd,
    ifindex: u32,
    mode_flags: u32,
) -> io::Result<libc::c_long> {
    // SAFETY: zeroed union of plain scalars is a valid value.
    let mut attr: BpfAttr = unsafe { mem::zeroed() };
    attr.link_create = BpfLinkCreate {
        prog_fd: prog_fd as u32,
        target_ifindex: ifindex,
        attach_type: BPF_XDP,
        flags: mode_flags,
    };
    bpf(BPF_LINK_CREATE, attr)
}

/// Load a BPF program from a raw instruction stream.
///
/// On verifier failure the returned error embeds the (truncated) verifier log.
pub(crate) fn load_prog(insns: &[BpfInsn], name: &[u8]) -> io::Result<libc::c_long> {
    let mut log = vec![0u8; 64 * 1024];
    let mut prog_name = [0u8; 16];
    let name_len = name.len().min(prog_name.len());
    prog_name[..name_len].copy_from_slice(&name[..name_len]);
    let license = b"GPL\0".as_ptr();

    // SAFETY: zeroed union of plain scalars is a valid value.
    let mut attr: BpfAttr = unsafe { mem::zeroed() };
    attr.prog_load = BpfProgLoad {
        prog_type: BPF_PROG_TYPE_XDP,
        insn_cnt: insns.len() as u32,
        insns: insns.as_ptr() as u64,
        license: license as u64,
        log_level: 1,
        log_size: log.len() as u32,
        log_buf: log.as_mut_ptr() as u64,
        kern_version: 0,
        prog_flags: 0,
        prog_name,
        prog_ifindex: 0,
        expected_attach_type: 0,
    };

    match bpf(BPF_PROG_LOAD, attr) {
        Ok(fd) => Ok(fd),
        Err(e) => {
            let tail = String::from_utf8_lossy(&log);
            let tail = tail.trim_end_matches('\0').trim();
            let tail = if tail.len() > 512 { &tail[tail.len() - 512..] } else { tail };
            Err(io::Error::other(format!("BPF_PROG_LOAD: {e}; verifier log: {tail}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insn_encoding_matches_uapi() {
        assert_eq!(
            BpfInsn::mov64_imm(0, 2),
            BpfInsn { code: 0xB7, dst_src: 0x00, off: 0, imm: 2 }
        );
        assert_eq!(
            BpfInsn::ld_map_fd(1, 0x0000_0042),
            [
                BpfInsn { code: 0x18, dst_src: 0x11, off: 0, imm: 0x42 },
                BpfInsn { code: 0x00, dst_src: 0x00, off: 0, imm: 0 },
            ]
        );
        assert_eq!(
            BpfInsn::jeq_imm(2, 0, 5),
            BpfInsn { code: 0x15, dst_src: 0x02, off: 5, imm: 0 }
        );
        assert_eq!(
            BpfInsn::call(BPF_REDIRECT_MAP),
            BpfInsn { code: 0x85, dst_src: 0, off: 0, imm: 51 }
        );
        assert_eq!(
            BpfInsn::exit(),
            BpfInsn { code: 0x95, dst_src: 0, off: 0, imm: 0 }
        );
        assert_eq!(
            BpfInsn::ldx_w(2, 1, 16),
            BpfInsn { code: 0x61, dst_src: 0x12, off: 16, imm: 0 }
        );
    }

    #[test]
    fn abi_sizes_match_uapi() {
        assert_eq!(mem::size_of::<XdpDesc>(), 16);
        assert_eq!(mem::size_of::<XdpUmemReg>(), 32);
        assert_eq!(mem::size_of::<XdpMmapOffsets>(), 128);
        assert_eq!(mem::size_of::<XdpStatistics>(), 48);
        assert_eq!(mem::size_of::<SockAddrXdp>(), 16);
        assert_eq!(mem::size_of::<BpfInsn>(), 8);
    }
}
