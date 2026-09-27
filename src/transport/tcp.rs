//! Checked IPv4/TCP wire format (RFC 9293). Unknown well-formed TCP options are
//! skipped; MSS is exposed. IPv4 options/fragments and TCP urgent data are not
//! implemented by the initial connection engine.
use crate::{
    device::PacketBuf,
    net::{checksum, finish_sum, sum_words},
};
use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4},
};

pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;
pub const URG: u8 = 0x20;
pub const ECE: u8 = 0x40;
pub const CWR: u8 = 0x80;

#[derive(Clone, Copy, Debug)]
pub struct Segment<'a> {
    pub source: SocketAddrV4,
    pub destination: SocketAddrV4,
    pub sequence: u32,
    pub acknowledgment: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: Option<u16>,
    pub payload: &'a [u8],
}

impl Segment<'_> {
    pub fn sequence_len(&self) -> u32 {
        self.payload.len() as u32
            + u32::from(self.flags & SYN != 0)
            + u32::from(self.flags & FIN != 0)
    }
}

pub fn ipv4_checksum(source: Ipv4Addr, destination: Ipv4Addr, segment: &[u8]) -> u16 {
    finish_sum(
        sum_words(&source.octets())
            + sum_words(&destination.octets())
            + 6
            + segment.len() as u64
            + sum_words(segment),
    )
}

pub fn parse_ipv4(bytes: &[u8]) -> io::Result<Segment<'_>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid IPv4 TCP segment");
    if bytes.len() < 40 || bytes[0] != 0x45 || bytes[9] != 6 || bytes[8] == 0 {
        return Err(invalid());
    }
    let total = read16(bytes, 2) as usize;
    if total < 40
        || total > bytes.len()
        || checksum(&bytes[..20]) != 0
        || read16(bytes, 6) & !0x4000 != 0
    {
        return Err(invalid());
    }
    let source = Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]);
    let destination = Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19]);
    let tcp = &bytes[20..total];
    let header_len = (tcp[12] >> 4) as usize * 4;
    if header_len < 20 || header_len > tcp.len() || ipv4_checksum(source, destination, tcp) != 0 {
        return Err(invalid());
    }
    let mut mss = None;
    let mut offset = 20;
    while offset < header_len {
        let kind = tcp[offset];
        if kind == 0 {
            break;
        }

        if kind == 1 {
            offset += 1;
            continue;
        }

        if offset + 2 > header_len {
            return Err(invalid());
        }

        let len = tcp[offset + 1] as usize;
        if len < 2 || offset + len > header_len {
            return Err(invalid());
        }

        if kind == 2 {
            if len != 4 || mss.is_some() {
                return Err(invalid());
            }
            let value = read16(tcp, offset + 2);
            if value == 0 {
                return Err(invalid());
            }
            mss = Some(value);
        }

        offset += len;
    }

    Ok(Segment {
        source: SocketAddrV4::new(source, read16(tcp, 0)),
        destination: SocketAddrV4::new(destination, read16(tcp, 2)),
        sequence: read32(tcp, 4),
        acknowledgment: read32(tcp, 8),
        flags: tcp[13],
        window: read16(tcp, 14),
        mss,
        payload: &tcp[header_len..],
    })
}

/// Build one checksummed IPv4 datagram. An MSS option is emitted only on SYN.
pub fn build_ipv4(frame: &mut PacketBuf, segment: &Segment<'_>) -> io::Result<()> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "TCP segment exceeds capacity or has invalid MSS",
        )
    };

    if segment.mss == Some(0) || (segment.mss.is_some() && segment.flags & SYN == 0) {
        return Err(invalid());
    }

    let tcp_header = if segment.mss.is_some() { 24 } else { 20 };
    let total = segment
        .payload
        .len()
        .checked_add(20 + tcp_header)
        .filter(|&n| n <= u16::MAX as usize && n <= frame.tail_capacity())
        .ok_or_else(invalid)?;
    frame.set_len(total);

    let bytes = frame.as_mut_packet();
    bytes[..20 + tcp_header].fill(0);
    bytes[0] = 0x45;
    bytes[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    bytes[6] = 0x40; // DF; PMTU adaptation is not yet implemented.
    bytes[8] = 64;
    bytes[9] = 6;
    bytes[12..16].copy_from_slice(&segment.source.ip().octets());
    bytes[16..20].copy_from_slice(&segment.destination.ip().octets());

    let sum = checksum(&bytes[..20]);
    bytes[10..12].copy_from_slice(&sum.to_be_bytes());

    let tcp = &mut bytes[20..];
    tcp[0..2].copy_from_slice(&segment.source.port().to_be_bytes());
    tcp[2..4].copy_from_slice(&segment.destination.port().to_be_bytes());
    tcp[4..8].copy_from_slice(&segment.sequence.to_be_bytes());
    tcp[8..12].copy_from_slice(&segment.acknowledgment.to_be_bytes());
    tcp[12] = ((tcp_header / 4) as u8) << 4;
    tcp[13] = segment.flags;
    tcp[14..16].copy_from_slice(&segment.window.to_be_bytes());

    if let Some(mss) = segment.mss {
        tcp[20..22].copy_from_slice(&[2, 4]);
        tcp[22..24].copy_from_slice(&mss.to_be_bytes());
    }

    tcp[tcp_header..].copy_from_slice(segment.payload);

    let sum = ipv4_checksum(*segment.source.ip(), *segment.destination.ip(), tcp);
    tcp[16..18].copy_from_slice(&sum.to_be_bytes());

    Ok(())
}

fn read16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}
fn read32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Serial-number arithmetic is valid for distances below half the sequence space.
pub(crate) fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
